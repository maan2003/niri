//! Screencasting for drv-cast.
//!
//! The core owns the cast session, picks targets and paces frames; the GPU process owns the
//! PipeWire streams and buffers (see `gpu::cast`).

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::os::fd::AsFd as _;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::{env, mem};

use anyhow::Context as _;
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::desktop::Window;
use smithay::output::Output;
use smithay::utils::{Physical, Point, Scale, Size};

use crate::gpu::protocol::{CastEvent, Request};
use crate::gpu::remote::RemoteRenderer;
use crate::niri::{CastTarget, Niri, OutputRenderElements, PointerRenderElements, State};
use crate::niri_render_elements;
use crate::render_helpers::{RenderCtx, RenderTarget};
use crate::utils::{get_monotonic_time, CastSessionId, CastStreamId};
use crate::window::mapped::{MappedId, WindowCastRenderElements};

mod cast;
use cast::{from_gpu_cursor_mode, to_gpu_cursor_mode, Cast, CastSizeChange, CursorData};

pub struct Screencasting {
    pub casts: Vec<Cast>,

    /// Sessions drv-cast started, by its own cast id.
    pub portal_casts: HashMap<CastSessionId, u64>,

    /// Dynamic-target casts waiting for their first target to start.
    pub pending_dynamic_casts: Vec<PendingCast>,

    /// Screencast output for each mapped window.
    pub mapped_cast_output: HashMap<Window, Output>,

    /// Window ID for the "dynamic cast" special window for the xdp-gnome picker.
    pub dynamic_cast_id_for_portal: MappedId,
}

/// How the pointer goes into a cast.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum CursorMode {
    #[default]
    Hidden,
    Embedded,
    Metadata,
}

#[derive(Debug, Clone)]
pub enum StreamTargetId {
    Output { name: String },
    Window { id: u64 },
}

pub enum ScreenCastToNiri {
    StartCast {
        session_id: CastSessionId,
        stream_id: CastStreamId,
        target: StreamTargetId,
        cursor_mode: CursorMode,
        /// drv-cast's id for the cast: what it hears the node id and the end under.
        portal_cast: u64,
    },
}

/// A screencast request that hasn't been started yet.
pub struct PendingCast {
    pub session_id: CastSessionId,
    pub stream_id: CastStreamId,
    pub cursor_mode: CursorMode,
    pub portal_cast: u64,
}

impl Screencasting {
    pub fn new() -> Self {
        Self {
            casts: vec![],
            portal_casts: HashMap::new(),
            pending_dynamic_casts: vec![],
            mapped_cast_output: HashMap::new(),
            dynamic_cast_id_for_portal: MappedId::next(),
        }
    }

    fn cast_mut(&mut self, stream: u64) -> Option<&mut Cast> {
        self.casts
            .iter_mut()
            .find(|cast| cast.stream_id.get() == stream)
    }
}

/// What to pass to the GPU when starting a stream.
struct StartCast {
    session_id: CastSessionId,
    stream_id: CastStreamId,
    target: CastTarget,
    size: Size<i32, Physical>,
    refresh: u32,
    alpha: bool,
    cursor_mode: CursorMode,
    portal_cast: u64,
}

impl State {
    /// Creates the PipeWire stream in the GPU process.
    fn start_cast(&mut self, params: StartCast) -> anyhow::Result<Cast> {
        let _span = tracy_client::span!("State::start_cast");
        let (allow_dmabuf, force_invalid_modifier) = {
            let config = self.niri.config.borrow();
            (
                !config.debug.disable_pipewire_dmabuf,
                config.debug.force_pipewire_invalid_modifier,
            )
        };
        let StartCast {
            session_id,
            stream_id,
            target,
            size,
            refresh,
            alpha,
            cursor_mode,
            portal_cast,
        } = params;

        // The sandboxed GPU process cannot connect to PipeWire itself; it gets a connected
        // socket with every start and uses it if it has no connection.
        let pipewire = connect_pipewire();
        let cursor_mode = self
            .backend
            .with_primary_renderer(|renderer| {
                renderer.client().cast_start(
                    stream_id.get(),
                    (size.w, size.h),
                    refresh,
                    alpha,
                    to_gpu_cursor_mode(cursor_mode),
                    allow_dmabuf,
                    force_invalid_modifier,
                    pipewire.as_ref().map(|s| s.as_fd()),
                )
            })
            .context("no renderer")??;

        Ok(Cast::new(
            self.niri.event_loop.clone(),
            session_id,
            stream_id,
            target,
            size,
            refresh,
            from_gpu_cursor_mode(cursor_mode),
            portal_cast,
        ))
    }

