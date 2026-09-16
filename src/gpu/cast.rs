//! PipeWire screencast streams, owned by the GPU process.
//!
//! The core decides what to cast and when (portal D-Bus, targets, frame pacing) and records
//! the frame's elements into a `Target::Cast` frame. Everything that touches PipeWire or
//! buffer memory lives here: stream negotiation, buffer allocation, damage tracking, rendering
//! into the dequeued buffer and cursor metadata. Events flow back as `CastEvent`s.

use std::cell::RefCell;
use std::cmp::min;
use std::collections::HashMap;
use std::io::Cursor;
use std::iter::zip;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::Duration;
use std::{mem, slice};

use anyhow::{bail, ensure, Context as _};
use calloop::channel::{Channel as CalloopChannel, Sender};
use calloop::RegistrationToken;
use pipewire::context::ContextRc;
use pipewire::core::{CoreRc, PW_ID_CORE};
use pipewire::loop_::Timeout;
use pipewire::main_loop::MainLoopRc;
use pipewire::properties::PropertiesBox;
use pipewire::spa::buffer::DataType;
use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
use pipewire::spa::param::format_utils::parse_format;
use pipewire::spa::param::video::{VideoFormat, VideoInfoRaw};
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::deserialize::PodDeserializer;
use pipewire::spa::pod::serialize::PodSerializer;
use pipewire::spa::pod::{self, ChoiceValue, Pod, PodPropFlags, Property, PropertyFlags};
use pipewire::spa::sys::*;
use pipewire::spa::utils::{
    Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes,
};
use pipewire::spa::{self};
use pipewire::stream::{Stream, StreamFlags, StreamListener, StreamRc, StreamState};
use pipewire::sys::{pw_buffer, pw_check_library_version, pw_stream_queue_buffer};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::GbmDevice;
use smithay::backend::allocator::{Buffer as _, Fourcc};
use smithay::backend::renderer::damage::OutputDamageTracker;
use smithay::backend::renderer::gles::{GlesRenderer, GlesTexture};
use smithay::backend::renderer::sync::SyncPoint;
use smithay::backend::renderer::{Bind, Color32F, ExportMem, Frame as _, Offscreen, Renderer};
use smithay::output::OutputModeSource;
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::{Interest, LoopHandle, Mode, PostAction};
use smithay::reexports::gbm::Modifier;
use smithay::reexports::rustix::fd::OwnedFd;
use smithay::reexports::rustix::fs::{
    fcntl_add_seals, ftruncate, memfd_create, MemfdFlags, SealFlags,
};
use smithay::reexports::rustix::mm::{mmap, munmap, MapFlags, ProtFlags};
use smithay::utils::{Buffer, DeviceFd, Physical, Point, Scale, Size, Transform};

use super::drm::allocate_gbm_dmabuf;
use super::exec::{Deferred, Executor, Tables};
use super::protocol::{CastCursorMode, CastEvent, Command, CursorMeta};
use super::scene::{self, split_elements, ElementTracks, SceneElement};
use super::server::Server;

const SHM_BLOCKS: usize = 1;
const SHM_BYTES_PER_PIXEL: usize = 4;

const CURSOR_FORMAT: spa_video_format = SPA_VIDEO_FORMAT_BGRA;
const CURSOR_BPP: u32 = 4;
const CURSOR_WIDTH: u32 = 384;
const CURSOR_HEIGHT: u32 = 384;
const CURSOR_BITMAP_SIZE: usize = (CURSOR_WIDTH * CURSOR_HEIGHT * CURSOR_BPP) as usize;
const CURSOR_META_SIZE: usize =
    mem::size_of::<spa_meta_cursor>() + mem::size_of::<spa_meta_bitmap>() + CURSOR_BITMAP_SIZE;
const BITMAP_META_OFFSET: usize = mem::size_of::<spa_meta_cursor>();
const BITMAP_DATA_OFFSET: usize = mem::size_of::<spa_meta_bitmap>();

/// All screencast streams of this GPU process.
pub struct Casting {
    // Casts are declared (and so dropped) before PipeWire to prevent a double-free.
    casts: HashMap<u64, Cast>,
    pw: Option<PipeWire>,
    loop_handle: LoopHandle<'static, Server>,
    tx: Sender<CastEvent>,
}

struct PipeWire {
    _context: ContextRc,
    core: CoreRc,
    token: RegistrationToken,
}

pub struct StartParams {
    pub stream: u64,
    pub size: Size<i32, Physical>,
    pub refresh: u32,
    pub alpha: bool,
    pub cursor_mode: CastCursorMode,
    /// Dmabuf formats to offer; empty means shm only.
    pub formats: FormatSet,
    pub gbm: Option<GbmDevice<DeviceFd>>,
}

/// Per-frame parameters the core sends before the cast frame itself.
#[derive(Debug, Clone, Copy)]
struct FrameInfo {
    scale: f64,
    target_time_ns: u64,
    cursor: Option<CursorMeta>,
}

pub struct Cast {
    stream_id: u64,
    loop_handle: LoopHandle<'static, Server>,
    // Listener is dropped before Stream to prevent a use-after-free.
    _listener: StreamListener<()>,
    stream: StreamRc,
    formats: FormatSet,
    offer_alpha: bool,
    cursor_mode: CastCursorMode,
    // Incremented once per successful frame, stored in buffer meta.
    sequence_counter: u64,
    inner: Rc<RefCell<CastInner>>,
    tracks: ElementTracks,
    cursor_tracks: ElementTracks,
    pending_info: Option<FrameInfo>,
    pending_cursor: Option<(Size<i32, Physical>, Vec<Command>)>,
}

/// Mutable `Cast` state shared with PipeWire callbacks.
struct CastInner {
    stream_id: u64,
    tx: Sender<CastEvent>,
    is_active: bool,
    node_id: Option<u32>,
    state: CastState,
    refresh: u32,
    min_time_between_frames: Duration,
    dmabufs: HashMap<i64, Dmabuf>,
    shmbufs: HashMap<i64, Shmbuf>,
    /// Buffers dequeued from PipeWire in process of rendering, oldest first. They are queued
    /// back in this order once their `SyncPoint`s are reached.
    rendering_buffers: Vec<(NonNull<pw_buffer>, SyncPoint)>,
    /// Last `CastEvent::State` sent, to avoid repeats.
    last_emitted: Option<EmittedState>,
}

/// (active, ready size, min frame time in ns) as last reported to the core.
type EmittedState = (bool, Option<(i32, i32)>, u64);

#[derive(Debug, Clone, Copy)]
struct DmaNegotiation {
    modifier: Modifier,
    plane_count: i32,
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum CastState {
    // dma_negotiation = Some(_) means DMA sharing
    // dma_negotiation = None    means SHM sharing
    ResizePending {
        pending_size: Size<u32, Physical>,
    },
    ConfirmationPending {
        size: Size<u32, Physical>,
        alpha: bool,
        dma_negotiation: Option<DmaNegotiation>,
    },
    Ready {
        size: Size<u32, Physical>,
        alpha: bool,
        dma_negotiation: Option<DmaNegotiation>,
        // Lazily-initialized to keep the initialization to a single place.
        damage_tracker: Option<OutputDamageTracker>,
        cursor_damage_tracker: Option<OutputDamageTracker>,
        last_cursor_location: Option<Point<i32, Physical>>,
    },
}

fn make_video_params(
    format: VideoFormat,
    modifiers: &[Modifier],
    size: Size<u32, Physical>,
    refresh: u32,
) -> pod::Object {
    let mut properties = vec![
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(
            FormatProperties::VideoSize,
            Rectangle,
            Rectangle {
                width: size.w,
                height: size.h,
            }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Fraction,
            Fraction { num: 0, denom: 1 }
        ),
        pod::property!(
            FormatProperties::VideoMaxFramerate,
            Choice,
            Range,
            Fraction,
            Fraction {
                num: refresh,
                denom: 1000
            },
            Fraction { num: 1, denom: 1 },
            Fraction {
                num: refresh,
                denom: 1000
            }
        ),
        pod::property!(FormatProperties::VideoFormat, Id, format),
    ];

    if !modifiers.is_empty() {
        let dont_fixate = if modifiers.len() > 1 {
            PropertyFlags::DONT_FIXATE
        } else {
            PropertyFlags::empty()
        };
        let flags = PropertyFlags::MANDATORY | dont_fixate;
        let modifiers_i64 = modifiers
            .iter()
            .map(|m| u64::from(*m) as i64)
            .collect::<Vec<_>>();

        let prop = Property {
            key: FormatProperties::VideoModifier.as_raw(),
            flags,
            value: pod::Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: modifiers_i64[0],
                    alternatives: modifiers_i64,
                },
            ))),
        };

        properties.push(prop);
    }

    pod::Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties,
    }
}

