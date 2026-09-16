//! TTY backend, core side.
//!
//! Owns the session (libseat), udev and libinput, and decides output policy: which connector
//! is on, which mode, VRR, gamma. Everything that touches DRM/KMS or the GPU lives in the GPU
//! process (`crate::gpu::drm`); this side talks to it over the [`GpuClient`] connection.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::iter::zip;
use std::mem;
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, ensure, Context};
use drm_ffi::drm_mode_modeinfo;
use libc::dev_t;
use niri_config::output::{HdrMode, Modeline};
use niri_config::{Config, OutputName};
use niri_ipc::{HSyncPolarity, VSyncPolarity};
use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::drm::{DrmNode, NodeType};
use smithay::backend::libinput::{LibinputInputBackend, LibinputSessionInterface};
use smithay::backend::renderer::element::RenderElementStates;
use smithay::backend::renderer::ImportDma as _;
use smithay::backend::session::libseat::LibSeatSession;
use smithay::backend::session::{Event as SessionEvent, Session};
use smithay::backend::udev::{self, UdevBackend, UdevEvent};
use smithay::desktop::utils::OutputPresentationFeedback;
use smithay::output::{Mode, Output, PhysicalProperties, Subpixel};
use smithay::reexports::calloop::generic::Generic;
use smithay::reexports::calloop::ping::make_ping;
use smithay::reexports::calloop::timer::{TimeoutAction, Timer};
use smithay::reexports::calloop::{
    Dispatcher, Interest, LoopHandle, Mode as CalloopMode, PostAction,
};
use smithay::reexports::drm::control::{Mode as DrmMode, ModeFlags, ModeTypeFlags};
use smithay::reexports::input::Libinput;
use smithay::reexports::rustix::fs::OFlags;
use smithay::reexports::wayland_protocols;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Scale;
use smithay::wayland::color::management::{
    ImageDescription, Primaries as CmPrimaries, TransferFunction as CmTransferFunction,
};
use smithay::wayland::dmabuf::{DmabufFeedbackBuilder, DmabufGlobal};
use smithay::wayland::presentation::Refresh;
use wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;

use super::{IpcOutputMap, OutputHdrCaps, RenderResult};
use crate::backend::OutputId;
use crate::frame_clock::FrameClock;
use crate::gpu::client::{GpuClient, Mode as GpuMode};
use crate::gpu::convert;
use crate::gpu::protocol::{
    CastEvent, ColorState, ConnectorInfo, ElementState, Event, GpuEvent, HdrCaps, HdrMetadataDesc,
    ModeDesc, OutputGeometry, OutputRef, PresentFlags, Request,
};
use crate::gpu::record::Recorder;
use crate::gpu::remote::{DmabufAllocator, RemoteRenderer};
use crate::niri::{Niri, RedrawState, State};
use crate::render_helpers::blend::{BlendSpace, DEFAULT_REFERENCE_LUMINANCE};
use crate::render_helpers::debug::draw_damage;
use crate::render_helpers::{shaders, RenderCtx, RenderTarget};
use crate::utils::{get_monotonic_time, is_laptop_panel, logical_output, PanelOrientation};

pub struct Tty {
    config: Rc<RefCell<Config>>,
    session: LibSeatSession,
    udev_dispatcher: Dispatcher<'static, UdevBackend, State>,
    libinput: Libinput,
    /// Connection to the GPU process; also carries all DRM requests.
    renderer: RemoteRenderer,
    /// Set once the GPU process has a renderer (after the primary device was added).
    renderer_ready: bool,
    /// Wakes the event loop to dispatch GPU events queued while waiting for a reply.
    primary_node: DrmNode,
    primary_render_node: DrmNode,
    ignored_nodes: HashSet<DrmNode>,
    devices: HashMap<DrmNode, OutputDevice>,
    dmabuf_global: Option<DmabufGlobal>,
    update_output_config_on_resume: bool,
    debug_tint: bool,
    ipc_outputs: Arc<Mutex<IpcOutputMap>>,
    /// Frames queued for scanout, waiting for their vblank.
    /// Frames sent to the GPU process whose vblank hasn't arrived yet.
    pending_frames: HashMap<u64, PendingFrame>,
    next_frame_id: u64,
}

pub struct OutputDevice {
    /// Our copy of the DRM fd; the GPU process has a dup. Closed through libseat on removal.
    fd: OwnedFd,
    connectors: HashMap<u32, Connector>,
}

struct Connector {
    info: ConnectorInfo,
    id: OutputId,
    name: OutputName,
    /// Set while the output is enabled.
    surface: Option<Surface>,
}

struct Surface {
    output: Output,
    mode: DrmMode,
    vrr_enabled: bool,
    vrr_supported: bool,
    /// Gamma change requested while the session was inactive; applied on resume.
    pending_gamma_change: Option<Option<Vec<u16>>>,
    /// Connector color state currently staged in the GPU process (HDR signalling, max bpc).
    color_state: ColorState,
    /// The last color state the driver rejected, so it isn't re-tested every frame (each test
    /// is an atomic TEST_ONLY commit). Cleared on config change and session resume.
    failed_color_state: Option<ColorState>,
    /// Recreated whenever the output geometry changes.
    /// Element recording state (damage since last frame, effect caches).
    recorder: Recorder,
    /// Geometry the GPU process currently has for this output.
    geometry: Option<OutputGeometry>,
    vblank_frame: Option<tracy_client::Frame>,
    vblank_frame_name: tracy_client::FrameName,
}

/// Stored in `Output::user_data()` to find the DRM output behind a wl_output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TtyOutputState(pub OutputRef);

struct PendingFrame {
    output: Output,
    /// Filled in when the GPU reports the frame was submitted for scanout.
    feedback: Option<OutputPresentationFeedback>,
}

fn output_ref_of(output: &Output) -> OutputRef {
    output.user_data().get::<TtyOutputState>().unwrap().0
}

impl Tty {
    pub fn new(
        config: Rc<RefCell<Config>>,
        event_loop: LoopHandle<'static, State>,
    ) -> anyhow::Result<Self> {
        let _span = tracy_client::span!("Tty::new");

        let (session, notifier) = LibSeatSession::new().context(
            "Error creating a session. This might mean that you're trying to run niri on a TTY \
             that is already busy, for example if you're running this inside tmux that had been \
             originally started on a different TTY",
        )?;
        let seat_name = session.seat();

        let udev_backend =
            UdevBackend::new(session.seat()).context("error creating a udev backend")?;
        let udev_dispatcher = Dispatcher::new(udev_backend, move |event, _, state: &mut State| {
            state.backend.tty().on_udev_event(&mut state.niri, event);
        });
        event_loop
            .register_dispatcher(udev_dispatcher.clone())
            .unwrap();

        let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
        unsafe { init_libinput_plugin_system(&libinput) };
        {
            let _span = tracy_client::span!("Libinput::udev_assign_seat");
            libinput.udev_assign_seat(&seat_name)
        }
        .map_err(|()| anyhow!("error assigning the seat to libinput"))?;

        if !session.is_active() {
            debug!("session is not active, starting libinput in paused state");
            libinput.suspend();
        }

        let input_backend = LibinputInputBackend::new(libinput.clone());
        event_loop
            .insert_source(input_backend, |mut event, _, state| {
                state.process_libinput_event(&mut event);
                state.process_input_event(event);
            })
            .unwrap();

        event_loop
            .insert_source(notifier, move |event, _, state| {
                state.backend.tty().on_session_event(&mut state.niri, event);
            })
            .unwrap();

        // The GPU process. NIRI_GPU_THREAD runs it in-process, for debugging.
        let mut client = if std::env::var_os("NIRI_GPU_THREAD").is_some() {
            warn!("running the GPU server in-process (NIRI_GPU_THREAD)");
            GpuClient::spawn_thread(GpuMode::Drm)?
        } else {
            let exe = std::env::current_exe().context("error getting our executable path")?;
            GpuClient::spawn_process(&exe, GpuMode::Drm)
                .context("error spawning the GPU process")?
        };
        let poll_fd = client.as_fd().try_clone_to_owned()?;
        // Events queued during a synchronous request are dispatched via this ping.
        let (ping, ping_source) = make_ping().context("error creating ping")?;
        client.set_waker(move || ping.ping());
        let renderer = RemoteRenderer::new(client);

        event_loop
            .insert_source(
                Generic::new(poll_fd, Interest::READ, CalloopMode::Level),
                |_, _, state| {
                    let tty = state.backend.tty();
                    if !tty.renderer.client().is_readable() {
                        // A sync request in an earlier callback already consumed it.
                        let casts = tty.dispatch_gpu_events(&mut state.niri);
                        state.on_cast_events(casts);
                        return Ok(PostAction::Continue);
                    }
                    if let Err(err) = tty.renderer.client().recv_event() {
                        // Nothing can be drawn without it; bail out like a GPU crash would.
                        error!("lost the GPU process, exiting: {err:#}");
                        state.niri.stop_signal.stop();
                        return Ok(PostAction::Remove);
                    }
                    let casts = tty.dispatch_gpu_events(&mut state.niri);
                    state.on_cast_events(casts);
                    Ok(PostAction::Continue)
                },
            )
            .unwrap();

        event_loop
            .insert_source(ping_source, |_, _, state| {
                let casts = state.backend.tty().dispatch_gpu_events(&mut state.niri);
                state.on_cast_events(casts);
            })
            .unwrap();

        let (primary_node, primary_render_node) = primary_node_from_config(&config.borrow())
            .ok_or(())
            .or_else(|()| {
                let primary_gpu_path = udev::primary_gpu(&seat_name)
                    .context("error getting the primary GPU")?
                    .context("couldn't find a GPU")?;
                let primary_node = DrmNode::from_path(primary_gpu_path)
                    .context("error opening the primary GPU DRM node")?;
                let primary_render_node = primary_node
                    .node_with_type(NodeType::Render)
                    .and_then(Result::ok)
                    .unwrap_or_else(|| {
                        warn!(
                            "error getting the render node for the primary GPU; proceeding anyway"
                        );
                        primary_node
                    });
                Ok::<_, anyhow::Error>((primary_node, primary_render_node))
            })?;

        let mut node_path = String::new();
        if let Some(path) = primary_render_node.dev_path() {
            write!(node_path, "{path:?}").unwrap();
        } else {
            write!(node_path, "{primary_render_node}").unwrap();
        }
        info!("using as the render node: {node_path}");

        Ok(Self {
            config,
            session,
            udev_dispatcher,
            libinput,
            renderer,
            renderer_ready: false,
            primary_node,
            primary_render_node,
            ignored_nodes: HashSet::new(),
            devices: HashMap::new(),
            dmabuf_global: None,
            update_output_config_on_resume: false,
            debug_tint: false,
            ipc_outputs: Arc::new(Mutex::new(HashMap::new())),
            pending_frames: HashMap::new(),
            next_frame_id: 1,
        })
    }

    /// Sends a DRM request to the GPU process and waits for the reply.
    fn request(&self, req: Request) -> anyhow::Result<Event> {
        self.request_with_fds(req, &[])
    }

    fn request_with_fds(&self, req: Request, fds: &[OwnedFd]) -> anyhow::Result<Event> {
        let fds: Vec<_> = fds.iter().map(|fd| fd.as_fd()).collect();
        self.renderer.client().request(&req, &fds)
    }