    /// Screencast events from the GPU process.
    pub fn on_cast_events(&mut self, events: Vec<CastEvent>) {
        for event in events {
            match event {
                CastEvent::NodeId { stream, node_id } => {
                    let Some(cast) = self.niri.casting.cast_mut(stream) else {
                        continue;
                    };
                    cast.node_id = Some(node_id);
                    let (width, height) = cast.size().map_or((0, 0), |s| (s.w, s.h));
                    let cast = cast.portal_cast;
                    debug!("telling drv-cast cast {cast} is node {node_id}");
                    self.niri.tell_portal(drv_cast::compositor::FromCompositor::Started {
                        cast,
                        node_id,
                        width,
                        height,
                    });
                }
                CastEvent::State {
                    stream,
                    active,
                    ready_size,
                    min_frame_time_ns,
                } => {
                    if let Some(cast) = self.niri.casting.cast_mut(stream) {
                        cast.set_state(active, ready_size, Duration::from_nanos(min_frame_time_ns));
                    }
                }
                CastEvent::Redraw { stream } => {
                    if let Some(cast) = self.niri.casting.cast_mut(stream) {
                        let stream_id = cast.stream_id;
                        self.redraw_cast(stream_id);
                    }
                }
                CastEvent::Rendered {
                    stream,
                    target_time_ns,
                } => {
                    if let Some(cast) = self.niri.casting.cast_mut(stream) {
                        cast.on_rendered(Duration::from_nanos(target_time_ns));
                    }
                }
                CastEvent::Skipped {
                    stream,
                    target_time_ns,
                } => {
                    if let Some(cast) = self.niri.casting.cast_mut(stream) {
                        cast.on_skipped(Duration::from_nanos(target_time_ns));
                    }
                }
                CastEvent::Stop { stream } => {
                    if let Some(cast) = self.niri.casting.cast_mut(stream) {
                        let session_id = cast.session_id;
                        self.niri.stop_cast(session_id);
                    }
                }
                CastEvent::PipeWireFatal => {
                    warn!("PipeWire failed in the GPU process; stopping all casts");
                    let casting = &self.niri.casting;
                    let mut ids = HashSet::new();
                    for cast in &casting.pending_dynamic_casts {
                        ids.insert(cast.session_id);
                    }
                    for cast in &casting.casts {
                        ids.insert(cast.session_id);
                    }
                    for id in ids {
                        self.niri.stop_cast(id);
                    }
                }
            }
        }
    }