fn make_initial_video_params(
    possible_modifiers: &FormatSet,
    size: Size<u32, Physical>,
    refresh: u32,
    alpha: bool,
) -> Vec<pod::Object> {
    let mut rv = Vec::new();

    let mut push_alpha = |alpha| {
        let format = if alpha {
            VideoFormat::BGRA
        } else {
            VideoFormat::BGRx
        };

        let fourcc = if alpha {
            Fourcc::Argb8888
        } else {
            Fourcc::Xrgb8888
        };

        let modifiers: Vec<_> = possible_modifiers
            .iter()
            .filter_map(|f| (f.code == fourcc).then_some(f.modifier))
            .collect();

        trace!("offering: {modifiers:?}");

        if !modifiers.is_empty() {
            rv.push(make_video_params(format, &modifiers, size, refresh));
        }
        rv.push(make_video_params(format, &[], size, refresh));
    };

    if alpha {
        push_alpha(true);
    }
    push_alpha(false);

    rv
}

macro_rules! make_params {
    ($params:ident, $formats:expr, $size:expr, $refresh:expr, $alpha:expr) => {
        let $params = make_initial_video_params($formats, $size, $refresh, $alpha);
        let mut bufs = [const { Vec::new() }; 4]; // Maximum possible params len.
        let mut $params: Vec<_> = $params
            .into_iter()
            .zip(&mut bufs)
            .map(|(obj, buf)| make_pod(buf, obj))
            .collect();
    };
}

impl Casting {
    pub(super) fn new(loop_handle: LoopHandle<'static, Server>) -> Self {
        let (tx, rx): (Sender<CastEvent>, CalloopChannel<CastEvent>) = calloop::channel::channel();
        loop_handle
            .insert_source(rx, |event, _, server: &mut Server| {
                if let calloop::channel::Event::Msg(event) = event {
                    server.on_cast_event(event);
                }
            })
            .unwrap();
        Self {
            casts: HashMap::new(),
            pw: None,
            loop_handle,
            tx,
        }
    }

    /// Drops every stream and the PipeWire connection (after a fatal connection error).
    pub fn reset(&mut self) {
        self.casts.clear();
        if let Some(pw) = self.pw.take() {
            self.loop_handle.remove(pw.token);
        }
    }

    fn pipewire(&mut self) -> anyhow::Result<&PipeWire> {
        if self.pw.is_none() {
            let pw = PipeWire::new(&self.loop_handle, self.tx.clone())
                .context("error initializing PipeWire")?;
            self.pw = Some(pw);
        }
        Ok(self.pw.as_ref().unwrap())
    }

    /// Returns the effective cursor mode.
    pub fn start(&mut self, params: StartParams) -> anyhow::Result<CastCursorMode> {
        ensure!(
            !self.casts.contains_key(&params.stream),
            "stream {} already exists",
            params.stream
        );
        let loop_handle = self.loop_handle.clone();
        let tx = self.tx.clone();
        let pw = self.pipewire()?;
        let cast = pw.start_cast(loop_handle, tx, params)?;
        let cursor_mode = cast.cursor_mode;
        self.casts.insert(cast.stream_id, cast);
        Ok(cursor_mode)
    }

    pub fn configure(
        &mut self,
        stream: u64,
        size: Size<i32, Physical>,
        refresh: u32,
    ) -> anyhow::Result<()> {
        let cast = self.casts.get_mut(&stream).context("unknown stream")?;
        cast.set_refresh(refresh)?;
        cast.ensure_size(size)?;
        Ok(())
    }

    pub fn clear(
        &mut self,
        stream: u64,
        target_time_ns: u64,
        exec: &mut Executor,
    ) -> anyhow::Result<()> {
        let cast = self.casts.get_mut(&stream).context("unknown stream")?;
        let renderer = exec.renderer()?;
        let sent = cast.dequeue_buffer_and_clear(renderer);
        self.report(stream, target_time_ns, sent);
        Ok(())
    }

    /// Tells the core whether a frame it recorded went out, for pacing.
    fn report(&self, stream: u64, target_time_ns: u64, sent: bool) {
        let event = if sent {
            CastEvent::Rendered {
                stream,
                target_time_ns,
            }
        } else {
            CastEvent::Skipped {
                stream,
                target_time_ns,
            }
        };
        if let Err(err) = self.tx.send(event) {
            warn!("error sending cast frame report: {err:?}");
        }
    }

    pub fn stop(&mut self, stream: u64) {
        if let Some(cast) = self.casts.remove(&stream) {
            if let Err(err) = cast.stream.disconnect() {
                warn!("error disconnecting stream: {err:?}");
            }
        }
    }

    pub fn queue_completed_buffers(&mut self, stream: u64) {
        if let Some(cast) = self.casts.get_mut(&stream) {
            cast.queue_completed_buffers();
        }
    }

    /// Renders the cast frames the executor collected from the last batch.
    pub fn handle_deferred(&mut self, deferred: Vec<Deferred>, exec: &mut Executor) {
        let Some(renderer) = exec.renderer.as_mut() else {
            warn!("cast frames without a renderer");
            return;
        };
        let tables = &exec.tables;
        for item in deferred {
            let stream = match &item {
                Deferred::CastInfo { stream, .. }
                | Deferred::CastCursor { stream, .. }
                | Deferred::CastFrame { stream, .. } => *stream,
            };
            // The core may still record for a stream we just stopped.
            let Some(cast) = self.casts.get_mut(&stream) else {
                trace!("cast frame for unknown stream {stream}");
                continue;
            };
            match item {
                Deferred::CastInfo {
                    scale,
                    target_time_ns,
                    cursor,
                    ..
                } => {
                    cast.pending_info = Some(FrameInfo {
                        scale,
                        target_time_ns,
                        cursor,
                    });
                }
                Deferred::CastCursor { size, commands, .. } => {
                    cast.pending_cursor = Some((size, commands));
                }
                Deferred::CastFrame { size, commands, .. } => {
                    let info = cast.pending_info.take().unwrap_or(FrameInfo {
                        scale: 1.0,
                        target_time_ns: 0,
                        cursor: None,
                    });
                    let sent = match cast.render_frame(renderer, tables, info, size, commands) {
                        Ok(sent) => sent,
                        Err(err) => {
                            warn!("error rendering cast frame: {err:?}");
                            false
                        }
                    };
                    self.report(stream, info.target_time_ns, sent);
                }
            }
        }
    }
}

impl PipeWire {
    fn new(
        loop_handle: &LoopHandle<'static, Server>,
        tx: Sender<CastEvent>,
    ) -> anyhow::Result<Self> {
        let main_loop = MainLoopRc::new(None).context("error creating MainLoop")?;
        let context = ContextRc::new(&main_loop, None).context("error creating Context")?;
        let core = context.connect_rc(None).context("error creating Core")?;

        let listener = core
            .add_listener_local()
            .error(move |id, seq, res, message| {
                warn!(id, seq, res, message, "pw error");

                // Reset PipeWire on connection errors.
                if id == PW_ID_CORE && res == -32 {
                    if let Err(err) = tx.send(CastEvent::PipeWireFatal) {
                        warn!("error sending FatalError: {err:?}");
                    }
                }
            })
            .register();
        mem::forget(listener);

        struct AsFdWrapper(MainLoopRc);
        impl AsFd for AsFdWrapper {
            fn as_fd(&self) -> BorrowedFd<'_> {
                self.0.loop_().fd()
            }
        }
        let generic = Generic::new(AsFdWrapper(main_loop), Interest::READ, Mode::Level);
        let token = loop_handle
            .insert_source(generic, move |_, wrapper, _| {
                let _span = tracy_client::span!("pipewire iteration");
                wrapper.0.loop_().iterate(Timeout::None);
                Ok(PostAction::Continue)
            })
            .unwrap();