    fn request_ack(&self, req: Request) -> anyhow::Result<()> {
        GpuClient::expect_ack(self.request(req)?)
    }

    /// Handles GPU events; returns the screencast events, which need the whole `State`.
    fn dispatch_gpu_events(&mut self, niri: &mut Niri) -> Vec<CastEvent> {
        let events = self.renderer.client().take_events();
        let mut cast_events = Vec::new();
        for event in events {
            match event {
                GpuEvent::Cast(event) => cast_events.push(event),
                GpuEvent::VBlank {
                    output,
                    sequence,
                    time_ns,
                    frame,
                } => {
                    let time = time_ns.map(Duration::from_nanos);
                    self.on_vblank(niri, output, sequence, time, frame);
                }
                GpuEvent::Presented {
                    output,
                    frame,
                    submitted,
                    states,
                } => self.on_presented(niri, output, frame, submitted, &states),
                GpuEvent::Error { message } => {
                    warn!("GPU process request failed: {message}");
                }
                GpuEvent::DeviceError { dev, message } => {
                    warn!("DRM device {dev} error: {message}");
                }
                GpuEvent::Png { token, data } => niri.on_screenshot_encoded(token, data),
            }
        }
        cast_events
    }

    pub fn init(&mut self, niri: &mut Niri) {
        if !self.session.is_active() {
            return;
        }

        self.ignored_nodes = self.compute_ignored_nodes();

        let udev = self.udev_dispatcher.clone();
        let udev = udev.as_source_ref();

        // The primary device must come first: it brings up the renderer that display-only
        // devices scan out from.
        if let Some((primary_device_id, primary_device_path)) = udev
            .device_list()
            .find(|&(device_id, _)| device_id == self.primary_node.dev_id())
        {
            if let Err(err) = self.device_added(primary_device_id, primary_device_path, niri) {
                warn!(
                    "error adding primary node device, display-only devices may not work: {err:?}"
                );
            }
        } else {
            warn!("primary node is missing, display-only devices may not work");
        };

        for (device_id, path) in udev.device_list() {
            if device_id == self.primary_node.dev_id() {
                continue;
            }
            if let Err(err) = self.device_added(device_id, path, niri) {
                warn!("error adding device: {err:?}");
            }
        }
    }

    fn on_udev_event(&mut self, niri: &mut Niri, event: UdevEvent) {
        let _span = tracy_client::span!("Tty::on_udev_event");
        match event {
            UdevEvent::Added { device_id, path } => {
                if !self.session.is_active() {
                    debug!("skipping UdevEvent::Added as session is inactive");
                    return;
                }
                self.ignored_nodes = self.compute_ignored_nodes();
                if let Err(err) = self.device_added(device_id, &path, niri) {
                    warn!("error adding device: {err:?}");
                }
            }
            UdevEvent::Changed { device_id } => {
                if !self.session.is_active() {
                    debug!("skipping UdevEvent::Changed as session is inactive");
                    return;
                }
                self.device_changed(device_id, niri, false)
            }
            UdevEvent::Removed { device_id } => {
                if !self.session.is_active() {
                    debug!("skipping UdevEvent::Removed as session is inactive");
                    return;
                }
                self.device_removed(device_id, niri)
            }
        }
    }

    fn on_session_event(&mut self, niri: &mut Niri, event: SessionEvent) {
        let _span = tracy_client::span!("Tty::on_session_event");
        match event {
            SessionEvent::PauseSession => {
                debug!("pausing session");
                self.libinput.suspend();
                if let Err(err) = self.request_ack(Request::PauseDevices) {
                    warn!("error pausing DRM devices: {err:?}");
                }
            }
            SessionEvent::ActivateSession => {
                debug!("resuming session");

                if self.libinput.resume().is_err() {
                    warn!("error resuming libinput");
                }

                self.ignored_nodes = self.compute_ignored_nodes();

                let mut device_list = self
                    .udev_dispatcher
                    .as_source_ref()
                    .device_list()
                    .map(|(device_id, path)| (device_id, path.to_owned()))
                    .collect::<HashMap<_, _>>();

                let removed_devices = self
                    .devices
                    .keys()
                    .filter(|node| {
                        !device_list.contains_key(&node.dev_id())
                            || self.ignored_nodes.contains(node)
                    })
                    .copied()
                    .collect::<Vec<_>>();
                let remained_devices = self
                    .devices
                    .keys()
                    .filter(|node| {
                        device_list.contains_key(&node.dev_id())
                            && !self.ignored_nodes.contains(node)
                    })
                    .copied()
                    .collect::<Vec<_>>();

                for node in removed_devices {
                    device_list.remove(&node.dev_id());
                    self.device_removed(node.dev_id(), niri);
                }

                let force_disable = self
                    .config
                    .borrow()
                    .debug
                    .force_disable_connectors_on_resume;
                if let Err(err) = self.request_ack(Request::ResumeDevices { force_disable }) {
                    warn!("error activating DRM devices: {err:?}");
                }

                for node in remained_devices {
                    device_list.remove(&node.dev_id());

                    // Re-read connectors and drop any stale kernel state.
                    self.device_changed(node.dev_id(), niri, true);

                    // Apply gamma changes requested while we were inactive.
                    let device = self.devices.get_mut(&node).unwrap();
                    let mut pending = Vec::new();
                    for (crtc, connector) in device.connectors.iter_mut() {
                        if let Some(surface) = &mut connector.surface {
                            // Give a rejected HDR color state another chance after resume.
                            surface.failed_color_state = None;
                            if let Some(ramp) = surface.pending_gamma_change.take() {
                                pending.push((*crtc, ramp));
                            }
                        }
                    }
                    for (crtc, ramp) in pending {
                        let output = OutputRef {
                            dev: node.dev_id(),
                            crtc,
                        };
                        if let Err(err) = self.request_ack(Request::SetGamma { output, ramp }) {
                            warn!("error applying pending gamma change: {err:?}");
                        }
                    }
                }

                // Add new devices, primary first.
                let primary_device_id = self.primary_node.dev_id();
                let primary_device_path = device_list.remove(&primary_device_id);
                let primary = primary_device_path.map(|path| (primary_device_id, path));
                for (device_id, path) in primary.into_iter().chain(device_list) {
                    if let Err(err) = self.device_added(device_id, &path, niri) {
                        warn!("error adding device: {err:?}");
                    }
                }

                if self.update_output_config_on_resume {
                    self.on_output_config_changed(niri);
                }

                self.refresh_ipc_outputs(niri);

                niri.notify_activity();
                niri.monitors_active = true;
                self.set_monitors_active(true);
                niri.queue_redraw_all();
            }
        }
    }

    fn device_added(
        &mut self,
        device_id: dev_t,
        path: &Path,
        niri: &mut Niri,
    ) -> anyhow::Result<()> {
        debug!("adding device: {device_id} {path:?}");

        let node = DrmNode::from_dev_id(device_id)?;
        let is_primary = node == self.primary_node;
        if is_primary {
            debug!("this is the primary node");
        }
        if node.ty() != NodeType::Primary {
            debug!("not a primary node, skipping");
            return Ok(());
        }
        if self.ignored_nodes.contains(&node) {
            debug!("node is ignored, skipping");
            return Ok(());
        }
        if self.devices.contains_key(&node) {
            debug!("device already added");
            return Ok(());
        }

        let _span = tracy_client::span!("Tty::device_added");

        let open_flags = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOCTTY | OFlags::NONBLOCK;
        let fd = {
            let _span = tracy_client::span!("LibSeatSession::open");
            self.session.open(path, open_flags)
        }?;
        let gpu_fd = fd.try_clone().context("error duplicating DRM fd")?;

        let res = self.request_with_fds(
            Request::AddDevice {
                dev: device_id,
                path: path.to_string_lossy().into_owned(),
                primary: is_primary,
            },
            &[gpu_fd],
        );
        let (render_node, caps) = match res {
            Ok(Event::DeviceAdded { render_node, caps }) => (render_node, caps),
            Ok(other) => bail!("unexpected reply to AddDevice: {other:?}"),
            Err(err) => {
                if let Err(err) = self.session.close(fd) {
                    warn!("error closing DRM device fd: {err:?}");
                }
                return Err(err.context("GPU process failed to add device"));
            }
        };

        if let Some(caps) = caps {
            debug!("the GPU process brought up the renderer");
            self.renderer.set_caps(caps);
            self.renderer_ready = true;

            {
                let config = self.config.borrow();
                if let Some(src) = config.animations.window_resize.custom_shader.as_deref() {
                    shaders::set_custom_resize_program(&mut self.renderer, Some(src));
                }
                if let Some(src) = config.animations.window_close.custom_shader.as_deref() {
                    shaders::set_custom_close_program(&mut self.renderer, Some(src));
                }
                if let Some(src) = config.animations.window_open.custom_shader.as_deref() {
                    shaders::set_custom_open_program(&mut self.renderer, Some(src));
                }
            }
            niri.update_shaders();

            if self.dmabuf_global.is_none() {
                let render_dev = render_node.unwrap_or(self.primary_render_node.dev_id());
                let formats = self.renderer.dmabuf_formats();
                let default_feedback = DmabufFeedbackBuilder::new(render_dev, formats)
                    .build()
                    .context("error building default dmabuf feedback")?;
                let dmabuf_global = niri
                    .dmabuf_state
                    .create_global_with_default_feedback::<State>(
                        &niri.display_handle,
                        &default_feedback,
                    );
                self.dmabuf_global = Some(dmabuf_global);
            }
        }

        self.devices.insert(
            node,
            OutputDevice {
                fd,
                connectors: HashMap::new(),
            },
        );

        self.device_changed(device_id, niri, true);
        Ok(())
    }