    fn redraw_cast(&mut self, stream_id: CastStreamId) {
        let _span = tracy_client::span!("State::redraw_cast");

        let casts = &mut self.niri.casting.casts;
        let Some(idx) = casts.iter().position(|cast| cast.stream_id == stream_id) else {
            warn!("cast to redraw is missing");
            return;
        };
        let cast = &mut casts[idx];

        let id = match &cast.target {
            CastTarget::Nothing => {
                let now = get_monotonic_time();
                let res = self
                    .backend
                    .with_primary_renderer(|renderer| cast.clear(renderer, now));
                if let Some(Err(err)) = res {
                    warn!("error clearing cast: {err:?}");
                }
                return;
            }
            CastTarget::Output { output, .. } => {
                if let Some(output) = output.upgrade() {
                    self.niri.queue_redraw(&output);
                }
                return;
            }
            CastTarget::Window { id } => *id,
        };

        // Lack of partial borrowing strikes again...
        let mut casts = mem::take(&mut self.niri.casting.casts);
        let cast = &mut casts[idx];
        let mut stop = false;
        // Use a loop {} so we can break instead of early-return.
        #[allow(clippy::never_loop)]
        loop {
            let mut windows = self.niri.layout.windows();
            let Some((_, mapped)) = windows.find(|(_, mapped)| mapped.id().get() == id) else {
                break;
            };

            // Use the cached output since it will be present even if the output was
            // currently disconnected.
            let Some(output) = self.niri.casting.mapped_cast_output.get(&mapped.window) else {
                break;
            };

            if self.niri.is_locked() {
                // Casts of a locked session are black, windows included.
                let now = get_monotonic_time();
                let res = self
                    .backend
                    .with_primary_renderer(|renderer| cast.clear(renderer, now));
                if let Some(Err(err)) = res {
                    warn!("error clearing cast: {err:?}");
                }
                break;
            }

            let scale = Scale::from(output.current_scale().fractional_scale());
            let bbox = mapped
                .window
                .bbox_with_popups()
                .to_physical_precise_up(scale);

            let res = self.backend.with_primary_renderer(|renderer| {
                match cast.ensure_size(renderer, bbox.size) {
                    Ok(CastSizeChange::Ready) => (),
                    Ok(CastSizeChange::Pending) => return Ok(()),
                    Err(err) => return Err(err),
                }

                let mut elements = Vec::new();
                let mut pointer_location = Point::default();

                if self.niri.pointer_visibility.is_visible() {
                    if let Some((pointer_pos, win_pos)) =
                        self.niri.pointer_pos_for_window_cast(mapped)
                    {
                        // Pointer location must be relative to the screencast buffer.
                        // - win_pos is the position of the main window surface in output-local
                        //   coordinates
                        // - bbox.loc moves us relative to the screencast buffer
                        let buf_pos = win_pos + bbox.loc.to_f64().to_logical(scale);
                        let output_pos =
                            self.niri.global_space.output_geometry(output).unwrap().loc;
                        pointer_location = pointer_pos - output_pos.to_f64() - buf_pos;

                        let pos = buf_pos.to_physical_precise_round(scale).upscale(-1);
                        self.niri.render_pointer(renderer, output, &mut |elem| {
                            let elem =
                                RelocateRenderElement::from_element(elem, pos, Relocate::Relative);
                            elements.push(CastRenderElement::from(elem));
                        });
                    }
                }

                let main_start = elements.len();
                mapped.render_for_screen_cast(renderer, scale, &mut |elem| {
                    elements.push(CastRenderElement::from(elem))
                });

                let cursor_data =
                    CursorData::compute(&elements, main_start, pointer_location, scale);

                cast.record(
                    renderer,
                    &elements,
                    &cursor_data,
                    bbox.size,
                    scale,
                    get_monotonic_time(),
                )
            });
            if let Some(Err(err)) = res {
                warn!("error rendering window cast, stopping screencast: {err:?}");
                stop = true;
            }

            break;
        }
        let session_id = cast.session_id;
        self.niri.casting.casts = casts;

        if stop {
            self.niri.stop_cast(session_id);
        }
    }

    pub fn set_dynamic_cast_target(&mut self, target: CastTarget) {
        let _span = tracy_client::span!("State::set_dynamic_cast_target");

        let mut refresh = None;
        match &target {
            // Leave refresh as is when clearing. Chances are, the next refresh will match it,
            // then we'll avoid reconfiguring.
            CastTarget::Nothing => (),
            CastTarget::Output { output, .. } => {
                if let Some(output) = output.upgrade() {
                    refresh = Some(output.current_mode().unwrap().refresh as u32);
                }
            }
            CastTarget::Window { id } => {
                let mut windows = self.niri.layout.windows();
                if let Some((_, mapped)) = windows.find(|(_, mapped)| mapped.id().get() == *id) {
                    if let Some(output) = self.niri.casting.mapped_cast_output.get(&mapped.window) {
                        refresh = Some(output.current_mode().unwrap().refresh as u32);
                    }
                }
            }
        }

        let mut to_redraw = Vec::new();
        for cast in &mut self.niri.casting.casts {
            if !cast.dynamic_target {
                continue;
            }

            if let Some(refresh) = refresh {
                cast.set_refresh(refresh);
            }

            cast.target = target.clone();
            to_redraw.push(cast.stream_id);
        }

        for id in to_redraw {
            self.redraw_cast(id);
        }

        // Start any pending dynamic casts if we have a real target.
        if !matches!(target, CastTarget::Nothing) {
            self.start_pending_dynamic_casts(&target);
        }
    }