        Ok(Self {
            _context: context,
            core,
            token,
        })
    }

    fn start_cast(
        &self,
        loop_handle: LoopHandle<'static, Server>,
        tx: Sender<CastEvent>,
        params: StartParams,
    ) -> anyhow::Result<Cast> {
        let _span = tracy_client::span!("PipeWire::start_cast");
        let StartParams {
            stream: stream_id,
            size,
            refresh,
            alpha,
            mut cursor_mode,
            formats,
            gbm,
        } = params;
        let _span = debug_span!("start_cast", %stream_id).entered();

        let tx_ = tx.clone();
        let stop_cast = move || {
            if let Err(err) = tx_.send(CastEvent::Stop { stream: stream_id }) {
                warn!("error sending Stop: {err:?}");
            }
        };
        let tx_ = tx.clone();
        let redraw = move || {
            if let Err(err) = tx_.send(CastEvent::Redraw { stream: stream_id }) {
                warn!("error sending Redraw: {err:?}");
            }
        };
        let redraw_ = redraw.clone();

        let stream = StreamRc::new(
            self.core.clone(),
            "niri-screen-cast-src",
            PropertiesBox::new(),
        )
        .context("error creating Stream")?;

        if cursor_mode == CastCursorMode::Metadata && !pw_version_supports_cursor_metadata() {
            debug!(
                "metadata cursor mode requested, but PipeWire is too old (need >= 1.4.8); \
                 switching to embedded cursor"
            );
            cursor_mode = CastCursorMode::Embedded;
        }

        let pending_size = Size::from((size.w as u32, size.h as u32));

        let formats = if gbm.is_some() {
            formats
        } else {
            debug!("no dmabuf allocator; advertising only shm formats");
            FormatSet::default()
        };

        let inner = Rc::new(RefCell::new(CastInner {
            stream_id,
            tx,
            is_active: false,
            node_id: None,
            state: CastState::ResizePending { pending_size },
            refresh,
            min_time_between_frames: Duration::ZERO,
            dmabufs: HashMap::new(),
            shmbufs: HashMap::new(),
            rendering_buffers: Vec::new(),
            last_emitted: None,
        }));

        let listener =
            stream
                .add_local_listener_with_user_data(())
                .state_changed({
                    let inner = inner.clone();
                    let stop_cast = stop_cast.clone();
                    move |stream, (), old, new| {
                        let _span = debug_span!("state_changed", %stream_id).entered();
                        debug!("{old:?} -> {new:?}");
                        let mut inner = inner.borrow_mut();

                        match new {
                            StreamState::Paused => {
                                if inner.node_id.is_none() {
                                    let id = stream.node_id();
                                    inner.node_id = Some(id);
                                    debug!("sending node id {id}");
                                    if let Err(err) = inner.tx.send(CastEvent::NodeId {
                                        stream: stream_id,
                                        node_id: id,
                                    }) {
                                        warn!("error sending NodeId: {err:?}");
                                        stop_cast();
                                    }
                                }

                                inner.is_active = false;
                            }
                            StreamState::Error(_) => {
                                if inner.is_active {
                                    inner.is_active = false;
                                    stop_cast();
                                }
                            }
                            StreamState::Unconnected => (),
                            StreamState::Connecting => (),
                            StreamState::Streaming => {
                                inner.is_active = true;
                                redraw();
                            }
                        }
                        inner.emit_state();
                    }
                })
                .param_changed({
                    let inner = inner.clone();
                    let stop_cast = stop_cast.clone();
                    let gbm = gbm.clone();
                    let formats = formats.clone();
                    move |stream, (), id, pod| {
                        let id = ParamType::from_raw(id);
                        trace!(%stream_id, ?id, "param_changed");
                        let mut inner = inner.borrow_mut();
                        let inner = &mut *inner;

                        if id != ParamType::Format {
                            return;
                        }

                        let _span = debug_span!("param_changed", %stream_id).entered();

                        let Some(pod) = pod else { return };

                        let (m_type, m_subtype) = match parse_format(pod) {
                            Ok(x) => x,
                            Err(err) => {
                                warn!("error parsing format: {err:?}");
                                return;
                            }
                        };

                        if m_type != MediaType::Video || m_subtype != MediaSubtype::Raw {
                            return;
                        }

                        let mut format = VideoInfoRaw::new();
                        format.parse(pod).unwrap();
                        debug!("got format = {format:?}");

                        let format_size = Size::from((format.size().width, format.size().height));

                        let state = &mut inner.state;
                        if format_size != state.expected_format_size() {
                            if !matches!(&*state, CastState::ResizePending { .. }) {
                                warn!("wrong size, but we're not resizing");
                                stop_cast();
                                return;
                            }

                            debug!("wrong size, waiting");
                            return;
                        }

                        let format_has_alpha = format.format() == VideoFormat::BGRA;
                        let fourcc = if format_has_alpha {
                            Fourcc::Argb8888
                        } else {
                            Fourcc::Xrgb8888
                        };

                        let max_frame_rate = format.max_framerate();
                        let min_frame_time = Duration::from_micros(
                            1_000_000 * u64::from(max_frame_rate.denom)
                                / u64::from(max_frame_rate.num),
                        );
                        inner.min_time_between_frames = min_frame_time;

                        // We have following cases when param_changed:
                        //
                        // 1. Modifier exists and its flags contain DONT_FIXATE
                        //
                        //    Do test allocation, set CastState to ConfirmationPending and send
                        //    param again.
                        //
                        // 2. Modifier exists and it doesn't need fixation
                        //
                        //    Do test allocation to ensure the modifier work, then set CastState to
                        //    Ready. Then set buffer to DMA.
                        //
                        // 3. Modifier doesn't exist
                        //
                        //    Set CastState to Ready and set buffer to SHM.

                        let object = pod.as_object().unwrap();
                        let prop_modifier =
                            object.find_prop(spa::utils::Id(FormatProperties::VideoModifier.0));

                        match prop_modifier {
                            Some(prop_modifier)
                                if prop_modifier.flags().contains(PodPropFlags::DONT_FIXATE) =>
                            {
                                debug!("fixating the modifier");

                                let Some(gbm) = &gbm else {
                                    error!("negotiated dmabuf without gbm");
                                    stop_cast();
                                    return;
                                };

                                let pod_modifier = prop_modifier.value();
                                let Ok((_, modifiers)) =
                                    PodDeserializer::deserialize_from::<Choice<i64>>(
                                        pod_modifier.as_bytes(),
                                    )
                                else {
                                    warn!("wrong modifier property type");
                                    stop_cast();
                                    return;
                                };

                                let ChoiceEnum::Enum { alternatives, .. } = modifiers.1 else {
                                    warn!("wrong modifier choice type");
                                    stop_cast();
                                    return;
                                };

                                let (modifier, plane_count) = match find_preferred_modifier(
                                    gbm,
                                    format_size,
                                    fourcc,
                                    alternatives,
                                ) {
                                    Ok(x) => x,
                                    Err(err) => {
                                        warn!("couldn't find preferred modifier: {err:?}");
                                        stop_cast();
                                        return;
                                    }
                                };

                                debug!(
                                    "allocation successful \
                                     (modifier={modifier:?}, plane_count={plane_count}), \
                                     moving to confirmation pending"
                                );

                                *state = CastState::ConfirmationPending {
                                    size: format_size,
                                    alpha: format_has_alpha,
                                    dma_negotiation: Some(DmaNegotiation {
                                        modifier,
                                        plane_count: plane_count as i32,
                                    }),
                                };
                                inner.emit_state();

                                let o = make_video_params(
                                    format.format(),
                                    &[modifier],
                                    format_size,
                                    inner.refresh,
                                );
                                let mut b = Vec::new();
                                let pod = make_pod(&mut b, o);

                                make_params!(
                                    params,
                                    &formats,
                                    format_size,
                                    inner.refresh,
                                    format_has_alpha
                                );
                                params.insert(0, pod);

                                if let Err(err) = stream.update_params(&mut params) {
                                    warn!("error updating stream params: {err:?}");
                                    stop_cast();
                                }

                                return;
                            }
                            _ => (),
                        }

                        let o1 = if prop_modifier.is_some() {
                            // Verify that alpha and modifier didn't change.
                            let plane_count = match &*state {
                                CastState::ConfirmationPending {
                                    size,
                                    alpha,
                                    dma_negotiation: Some(dma_negotiation),
                                }
                                | CastState::Ready {
                                    size,
                                    alpha,
                                    dma_negotiation: Some(dma_negotiation),
                                    ..
                                } if *alpha == format_has_alpha
                                    && dma_negotiation.modifier
                                        == Modifier::from(format.modifier()) =>
                                {
                                    let size = *size;
                                    let alpha = *alpha;
                                    let dma_negotiation = *dma_negotiation;

                                    let (damage_tracker, cursor_damage_tracker) =
                                        if let CastState::Ready {
                                            damage_tracker,
                                            cursor_damage_tracker,
                                            ..
                                        } = &mut *state
                                        {
                                            (damage_tracker.take(), cursor_damage_tracker.take())
                                        } else {
                                            (None, None)
                                        };

                                    debug!("moving to ready state");

                                    *state = CastState::Ready {
                                        size,
                                        alpha,
                                        dma_negotiation: Some(dma_negotiation),
                                        damage_tracker,
                                        cursor_damage_tracker,
                                        last_cursor_location: None,
                                    };

                                    dma_negotiation.plane_count
                                }
                                _ => {
                                    let Some(gbm) = &gbm else {
                                        error!("negotiated dmabuf without gbm");
                                        stop_cast();
                                        return;
                                    };

                                    // We're negotiating a single modifier, or alpha or modifier
                                    // changed, so we need to do a test allocation.
                                    let (modifier, plane_count) = match find_preferred_modifier(
                                        gbm,
                                        format_size,
                                        fourcc,
                                        vec![format.modifier() as i64],
                                    ) {
                                        Ok(x) => x,
                                        Err(err) => {
                                            warn!("test allocation failed: {err:?}");
                                            stop_cast();
                                            return;
                                        }
                                    };

                                    debug!(
                                        "allocation successful \
                                         (modifier={modifier:?}, plane_count={plane_count}), \
                                         moving to ready"
                                    );

                                    *state = CastState::Ready {
                                        size: format_size,
                                        alpha: format_has_alpha,
                                        dma_negotiation: Some(DmaNegotiation {
                                            modifier,
                                            plane_count: plane_count as i32,
                                        }),
                                        damage_tracker: None,
                                        cursor_damage_tracker: None,
                                        last_cursor_location: None,
                                    };

                                    plane_count as i32
                                }
                            };

                            pod::object!(
                                SpaTypes::ObjectParamBuffers,
                                ParamType::Buffers,
                                Property::new(
                                    SPA_PARAM_BUFFERS_buffers,
                                    pod::Value::Choice(ChoiceValue::Int(Choice(
                                        ChoiceFlags::empty(),
                                        ChoiceEnum::Range {
                                            default: 8,
                                            min: 2,
                                            max: 16
                                        }
                                    ))),
                                ),
                                Property::new(
                                    SPA_PARAM_BUFFERS_blocks,
                                    pod::Value::Int(plane_count)
                                ),
                                Property::new(
                                    SPA_PARAM_BUFFERS_dataType,
                                    pod::Value::Choice(ChoiceValue::Int(Choice(
                                        ChoiceFlags::empty(),
                                        ChoiceEnum::Flags {
                                            default: 1 << DataType::DmaBuf.as_raw(),
                                            flags: vec![1 << DataType::DmaBuf.as_raw()],
                                        },
                                    ))),
                                ),
                            )
                        } else {
                            debug!("negotiated inefficient shm stream, moving to ready state");

                            *state = CastState::Ready {
                                size: format_size,
                                alpha: format_has_alpha,
                                dma_negotiation: None,
                                damage_tracker: None,
                                cursor_damage_tracker: None,
                                last_cursor_location: None,
                            };
                            pod::object!(
                                SpaTypes::ObjectParamBuffers,
                                ParamType::Buffers,
                                Property::new(
                                    SPA_PARAM_BUFFERS_buffers,
                                    pod::Value::Choice(ChoiceValue::Int(Choice(
                                        ChoiceFlags::empty(),
                                        ChoiceEnum::Range {
                                            default: 8,
                                            min: 2,
                                            max: 16
                                        }
                                    ))),
                                ),
                                Property::new(
                                    SPA_PARAM_BUFFERS_blocks,
                                    pod::Value::Int(SHM_BLOCKS as i32),
                                ),
                                Property::new(
                                    SPA_PARAM_BUFFERS_dataType,
                                    pod::Value::Choice(ChoiceValue::Int(Choice(
                                        ChoiceFlags::empty(),
                                        ChoiceEnum::Flags {
                                            default: 1 << DataType::MemFd.as_raw(),
                                            flags: vec![1 << DataType::MemFd.as_raw()],
                                        },
                                    ))),
                                ),
                            )
                        };
                        inner.emit_state();

                        let o2 = pod::object!(
                            SpaTypes::ObjectParamMeta,
                            ParamType::Meta,
                            Property::new(
                                SPA_PARAM_META_type,
                                pod::Value::Id(spa::utils::Id(SPA_META_Header))
                            ),
                            Property::new(
                                SPA_PARAM_META_size,
                                pod::Value::Int(size_of::<spa_meta_header>() as i32)
                            ),
                        );

                        let mut b1 = vec![];
                        let mut b2 = vec![];

                        let mut params = vec![make_pod(&mut b1, o1), make_pod(&mut b2, o2)];

                        let mut b_cursor = vec![];
                        if cursor_mode == CastCursorMode::Metadata {
                            let o_cursor = pod::object!(
                                SpaTypes::ObjectParamMeta,
                                ParamType::Meta,
                                Property::new(
                                    SPA_PARAM_META_type,
                                    pod::Value::Id(spa::utils::Id(SPA_META_Cursor))
                                ),
                                Property::new(
                                    SPA_PARAM_META_size,
                                    pod::Value::Int(CURSOR_META_SIZE as i32)
                                ),
                            );
                            params.push(make_pod(&mut b_cursor, o_cursor));
                        }

                        if let Err(err) = stream.update_params(&mut params) {
                            warn!("error updating stream params: {err:?}");
                            stop_cast();
                        }
                    }
                })
                .add_buffer({
                    let inner = inner.clone();
                    let stop_cast = stop_cast.clone();
                    move |stream, (), buffer| {
                        let _span = debug_span!("add_buffer", %stream_id).entered();

                        match unsafe { inner.borrow_mut().on_add_buffer(gbm.as_ref(), buffer) } {
                            Ok(redraw) => {
                                // During size re-negotiation, the stream sometimes just keeps
                                // running, in which case we may need to force a redraw once we got
                                // a newly sized buffer.
                                if redraw && stream.state() == StreamState::Streaming {
                                    redraw_();
                                }
                            }
                            Err(err) => {
                                warn!("error adding pw buffer: {err:?}");
                                stop_cast();
                            }
                        };
                    }
                })
                .remove_buffer({
                    let inner = inner.clone();
                    move |_stream, (), buffer| {
                        let _span = debug_span!("remove_buffer", %stream_id).entered();

                        unsafe {
                            inner.borrow_mut().on_remove_buffer(buffer);
                        }
                    }
                })
                .register()
                .unwrap();

        trace!("starting pw stream with size={pending_size:?}, refresh={refresh:?}");

        make_params!(params, &formats, pending_size, refresh, alpha);
        stream
            .connect(
                Direction::Output,
                None,
                StreamFlags::DRIVER | StreamFlags::ALLOC_BUFFERS,
                &mut params,
            )
            .context("error connecting stream")?;

        Ok(Cast {
            stream_id,
            loop_handle,
            stream,
            _listener: listener,
            formats,
            offer_alpha: alpha,
            cursor_mode,
            sequence_counter: 0,
            inner,
            tracks: ElementTracks::default(),
            cursor_tracks: ElementTracks::default(),
            pending_info: None,
            pending_cursor: None,
        })
    }
}