    fn device_changed(&mut self, device_id: dev_t, niri: &mut Niri, cleanup: bool) {
        debug!("device changed: {device_id}");

        let Ok(node) = DrmNode::from_dev_id(device_id) else {
            warn!("error creating DrmNode");
            return;
        };
        if node.ty() != NodeType::Primary {
            debug!("not a primary node, skipping");
            return;
        }
        if self.ignored_nodes.contains(&node) {
            debug!("node is ignored, skipping");
            return;
        }
        if !self.devices.contains_key(&node) {
            if let Some(path) = node.dev_path() {
                warn!("unknown device; trying to add");
                if let Err(err) = self.device_added(device_id, &path, niri) {
                    warn!("error adding device: {err:?}");
                }
            } else {
                warn!("unknown device");
            }
            return;
        }

        let (mut connected, changed, disconnected) =
            match self.request(Request::RescanDevice { dev: device_id }) {
                Ok(Event::Scan {
                    connected,
                    changed,
                    disconnected,
                }) => (connected, changed, disconnected),
                Ok(other) => {
                    warn!("unexpected reply to RescanDevice: {other:?}");
                    return;
                }
                Err(err) => {
                    warn!("error scanning connectors: {err:?}");
                    return;
                }
            };

        for output in &disconnected {
            self.connector_disconnected(niri, *output);
        }
        let Some(device) = self.devices.get_mut(&node) else {
            error!("device disappeared");
            return;
        };
        for output in disconnected {
            if device.connectors.remove(&output.crtc).is_none() {
                error!("output ID missing for disconnected crtc: {}", output.crtc);
            }
        }

        // Some devices, notably USB-C docks with DP-MST/alt-mode, report Connected before the
        // EDID has been read, with an empty mode list, then populate it later. Keep the
        // connector's identity and pick up the new modes; on_output_config_changed() below
        // chooses a mode if needed.
        for info in changed {
            match device.connectors.get_mut(&info.output.crtc) {
                Some(connector) => {
                    debug!("connector changed: {}", info.name);
                    // The EDID may have arrived just now. A live output already carries its
                    // name in user data, so only rename while nothing is connected yet.
                    if connector.surface.is_none() {
                        connector.name = OutputName {
                            connector: info.name.clone(),
                            make: info.make.clone(),
                            model: info.model.clone(),
                            serial: info.serial.clone(),
                        };
                    }
                    connector.info = info;
                }
                None => {
                    warn!(
                        "changed connector {} was unknown; treating as connected",
                        info.name
                    );
                    connected.push(info);
                }
            }
        }

        for info in connected {
            let mut name = OutputName {
                connector: info.name.clone(),
                make: info.make.clone(),
                model: info.model.clone(),
                serial: info.serial.clone(),
            };
            debug!(
                "new connector: {} \"{}\"",
                &name.connector,
                name.format_make_model_serial(),
            );

            // Connectors sharing make/model/serial can't be told apart by name; unname the
            // newcomer so config matching stays unambiguous.
            let formatted = name.format_make_model_serial_or_connector();
            for known in self.devices.values().flat_map(|d| d.connectors.values()) {
                if known.name.matches(&formatted) {
                    let connector = mem::take(&mut name.connector);
                    warn!(
                        "new connector {connector} duplicates make/model/serial \
                         of existing connector {}, unnaming",
                        known.name.connector,
                    );
                    name = OutputName {
                        connector,
                        make: None,
                        model: None,
                        serial: None,
                    };
                    break;
                }
            }

            let device = self.devices.get_mut(&node).unwrap();
            device.connectors.insert(
                info.output.crtc,
                Connector {
                    id: OutputId::next(),
                    name,
                    info,
                    surface: None,
                },
            );
        }

        if cleanup {
            let disable_laptop_panels = self.should_disable_laptop_panels(niri.is_lid_closed);
            let config = self.config.borrow();
            let disable_monitor_names = config.debug.disable_monitor_names;
            let device = self.devices.get(&node).unwrap();
            let off: Vec<u32> = device
                .connectors
                .iter()
                .filter(|(_, connector)| {
                    let output_name = connector.output_name(disable_monitor_names);
                    let c = config
                        .outputs
                        .find(&output_name)
                        .cloned()
                        .unwrap_or_default();
                    c.off || (disable_laptop_panels && is_laptop_panel(&output_name.connector))
                })
                .map(|(crtc, _)| *crtc)
                .collect();
            drop(config);

            if let Err(err) = self.request_ack(Request::CleanupDevice {
                dev: device_id,
                off,
            }) {
                warn!("error cleaning up connectors: {err:?}");
            }
        }

        self.on_output_config_changed(niri);
    }

    fn device_removed(&mut self, device_id: dev_t, niri: &mut Niri) {
        debug!("removing device: {device_id}");

        let Ok(node) = DrmNode::from_dev_id(device_id) else {
            warn!("error creating DrmNode");
            return;
        };
        if node.ty() != NodeType::Primary {
            debug!("not a primary node, skipping");
            return;
        }
        let Some(device) = self.devices.get(&node) else {
            warn!("unknown device");
            return;
        };

        let outputs: Vec<OutputRef> = device.connectors.values().map(|c| c.info.output).collect();
        for output in outputs {
            self.connector_disconnected(niri, output);
        }

        if let Err(err) = self.request_ack(Request::RemoveDevice { dev: device_id }) {
            warn!("error removing device in GPU process: {err:?}");
        }
        let device = self.devices.remove(&node).unwrap();

        if node == self.primary_node {
            debug!("the primary device is gone; disabling the dmabuf global");
            self.renderer_ready = false;
            // Cursor textures lived in the renderer that just went away.
            niri.cursor_manager.clear_cache();
            if let Some(global) = self.dmabuf_global.take() {
                niri.dmabuf_state
                    .disable_global::<State>(&niri.display_handle, &global);
                niri.event_loop
                    .insert_source(
                        Timer::from_duration(Duration::from_secs(10)),
                        move |_, _, state| {
                            state
                                .niri
                                .dmabuf_state
                                .destroy_global::<State>(&state.niri.display_handle, global);
                            TimeoutAction::Drop
                        },
                    )
                    .unwrap();
            }
        }

        self.refresh_ipc_outputs(niri);

        if let Err(err) = self.session.close(device.fd) {
            warn!("error closing DRM device fd: {err:?}");
        }
    }

    fn connector_connected(
        &mut self,
        niri: &mut Niri,
        output_ref: OutputRef,
    ) -> anyhow::Result<()> {
        let node = DrmNode::from_dev_id(output_ref.dev)?;
        let device = self.devices.get_mut(&node).context("missing device")?;
        let connector = device
            .connectors
            .get_mut(&output_ref.crtc)
            .context("missing connector")?;
        ensure!(connector.surface.is_none(), "connector already connected");
        let info = &connector.info;
        let connector_name = info.name.clone();
        debug!("connecting connector: {connector_name}");

        if info.non_desktop {
            // DRM leasing isn't supported in this split yet; leave the connector alone.
            debug!("output is non desktop, ignoring");
            return Ok(());
        }

        let disable_monitor_names = self.config.borrow().debug.disable_monitor_names;
        let output_name = connector.output_name(disable_monitor_names);
        let config = self
            .config
            .borrow()
            .outputs
            .find(&output_name)
            .cloned()
            .unwrap_or_default();

        let modes: Vec<DrmMode> = info.modes.iter().map(DrmMode::from).collect();
        for m in &modes {
            trace!("{m:?}");
        }

        let mut mode = None;
        if let Some(modeline) = &config.modeline {
            match calculate_drm_mode_from_modeline(modeline) {
                Ok(x) => mode = Some(x),
                Err(err) => {
                    warn!("invalid custom modeline; falling back to advertised modes: {err:?}");
                }
            }
        }
        let (mode, fallback) = match mode {
            Some(x) => (x, false),
            None => pick_mode(&modes, config.mode).ok_or_else(|| anyhow!("no mode"))?,
        };
        if fallback {
            let target = config.mode.unwrap();
            warn!(
                "configured mode {}x{}{} could not be found, falling back to preferred",
                target.mode.width,
                target.mode.height,
                if let Some(refresh) = target.mode.refresh {
                    format!("@{refresh}")
                } else {
                    String::new()
                },
            );
        }
        debug!("picking mode: {mode:?}");

        let orientation = info.panel_orientation.map(convert::to_transform);
        let physical_size = info.physical_size_mm.unwrap_or((0, 0));
        let connector_handle = info.connector;
        let hdr_caps = info.hdr;
        let max_bpc_range = info.max_bpc_range;
        debug!(?hdr_caps, ?max_bpc_range, "connector color capabilities");
        if config.hdr.is_some() && !hdr_caps.supported {
            warn!(
                "output {connector_name}: hdr is enabled in the config, but the driver or \
                 display does not support it (needs Colorspace BT2020_RGB, HDR_OUTPUT_METADATA \
                 and an EDID advertising PQ)"
            );
        }
        // 10 bits only where they pay off: HDR (so the PQ signal isn't crushed) and wide-gamut
        // P3 (the gamut remap stretches the 8-bit code points). SDR outputs stay 8-bit.
        let prefer_10bit = ((config.hdr.is_some() && hdr_caps.supported) || config.wide_gamut_p3)
            && !self.config.borrow().debug.disable_10bit_output;
        // Start SDR with the configured max bpc; the render loop reconciles HDR from there.
        let color_state = ColorState {
            hdr: None,
            max_bpc: effective_max_bpc(&config, max_bpc_range),
        };

        let reply = self.request(Request::EnableOutput {
            output: output_ref,
            connector: connector_handle,
            mode: ModeDesc::from(mode),
            vrr: config.is_vrr_always_on(),
            color: color_state,
            clear: !niri.monitors_active,
            prefer_10bit,
        })?;
        let Event::OutputState {
            mode: mode_desc,
            vrr_enabled,
            vrr_supported,
            max_bpc,
            ..
        } = reply
        else {
            bail!("unexpected reply to EnableOutput: {reply:?}");
        };
        let mode = DrmMode::from(&mode_desc);
        if !vrr_supported && !config.is_vrr_always_off() {
            warn!("cannot enable VRR because connector does not support it");
        }

        let output = Output::new(
            connector_name.clone(),
            PhysicalProperties {
                size: (physical_size.0 as i32, physical_size.1 as i32).into(),
                subpixel: Subpixel::Unknown,
                model: output_name.model.as_deref().unwrap_or("Unknown").to_owned(),
                make: output_name.make.as_deref().unwrap_or("Unknown").to_owned(),
                serial_number: output_name
                    .serial
                    .as_deref()
                    .unwrap_or("Unknown")
                    .to_owned(),
            },
        );

        let wl_mode = Mode::from(mode);
        output.change_current_state(Some(wl_mode), None, None, None);
        output.set_preferred(wl_mode);

        output
            .user_data()
            .insert_if_missing(|| TtyOutputState(output_ref));
        output.user_data().insert_if_missing(|| output_name.clone());
        output.user_data().insert_if_missing(|| OutputHdrCaps {
            supported: hdr_caps.supported,
            max_luminance: hdr_caps.max_luminance,
            min_luminance: hdr_caps.min_luminance,
            max_frame_avg_luminance: hdr_caps.max_frame_avg_luminance,
        });
        if let Some(x) = orientation {
            output.user_data().insert_if_missing(|| PanelOrientation(x));
        }

        let vblank_frame_name =
            tracy_client::FrameName::new_leak(format!("vblank on {connector_name}"));

        let device = self.devices.get_mut(&node).unwrap();
        let connector = device.connectors.get_mut(&output_ref.crtc).unwrap();
        connector.info.max_bpc = max_bpc;
        connector.surface = Some(Surface {
            output: output.clone(),
            mode,
            vrr_enabled,
            vrr_supported,
            pending_gamma_change: None,
            color_state,
            failed_color_state: None,
            recorder: Recorder::default(),
            geometry: None,
            vblank_frame: None,
            vblank_frame_name,
        });

        niri.add_output(output.clone(), Some(refresh_interval(mode)), vrr_enabled);

        if niri.monitors_active {
            // Redraw once the output is fully set up.
            niri.event_loop.insert_idle(move |state| {
                if state.niri.output_state.contains_key(&output) {
                    state.niri.queue_redraw(&output);
                }
            });
        }
        Ok(())
    }

    /// Tears down our side of an output. `disable_in_gpu` is false when the GPU process
    /// already dropped it (connector unplugged).
    fn connector_disconnected(&mut self, niri: &mut Niri, output_ref: OutputRef) {
        self.disable_output(niri, output_ref, false);
    }