    fn start_pending_dynamic_casts(&mut self, target: &CastTarget) {
        let pending = &self.niri.casting.pending_dynamic_casts;
        if pending.is_empty() {
            return;
        }
        debug!("starting {} pending dynamic cast(s)", pending.len());

        let _span = tracy_client::span!("State::start_pending_dynamic_casts");

        // We don't stop dynamic casts on missing output/window.
        let (size, refresh) = match target {
            CastTarget::Nothing => panic!("dynamic cast starting target must not be Nothing"),
            CastTarget::Output { output, .. } => {
                let Some(output) = output.upgrade() else {
                    return;
                };
                cast_params_for_output(&output)
            }
            CastTarget::Window { id } => {
                let Some((size, refresh)) = self.niri.cast_params_for_window(*id) else {
                    return;
                };
                (size, refresh)
            }
        };

        // Alpha is always true since the dynamic target can change between window & output.
        let alpha = true;

        // Start each pending cast.
        let mut to_stop = HashSet::new();
        let pending: Vec<_> = self.niri.casting.pending_dynamic_casts.drain(..).collect();
        for pending in pending {
            let res = self.start_cast(StartCast {
                session_id: pending.session_id,
                stream_id: pending.stream_id,
                target: target.clone(),
                size,
                refresh,
                alpha,
                cursor_mode: pending.cursor_mode,
                portal_cast: pending.portal_cast,
            });
            match res {
                Ok(mut cast) => {
                    cast.dynamic_target = true;
                    self.niri.casting.casts.push(cast);
                }
                Err(err) => {
                    warn!("error starting pending screencast: {err:?}");
                    to_stop.insert(pending.session_id);
                }
            }
        }

        for session_id in to_stop {
            self.niri.stop_cast(session_id);
        }
        self.niri.refresh_cast_indicator();
    }

    pub fn on_screen_cast_msg(&mut self, msg: ScreenCastToNiri) {
        match msg {
            ScreenCastToNiri::StartCast {
                session_id,
                stream_id,
                target,
                cursor_mode,
                portal_cast,
            } => {
                let _span = tracy_client::span!("StartCast");
                let _span = debug_span!("StartCast", %session_id, %stream_id).entered();

                let (target, size, refresh, alpha) = match target {
                    StreamTargetId::Output { name } => {
                        let global_space = &self.niri.global_space;
                        let output = global_space.outputs().find(|out| out.name() == name);
                        let Some(output) = output else {
                            warn!("error starting screencast: requested output is missing");
                            self.niri.stop_cast(session_id);
                            return;
                        };

                        let (size, refresh) = cast_params_for_output(output);
                        (CastTarget::output(output), size, refresh, false)
                    }
                    StreamTargetId::Window { id }
                        if id == self.niri.casting.dynamic_cast_id_for_portal.get() =>
                    {
                        debug!("delaying dynamic cast until target is set");
                        self.niri.casting.pending_dynamic_casts.push(PendingCast {
                            session_id,
                            stream_id,
                            cursor_mode,
                            portal_cast,
                        });
                        self.niri.refresh_cast_indicator();
                        return;
                    }
                    StreamTargetId::Window { id } => {
                        let Some((size, refresh)) = self.niri.cast_params_for_window(id) else {
                            warn!("error starting screencast: requested window is missing");
                            self.niri.stop_cast(session_id);
                            return;
                        };
                        (CastTarget::Window { id }, size, refresh, true)
                    }
                };

                let res = self.start_cast(StartCast {
                    session_id,
                    stream_id,
                    target,
                    size,
                    refresh,
                    alpha,
                    cursor_mode,
                    portal_cast,
                });
                match res {
                    Ok(cast) => {
                        self.niri.casting.casts.push(cast);
                    }
                    Err(err) => {
                        warn!("error starting screencast: {err:?}");
                        self.niri.stop_cast(session_id);
                    }
                }
                self.niri.refresh_cast_indicator();
            }
        }
    }