impl Cast {
    fn ensure_size(&mut self, size: Size<i32, Physical>) -> anyhow::Result<()> {
        let mut inner = self.inner.borrow_mut();

        let new_size = Size::from((size.w as u32, size.h as u32));

        let state = &mut inner.state;
        if matches!(state, CastState::Ready { size, .. } if *size == new_size) {
            return Ok(());
        }

        if state.pending_size() == Some(new_size) {
            debug!("stream size still hasn't changed");
            return Ok(());
        }

        let _span = tracy_client::span!("Cast::ensure_size");
        debug!("cast size changed, updating stream size");

        *state = CastState::ResizePending {
            pending_size: new_size,
        };
        // Old frames are meaningless at the new size.
        self.tracks.clear();
        inner.emit_state();

        make_params!(
            params,
            &self.formats,
            new_size,
            inner.refresh,
            self.offer_alpha
        );
        self.stream
            .update_params(&mut params)
            .context("error updating stream params")?;

        Ok(())
    }

    fn set_refresh(&mut self, refresh: u32) -> anyhow::Result<()> {
        let mut inner = self.inner.borrow_mut();

        if inner.refresh == refresh {
            return Ok(());
        }

        let _span = tracy_client::span!("Cast::set_refresh");
        debug!("cast FPS changed, updating stream FPS");
        inner.refresh = refresh;

        let size = inner.state.expected_format_size();
        make_params!(params, &self.formats, size, refresh, self.offer_alpha);
        self.stream
            .update_params(&mut params)
            .context("error updating stream params")?;

        Ok(())
    }