    fn disable_output(&mut self, niri: &mut Niri, output_ref: OutputRef, disable_in_gpu: bool) {
        let Ok(node) = DrmNode::from_dev_id(output_ref.dev) else {
            return;
        };
        let Some(device) = self.devices.get_mut(&node) else {
            debug!("disconnecting connector for crtc: {}", output_ref.crtc);
            error!("missing device");
            return;
        };
        let Some(connector) = device.connectors.get_mut(&output_ref.crtc) else {
            debug!("disconnecting connector for crtc: {}", output_ref.crtc);
            error!("missing connector");
            return;
        };
        let Some(surface) = connector.surface.take() else {
            debug!("crtc {} wasn't enabled", output_ref.crtc);
            return;
        };
        debug!("disconnecting connector: {:?}", connector.name.connector);

        if disable_in_gpu {
            if let Err(err) = self.request_ack(Request::DisableOutput { output: output_ref }) {
                warn!("error disabling output in GPU process: {err:?}");
            }
        }

        niri.remove_output(&surface.output);
    }

    fn find_connector(&mut self, output_ref: OutputRef) -> Option<&mut Connector> {
        let node = DrmNode::from_dev_id(output_ref.dev).ok()?;
        self.devices
            .get_mut(&node)?
            .connectors
            .get_mut(&output_ref.crtc)
    }

    fn find_surface(&mut self, output_ref: OutputRef) -> Option<&mut Surface> {
        let node = DrmNode::from_dev_id(output_ref.dev).ok()?;
        self.devices
            .get_mut(&node)?
            .connectors
            .get_mut(&output_ref.crtc)?
            .surface
            .as_mut()
    }

    fn on_vblank(
        &mut self,
        niri: &mut Niri,
        output_ref: OutputRef,
        sequence: u64,
        presentation_time: Option<Duration>,
        frame: Option<u64>,
    ) {
        let span = tracy_client::span!("Tty::on_vblank");
        let now = get_monotonic_time();

        let Some(surface) = self.find_surface(output_ref) else {
            error!(
                "missing surface in vblank callback for crtc {}",
                output_ref.crtc
            );
            return;
        };
        drop(surface.vblank_frame.take());
        let output = surface.output.clone();
        let name = output.name();
        span.emit_text(&name);

        let presentation_time = presentation_time.unwrap_or(Duration::ZERO);
        let presentation_time = if niri.config.borrow().debug.emulate_zero_presentation_time {
            Duration::ZERO
        } else {
            presentation_time
        };
        trace!("vblank on {name}, sequence {sequence}, presentation time {presentation_time:?}");

        let Some(output_state) = niri.output_state.get_mut(&output) else {
            error!("missing output state for {name}");
            return;
        };

        let refresh_interval = output_state.frame_clock.refresh_interval();
        let time = if presentation_time.is_zero() {
            now
        } else {
            presentation_time
        };

        if output_state
            .vblank_throttle
            .throttle(refresh_interval, time, move |state| {
                let tty = state.backend.tty();
                tty.on_vblank(&mut state.niri, output_ref, sequence, None, frame);
            })
        {
            return;
        }

        let redraw_needed = match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
            RedrawState::WaitingForVBlank { redraw_needed } => redraw_needed,
            state @ (RedrawState::Idle
            | RedrawState::Queued
            | RedrawState::WaitingForEstimatedVBlank(_)
            | RedrawState::WaitingForEstimatedVBlankAndQueued(_)) => {
                error!(
                    "unexpected redraw state for output {name} (should be WaitingForVBlank); \
                     can happen when resuming from sleep or powering on monitors: {state:?}"
                );
                true
            }
        };

        if let Some(pending) = frame.and_then(|frame| self.pending_frames.remove(&frame)) {
            if let Some(mut feedback) = pending.feedback {
                let refresh = match refresh_interval {
                    Some(refresh) => {
                        if output_state.frame_clock.vrr() {
                            Refresh::Variable(refresh)
                        } else {
                            Refresh::Fixed(refresh)
                        }
                    }
                    None => Refresh::Unknown,
                };
                let mut flags = wp_presentation_feedback::Kind::Vsync
                    | wp_presentation_feedback::Kind::HwCompletion;
                if !presentation_time.is_zero() {
                    flags.insert(wp_presentation_feedback::Kind::HwClock);
                }
                feedback.presented::<_, smithay::utils::Monotonic>(time, refresh, sequence, flags);
            }
        }
        // Frames whose vblank never came (mode change, output gone) would pile up otherwise.
        if self.pending_frames.len() > 64 {
            self.pending_frames.clear();
        }

        output_state.last_drm_sequence = Some(sequence as u32);
        output_state.frame_clock.presented(presentation_time);