    /// drv-cast's line. It speaks only after the person consented, so what it asks
    /// for is started as is; the app behind it never reaches us.
    pub fn on_portal_msg(&mut self, msg: drv_cast::compositor::ToCompositor) {
        use drv_cast::compositor::{Cursor, FromCompositor, Output, Source, ToCompositor, Window, VERSION};
        match msg {
            ToCompositor::Hello { version } => {
                if version != VERSION {
                    warn!("drv-cast speaks cast protocol {version}, we speak {VERSION}");
                }
                self.niri.tell_portal(FromCompositor::Hello { version: VERSION });
            }
            ToCompositor::Outputs => {
                let outputs = self.backend.ipc_outputs();
                let outputs = outputs.lock().unwrap();
                let mut list: Vec<Output> = outputs
                    .values()
                    .map(|o| {
                        let (width, height) = o
                            .logical
                            .as_ref()
                            .map_or((0, 0), |l| (l.width as i32, l.height as i32));
                        Output {
                            name: o.name.clone(),
                            make: o.make.clone(),
                            model: o.model.clone(),
                            width,
                            height,
                        }
                    })
                    .collect();
                list.sort_by(|a, b| a.name.cmp(&b.name));
                self.niri.tell_portal(FromCompositor::Outputs(list));
            }
            ToCompositor::Windows => {
                use crate::niri::ClientState;
                use crate::utils::with_toplevel_role;
                use smithay::reexports::wayland_server::Resource as _;
                let mut list = Vec::new();
                self.niri.layout.with_windows(|mapped, _, _, _| {
                    let toplevel = mapped.toplevel();
                    // The client's policy name: the compositor's word, not the window's.
                    let app = toplevel
                        .wl_surface()
                        .client()
                        .and_then(|c| c.get_data::<ClientState>().map(|d| d.policy.name.clone()))
                        .unwrap_or_default();
                    let title = with_toplevel_role(toplevel, |role| role.title.clone().unwrap_or_default());
                    list.push(Window { id: mapped.id().get(), title, app });
                });
                self.niri.tell_portal(FromCompositor::Windows(list));
            }
            ToCompositor::Start { cast, source, cursor } => {
                let session_id = CastSessionId::next();
                let stream_id = CastStreamId::next();
                self.niri.casting.portal_casts.insert(session_id, cast);
                let cursor_mode = match cursor {
                    Cursor::Hidden => CursorMode::Hidden,
                    Cursor::Embedded => CursorMode::Embedded,
                    Cursor::Metadata => CursorMode::Metadata,
                };
                let target = match source {
                    Source::Screen(name) => StreamTargetId::Output { name },
                    Source::Window(id) => StreamTargetId::Window { id },
                };
                self.on_screen_cast_msg(ScreenCastToNiri::StartCast {
                    session_id,
                    stream_id,
                    target,
                    cursor_mode,
                    portal_cast: cast,
                });
            }
            ToCompositor::Devices { mic, camera } => {
                if self.niri.cast_indicator.set_devices(mic, camera) {
                    self.niri.queue_redraw_all();
                }
            }
            ToCompositor::Stop { cast } => {
                let session = self
                    .niri
                    .casting
                    .portal_casts
                    .iter()
                    .find(|(_, c)| **c == cast)
                    .map(|(s, _)| *s);
                match session {
                    Some(session_id) => self.niri.stop_cast(session_id),
                    None => debug!("drv-cast stopped cast {cast}, which is not running"),
                }
            }
        }
    }
}

impl Niri {
    pub fn refresh_mapped_cast_window_rules(&mut self) {
        // O(N^2) but should be fine since there aren't many casts usually.
        self.layout.with_windows_mut(|mapped, _| {
            let id = mapped.id().get();
            // Find regardless of cast.is_active.
            let value = self
                .casting
                .casts
                .iter()
                .any(|cast| cast.target == (CastTarget::Window { id }));
            mapped.set_is_window_cast_target(value);
        });
    }