    fn queue_completed_buffers(&mut self) {
        let mut inner = self.inner.borrow_mut();

        // We want to queue buffers in order, so find the first still-rendering buffer, and queue
        // everything up to that. Even if there are completed buffers past the first
        // still-rendering buffer, we do not want to queue them, since that would send frames out
        // of order.
        let first_in_progress_idx = inner
            .rendering_buffers
            .iter()
            .position(|(_, sync)| !sync.is_reached())
            .unwrap_or(inner.rendering_buffers.len());

        for (buffer, _) in inner.rendering_buffers.drain(..first_in_progress_idx) {
            trace!("queueing completed buffer");
            unsafe {
                pw_stream_queue_buffer(self.stream.as_raw_ptr(), buffer.as_ptr());
            }
        }
    }

    /// Renders the elements the core recorded for this stream into the next PipeWire buffer.
    /// Returns whether a buffer was submitted.
    fn render_frame(
        &mut self,
        renderer: &mut GlesRenderer,
        tables: &RefCell<Tables>,
        info: FrameInfo,
        size: Size<i32, Physical>,
        commands: Vec<Command>,
    ) -> anyhow::Result<bool> {
        let _span = tracy_client::span!("Cast::render_frame");
        let (cursor_size, cursor_commands) = self.pending_cursor.take().unwrap_or_default();
        let scale = Scale::from(info.scale);
        let cursor_location: Point<i32, Physical> = info
            .cursor
            .map(|c| Point::from(c.location))
            .unwrap_or_default();

        let segments = split_elements(&commands);
        self.tracks.update(&segments);
        let cursor_segments = split_elements(&cursor_commands);
        self.cursor_tracks.update(&cursor_segments);
        let (storages, cursor_storages) = {
            let tables = tables.borrow();
            (
                scene::element_storages(&tables, &segments),
                scene::element_storages(&tables, &cursor_segments),
            )
        };
        let elements = scene::scene_elements(&self.tracks, &segments, &storages, tables);
        let cursor_elements = scene::scene_elements(
            &self.cursor_tracks,
            &cursor_segments,
            &cursor_storages,
            tables,
        );

        let mut inner = self.inner.borrow_mut();
        let CastState::Ready {
            damage_tracker,
            cursor_damage_tracker,
            last_cursor_location,
            size: ready_size,
            ..
        } = &mut inner.state
        else {
            trace!("cast not ready, dropping frame");
            return Ok(false);
        };
        ensure!(
            *ready_size == Size::from((size.w as u32, size.h as u32)),
            "frame size {size:?} does not match stream size {ready_size:?}"
        );
        let damage_tracker = damage_tracker
            .get_or_insert_with(|| OutputDamageTracker::new(size, scale, Transform::Normal));
        let cursor_damage_tracker = cursor_damage_tracker.get_or_insert_with(|| {
            OutputDamageTracker::new(
                Size::from((CURSOR_WIDTH as _, CURSOR_HEIGHT as _)),
                scale,
                Transform::Normal,
            )
        });

        // Size change will drop the damage tracker, but scale change won't, so check it here.
        let OutputModeSource::Static { scale: t_scale, .. } = damage_tracker.mode() else {
            unreachable!();
        };
        if *t_scale != scale {
            *damage_tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
            *cursor_damage_tracker = OutputDamageTracker::new(
                Size::from((CURSOR_WIDTH as _, CURSOR_HEIGHT as _)),
                scale,
                Transform::Normal,
            );
        }

        let mut has_cursor_update = false;
        let mut redraw_cursor = false;

        let (damage, states) = damage_tracker.damage_output(1, &elements).unwrap();
        let has_damage = damage.is_some();

        if self.cursor_mode == CastCursorMode::Metadata {
            let (damage, _states) = cursor_damage_tracker
                .damage_output(1, &cursor_elements)
                .unwrap();
            redraw_cursor = damage.is_some();
            has_cursor_update = redraw_cursor || *last_cursor_location != Some(cursor_location);
        }

        if !has_damage && !has_cursor_update {
            trace!("no damage, skipping frame");
            return Ok(false);
        }
        *last_cursor_location = Some(cursor_location);
        drop(inner);

        let Some(pw_buffer) = (unsafe { NonNull::new(self.stream.dequeue_raw_buffer()) }) else {
            warn!("no available buffer in pw stream, skipping frame");
            return Ok(false);
        };
        let buffer = pw_buffer.as_ptr();

        let mut inner = self.inner.borrow_mut();
        let inner_ = &mut *inner;
        let CastState::Ready {
            damage_tracker,
            alpha,
            ..
        } = &mut inner_.state
        else {
            unreachable!()
        };
        let damage_tracker = damage_tracker.as_mut().unwrap();
        let alpha = *alpha;

        unsafe {
            let spa_buffer = (*buffer).buffer;

            if self.cursor_mode == CastCursorMode::Metadata {
                add_cursor_metadata(
                    renderer,
                    spa_buffer,
                    info.cursor.unwrap_or(CursorMeta {
                        location: (0, 0),
                        hotspot: (0, 0),
                    }),
                    cursor_size,
                    scale,
                    &cursor_elements,
                    redraw_cursor,
                );
            }

            // FIXME: would be good to skip rendering the full frame if only the pointer changed.
            // Unfortunately, I think the OBS PipeWire code needs to be updated first to cleanly
            // allow for that codepath.
            let fd = (*(*spa_buffer).datas).fd;

            let res = match (*(*spa_buffer).datas).type_ {
                x if x == DataType::DmaBuf.as_raw() => {
                    let dmabuf = inner_.dmabufs[&fd].clone();
                    render_to_dmabuf(renderer, damage_tracker, dmabuf, &elements, states)
                        .map(|x| (x, SharingBuf::Dma))
                }
                x if x == DataType::MemFd.as_raw() => {
                    let shmbuf = inner_.shmbufs[&fd].clone();

                    let fourcc = if alpha {
                        Fourcc::Argb8888
                    } else {
                        Fourcc::Xrgb8888
                    };

                    render_to_shmbuf(renderer, damage_tracker, &shmbuf, fourcc, &elements, states)
                        .map(|()| (SyncPoint::signaled(), SharingBuf::Shm(shmbuf)))
                }
                _ => Err(anyhow::anyhow!("unknown data type in render_frame")),
            };

            drop(inner);
            match res {
                Ok((sync_point, buf)) => {
                    mark_buffer_as_good(pw_buffer, &mut self.sequence_counter, buf);
                    trace!("queueing buffer with seq={}", self.sequence_counter);
                    queue_after_sync(
                        &self.loop_handle,
                        &self.inner,
                        &self.stream,
                        pw_buffer,
                        sync_point,
                    );
                    Ok(true)
                }
                Err(err) => {
                    return_unused_buffer(&self.stream, pw_buffer);
                    Err(err.context("error rendering to buffer"))
                }
            }
        }
    }

    fn dequeue_buffer_and_clear(&mut self, renderer: &mut GlesRenderer) -> bool {
        let mut inner = self.inner.borrow_mut();

        // Clear out the damage tracker if we're in Ready state.
        if let CastState::Ready {
            damage_tracker,
            cursor_damage_tracker,
            ..
        } = &mut inner.state
        {
            *damage_tracker = None;
            *cursor_damage_tracker = None;
        };
        drop(inner);
        self.tracks.clear();
        self.cursor_tracks.clear();

        let Some(pw_buffer) = (unsafe { NonNull::new(self.stream.dequeue_raw_buffer()) }) else {
            warn!("no available buffer in pw stream, skipping frame");
            return false;
        };
        let buffer = pw_buffer.as_ptr();

        unsafe {
            let spa_buffer = (*buffer).buffer;

            if self.cursor_mode == CastCursorMode::Metadata {
                add_invisible_cursor(spa_buffer);
            }

            let fd = (*(*spa_buffer).datas).fd;

            let res = match (*(*(*buffer).buffer).datas).type_ {
                x if x == DataType::DmaBuf.as_raw() => {
                    let dmabuf = self.inner.borrow().dmabufs[&fd].clone();
                    clear_dmabuf(renderer, dmabuf).map(|x| (x, SharingBuf::Dma))
                }
                x if x == DataType::MemFd.as_raw() => {
                    let shmbuf = self.inner.borrow().shmbufs[&fd].clone();
                    clear_shmbuf(&shmbuf).map(|()| (SyncPoint::signaled(), SharingBuf::Shm(shmbuf)))
                }
                _ => Err(anyhow::anyhow!(
                    "unknown data type in dequeue_buffer_and_clear"
                )),
            };

            match res {
                Ok((sync_point, buf)) => {
                    mark_buffer_as_good(pw_buffer, &mut self.sequence_counter, buf);
                    trace!("queueing clear buffer with seq={}", self.sequence_counter);
                    queue_after_sync(
                        &self.loop_handle,
                        &self.inner,
                        &self.stream,
                        pw_buffer,
                        sync_point,
                    );
                    true
                }
                Err(err) => {
                    warn!("error clearing buffer: {err:?}");
                    return_unused_buffer(&self.stream, pw_buffer);
                    false
                }
            }
        }
    }
}