        if redraw_needed || output_state.unfinished_animations_remain {
            if let Some(surface) = self.find_surface(output_ref) {
                let vblank_frame = tracy_client::Client::running()
                    .unwrap()
                    .non_continuous_frame(surface.vblank_frame_name);
                surface.vblank_frame = Some(vblank_frame);
            }
            niri.queue_redraw(&output);
        } else {
            niri.send_frame_callbacks(&output);
        }
    }

    fn on_estimated_vblank_timer(&self, niri: &mut Niri, output: Output) {
        let span = tracy_client::span!("Tty::on_estimated_vblank_timer");
        let name = output.name();
        span.emit_text(&name);

        let Some(output_state) = niri.output_state.get_mut(&output) else {
            error!("missing output state for {name}");
            return;
        };

        output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);

        match mem::replace(&mut output_state.redraw_state, RedrawState::Idle) {
            RedrawState::Idle => unreachable!(),
            RedrawState::Queued => unreachable!(),
            RedrawState::WaitingForVBlank { .. } => unreachable!(),
            RedrawState::WaitingForEstimatedVBlank(_) => (),
            RedrawState::WaitingForEstimatedVBlankAndQueued(_) => {
                output_state.redraw_state = RedrawState::Queued;
                return;
            }
        }

        if output_state.unfinished_animations_remain {
            niri.queue_redraw(&output);
        } else {
            niri.send_frame_callbacks(&output);
        }
    }

    pub fn seat_name(&self) -> String {
        self.session.seat()
    }

    /// Allocator for screencast / capture buffers; `None` until the GPU has a renderer.
    pub fn dmabuf_allocator(&self) -> Option<DmabufAllocator> {
        self.renderer_ready
            .then(|| self.renderer.dmabuf_allocator())
    }

    pub fn gpu_handle(&self) -> crate::gpu::remote::GpuHandle {
        self.renderer.gpu_handle()
    }

    pub fn with_primary_renderer<T>(
        &mut self,
        f: impl FnOnce(&mut RemoteRenderer) -> T,
    ) -> Option<T> {
        if !self.renderer_ready {
            return None;
        }
        Some(f(&mut self.renderer))
    }

    pub fn primary_render_node(&mut self) -> Option<DrmNode> {
        self.renderer_ready.then_some(self.primary_render_node)
    }

    pub fn render(
        &mut self,
        niri: &mut Niri,
        output: &Output,
        target_presentation_time: Duration,
    ) -> RenderResult {
        let span = tracy_client::span!("Tty::render");
        let rv = RenderResult::Skipped;

        let output_ref = output_ref_of(output);
        if !self.renderer_ready || !self.session.is_active() {
            return rv;
        }
        let Some(surface) = self.find_surface(output_ref) else {
            error!("missing surface");
            return rv;
        };
        span.emit_text(&output.name());

        // Tell the GPU process about scale/transform changes before recording the frame.
        let geometry = OutputGeometry {
            scale: output.current_scale().fractional_scale(),
            transform: convert::transform(output.current_transform()),
        };
        if surface.geometry != Some(geometry) {
            surface.geometry = Some(geometry);
            // The GPU forgets its element history on geometry changes; start over too.
            surface.recorder.clear();
            if let Err(err) = self.request_ack(Request::SetOutputGeometry {
                output: output_ref,
                geometry,
            }) {
                warn!("error setting output geometry: {err:?}");
                return rv;
            }
        }

        // Reconcile the output's blend space and HDR signalling with the config and content.
        //
        // With hdr mode="on", the connector stays in HDR (BT.2020 + PQ) and the desktop is
        // composited into that blend space. In auto mode, HDR engages only while a fullscreen
        // surface carries an HDR image description (passthrough), so the output is SDR
        // otherwise.
        //
        // The connector state is only *staged* in the GPU process; smithay applies it inside
        // its own commit as a single atomic modeset together with mode, CRTC and plane state
        // (committing connector color properties standalone hangs some drivers, notably
        // nvidia).
        let (blend, content_in_blend_space) = self.reconcile_color_state(niri, output);

        let ctx = RenderCtx {
            renderer: &mut self.renderer,
            target: RenderTarget::Output,
            xray: None,
        };
        let mut elements = niri.render_to_vec(ctx, output, true);

        if niri.debug_draw_damage {
            let output_state = niri.output_state.get_mut(output).unwrap();
            draw_damage(&mut output_state.debug_damage_tracker, &mut elements);
        }

        // Record every element with its damage and opaque regions. The GPU process feeds them
        // to its DRM compositor, which does the damage tracking and culling.
        let mode = output.current_mode().unwrap();
        let scale = Scale::from(output.current_scale().fractional_scale());
        let transform = output.current_transform();
        let surface = self.find_surface(output_ref).unwrap();
        let mut recorder = mem::take(&mut surface.recorder);
        let target = self
            .renderer
            .output_target(output_ref, transform.transform_size(mode.size));
        // Only this output's frame is in the blend space; casts and screenshots stay SDR.
        self.renderer.set_frame_blend(blend.map(BlendSpace::params));
        let res = recorder.record(
            &mut self.renderer,
            target,
            mode.size,
            transform,
            scale,
            &elements,
        );
        self.renderer.set_frame_blend(None);
        let surface = self.find_surface(output_ref).unwrap();
        surface.recorder = recorder;
        if let Err(err) = res {
            warn!("error recording frame: {err:?}");
            // The GPU never saw this frame; resend everything next time.
            surface.recorder.clear();
            drop(surface.vblank_frame.take());
            queue_estimated_vblank_timer(niri, output.clone(), target_presentation_time);
            return rv;
        }

        let frame_id = self.next_frame_id;
        self.next_frame_id += 1;

        // Overlay planes are disabled by default as they cause weird performance issues on my
        // system.
        let flags = {
            let debug = &self.config.borrow().debug;
            let vrr = niri.output_state.get(output).unwrap().frame_clock.vrr();
            let mut flags = PresentFlags {
                primary_scanout: !debug.disable_direct_scanout,
                primary_scanout_any_format: !debug.restrict_primary_scanout_to_matching_format,
                overlay_planes: debug.enable_overlay_planes && !debug.disable_direct_scanout,
                cursor_plane: !debug.disable_cursor_plane,
                skip_cursor_only_updates: debug.skip_cursor_only_updates_during_vrr && vrr,
            };
            if blend.is_some() {
                // The cursor and overlay planes are filled without going through GLES, so
                // their content would bypass the blend transform; composite them instead.
                flags.cursor_plane = false;
                flags.overlay_planes = false;
                // Fullscreen content already encoded in the blend space may scan out directly;
                // SDR content must go through the blend shader.
                if !content_in_blend_space {
                    flags.primary_scanout = false;
                }
            }
            flags
        };

        // Both are one-way: the outcome arrives as GpuEvent::Presented, then VBlank.
        let sent = self
            .renderer
            .flush()
            .map_err(anyhow::Error::from)
            .and_then(|()| {
                self.renderer.client().send_oneway(
                    &Request::Present {
                        output: output_ref,
                        frame: frame_id,
                        flags,
                    },
                    &[],
                )
            });
        if let Err(err) = sent {
            warn!("error sending frame to the GPU process: {err:?}");
            let surface = self.find_surface(output_ref).unwrap();
            surface.recorder.clear();
            drop(surface.vblank_frame.take());
            queue_estimated_vblank_timer(niri, output.clone(), target_presentation_time);
            return rv;
        }

        self.pending_frames.insert(
            frame_id,
            PendingFrame {
                output: output.clone(),
                feedback: None,
            },
        );

        let output_state = niri.output_state.get_mut(output).unwrap();
        let new_state = RedrawState::WaitingForVBlank {
            redraw_needed: false,
        };
        match mem::replace(&mut output_state.redraw_state, new_state) {
            RedrawState::Idle => unreachable!(),
            RedrawState::Queued => (),
            RedrawState::WaitingForVBlank { .. } => unreachable!(),
            RedrawState::WaitingForEstimatedVBlank(_) => unreachable!(),
            RedrawState::WaitingForEstimatedVBlankAndQueued(token) => {
                niri.event_loop.remove(token);
            }
        };
        output_state.frame_callback_sequence = output_state.frame_callback_sequence.wrapping_add(1);
        RenderResult::Submitted
    }

    /// The GPU process finished compositing a frame we sent with `Present`.
    fn on_presented(
        &mut self,
        niri: &mut Niri,
        output_ref: OutputRef,
        frame: u64,
        submitted: bool,
        states: &[ElementState],
    ) {
        let _span = tracy_client::span!("Tty::on_presented");
        let Some(pending) = self.pending_frames.get(&frame) else {
            debug!("presented event for unknown frame {frame}");
            return;
        };
        let output = pending.output.clone();

        let states = match self.find_surface(output_ref) {
            Some(surface) => surface.recorder.element_states(states),
            None => RenderElementStates::default(),
        };
        niri.update_primary_scanout_output(&output, &states);

        if submitted {
            let feedback = niri.take_presentation_feedbacks(&output, &states);
            if let Some(pending) = self.pending_frames.get_mut(&frame) {
                pending.feedback = Some(feedback);
            }
            return;
        }

        // Nothing changed on screen, so no vblank will come for this frame. Fall back to the
        // estimated vblank timer like an in-process no-damage render would.
        self.pending_frames.remove(&frame);
        if let Some(surface) = self.find_surface(output_ref) {
            drop(surface.vblank_frame.take());
        }
        let Some(output_state) = niri.output_state.get_mut(&output) else {
            return;
        };
        let redraw_needed = match output_state.redraw_state {
            RedrawState::WaitingForVBlank { redraw_needed } => redraw_needed,
            // Something else (VT switch, output change) already moved the state on.
            _ => return,
        };
        output_state.redraw_state = RedrawState::Queued;
        let target_presentation_time = output_state.frame_clock.next_presentation_time();
        queue_estimated_vblank_timer(niri, output.clone(), target_presentation_time);
        if redraw_needed {
            let output_state = niri.output_state.get_mut(&output).unwrap();
            if let RedrawState::WaitingForEstimatedVBlank(token) = output_state.redraw_state {
                output_state.redraw_state = RedrawState::WaitingForEstimatedVBlankAndQueued(token);
            }
        }
    }

    pub fn change_vt(&mut self, vt: i32) {
        if let Err(err) = self.session.change_vt(vt) {
            warn!("error changing VT: {err}");
        }
    }

    pub fn suspend(&self) {
        #[cfg(feature = "dbus")]
        if let Err(err) = suspend() {
            warn!("error suspending: {err:?}");
        }
    }

    pub fn toggle_debug_tint(&mut self) {
        self.debug_tint = !self.debug_tint;
        if let Err(err) = self.request_ack(Request::SetDebugTint {
            enable: self.debug_tint,
        }) {
            warn!("error toggling debug tint: {err:?}");
        }
    }

    pub fn import_dmabuf(&mut self, dmabuf: &Dmabuf) -> bool {
        if !self.renderer_ready {
            return false;
        }
        match self.renderer.import_dmabuf(dmabuf, None) {
            Ok(_texture) => {
                if dmabuf.node().is_none() {
                    dmabuf.set_node(Some(self.primary_render_node));
                }
                true
            }
            Err(err) => {
                debug!("error importing dmabuf: {err:?}");
                false
            }
        }
    }

    pub fn early_import(&mut self, _surface: &WlSurface) {
        // Imports happen at draw time in the GPU process; nothing to do ahead of time.
    }

    pub fn get_gamma_size(&self, output: &Output) -> anyhow::Result<u32> {
        let output_ref = output_ref_of(output);
        let node = DrmNode::from_dev_id(output_ref.dev)?;
        let device = self.devices.get(&node).context("missing device")?;
        let connector = device
            .connectors
            .get(&output_ref.crtc)
            .context("missing connector")?;
        ensure!(
            connector.info.gamma_size != 0,
            "setting gamma is not supported"
        );
        Ok(connector.info.gamma_size)
    }

    pub fn set_gamma(&mut self, output: &Output, ramp: Option<Vec<u16>>) -> anyhow::Result<()> {
        let output_ref = output_ref_of(output);
        let active = self.session.is_active();
        let surface = self.find_surface(output_ref).context("missing surface")?;
        if !active {
            surface.pending_gamma_change = Some(ramp);
            return Ok(());
        }
        self.request_ack(Request::SetGamma {
            output: output_ref,
            ramp,
        })
    }

    pub fn set_ctm(&mut self, output: &Output, ctm: Option<[f64; 9]>) -> anyhow::Result<()> {
        let output_ref = output_ref_of(output);
        ensure!(self.find_surface(output_ref).is_some(), "missing surface");
        // The GPU process applies it now, or on resume if the device is inactive.
        self.request_ack(Request::SetCtm {
            output: output_ref,
            matrix: ctm,
        })
    }

    /// Stages the connector color state matching the config and current content, and returns
    /// the blend space to composite the frame in plus whether the fullscreen content is already
    /// encoded in it.
    fn reconcile_color_state(
        &mut self,
        niri: &Niri,
        output: &Output,
    ) -> (Option<BlendSpace>, bool) {
        let output_ref = output_ref_of(output);
        let (hdr_config, wide_gamut_p3, max_bpc, hdr_caps) = {
            let Some(connector) = self.find_connector(output_ref) else {
                return (None, false);
            };
            let hdr_caps = connector.info.hdr;
            let max_bpc_range = connector.info.max_bpc_range;
            let name = connector.name.clone();
            let config = self.config.borrow();
            let output_config = config.outputs.find(&name).cloned().unwrap_or_default();
            (
                output_config.hdr.clone(),
                output_config.wide_gamut_p3,
                effective_max_bpc(&output_config, max_bpc_range),
                hdr_caps,
            )
        };

        let hdr_allowed = hdr_config.is_some() && hdr_caps.supported;
        let always_on = hdr_config.as_ref().is_some_and(|h| h.mode == HdrMode::On);
        let reference_luminance = hdr_config
            .as_ref()
            .and_then(|h| h.reference_luminance)
            .map(|v| v.0)
            .unwrap_or(DEFAULT_REFERENCE_LUMINANCE);

        let hdr_desc = hdr_allowed
            .then(|| niri.output_hdr_image_description(output))
            .flatten();
        let blend_hdr = hdr_allowed && (always_on || hdr_desc.is_some());

        let desired = if blend_hdr {
            // Without fullscreen HDR content, the metadata comes from the sink's EDID.
            let desc = hdr_desc.unwrap_or(ImageDescription {
                transfer: CmTransferFunction::St2084Pq,
                primaries: CmPrimaries::Bt2020,
                max_cll: None,
                max_fall: None,
                mastering_luminance: None,
                luminances: None,
            });
            ColorState {
                hdr: Some(build_hdr_metadata(&desc, &hdr_caps)),
                max_bpc,
            }
        } else {
            ColorState { hdr: None, max_bpc }
        };

        let surface = self.find_surface(output_ref).unwrap();
        if surface.color_state != desired && surface.failed_color_state != Some(desired) {
            let name = output.name();
            match self.request(Request::SetColorState {
                output: output_ref,
                state: desired,
            }) {
                Ok(Event::OutputState {
                    max_bpc: committed, ..
                }) => {
                    let connector = self.find_connector(output_ref).unwrap();
                    connector.info.max_bpc = committed;
                    let surface = connector.surface.as_mut().unwrap();
                    surface.color_state = desired;
                    surface.failed_color_state = None;
                    info!(
                        output = name,
                        hdr = desired.hdr.is_some(),
                        "updated HDR signalling to match content"
                    );
                }
                Ok(other) => warn!("unexpected reply to SetColorState: {other:?}"),
                Err(err) => {
                    let surface = self.find_surface(output_ref).unwrap();
                    surface.failed_color_state = Some(desired);
                    warn!("output {name:?}: failed to update HDR signalling: {err:?}");
                }
            }
        }

        // Blend in HDR only when the connector actually is in HDR; otherwise the PQ-encoded
        // frame would be shown as SDR.
        let surface = self.find_surface(output_ref).unwrap();
        let hdr_active = surface.color_state.hdr.is_some();
        if blend_hdr && hdr_active {
            let blend = BlendSpace::HdrPq {
                reference_luminance,
            };
            (Some(blend), hdr_desc.is_some())
        } else if wide_gamut_p3 {
            // No connector signalling: the panel scans out in its native (P3) colorspace.
            let in_space = niri.output_p3_image_description(output).is_some();
            (Some(BlendSpace::DisplayP3), in_space)
        } else {
            (None, false)
        }
    }

    fn refresh_ipc_outputs(&self, niri: &mut Niri) {
        let _span = tracy_client::span!("Tty::refresh_ipc_outputs");

        let mut ipc_outputs = HashMap::new();
        let disable_monitor_names = self.config.borrow().debug.disable_monitor_names;

        for device in self.devices.values() {
            for connector in device.connectors.values() {
                let info = &connector.info;
                let output_name = connector.output_name(disable_monitor_names);
                let surface = connector.surface.as_ref();
                let current_crtc_mode = surface.map(|s| s.mode);
                let mut current_mode = None;
                let mut is_custom_mode = false;

                let connector_modes: Vec<DrmMode> = info.modes.iter().map(DrmMode::from).collect();
                let mut modes: Vec<niri_ipc::Mode> = connector_modes
                    .iter()
                    .filter(|m| !m.flags().contains(ModeFlags::INTERLACE))
                    .enumerate()
                    .map(|(idx, m)| {
                        if Some(*m) == current_crtc_mode {
                            current_mode = Some(idx);
                        }
                        niri_ipc::Mode {
                            width: m.size().0,
                            height: m.size().1,
                            refresh_rate: Mode::from(*m).refresh as u32,
                            is_preferred: m.mode_type().contains(ModeTypeFlags::PREFERRED),
                        }
                    })
                    .collect();

                if let Some(crtc_mode) = current_crtc_mode {
                    if crtc_mode.mode_type().contains(ModeTypeFlags::USERDEF) {
                        modes.insert(
                            0,
                            niri_ipc::Mode {
                                width: crtc_mode.size().0,
                                height: crtc_mode.size().1,
                                refresh_rate: Mode::from(crtc_mode).refresh as u32,
                                is_preferred: false,
                            },
                        );
                        current_mode = Some(0);
                        is_custom_mode = true;
                    }
                    if current_mode.is_none() {
                        if crtc_mode.flags().contains(ModeFlags::INTERLACE) {
                            warn!("connector mode list missing current mode (interlaced)");
                        } else {
                            error!("connector mode list missing current mode");
                        }
                    }
                }

                let vrr_supported = surface
                    .map(|s| s.vrr_supported)
                    .unwrap_or(info.vrr_capable == Some(true));
                let vrr_enabled = surface.is_some_and(|s| s.vrr_enabled);
                let logical = surface.map(|s| logical_output(&s.output));

                ipc_outputs.insert(
                    connector.id,
                    niri_ipc::Output {
                        name: info.name.clone(),
                        make: output_name.make.unwrap_or_else(|| "Unknown".into()),
                        model: output_name.model.unwrap_or_else(|| "Unknown".into()),
                        serial: output_name.serial,
                        physical_size: info.physical_size_mm,
                        modes,
                        current_mode,
                        is_custom_mode,
                        vrr_supported,
                        vrr_enabled,
                        logical,
                        max_bpc: info.max_bpc,
                    },
                );
            }
        }

        let mut guard = self.ipc_outputs.lock().unwrap();
        *guard = ipc_outputs;
        niri.ipc_outputs_changed = true;
    }

    pub fn ipc_outputs(&self) -> Arc<Mutex<IpcOutputMap>> {
        self.ipc_outputs.clone()
    }

    pub fn set_monitors_active(&mut self, active: bool) {
        // Turning monitors on happens by rendering; off means showing black.
        if active {
            return;
        }
        if let Err(err) = self.request_ack(Request::ClearOutputs) {
            warn!("error clearing outputs: {err:?}");
        }
    }

    pub fn set_output_on_demand_vrr(&mut self, niri: &mut Niri, output: &Output, enable_vrr: bool) {
        let _span = tracy_client::span!("Tty::set_output_on_demand_vrr");

        let output_state = niri.output_state.get_mut(output).unwrap();
        output_state.on_demand_vrr_enabled = enable_vrr;
        if output_state.frame_clock.vrr() == enable_vrr {
            return;
        }
        let output_ref = output_ref_of(output);
        if self.find_surface(output_ref).is_none() {
            return;
        }
        match self.set_vrr(output_ref, enable_vrr) {
            Ok(vrr_enabled) => {
                let output_state = niri.output_state.get_mut(output).unwrap();
                output_state.frame_clock.set_vrr(vrr_enabled);
                self.refresh_ipc_outputs(niri);
            }
            Err(err) => warn!("output {:?}: error setting VRR: {err:?}", output.name()),
        }
    }

    /// Returns whether VRR is enabled afterwards.
    fn set_vrr(&mut self, output_ref: OutputRef, enable: bool) -> anyhow::Result<bool> {
        let reply = self.request(Request::SetVrr {
            output: output_ref,
            enable,
        })?;
        let Event::OutputState {
            vrr_enabled,
            vrr_supported,
            ..
        } = reply
        else {
            bail!("unexpected reply to SetVrr: {reply:?}");
        };
        if let Some(surface) = self.find_surface(output_ref) {
            surface.vrr_enabled = vrr_enabled;
            surface.vrr_supported = vrr_supported;
        }
        Ok(vrr_enabled)
    }

    fn compute_ignored_nodes(&self) -> HashSet<DrmNode> {
        let mut ignored_nodes = ignored_nodes_from_config(&self.config.borrow());
        if ignored_nodes.remove(&self.primary_node)
            || ignored_nodes.remove(&self.primary_render_node)
        {
            warn!("ignoring the primary node or render node is not allowed");
        }
        ignored_nodes
    }

    pub fn update_ignored_nodes_config(&mut self, niri: &mut Niri) {
        let _span = tracy_client::span!("Tty::update_ignored_nodes_config");

        if !self.session.is_active() {
            return;
        }

        let ignored_nodes = self.compute_ignored_nodes();
        if ignored_nodes == self.ignored_nodes {
            return;
        }
        self.ignored_nodes = ignored_nodes;

        let mut device_list = self
            .udev_dispatcher
            .as_source_ref()
            .device_list()
            .map(|(device_id, path)| (device_id, path.to_owned()))
            .collect::<HashMap<_, _>>();

        let removed_devices = self
            .devices
            .keys()
            .filter(|node| {
                self.ignored_nodes.contains(node) || !device_list.contains_key(&node.dev_id())
            })
            .copied()
            .collect::<Vec<_>>();
        for node in removed_devices {
            device_list.remove(&node.dev_id());
            self.device_removed(node.dev_id(), niri);
        }
        for node in self.devices.keys() {
            device_list.remove(&node.dev_id());
        }
        for (device_id, path) in device_list {
            if let Err(err) = self.device_added(device_id, &path, niri) {
                warn!("error adding device {path:?}: {err:?}");
            }
        }
    }

    fn should_disable_laptop_panels(&self, is_lid_closed: bool) -> bool {
        if !is_lid_closed {
            return false;
        }
        let config = self.config.borrow();
        if !config.debug.keep_laptop_panel_on_when_lid_is_closed {
            // Only if there's some other connected output to show things on.
            for device in self.devices.values() {
                for connector in device.connectors.values() {
                    if !is_laptop_panel(&connector.info.name) {
                        return true;
                    }
                }
            }
        }
        false
    }

    pub fn on_output_config_changed(&mut self, niri: &mut Niri) {
        let _span = tracy_client::span!("Tty::on_output_config_changed");

        if !self.session.is_active() {
            self.update_output_config_on_resume = true;
            return;
        }
        self.update_output_config_on_resume = false;

        let disable_laptop_panels = self.should_disable_laptop_panels(niri.is_lid_closed);
        let should_disable = |connector: &str| disable_laptop_panels && is_laptop_panel(connector);
        let disable_monitor_names = self.config.borrow().debug.disable_monitor_names;

        let mut to_disconnect = vec![];
        let mut to_connect = vec![];
        // (output, new mode, fallback flag, VRR to set, max_bpc)
        struct Change {
            output_ref: OutputRef,
            mode: Option<(DrmMode, bool)>,
            vrr: Option<bool>,
        }
        let mut changes: Vec<Change> = vec![];

        for device in self.devices.values() {
            for connector in device.connectors.values() {
                let output_ref = connector.info.output;
                let output_name = connector.output_name(disable_monitor_names);
                let config = self
                    .config
                    .borrow()
                    .outputs
                    .find(&output_name)
                    .cloned()
                    .unwrap_or_default();
                let off = config.off || should_disable(&output_name.connector);

                let Some(surface) = &connector.surface else {
                    if !off && !connector.info.non_desktop {
                        to_connect.push((output_ref, output_name));
                    }
                    continue;
                };
                if off {
                    to_disconnect.push(output_ref);
                    continue;
                }

                let modes: Vec<DrmMode> = connector.info.modes.iter().map(DrmMode::from).collect();
                let mut mode = None;
                if let Some(modeline) = &config.modeline {
                    match calculate_drm_mode_from_modeline(modeline) {
                        Ok(x) => mode = Some(x),
                        Err(err) => {
                            warn!(
                                "output {:?}: invalid custom modeline; \
                                 falling back to advertised modes: {err:?}",
                                output_name.connector
                            );
                        }
                    }
                }
                let (mode, fallback) = match mode {
                    Some(x) => (x, false),
                    None => match pick_mode(&modes, config.mode) {
                        Some(result) => result,
                        None => {
                            warn!("couldn't pick mode for enabled connector");
                            continue;
                        }
                    },
                };

                let change_mode = surface.mode != mode;
                let vrr_enabled = surface.vrr_enabled;
                let change_always_vrr = vrr_enabled != config.is_vrr_always_on();
                let is_on_demand_vrr = config.is_vrr_on_demand();

                let mut vrr = None;
                if let Some(output_state) = niri.output_state.get(&surface.output) {
                    if (is_on_demand_vrr && vrr_enabled != output_state.on_demand_vrr_enabled)
                        || (!is_on_demand_vrr && change_always_vrr)
                    {
                        vrr = Some(!vrr_enabled);
                    }
                }

                changes.push(Change {
                    output_ref,
                    mode: change_mode.then_some((mode, fallback)),
                    vrr,
                });
            }
        }

        for change in changes {
            let output_ref = change.output_ref;
            let Some(surface) = self.find_surface(output_ref) else {
                continue;
            };
            // max-bpc and hdr changes flow through the render loop's color state
            // reconciliation; give a previously rejected state another chance with the new
            // config.
            surface.failed_color_state = None;
            let output = surface.output.clone();
            let name = output.name();
            niri.queue_redraw(&output);

            if let Some(vrr) = change.vrr {
                match self.set_vrr(output_ref, vrr) {
                    Ok(vrr_enabled) => {
                        if let Some(output_state) = niri.output_state.get_mut(&output) {
                            output_state.frame_clock.set_vrr(vrr_enabled);
                        }
                    }
                    Err(err) => {
                        let word = if vrr { "enabling" } else { "disabling" };
                        warn!("output {name:?}: error {word} VRR: {err:?}");
                    }
                }
            }

            if let Some((mode, fallback)) = change.mode {
                if fallback {
                    let config = self
                        .config
                        .borrow()
                        .outputs
                        .find(output.user_data().get::<OutputName>().unwrap())
                        .cloned()
                        .unwrap_or_default();
                    if let Some(target) = config.mode {
                        warn!(
                            "output {name:?}: configured mode {}x{}{} could not be found, \
                             falling back to preferred",
                            target.mode.width,
                            target.mode.height,
                            if let Some(refresh) = target.mode.refresh {
                                format!("@{refresh}")
                            } else {
                                String::new()
                            },
                        );
                    }
                }

                debug!("output {name:?}: picking mode: {mode:?}");
                let reply = self.request(Request::SetMode {
                    output: output_ref,
                    mode: ModeDesc::from(mode),
                });
                let (mode, vrr_enabled) = match reply {
                    Ok(Event::OutputState {
                        mode, vrr_enabled, ..
                    }) => (DrmMode::from(&mode), vrr_enabled),
                    Ok(other) => {
                        warn!("unexpected reply to SetMode: {other:?}");
                        continue;
                    }
                    Err(err) => {
                        warn!("error changing mode: {err:?}");
                        continue;
                    }
                };

                if let Some(surface) = self.find_surface(output_ref) {
                    surface.mode = mode;
                    surface.vrr_enabled = vrr_enabled;
                }
                let wl_mode = Mode::from(mode);
                output.change_current_state(Some(wl_mode), None, None, None);
                output.set_preferred(wl_mode);
                if let Some(output_state) = niri.output_state.get_mut(&output) {
                    output_state.frame_clock =
                        FrameClock::new(Some(refresh_interval(mode)), vrr_enabled);
                }
                niri.output_resized(&output);
            }
        }

        for output_ref in to_disconnect {
            self.disable_output(niri, output_ref, true);
        }

        // Connect in a predictable order so the position logic sees a stable sequence.
        to_connect.sort_unstable_by(|a, b| a.1.compare(&b.1));
        for (output_ref, _name) in to_connect {
            if let Err(err) = self.connector_connected(niri, output_ref) {
                warn!("error connecting connector: {err:?}");
            }
        }

        self.refresh_ipc_outputs(niri);
    }

    pub fn disconnected_connector_name_by_name_match(&self, target: &str) -> Option<OutputName> {
        let disable_monitor_names = self.config.borrow().debug.disable_monitor_names;
        for device in self.devices.values() {
            for connector in device.connectors.values() {
                if connector.surface.is_some() || connector.info.non_desktop {
                    continue;
                }
                let output_name = connector.output_name(disable_monitor_names);
                if output_name.matches(target) {
                    return Some(output_name);
                }
            }
        }
        None
    }
}