    pub fn refresh_mapped_cast_outputs(&mut self) {
        let mut seen = HashSet::new();
        let mut output_changed = vec![];

        self.layout.with_windows(|mapped, output, _, _| {
            seen.insert(mapped.window.clone());

            let Some(output) = output else {
                return;
            };

            match self.casting.mapped_cast_output.entry(mapped.window.clone()) {
                Entry::Occupied(mut entry) => {
                    if entry.get() != output {
                        entry.insert(output.clone());
                        output_changed.push((mapped.id(), output.clone()));
                    }
                }
                Entry::Vacant(entry) => {
                    entry.insert(output.clone());
                }
            }
        });

        self.casting
            .mapped_cast_output
            .retain(|win, _| seen.contains(win));

        for (id, out) in output_changed {
            let refresh = out.current_mode().unwrap().refresh as u32;
            let target = CastTarget::Window { id: id.get() };
            for cast in self
                .casting
                .casts
                .iter_mut()
                .filter(|cast| cast.target == target)
            {
                cast.set_refresh(refresh);
            }
        }
    }

    pub fn render_for_screen_cast(
        &mut self,
        renderer: &mut RemoteRenderer,
        output: &Output,
        target_presentation_time: Duration,
    ) {
        let _span = tracy_client::span!("Niri::render_for_screen_cast");

        let weak = output.downgrade();
        let size = output.current_mode().unwrap().size;
        let transform = output.current_transform();
        let size = transform.transform_size(size);

        let scale = Scale::from(output.current_scale().fractional_scale());

        let mut elements: Vec<CastRenderElement<RemoteRenderer>> = Vec::new();
        let mut cursor_data = None;

        let mut casts_to_stop = vec![];

        let mut casts = mem::take(&mut self.casting.casts);
        for cast in &mut casts {
            if !cast.is_active() {
                continue;
            }

            if !cast.target.matches_output(&weak) {
                continue;
            }

            match cast.ensure_size(renderer, size) {
                Ok(CastSizeChange::Ready) => (),
                Ok(CastSizeChange::Pending) => continue,
                Err(err) => {
                    warn!("error updating stream size, stopping screencast: {err:?}");
                    casts_to_stop.push(cast.session_id);
                    continue;
                }
            }

            if cast.check_time_and_schedule(output, target_presentation_time) {
                continue;
            }

            if cursor_data.is_none() {
                let mut pointer_pos = Point::default();
                if self.pointer_visibility.is_visible() {
                    let output_geo = self.global_space.output_geometry(output).unwrap().to_f64();
                    let pointer_loc = self
                        .tablet_cursor_location
                        .unwrap_or_else(|| self.seat.get_pointer().unwrap().current_location());
                    // Only render when the pointer is within the output. Otherwise, it will
                    // happily appear anywhere outside the output video source in OBS.
                    if output_geo.contains(pointer_loc) {
                        pointer_pos = pointer_loc - output_geo.loc;
                        self.render_pointer(renderer, output, &mut |elem| {
                            elements.push(elem.into())
                        });
                    }
                }

                let main_start = elements.len();
                let ctx = RenderCtx {
                    renderer,
                    target: RenderTarget::Screencast,
                    xray: None,
                };
                self.render(ctx, output, false, &mut |elem| elements.push(elem.into()));

                cursor_data = Some(CursorData::compute(
                    &elements,
                    main_start,
                    pointer_pos,
                    scale,
                ));
            }
            let cursor_data = cursor_data.as_ref().unwrap();

            if let Err(err) = cast.record(
                renderer,
                &elements,
                cursor_data,
                size,
                scale,
                target_presentation_time,
            ) {
                warn!("error recording cast frame, stopping screencast: {err:?}");
                casts_to_stop.push(cast.session_id);
            }
        }
        self.casting.casts = casts;

        for id in casts_to_stop {
            self.stop_cast(id);
        }
    }