/// Queues the buffer to PipeWire once `sync_point` is reached, keeping submission order.
unsafe fn queue_after_sync(
    loop_handle: &LoopHandle<'static, Server>,
    inner: &Rc<RefCell<CastInner>>,
    stream: &StreamRc,
    pw_buffer: NonNull<pw_buffer>,
    sync_point: SyncPoint,
) {
    let _span = tracy_client::span!("Cast::queue_after_sync");

    let mut sync_point = sync_point;
    let sync_fd = match sync_point.export() {
        Some(sync_fd) => Some(sync_fd),
        None => {
            // Either the SyncPoint is pre-signalled (buffer is ready), or exporting a fence fd
            // failed. Without a fd we cannot schedule a queue on completion, so mark the buffer
            // submittable: queueing an incomplete buffer beats getting stuck.
            sync_point = SyncPoint::signaled();
            None
        }
    };

    let stream_id = inner.borrow().stream_id;
    inner
        .borrow_mut()
        .rendering_buffers
        .push((pw_buffer, sync_point));

    match sync_fd {
        None => {
            trace!("sync_fd is None, queueing completed buffers");
            // Same logic as Cast::queue_completed_buffers, without needing the Cast.
            let mut inner = inner.borrow_mut();
            let first_in_progress_idx = inner
                .rendering_buffers
                .iter()
                .position(|(_, sync)| !sync.is_reached())
                .unwrap_or(inner.rendering_buffers.len());
            for (buffer, _) in inner.rendering_buffers.drain(..first_in_progress_idx) {
                pw_stream_queue_buffer(stream.as_raw_ptr(), buffer.as_ptr());
            }
        }
        Some(sync_fd) => {
            trace!("scheduling buffer to queue");
            let source = Generic::new(sync_fd, Interest::READ, Mode::OneShot);
            loop_handle
                .insert_source(source, move |_, _, server: &mut Server| {
                    server.casting.queue_completed_buffers(stream_id);
                    Ok(PostAction::Remove)
                })
                .unwrap();
        }
    }
}

impl CastInner {
    /// Tells the core about activity / readiness changes, once per change.
    fn emit_state(&mut self) {
        let ready_size = match &self.state {
            CastState::Ready { size, .. } => Some((size.w as i32, size.h as i32)),
            _ => None,
        };
        let min_ns = self.min_time_between_frames.as_nanos() as u64;
        let current = (self.is_active, ready_size, min_ns);
        if self.last_emitted == Some(current) {
            return;
        }
        self.last_emitted = Some(current);
        if let Err(err) = self.tx.send(CastEvent::State {
            stream: self.stream_id,
            active: self.is_active,
            ready_size,
            min_frame_time_ns: min_ns,
        }) {
            warn!("error sending cast State: {err:?}");
        }
    }

    unsafe fn on_add_buffer(
        &mut self,
        gbm: Option<&GbmDevice<DeviceFd>>,
        buffer: *mut pw_buffer,
    ) -> anyhow::Result<bool> {
        let CastState::Ready {
            size,
            alpha,
            dma_negotiation,
            ..
        } = self.state
        else {
            trace!("pw stream: add_buffer, but not ready yet");
            return Ok(false);
        };

        match dma_negotiation {
            Some(DmaNegotiation { modifier, .. }) => {
                trace!(
                    "pw stream: add_buffer (dma), size={size:?}, \
                     alpha={alpha}, modifier={modifier:?}"
                );

                let Some(gbm) = gbm else {
                    error!("add_buffer(dma) without gbm");
                    bail!("missing gbm");
                };

                unsafe {
                    let spa_buffer = (*buffer).buffer;

                    let fourcc = if alpha {
                        Fourcc::Argb8888
                    } else {
                        Fourcc::Xrgb8888
                    };

                    let dmabuf = allocate_dmabuf(gbm, size, fourcc, modifier)
                        .context("error allocating dmabuf")?;

                    let plane_count = dmabuf.num_planes();
                    assert_eq!((*spa_buffer).n_datas as usize, plane_count);

                    for (i, (fd, (stride, offset))) in
                        zip(dmabuf.handles(), zip(dmabuf.strides(), dmabuf.offsets())).enumerate()
                    {
                        let spa_data = (*spa_buffer).datas.add(i);
                        assert!((*spa_data).type_ & (1 << DataType::DmaBuf.as_raw()) > 0);

                        (*spa_data).type_ = DataType::DmaBuf.as_raw();

                        // With DMA-BUFs, consumers should ignore the maxsize field, and
                        // producers are allowed to set it to 0.
                        //
                        // https://docs.pipewire.org/page_dma_buf.html
                        (*spa_data).maxsize = 1;
                        (*spa_data).fd = fd.as_raw_fd() as i64;
                        (*spa_data).flags = SPA_DATA_FLAG_READWRITE;

                        let chunk = (*spa_data).chunk;
                        (*chunk).stride = stride as i32;
                        (*chunk).offset = offset;

                        trace!(
                            "pw buffer plane: fd={}, stride={stride}, offset={offset}",
                            (*spa_data).fd
                        );
                    }

                    let fd = (*(*spa_buffer).datas).fd;
                    assert!(self.dmabufs.insert(fd, dmabuf).is_none());
                }

                Ok(self.dmabufs.len() == 1)
            }
            None => {
                trace!("pw stream: add_buffer (shm), size={size:?}, alpha={alpha}");
                unsafe {
                    let spa_buffer = (*buffer).buffer;

                    let shmbuf = allocate_shmbuf(size).context("error allocating shmbuf")?;

                    assert_eq!((*spa_buffer).n_datas as usize, SHM_BLOCKS);

                    let spa_data = (*spa_buffer).datas;
                    assert!((*spa_data).type_ & (1 << DataType::MemFd.as_raw()) > 0);

                    (*spa_data).type_ = DataType::MemFd.as_raw();
                    (*spa_data).maxsize = shmbuf.layout.size;
                    (*spa_data).fd = shmbuf.fd.as_raw_fd() as i64;
                    (*spa_data).flags = SPA_DATA_FLAG_READWRITE;

                    let chunk = (*spa_data).chunk;
                    (*chunk).stride = shmbuf.layout.stride;
                    (*chunk).offset = 0;

                    let fd = (*(*spa_buffer).datas).fd;
                    assert!(self.shmbufs.insert(fd, shmbuf).is_none());
                }

                Ok(self.shmbufs.len() == 1)
            }
        }
    }

    unsafe fn on_remove_buffer(&mut self, buffer: *mut pw_buffer) {
        self.rendering_buffers
            .retain(|(buf, _)| buf.as_ptr() != buffer);

        unsafe {
            let spa_buffer = (*buffer).buffer;
            let spa_data = (*spa_buffer).datas;

            if (*spa_data).type_ == DataType::DmaBuf.as_raw() {
                trace!("pw stream: remove_buffer (dma)");
                assert!((*spa_buffer).n_datas > 0);

                let fd = (*spa_data).fd;
                self.dmabufs.remove(&fd);
            } else if (*spa_data).type_ == DataType::MemFd.as_raw() {
                trace!("pw stream: remove_buffer (shm)");
                assert_eq!((*spa_buffer).n_datas, SHM_BLOCKS as u32);

                let fd = (*spa_data).fd;
                self.shmbufs.remove(&fd);
            } else {
                error!(
                    "pw stream: remove_buffer (unknown type): {:?}",
                    (*spa_data).type_
                );
            }
        }
    }
}

impl CastState {
    fn pending_size(&self) -> Option<Size<u32, Physical>> {
        match self {
            CastState::ResizePending { pending_size } => Some(*pending_size),
            CastState::ConfirmationPending { size, .. } => Some(*size),
            CastState::Ready { .. } => None,
        }
    }

    fn expected_format_size(&self) -> Size<u32, Physical> {
        match self {
            CastState::ResizePending { pending_size } => *pending_size,
            CastState::ConfirmationPending { size, .. } => *size,
            CastState::Ready { size, .. } => *size,
        }
    }
}

fn pw_version_supports_cursor_metadata() -> bool {
    // This PipeWire version fixed a critical memory issue with cursor metadata:
    // https://gitlab.freedesktop.org/pipewire/pipewire/-/merge_requests/2538
    unsafe { pw_check_library_version(1, 4, 8) }
}