impl Connector {
    fn output_name(&self, disable_monitor_names: bool) -> OutputName {
        if disable_monitor_names {
            return OutputName {
                connector: self.info.name.clone(),
                make: None,
                model: None,
                serial: None,
            };
        }
        self.name.clone()
    }
}

fn primary_node_from_render_node(path: &Path) -> Option<(DrmNode, DrmNode)> {
    match DrmNode::from_path(path) {
        Ok(node) => {
            if node.ty() == NodeType::Render {
                match node.node_with_type(NodeType::Primary) {
                    Some(Ok(primary_node)) => {
                        return Some((primary_node, node));
                    }
                    Some(Err(err)) => {
                        warn!("error opening primary node for render node {path:?}: {err:?}");
                    }
                    None => {
                        warn!("error opening primary node for render node {path:?}");
                    }
                }
            } else {
                warn!("DRM node {path:?} is not a render node");
                if let Some(Ok(render_node)) = node.node_with_type(NodeType::Render) {
                    return Some((node, render_node));
                }
                warn!("could not get render node for DRM node {path:?}; proceeding anyway");
                return Some((node, node));
            }
        }
        Err(err) => {
            warn!("error opening {path:?} as DRM node: {err:?}");
        }
    }
    None
}

fn primary_node_from_config(config: &Config) -> Option<(DrmNode, DrmNode)> {
    let path = config.debug.render_drm_device.as_ref()?;
    debug!("attempting to use render node from config: {path:?}");
    primary_node_from_render_node(path)
}