    pub fn render_windows_for_screen_cast(
        &mut self,
        renderer: &mut RemoteRenderer,
        output: &Output,
        target_presentation_time: Duration,
    ) {
        let _span = tracy_client::span!("Niri::render_windows_for_screen_cast");

        let scale = Scale::from(output.current_scale().fractional_scale());

        let mut casts_to_stop = vec![];

        let mut casts = mem::take(&mut self.casting.casts);
        for cast in &mut casts {
            if !cast.is_active() {
                continue;
            }

            let CastTarget::Window { id } = cast.target else {
                continue;
            };

            let mut windows = self.layout.windows_for_output(output);
            let Some(mapped) = windows.find(|win| win.id().get() == id) else {
                continue;
            };

            let bbox = mapped
                .window
                .bbox_with_popups()
                .to_physical_precise_up(scale);

            match cast.ensure_size(renderer, bbox.size) {
                Ok(CastSizeChange::Ready) => (),
                Ok(CastSizeChange::Pending) => continue,
                Err(err) => {
                    warn!("error updating stream size, stopping screencast: {err:?}");
                    casts_to_stop.push(cast.session_id);
                    continue;
                }
            }

            if cast.check_time_and_schedule(output, target_presentation_time) {
                continue;
            }

            if self.is_locked() {
                // Casts of a locked session are black, windows included.
                if let Err(err) = cast.clear(renderer, target_presentation_time) {
                    warn!("error clearing cast: {err:?}");
                }
                continue;
            }

            let mut elements = Vec::new();
            let mut pointer_location = Point::default();

            if self.pointer_visibility.is_visible() {
                if let Some((pointer_pos, win_pos)) = self.pointer_pos_for_window_cast(mapped) {
                    // Pointer location must be relative to the screencast buffer.
                    // - win_pos is the position of the main window surface in output-local
                    //   coordinates
                    // - bbox.loc moves us relative to the screencast buffer
                    let buf_pos = win_pos + bbox.loc.to_f64().to_logical(scale);
                    let output_pos = self.global_space.output_geometry(output).unwrap().loc;
                    pointer_location = pointer_pos - output_pos.to_f64() - buf_pos;

                    let pos = buf_pos.to_physical_precise_round(scale).upscale(-1);
                    self.render_pointer(renderer, output, &mut |elem| {
                        let elem =
                            RelocateRenderElement::from_element(elem, pos, Relocate::Relative);
                        elements.push(CastRenderElement::from(elem));
                    });
                }
            }

            let main_start = elements.len();
            mapped.render_for_screen_cast(renderer, scale, &mut |elem| {
                elements.push(CastRenderElement::from(elem))
            });

            let cursor_data = CursorData::compute(&elements, main_start, pointer_location, scale);

            if let Err(err) = cast.record(
                renderer,
                &elements,
                &cursor_data,
                bbox.size,
                scale,
                target_presentation_time,
            ) {
                warn!("error recording cast frame, stopping screencast: {err:?}");
                casts_to_stop.push(cast.session_id);
            }
        }
        self.casting.casts = casts;

        for id in casts_to_stop {
            self.stop_cast(id);
        }
    }

    pub fn stop_cast(&mut self, session_id: CastSessionId) {
        let _span = tracy_client::span!("Niri::stop_cast");
        let _span = debug_span!("stop_cast", %session_id).entered();

        self.casting
            .pending_dynamic_casts
            .retain(|p| p.session_id != session_id);

        for i in (0..self.casting.casts.len()).rev() {
            let cast = &self.casting.casts[i];
            if cast.session_id != session_id {
                continue;
            }

            let cast = self.casting.casts.swap_remove(i);
            self.stop_cast_stream(&cast);
        }

        if let Some(cast) = self.casting.portal_casts.remove(&session_id) {
            self.tell_portal(drv_cast::compositor::FromCompositor::Stopped { cast });
        }

        self.refresh_cast_indicator();
    }

    /// One message to drv-cast, if its line is up. Nonblocking: a drv-cast that stopped
    /// reading takes the whole set down anyway.
    pub fn tell_portal(&self, msg: drv_cast::compositor::FromCompositor) {
        let Some(sock) = &self.portal else {
            warn!("no line to drv-cast; dropping {msg:?}");
            return;
        };
        if let Err(err) = drv_policy::seq::send(sock, &msg, &[]) {
            warn!("to drv-cast: {err}");
        }
    }

    /// Stops every screencast session: the human's kill switch. Each session gets the Mutter
    /// `Closed` signal, so the portal and the app see the lease end.
    pub fn stop_all_casts(&mut self) {
        let ids = self.cast_session_ids();
        debug!("stopping {} screencast session(s)", ids.len());
        for id in ids {
            self.stop_cast(id);
        }
        self.tell_portal(drv_cast::compositor::FromCompositor::Revoke);
    }