fn make_pod(buffer: &mut Vec<u8>, object: pod::Object) -> &Pod {
    PodSerializer::serialize(Cursor::new(&mut *buffer), &pod::Value::Object(object)).unwrap();
    Pod::from_bytes(buffer).unwrap()
}

fn find_preferred_modifier(
    gbm: &GbmDevice<DeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifiers: Vec<i64>,
) -> anyhow::Result<(Modifier, usize)> {
    debug!("find_preferred_modifier: size={size:?}, fourcc={fourcc}, modifiers={modifiers:?}");

    let modifiers: Vec<u64> = modifiers.iter().map(|m| *m as u64).collect();
    let dmabuf = allocate_gbm_dmabuf(gbm, size.w, size.h, fourcc, &modifiers)?;
    let plane_count = dmabuf.num_planes();

    // FIXME: Ideally this also needs to try binding the dmabuf for rendering.

    Ok((dmabuf.format().modifier, plane_count))
}

fn allocate_dmabuf(
    gbm: &GbmDevice<DeviceFd>,
    size: Size<u32, Physical>,
    fourcc: Fourcc,
    modifier: Modifier,
) -> anyhow::Result<Dmabuf> {
    allocate_gbm_dmabuf(gbm, size.w, size.h, fourcc, &[u64::from(modifier)])
}

#[derive(Debug, Clone)]
struct Shmbuf {
    fd: Rc<OwnedFd>,
    layout: ShmLayout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShmLayout {
    stride: i32,
    size: u32,
}

impl ShmLayout {
    fn new(size: Size<u32, Physical>) -> anyhow::Result<Self> {
        let stride = size
            .w
            .checked_mul(SHM_BYTES_PER_PIXEL as u32)
            .context("SHM stride overflows u32")?;
        let buffer_size = stride
            .checked_mul(size.h)
            .context("SHM buffer size overflows u32")?;
        Ok(Self {
            stride: stride.try_into().context("SHM stride exceeds i32")?,
            size: buffer_size,
        })
    }