fn ignored_nodes_from_config(config: &Config) -> HashSet<DrmNode> {
    let mut disabled_nodes = HashSet::new();
    for path in &config.debug.ignored_drm_devices {
        if let Some((primary_node, render_node)) = primary_node_from_render_node(path) {
            disabled_nodes.insert(primary_node);
            disabled_nodes.insert(render_node);
        }
    }
    disabled_nodes
}

fn refresh_interval(mode: DrmMode) -> Duration {
    let clock = mode.clock() as u64;
    let htotal = mode.hsync().2 as u64;
    let vtotal = mode.vsync().2 as u64;

    let mut numerator = htotal * vtotal * 1_000_000;
    let mut denominator = clock;

    if mode.flags().contains(ModeFlags::INTERLACE) {
        denominator *= 2;
    }
    if mode.flags().contains(ModeFlags::DBLSCAN) {
        numerator *= 2;
    }
    if mode.vscan() > 1 {
        numerator *= mode.vscan() as u64;
    }

    let refresh_interval = (numerator + denominator / 2) / denominator;
    Duration::from_nanos(refresh_interval)
}

#[cfg(feature = "dbus")]
fn suspend() -> anyhow::Result<()> {
    let conn = zbus::blocking::Connection::system().context("error connecting to system bus")?;
    conn.call_method(
        Some("org.freedesktop.login1"),
        "/org/freedesktop/login1",
        Some("org.freedesktop.login1.Manager"),
        "Suspend",
        &(true),
    )
    .context("error suspending")?;
    Ok(())
}

fn queue_estimated_vblank_timer(
    niri: &mut Niri,
    output: Output,
    target_presentation_time: Duration,
) {
    let output_state = niri.output_state.get_mut(&output).unwrap();
    match mem::take(&mut output_state.redraw_state) {
        RedrawState::Idle => unreachable!(),
        RedrawState::Queued => (),
        RedrawState::WaitingForVBlank { .. } => unreachable!(),
        RedrawState::WaitingForEstimatedVBlank(token)
        | RedrawState::WaitingForEstimatedVBlankAndQueued(token) => {
            output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
            return;
        }
    }

    let now = get_monotonic_time();
    let mut duration = target_presentation_time.saturating_sub(now);
    // No time left? Wait one more refresh so we don't spin.
    if duration.is_zero() {
        duration += output_state
            .frame_clock
            .refresh_interval()
            .unwrap_or(Duration::from_micros(16_667));
    }

    trace!("queueing estimated vblank timer to fire in {duration:?}");

    let timer = Timer::from_duration(duration);
    let token = niri
        .event_loop
        .insert_source(timer, move |_, _, data| {
            data.backend
                .tty()
                .on_estimated_vblank_timer(&mut data.niri, output.clone());
            TimeoutAction::Drop
        })
        .unwrap();
    output_state.redraw_state = RedrawState::WaitingForEstimatedVBlank(token);
}

pub fn calculate_drm_mode_from_modeline(modeline: &Modeline) -> anyhow::Result<DrmMode> {
    ensure!(
        modeline.hdisplay < modeline.hsync_start,
        "hdisplay {} must be < hsync_start {}",
        modeline.hdisplay,
        modeline.hsync_start
    );
    ensure!(
        modeline.hsync_start < modeline.hsync_end,
        "hsync_start {} must be < hsync_end {}",
        modeline.hsync_start,
        modeline.hsync_end
    );
    ensure!(
        modeline.hsync_end < modeline.htotal,
        "hsync_end {} must be < htotal {}",
        modeline.hsync_end,
        modeline.htotal
    );
    ensure!(
        modeline.vdisplay < modeline.vsync_start,
        "vdisplay {} must be < vsync_start {}",
        modeline.vdisplay,
        modeline.vsync_start
    );
    ensure!(
        modeline.vsync_start < modeline.vsync_end,
        "vsync_start {} must be < vsync_end {}",
        modeline.vsync_start,
        modeline.vsync_end
    );
    ensure!(
        modeline.vsync_end < modeline.vtotal,
        "vsync_end {} must be < vtotal {}",
        modeline.vsync_end,
        modeline.vtotal
    );

    let pixel_clock_kilo_hertz = modeline.clock * 1000.0;
    // Calculated as documented in the CVT 1.2 standard:
    // https://app.box.com/s/vcocw3z73ta09txiskj7cnk6289j356b/file/93518784646
    let vrefresh_hertz = (pixel_clock_kilo_hertz * 1000.0)
        / (modeline.htotal as u64 * modeline.vtotal as u64) as f64;
    ensure!(
        vrefresh_hertz.is_finite(),
        "calculated refresh rate is not finite"
    );
    let vrefresh_rounded = vrefresh_hertz.round() as u32;

    let flags = match modeline.hsync_polarity {
        HSyncPolarity::PHSync => ModeFlags::PHSYNC,
        HSyncPolarity::NHSync => ModeFlags::NHSYNC,
    } | match modeline.vsync_polarity {
        VSyncPolarity::PVSync => ModeFlags::PVSYNC,
        VSyncPolarity::NVSync => ModeFlags::NVSYNC,
    };

    let mode_name = format!(
        "{}x{}@{:.2}",
        modeline.hdisplay, modeline.vdisplay, vrefresh_hertz
    );
    let name = modeinfo_name_slice_from_string(&mode_name);

    // https://www.kernel.org/doc/html/v6.17/gpu/drm-uapi.html#c.drm_mode_modeinfo
    Ok(DrmMode::from(drm_mode_modeinfo {
        clock: pixel_clock_kilo_hertz.round() as u32,
        hdisplay: modeline.hdisplay,
        hsync_start: modeline.hsync_start,
        hsync_end: modeline.hsync_end,
        htotal: modeline.htotal,
        vdisplay: modeline.vdisplay,
        vsync_start: modeline.vsync_start,
        vsync_end: modeline.vsync_end,
        vtotal: modeline.vtotal,
        vrefresh: vrefresh_rounded,
        flags: flags.bits(),
        name,
        // Defaults
        type_: drm_ffi::DRM_MODE_TYPE_USERDEF,
        hskew: 0,
        vscan: 0,
    }))
}

pub fn calculate_mode_cvt(width: u16, height: u16, refresh: f64) -> DrmMode {
    // Cross-checked with sway's implementation:
    // https://gitlab.freedesktop.org/wlroots/wlroots/-/blob/22528542970687720556035790212df8d9bb30bb/backend/drm/util.c#L251

    let options = libdisplay_info::cvt::Options {
        red_blank_ver: libdisplay_info::cvt::ReducedBlankingVersion::None,
        h_pixels: width as i32,
        v_lines: height as i32,
        ip_freq_rqd: refresh,

        // Defaults
        video_opt: false,
        vblank: 0f64,
        additional_hblank: 0,
        early_vsync_rqd: false,
        int_rqd: false,
        margins_rqd: false,
    };
    let cvt_timing = libdisplay_info::cvt::Timing::compute(options);

    let hsync_start = width.saturating_add(cvt_timing.h_front_porch as u16);
    let vsync_start = (cvt_timing.v_lines_rnd + cvt_timing.v_front_porch) as u16;
    let hsync_end = hsync_start.saturating_add(cvt_timing.h_sync as u16);
    let vsync_end = vsync_start.saturating_add(cvt_timing.v_sync as u16);

    let htotal = hsync_end.saturating_add(cvt_timing.h_back_porch as u16);
    let vtotal = vsync_end.saturating_add(cvt_timing.v_back_porch as u16);

    let clock = f64::round(cvt_timing.act_pixel_freq * 1000f64) as u32;
    let vrefresh = f64::round(cvt_timing.act_frame_rate) as u32;

    let flags = drm_ffi::DRM_MODE_FLAG_NHSYNC | drm_ffi::DRM_MODE_FLAG_PVSYNC;

    let mode_name = format!("{width}x{height}@{:.2}", cvt_timing.act_frame_rate);
    let name = modeinfo_name_slice_from_string(&mode_name);

    let drm_ffi_mode = drm_ffi::drm_sys::drm_mode_modeinfo {
        clock,

        hdisplay: width,
        hsync_start,
        hsync_end,
        htotal,

        vdisplay: height,
        vsync_start,
        vsync_end,
        vtotal,

        vrefresh,

        flags,
        type_: drm_ffi::DRM_MODE_TYPE_USERDEF,
        name,

        // Defaults
        hskew: 0,
        vscan: 0,
    };

    DrmMode::from(drm_ffi_mode)
}

// Returns a c-string of maximally 31 Rust string chars + null terminator. Excess characters are
// dropped.
fn modeinfo_name_slice_from_string(mode_name: &str) -> [core::ffi::c_char; 32] {
    let mut name: [core::ffi::c_char; 32] = [0; 32];

    for (a, b) in zip(&mut name[..31], mode_name.as_bytes()) {
        // Can be u8 on aarch64 and i8 on x86_64.
        *a = *b as _;
    }

    name
}