    fn cast_session_ids(&self) -> HashSet<CastSessionId> {
        let casts = self.casting.casts.iter().map(|cast| cast.session_id);
        let pending = self
            .casting
            .pending_dynamic_casts
            .iter()
            .map(|p| p.session_id);
        casts.chain(pending).collect()
    }

    /// Keeps the on-screen indicator in step with the live sessions.
    pub fn refresh_cast_indicator(&mut self) {
        let sessions = self.cast_session_ids().len();
        if self.cast_indicator.set_sessions(sessions) {
            self.queue_redraw_all();
        }
    }

    /// Tears down the GPU-side stream. `Niri` doesn't own the backend, so this goes through
    /// the event loop like `stop_casts_for_target` does.
    fn stop_cast_stream(&self, cast: &Cast) {
        let stream = cast.stream_id.get();
        self.event_loop.insert_idle(move |state| {
            state.backend.with_primary_renderer(|renderer| {
                if let Err(err) = renderer
                    .client()
                    .send_oneway(&Request::CastStop { stream }, &[])
                {
                    warn!("error stopping cast stream: {err:?}");
                }
            });
        });
    }

    pub fn stop_casts_for_target(&mut self, target: CastTarget) {
        let _span = tracy_client::span!("Niri::stop_casts_for_target");

        // This is O(N^2) but it shouldn't be a problem I think.
        let mut saw_dynamic = false;
        let mut ids = Vec::new();
        for cast in &self.casting.casts {
            if cast.target != target {
                continue;
            }

            if cast.dynamic_target {
                saw_dynamic = true;
                continue;
            }

            ids.push(cast.session_id);
        }

        for id in ids {
            self.stop_cast(id);
        }

        // We don't stop dynamic casts, instead we switch them to Nothing.
        if saw_dynamic {
            self.event_loop
                .insert_idle(|state| state.set_dynamic_cast_target(CastTarget::Nothing));
        }
    }

    fn cast_params_for_window(&self, window_id: u64) -> Option<(Size<i32, Physical>, u32)> {
        let (_, mapped) = self
            .layout
            .windows()
            .find(|(_, m)| m.id().get() == window_id)?;
        let output = self.casting.mapped_cast_output.get(&mapped.window)?;
        let scale = Scale::from(output.current_scale().fractional_scale());
        let bbox = mapped
            .window
            .bbox_with_popups()
            .to_physical_precise_up(scale);
        let refresh = output.current_mode().unwrap().refresh as u32;
        Some((bbox.size, refresh))
    }
}

fn cast_params_for_output(output: &Output) -> (Size<i32, Physical>, u32) {
    let mode = output.current_mode().unwrap();
    let transform = output.current_transform();
    let size = transform.transform_size(mode.size);
    let refresh = mode.refresh as u32;
    (size, refresh)
}

niri_render_elements! {
    CastRenderElement<R> => {
        Output = OutputRenderElements<R>,
        Window = WindowCastRenderElements<R>,
        Pointer = PointerRenderElements<R>,
        RelocatedPointer = RelocateRenderElement<PointerRenderElements<R>>,
    }
}

/// Connects to the PipeWire daemon the way libpipewire would: `$PIPEWIRE_REMOTE` (a name or
/// an absolute path, default `pipewire-0`) under `$PIPEWIRE_RUNTIME_DIR` or `$XDG_RUNTIME_DIR`.
fn connect_pipewire() -> Option<UnixStream> {
    let remote = env::var_os("PIPEWIRE_REMOTE").unwrap_or_else(|| "pipewire-0".into());
    let path = if Path::new(&remote).is_absolute() {
        PathBuf::from(remote)
    } else {
        let dir = env::var_os("PIPEWIRE_RUNTIME_DIR").or_else(|| env::var_os("XDG_RUNTIME_DIR"));
        let Some(dir) = dir else {
            warn!("cannot find the PipeWire socket: XDG_RUNTIME_DIR is not set");
            return None;
        };
        Path::new(&dir).join(remote)
    };

    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(err) => {
            warn!("error connecting to PipeWire at {path:?}: {err}");
            return None;
        }
    };
    if let Err(err) = stream.set_nonblocking(true) {
        warn!("error making the PipeWire socket non-blocking: {err}");
    }
    Some(stream)
}