    fn size_usize(self) -> usize {
        self.size as usize
    }
}

enum SharingBuf {
    Dma,
    Shm(Shmbuf),
}

fn allocate_shmbuf(size: Size<u32, Physical>) -> anyhow::Result<Shmbuf> {
    let layout = ShmLayout::new(size)?;

    let fd = memfd_create(
        "niri-pw-stream-memfd",
        MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
    )
    .context("error creating memfd")?;
    ftruncate(&fd, layout.size.into()).context("error setting size of the fd")?;
    fcntl_add_seals(&fd, SealFlags::SEAL | SealFlags::SHRINK | SealFlags::GROW)
        .context("error sealing the fd")?;

    Ok(Shmbuf {
        fd: fd.into(),
        layout,
    })
}

unsafe fn return_unused_buffer(stream: &Stream, pw_buffer: NonNull<pw_buffer>) {
    // pw_stream_return_buffer() requires too new PipeWire (1.4.0). So, mark as
    // corrupted and queue.
    let pw_buffer = pw_buffer.as_ptr();
    let spa_buffer = (*pw_buffer).buffer;
    let chunk = (*(*spa_buffer).datas).chunk;
    // Some (older?) consumers will check for size == 0 instead of the CORRUPTED flag.
    (*chunk).size = 0;
    (*chunk).flags = SPA_CHUNK_FLAG_CORRUPTED as i32;

    if let Some(header) = find_meta_header(spa_buffer) {
        let header = header.as_ptr();
        (*header).flags = SPA_META_HEADER_FLAG_CORRUPTED;
    }

    pw_stream_queue_buffer(stream.as_raw_ptr(), pw_buffer);
}

unsafe fn mark_buffer_as_good(pw_buffer: NonNull<pw_buffer>, sequence: &mut u64, buf: SharingBuf) {
    let pw_buffer = pw_buffer.as_ptr();
    let spa_buffer = (*pw_buffer).buffer;
    let chunk = (*(*spa_buffer).datas).chunk;

    match buf {
        SharingBuf::Dma => {
            // With DMA-BUFs, consumers should ignore the size field, and producers are allowed
            // to set it to 0.
            //
            // https://docs.pipewire.org/page_dma_buf.html
            //
            // However, OBS checks for size != 0 as a workaround for old compositor versions,
            // so we set it to 1.
            (*chunk).size = 1;
            // Clear the corrupted flag we may have set before.
            (*chunk).flags = SPA_CHUNK_FLAG_NONE as i32;
        }
        SharingBuf::Shm(shmbuf) => {
            (*chunk).size = shmbuf.layout.size;
            (*chunk).flags = SPA_CHUNK_FLAG_NONE as i32;
        }
    }

    *sequence = sequence.wrapping_add(1);

    if let Some(header) = find_meta_header(spa_buffer) {
        let header = header.as_ptr();
        // Clear the corrupted flag we may have set before.
        (*header).flags = 0;
        (*header).seq = *sequence;
        // Set buffer timestamp as unknown.
        //
        // FIXME: we could try passing real presentation timestamps for rendered frames here.
        // However, then we must also ensure that the time base never jumps (e.g. when switching a
        // dynamic cast between outputs) as this would mess up the timing downstream.
        (*header).pts = -1;
    }
}

unsafe fn find_meta_header(buffer: *mut spa_buffer) -> Option<NonNull<spa_meta_header>> {
    let p = spa_buffer_find_meta_data(buffer, SPA_META_Header, size_of::<spa_meta_header>()).cast();
    NonNull::new(p)
}

unsafe fn add_invisible_cursor(spa_buffer: *mut spa_buffer) {
    unsafe {
        let cursor_meta_ptr: *mut spa_meta_cursor = spa_buffer_find_meta_data(
            spa_buffer,
            SPA_META_Cursor,
            mem::size_of::<spa_meta_cursor>(),
        )
        .cast();
        let Some(cursor_meta) = cursor_meta_ptr.as_mut() else {
            return;
        };

        // The cursor is present but invisible.
        cursor_meta.id = 1;
        cursor_meta.position.x = 0;
        cursor_meta.position.y = 0;
        cursor_meta.hotspot.x = 0;
        cursor_meta.hotspot.y = 0;
        cursor_meta.bitmap_offset = BITMAP_META_OFFSET as _;

        let bitmap_meta_ptr = cursor_meta_ptr
            .byte_add(BITMAP_META_OFFSET)
            .cast::<spa_meta_bitmap>();
        let bitmap_meta = &mut *bitmap_meta_ptr;

        // HACK: PipeWire docs say offset = 0 means invisible.
        //
        // Unfortunately, OBS doesn't actually check that, instead it checks that size isn't zero:
        // https://github.com/obsproject/obs-studio/blob/f4aaa5f0417c5ec40a3799551e125129fce1e007/plugins/linux-pipewire/pipewire.c#L900
        //
        // Unfortunately, libwebrtc, on top of ignoring offset, also treats size = 0 as "preserve
        // previous cursor":
        // https://webrtc.googlesource.com/src/+/97b46e12582606a238d4f0c8524365cf5bdcb411/modules/desktop_capture/linux/wayland/shared_screencast_stream.cc#765
        //
        // So, send a 1x1 transparent pixel instead...
        bitmap_meta.offset = BITMAP_DATA_OFFSET as _;
        bitmap_meta.size.width = 1;
        bitmap_meta.size.height = 1;
        bitmap_meta.stride = CURSOR_BPP as i32;
        bitmap_meta.format = CURSOR_FORMAT;

        let bitmap_data = bitmap_meta_ptr.cast::<u8>().add(BITMAP_DATA_OFFSET);
        let bitmap_slice = slice::from_raw_parts_mut(bitmap_data, CURSOR_BITMAP_SIZE);
        bitmap_slice[..4].copy_from_slice(&[0, 0, 0, 0]);
    }
}

/// Writes the cursor position and, if `redraw`, a freshly rendered bitmap of the (already
/// relocated to 0,0) cursor elements into the buffer's cursor metadata.
unsafe fn add_cursor_metadata(
    renderer: &mut GlesRenderer,
    spa_buffer: *mut spa_buffer,
    meta: CursorMeta,
    cursor_size: Size<i32, Physical>,
    scale: Scale<f64>,
    elements: &[SceneElement<'_>],
    redraw: bool,
) {
    unsafe {
        let cursor_meta_ptr: *mut spa_meta_cursor = spa_buffer_find_meta_data(
            spa_buffer,
            SPA_META_Cursor,
            mem::size_of::<spa_meta_cursor>(),
        )
        .cast();
        let Some(cursor_meta) = cursor_meta_ptr.as_mut() else {
            return;
        };

        cursor_meta.id = 1;
        cursor_meta.position.x = meta.location.0;
        cursor_meta.position.y = meta.location.1;
        cursor_meta.hotspot.x = meta.hotspot.0;
        cursor_meta.hotspot.y = meta.hotspot.1;

        if !redraw {
            trace!("cursor not damaged, skipping rerendering");
            cursor_meta.bitmap_offset = 0;
            return;
        }

        cursor_meta.bitmap_offset = BITMAP_META_OFFSET as _;

        let bitmap_meta_ptr = cursor_meta_ptr
            .byte_add(BITMAP_META_OFFSET)
            .cast::<spa_meta_bitmap>();
        let bitmap_meta = &mut *bitmap_meta_ptr;

        // Start with a 1x1 transparent pixel; see comment in add_invisible_cursor().
        bitmap_meta.offset = BITMAP_DATA_OFFSET as _;
        bitmap_meta.size.width = 1;
        bitmap_meta.size.height = 1;
        bitmap_meta.stride = CURSOR_BPP as i32;
        bitmap_meta.format = CURSOR_FORMAT;

        let bitmap_data = bitmap_meta_ptr.cast::<u8>().add(BITMAP_DATA_OFFSET);
        let bitmap_slice = slice::from_raw_parts_mut(bitmap_data, CURSOR_BITMAP_SIZE);
        bitmap_slice[..4].copy_from_slice(&[0, 0, 0, 0]);

        let size = Size::new(
            min(cursor_size.w, CURSOR_WIDTH as i32),
            min(cursor_size.h, CURSOR_HEIGHT as i32),
        );
        if size.w <= 0 || size.h <= 0 {
            trace!("cursor is invisible, skipping rendering");
            return;
        }

        let _span = tracy_client::span!("add_cursor_metadata render cursor");

        // FIXME: use a reliable buffer whenever we're rendering the cursor.
        //
        // PipeWire buffers are not normally guaranteed to reach the destination, so our buffer
        // with the rendered cursor bitmap may not reach the consumer.
        //
        // Reliable buffers should be available starting from 1.6.0:
        // https://gitlab.freedesktop.org/pipewire/pipewire/-/issues/4885
        let pixels = match render_and_download(renderer, size, scale, Fourcc::Argb8888, elements) {
            Ok(pixels) => pixels,
            Err(err) => {
                warn!("error rendering cursor: {err:?}");
                return;
            }
        };
        bitmap_slice[..pixels.len()].copy_from_slice(&pixels);

        // Fill the metadata now that everything succeeded.
        bitmap_meta.size.width = size.w as _;
        bitmap_meta.size.height = size.h as _;
        bitmap_meta.stride = size.w * CURSOR_BPP as i32;
    }
}

fn buffer_size(size: Size<i32, Physical>) -> Size<i32, Buffer> {
    Size::from((size.w, size.h))
}

/// Renders `elements` (top to bottom) into a fresh texture and reads it back.
fn render_and_download(
    renderer: &mut GlesRenderer,
    size: Size<i32, Physical>,
    scale: Scale<f64>,
    fourcc: Fourcc,
    elements: &[SceneElement<'_>],
) -> anyhow::Result<Vec<u8>> {
    let mut texture: GlesTexture = renderer
        .create_buffer(fourcc, buffer_size(size))
        .context("error creating texture")?;
    let mut fb = renderer
        .bind(&mut texture)
        .context("error binding texture")?;
    let mut tracker = OutputDamageTracker::new(size, scale, Transform::Normal);
    tracker
        .render_output(renderer, &mut fb, 0, elements, Color32F::TRANSPARENT)
        .context("error rendering")?;
    let mapping = renderer
        .copy_framebuffer(
            &fb,
            smithay::utils::Rectangle::from_size(buffer_size(size)),
            fourcc,
        )
        .context("error copying framebuffer")?;
    drop(fb);
    let bytes = renderer
        .map_texture(&mapping)
        .context("error mapping texture")?;
    Ok(bytes.to_vec())
}

fn render_to_dmabuf(
    renderer: &mut GlesRenderer,
    damage_tracker: &mut OutputDamageTracker,
    mut dmabuf: Dmabuf,
    elements: &[SceneElement<'_>],
    states: smithay::backend::renderer::element::RenderElementStates,
) -> anyhow::Result<SyncPoint> {
    let _span = tracy_client::span!("render_to_dmabuf");
    let mut fb = renderer.bind(&mut dmabuf).context("error binding dmabuf")?;
    let res = damage_tracker
        .render_output_with_states(
            renderer,
            &mut fb,
            0,
            elements,
            Color32F::TRANSPARENT,
            states,
        )
        .context("error rendering")?;
    Ok(res.sync.clone())
}

fn render_to_shmbuf(
    renderer: &mut GlesRenderer,
    damage_tracker: &mut OutputDamageTracker,
    buffer: &Shmbuf,
    fourcc: Fourcc,
    elements: &[SceneElement<'_>],
    states: smithay::backend::renderer::element::RenderElementStates,
) -> anyhow::Result<()> {
    let _span = tracy_client::span!("render_to_shmbuf");
    let (size, _scale, _transform): (Size<i32, Physical>, Scale<f64>, Transform) =
        damage_tracker.mode().try_into().unwrap();
    let expected_size = size.w as usize * size.h as usize * SHM_BYTES_PER_PIXEL;
    ensure!(
        buffer.layout.size_usize() == expected_size,
        "invalid buffer size"
    );

    let mut texture: GlesTexture = renderer
        .create_buffer(fourcc, buffer_size(size))
        .context("error creating texture")?;
    let mut fb = renderer
        .bind(&mut texture)
        .context("error binding texture")?;
    damage_tracker
        .render_output_with_states(
            renderer,
            &mut fb,
            0,
            elements,
            Color32F::TRANSPARENT,
            states,
        )
        .context("error rendering")?;
    let mapping = renderer
        .copy_framebuffer(
            &fb,
            smithay::utils::Rectangle::from_size(buffer_size(size)),
            fourcc,
        )
        .context("error copying framebuffer")?;
    drop(fb);
    let bytes = renderer
        .map_texture(&mapping)
        .context("error mapping texture")?;
    ensure!(bytes.len() == expected_size, "unexpected mapping size");

    unsafe {
        let buf = mmap(
            std::ptr::null_mut(),
            buffer.layout.size_usize(),
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            buffer.fd.clone(),
            0,
        )?;
        {
            let buf = slice::from_raw_parts_mut(buf.cast::<u8>(), buffer.layout.size_usize());
            buf.copy_from_slice(bytes);
        }
        if let Err(err) = munmap(buf, buffer.layout.size_usize()) {
            warn!("error unmapping shm buffer: {err:?}");
        }
    }

    Ok(())
}

fn clear_dmabuf(renderer: &mut GlesRenderer, mut dmabuf: Dmabuf) -> anyhow::Result<SyncPoint> {
    let size = dmabuf.size();
    let size: Size<i32, Physical> = Size::from((size.w, size.h));
    let mut fb = renderer.bind(&mut dmabuf).context("error binding dmabuf")?;
    let mut frame = renderer
        .render(&mut fb, size, Transform::Normal)
        .context("error starting frame")?;
    frame
        .clear(
            Color32F::TRANSPARENT,
            &[smithay::utils::Rectangle::from_size(size)],
        )
        .context("error clearing")?;
    let sync = frame.finish().context("error finishing frame")?;
    Ok(sync)
}

fn clear_shmbuf(buffer: &Shmbuf) -> anyhow::Result<()> {
    unsafe {
        let buf = mmap(
            std::ptr::null_mut(),
            buffer.layout.size_usize(),
            ProtFlags::READ | ProtFlags::WRITE,
            MapFlags::SHARED,
            buffer.fd.clone(),
            0,
        )?;
        {
            let buf = slice::from_raw_parts_mut(buf.cast::<u8>(), buffer.layout.size_usize());
            buf.fill(0);
        }
        if let Err(err) = munmap(buf, buffer.layout.size_usize()) {
            warn!("error unmapping shm buffer: {err:?}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shm_layout_uses_spa_representable_dimensions() {
        let layout = ShmLayout::new(Size::from((3840, 2160))).unwrap();
        assert_eq!(layout.stride, 15360);
        assert_eq!(layout.size, 33_177_600);

        assert!(ShmLayout::new(Size::from((536_870_912, 1))).is_err());
        assert!(ShmLayout::new(Size::from((500_000_000, 3))).is_err());
    }
}