fn pick_mode(
    modes: &[DrmMode],
    target: Option<niri_config::output::Mode>,
) -> Option<(DrmMode, bool)> {
    let mut mode = None;
    let mut fallback = false;

    if let Some(target) = target {
        let target_mode = target.mode;

        if target.custom {
            if let Some(refresh) = target_mode.refresh {
                let custom_mode =
                    calculate_mode_cvt(target_mode.width, target_mode.height, refresh);
                return Some((custom_mode, false));
            } else {
                warn!("ignoring custom mode without refresh rate");
            }
        }

        let refresh = target_mode.refresh.map(|r| (r * 1000.).round() as i32);
        for m in modes {
            if m.size() != (target.mode.width, target.mode.height) {
                continue;
            }
            // Interlaced modes don't appear to work.
            if m.flags().contains(ModeFlags::INTERLACE) {
                continue;
            }
            if let Some(refresh) = refresh {
                // If refresh is set, only pick modes with matching refresh.
                let wl_mode = Mode::from(*m);
                if wl_mode.refresh == refresh {
                    mode = Some(m);
                }
            } else if let Some(curr) = mode {
                // If refresh isn't set, pick the mode with the highest refresh.
                if curr.vrefresh() < m.vrefresh() {
                    mode = Some(m);
                }
            } else {
                mode = Some(m);
            }
        }

        if mode.is_none() {
            fallback = true;
        }
    }

    if mode.is_none() {
        // Pick a preferred mode.
        for m in modes {
            if !m.mode_type().contains(ModeTypeFlags::PREFERRED) {
                continue;
            }
            if let Some(curr) = mode {
                if curr.vrefresh() < m.vrefresh() {
                    mode = Some(m);
                }
            } else {
                mode = Some(m);
            }
        }
    }

    if mode.is_none() {
        // Last attempt.
        mode = modes.first();
    }

    mode.map(|m| (*m, fallback))
}

/// Initializes the libinput plugin system.
///
/// # Safety
///
/// This function must be called before libinput iterates through the devices, i.e. before
/// libinput_udev_assign_seat() or the first call to libinput_path_add_device().
unsafe fn init_libinput_plugin_system(libinput: &Libinput) {
    #[cfg(have_libinput_plugin_system)]
    unsafe {
        use std::ffi::{c_char, c_int, CString};
        use std::os::unix::ffi::OsStringExt;

        use directories::BaseDirs;
        use input::ffi::libinput;
        use input::AsRaw as _;

        extern "C" {
            fn libinput_plugin_system_append_path(libinput: *const libinput, path: *const c_char);
            fn libinput_plugin_system_append_default_paths(libinput: *const libinput);
            fn libinput_plugin_system_load_plugins(
                libinput: *const libinput,
                flags: c_int,
            ) -> c_int;
        }
        const LIBINPUT_PLUGIN_SYSTEM_FLAG_NONE: c_int = 0;
        let libinput = libinput.as_raw();

        // Also load plugins from $XDG_CONFIG_HOME/libinput/plugins.
        if let Some(dirs) = BaseDirs::new() {
            let mut plugins_dir = dirs.config_dir().to_path_buf();
            plugins_dir.push("libinput");
            plugins_dir.push("plugins");
            if let Ok(plugins_dir) = CString::new(plugins_dir.into_os_string().into_vec()) {
                libinput_plugin_system_append_path(libinput, plugins_dir.as_ptr());
            }
        }

        libinput_plugin_system_append_default_paths(libinput);
        libinput_plugin_system_load_plugins(libinput, LIBINPUT_PLUGIN_SYSTEM_FLAG_NONE);
    }
    #[cfg(not(have_libinput_plugin_system))]
    let _ = libinput;
}

/// The `max bpc` to request for an output: the configured value, or 10 when HDR is enabled but
/// no explicit value was given (HDR needs at least 10 bits per channel so the PQ signal isn't
/// crushed). Clamped to the connector's supported range; `None` when the connector has no
/// `max bpc` property at all.
fn effective_max_bpc(output: &niri_config::Output, range: Option<(u32, u32)>) -> Option<u32> {
    let (min, max) = range?;
    let requested = output
        .max_bpc
        .map(|max_bpc| max_bpc.0 as u32)
        .or_else(|| output.hdr.is_some().then_some(10))?;
    Some(requested.clamp(min, max))
}

/// Builds the HDR static metadata to signal on the connector for a client's image description:
/// a PQ infoframe with BT.2020 mastering primaries and D65 white point.
///
/// Luminance priority: what the client provided (clamped to the sink's EDID capabilities) >
/// the sink's EDID desired-content values > conservative ~500 nit placeholders.
fn build_hdr_metadata(desc: &ImageDescription, edid: &HdrCaps) -> HdrMetadataDesc {
    let to_u16 = |v: u32| v.min(u16::MAX as u32) as u16;
    // Clamps a client-provided value to the sink's EDID capability, when the EDID has one.
    let clamp_to = |v: u16, edid_cap: u16| if edid_cap > 0 { v.min(edid_cap) } else { v };

    let max_luminance = desc
        .mastering_luminance
        .map(|(_, max)| clamp_to(to_u16(max), edid.max_luminance))
        .or((edid.max_luminance > 0).then_some(edid.max_luminance))
        .unwrap_or(500);
    let min_luminance = desc
        .mastering_luminance
        .map(|(min, _)| to_u16(min).max(edid.min_luminance))
        .or((edid.min_luminance > 0).then_some(edid.min_luminance))
        .unwrap_or(50);
    let max_cll = desc
        .max_cll
        .map(|v| clamp_to(to_u16(v), edid.max_luminance))
        .or((edid.max_luminance > 0).then_some(edid.max_luminance))
        .unwrap_or(500);
    let max_fall = desc
        .max_fall
        .map(|v| clamp_to(to_u16(v), edid.max_frame_avg_luminance))
        .or((edid.max_frame_avg_luminance > 0).then_some(edid.max_frame_avg_luminance))
        .unwrap_or(500);

    HdrMetadataDesc {
        max_luminance,
        min_luminance,
        max_cll,
        max_fall,
    }
}

#[cfg(test)]
mod tests {
    use insta::assert_debug_snapshot;
    use niri_config::output::Modeline;
    use niri_ipc::{HSyncPolarity, VSyncPolarity};

    use crate::backend::tty::{calculate_drm_mode_from_modeline, calculate_mode_cvt};

    #[test]
    fn hdr_metadata_luminance_priorities() {
        use smithay::wayland::color::management::{ImageDescription, Primaries, TransferFunction};

        use crate::gpu::protocol::HdrCaps;

        let pq_desc = ImageDescription {
            transfer: TransferFunction::St2084Pq,
            primaries: Primaries::Bt2020,
            max_cll: None,
            max_fall: None,
            mastering_luminance: None,
            luminances: None,
        };
        let edid = HdrCaps {
            supported: true,
            max_luminance: 800,
            min_luminance: 100,
            max_frame_avg_luminance: 600,
        };

        // No client data, no EDID data: conservative placeholders.
        let meta = super::build_hdr_metadata(&pq_desc, &HdrCaps::default());
        assert_eq!(meta.max_luminance, 500);
        assert_eq!(meta.min_luminance, 50);
        assert_eq!(meta.max_cll, 500);
        assert_eq!(meta.max_fall, 500);

        // No client data: EDID desired-content values win.
        let meta = super::build_hdr_metadata(&pq_desc, &edid);
        assert_eq!(meta.max_luminance, 800);
        assert_eq!(meta.min_luminance, 100);
        assert_eq!(meta.max_cll, 800);
        assert_eq!(meta.max_fall, 600);

        // Client data within the sink's capabilities is used as-is.
        let desc = ImageDescription {
            mastering_luminance: Some((200, 700)),
            max_cll: Some(650),
            max_fall: Some(300),
            ..pq_desc
        };
        let meta = super::build_hdr_metadata(&desc, &edid);
        assert_eq!(meta.max_luminance, 700);
        assert_eq!(meta.min_luminance, 200);
        assert_eq!(meta.max_cll, 650);
        assert_eq!(meta.max_fall, 300);

        // Client data beyond the sink's capabilities is clamped to the EDID.
        let desc = ImageDescription {
            mastering_luminance: Some((1, 4000)),
            max_cll: Some(4000),
            max_fall: Some(2000),
            ..pq_desc
        };
        let meta = super::build_hdr_metadata(&desc, &edid);
        assert_eq!(meta.max_luminance, 800);
        assert_eq!(meta.min_luminance, 100);
        assert_eq!(meta.max_cll, 800);
        assert_eq!(meta.max_fall, 600);
    }

    #[test]
    fn test_calculate_drmmode_from_modeline() {
        let modeline1 = Modeline {
            clock: 173.0,
            hdisplay: 1920,
            vdisplay: 1080,
            hsync_start: 2048,
            hsync_end: 2248,
            htotal: 2576,
            vsync_start: 1083,
            vsync_end: 1088,
            vtotal: 1120,
            hsync_polarity: HSyncPolarity::NHSync,
            vsync_polarity: VSyncPolarity::PVSync,
        };
        assert_debug_snapshot!(calculate_drm_mode_from_modeline(&modeline1).unwrap(), @r#"
        Mode {
            name: "1920x1080@59.96",
            clock: 173000,
            size: (
                1920,
                1080,
            ),
            hsync: (
                2048,
                2248,
                2576,
            ),
            vsync: (
                1083,
                1088,
                1120,
            ),
            hskew: 0,
            vscan: 0,
            vrefresh: 60,
            mode_type: ModeTypeFlags(
                USERDEF,
            ),
        }
        "#);
        let modeline2 = Modeline {
            clock: 452.5,
            hdisplay: 1920,
            vdisplay: 1080,
            hsync_start: 2088,
            hsync_end: 2296,
            htotal: 2672,
            vsync_start: 1083,
            vsync_end: 1088,
            vtotal: 1177,
            hsync_polarity: HSyncPolarity::NHSync,
            vsync_polarity: VSyncPolarity::PVSync,
        };
        assert_debug_snapshot!(calculate_drm_mode_from_modeline(&modeline2).unwrap(), @r#"
        Mode {
            name: "1920x1080@143.88",
            clock: 452500,
            size: (
                1920,
                1080,
            ),
            hsync: (
                2088,
                2296,
                2672,
            ),
            vsync: (
                1083,
                1088,
                1177,
            ),
            hskew: 0,
            vscan: 0,
            vrefresh: 144,
            mode_type: ModeTypeFlags(
                USERDEF,
            ),
        }
        "#);
    }

    #[test]
    fn test_calc_cvt() {
        // Crosschecked with other calculators like the cvt commandline utility.
        assert_debug_snapshot!(calculate_mode_cvt(1920, 1080, 60.0), @r#"
        Mode {
            name: "1920x1080@59.96",
            clock: 173000,
            size: (
                1920,
                1080,
            ),
            hsync: (
                2048,
                2248,
                2576,
            ),
            vsync: (
                1083,
                1088,
                1120,
            ),
            hskew: 0,
            vscan: 0,
            vrefresh: 60,
            mode_type: ModeTypeFlags(
                USERDEF,
            ),
        }
        "#);
        assert_debug_snapshot!(calculate_mode_cvt(1920, 1080, 144.0), @r#"
        Mode {
            name: "1920x1080@143.88",
            clock: 452500,
            size: (
                1920,
                1080,
            ),
            hsync: (
                2088,
                2296,
                2672,
            ),
            vsync: (
                1083,
                1088,
                1177,
            ),
            hskew: 0,
            vscan: 0,
            vrefresh: 144,
            mode_type: ModeTypeFlags(
                USERDEF,
            ),
        }
        "#);
    }

    #[test]
    fn test_calc_cvt_extreme_size() {
        // Width and height come from the client through set_custom_mode, so the timing sums must
        // not overflow u16.
        for (width, height) in [(u16::MAX, u16::MAX), (u16::MAX, 1), (1, u16::MAX)] {
            calculate_mode_cvt(width, height, 60.0);
        }
    }
}
