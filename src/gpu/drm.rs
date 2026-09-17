//! GPU-process side of the TTY backend: DRM/KMS devices, scanout and vblanks.
//!
//! The core opens the device nodes through libseat and hands the fds over; policy (which
//! connector is on, which mode, VRR) stays in the core. This module just does what it is told
//! and reports connectors and vblanks back.

use std::collections::{HashMap, HashSet};
use std::iter::zip;
use std::mem;
use std::num::{NonZeroU32, NonZeroU64};
use std::os::fd::{AsFd, OwnedFd};

use anyhow::{anyhow, bail, ensure, Context as _};
use bytemuck::cast_slice_mut;
use smithay::backend::allocator::dmabuf::{AsDmabuf as _, Dmabuf};
use smithay::backend::allocator::format::FormatSet;
use smithay::backend::allocator::gbm::{GbmAllocator, GbmBuffer, GbmBufferFlags, GbmDevice};
use smithay::backend::allocator::Fourcc;
use smithay::backend::drm::compositor::{DrmCompositor, FrameFlags, PrimaryPlaneElement};
use smithay::backend::drm::exporter::gbm::GbmFramebufferExporter;
use smithay::backend::drm::{
    Colorspace, ConnectorColorState, DrmDevice, DrmDeviceFd, DrmDeviceNotifier, DrmEventMetadata,
    DrmEventTime, DrmNode, HdrOutputMetadata, VrrSupport,
};
use smithay::backend::egl::context::ContextPriority;
use smithay::backend::egl::{EGLContext, EGLDevice, EGLDisplay};
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::RenderElementPresentationState;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::backend::renderer::DebugFlags;
use smithay::output::OutputModeSource;
use smithay::reexports::calloop::RegistrationToken;
use smithay::reexports::drm::control::atomic::AtomicModeReq;
use smithay::reexports::drm::control::dumbbuffer::DumbBuffer;
use smithay::reexports::drm::control::{
    connector, crtc, plane, property, AtomicCommitFlags, Device as _, Mode as DrmMode, PlaneType,
    ResourceHandle,
};
use smithay::reexports::gbm::Modifier;
use smithay::utils::{DeviceFd, Physical, Scale, Size, Transform};
use smithay_drm_extras::drm_scanner::{DrmScanEvent, DrmScanner};

use super::convert;
use super::exec::Executor;
use super::gl::{blend, resources, shaders};
use super::protocol::{
    BlendParams, ColorState, ConnectorInfo, DevId, ElementState, Event, HdrCaps, ModeDesc,
    OutputGeometry, OutputRef, PresentFlags, Presentation,
};
use super::scene::{self, split_elements, ElementTracks};

/// Scanout formats for SDR outputs: 8-bit only, like upstream niri. Asking for a 10-bit
/// framebuffer isn't free (some drivers, notably nvidia, hang the initial modeset on 2101010),
/// so outputs that did not opt into HDR / wide gamut stay 8-bit.
const SDR_COLOR_FORMATS: [Fourcc; 2] = [Fourcc::Argb8888, Fourcc::Abgr8888];
/// 10-bit formats tried (each probed for renderability) on HDR / wide-gamut outputs, ahead of
/// the 8-bit ones.
const TEN_BIT_COLOR_FORMATS: [Fourcc; 2] = [Fourcc::Abgr2101010, Fourcc::Argb2101010];

type GbmDrmCompositor =
    DrmCompositor<GbmAllocator<DeviceFd>, GbmFramebufferExporter<DeviceFd>, u64, DeviceFd>;

#[derive(Default)]
pub struct DrmState {
    devices: HashMap<DevId, Device>,
    primary: Option<DevId>,
    debug_tint: bool,
}

struct Device {
    #[allow(dead_code)]
    node: DrmNode,
    drm: DrmDevice,
    gbm: GbmDevice<DeviceFd>,
    allocator: GbmAllocator<DeviceFd>,
    /// Set when the device renders on the primary render node (it owns or shares the renderer's
    /// GPU); `None` means display-only, allocating from the rendering device.
    render_node: Option<DrmNode>,
    drm_scanner: DrmScanner,
    surfaces: HashMap<crtc::Handle, Surface>,
    token: RegistrationToken,
}

struct Surface {
    connector: connector::Handle,
    compositor: GbmDrmCompositor,
    gamma_props: Option<GammaProps>,
    ctm_props: Option<CtmProps>,
    /// CTM to apply once the device is active again.
    pending_ctm: Option<Option<[f64; 9]>>,
    /// Blend space of the last presented frame. A change alters what every shader outputs
    /// without any element damage, so it forces a full redraw.
    last_blend: Option<Option<BlendParams>>,
    geometry: OutputGeometry,
    /// Damage tracking state per core element id, so the DRM compositor sees real elements.
    elements: ElementTracks,
}

struct GammaProps {
    crtc: crtc::Handle,
    gamma_lut: property::Handle,
    gamma_lut_size: property::Handle,
    previous_blob: Option<NonZeroU64>,
}

/// Read-only snapshot of a connector's DRM properties.
struct ConnectorProperties {
    properties: Vec<(property::Info, property::RawValue)>,
}

struct CtmProps {
    crtc: crtc::Handle,
    ctm: property::Handle,
    previous_blob: Option<NonZeroU64>,
}

pub struct AddedDevice {
    pub render_node: Option<DevId>,
    pub renderer_created: bool,
}

fn output_ref(dev: DevId, crtc: crtc::Handle) -> OutputRef {
    OutputRef {
        dev,
        crtc: crtc.into(),
    }
}

/// The committed "max bpc" value, if the connector has the property.
fn read_max_bpc(drm: &DrmDevice, connector: connector::Handle) -> Option<u8> {
    let props = ConnectorProperties::try_new(drm, connector).ok()?;
    let (info, value) = props.find(c"max bpc").ok()?;
    info.value_type()
        .convert_value(*value)
        .as_unsigned_range()
        .map(|v| v as u8)
}

fn crtc_handle(crtc: u32) -> anyhow::Result<crtc::Handle> {
    NonZeroU32::new(crtc)
        .map(crtc::Handle::from)
        .ok_or_else(|| anyhow!("invalid crtc handle {crtc}"))
}

fn connector_handle(conn: u32) -> anyhow::Result<connector::Handle> {
    NonZeroU32::new(conn)
        .map(connector::Handle::from)
        .ok_or_else(|| anyhow!("invalid connector handle {conn}"))
}

fn mode_size(mode: &DrmMode) -> Size<i32, Physical> {
    let (w, h) = mode.size();
    Size::from((w as i32, h as i32))
}

fn mode_source(mode: &DrmMode, geometry: OutputGeometry) -> OutputModeSource {
    OutputModeSource::Static {
        size: mode_size(mode),
        scale: Scale::from(geometry.scale),
        transform: convert::to_transform(geometry.transform),
    }
}

impl DrmState {
    pub fn device_ids(&self) -> Vec<DevId> {
        self.devices.keys().copied().collect()
    }

    pub fn add_device(
        &mut self,
        exec: &mut Executor,
        fd: OwnedFd,
        dev: DevId,
        primary_render_node: DevId,
        register: impl FnOnce(DrmDeviceNotifier, DevId) -> anyhow::Result<RegistrationToken>,
    ) -> anyhow::Result<AddedDevice> {
        ensure!(!self.devices.contains_key(&dev), "device already added");
        let node = DrmNode::from_dev_id(dev).context("error creating DrmNode")?;
        let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));
        // Render-only cards (no KMS) fail here, same as upstream niri. On Asahi that's the GPU's
        // own card node; Mesa renders through the display controller's node instead, so the
        // renderer comes up when that device is added below.
        let (drm, notifier) = DrmDevice::new(device_fd.clone(), false).context("DrmDevice::new")?;
        let gbm = GbmDevice::new(device_fd.device_fd()).context("GbmDevice::new")?;

        // Like upstream's try_initialize_gpu: probe EGL on every device and let the one that
        // resolves to the primary render node own the renderer.
        let try_initialize_gpu = || -> anyhow::Result<(EGLDisplay, DrmNode)> {
            let display = unsafe { EGLDisplay::new(gbm.clone()).context("EGLDisplay::new")? };
            let egl_device = EGLDevice::device_for_display(&display).context("EGLDevice")?;
            ensure!(
                !egl_device.is_software(),
                "software EGL renderers are skipped"
            );
            let render_node = egl_device
                .try_get_render_node()
                .ok()
                .flatten()
                .unwrap_or(node);
            Ok((display, render_node))
        };

        let mut render_node = None;
        let mut renderer_created = false;
        match try_initialize_gpu() {
            Ok((display, egl_render_node)) if egl_render_node.dev_id() == primary_render_node => {
                debug!("device {node} renders on the primary render node {egl_render_node}");
                render_node = Some(egl_render_node);
                if !exec.has_renderer() {
                    let context = EGLContext::new_with_priority(&display, ContextPriority::High)
                        .context("EGLContext::new")?;
                    let mut renderer =
                        unsafe { GlesRenderer::new(context).context("GlesRenderer::new")? };
                    resources::init(&mut renderer);
                    shaders::init(&mut renderer);
                    exec.set_renderer(renderer);
                    renderer_created = true;
                }
                if renderer_created || self.primary.is_none() {
                    self.primary = Some(dev);
                }
            }
            Ok((_, egl_render_node)) => {
                // A secondary GPU. We have a single renderer, so treat it as display-only: its
                // buffers come from the primary GPU and are imported for scanout.
                debug!("device {node} renders on {egl_render_node}, using it as display-only");
            }
            Err(err) => {
                debug!("failed to initialize EGL on {node}, using it as display-only: {err:?}");
            }
        }

        let allocator_gbm = if render_node.is_some() {
            gbm.clone()
        } else if let Some(primary) = self.primary.and_then(|p| self.devices.get(&p)) {
            primary.gbm.clone()
        } else {
            bail!("no allocator available for device (rendering device not added yet)");
        };
        let allocator = GbmAllocator::new(
            allocator_gbm,
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );

        let token = register(notifier, dev)?;

        self.devices.insert(
            dev,
            Device {
                node,
                drm,
                gbm,
                allocator,
                render_node,
                drm_scanner: DrmScanner::new(),
                surfaces: HashMap::new(),
                token,
            },
        );

        Ok(AddedDevice {
            render_node: render_node.map(|n| n.dev_id()),
            renderer_created,
        })
    }

    /// Returns the device's event source token and whether it was the primary (rendering)
    /// device, in which case the caller must drop the renderer too.
    pub fn remove_device(&mut self, dev: DevId) -> Option<(RegistrationToken, bool)> {
        let device = self.devices.remove(&dev)?;
        let was_primary = self.primary == Some(dev);
        if was_primary {
            self.primary = None;
        }
        // Surfaces (and their DRM state) go away with the device.
        Some((device.token, was_primary))
    }

    pub fn pause(&mut self) {
        for device in self.devices.values_mut() {
            device.drm.pause();
        }
    }

    pub fn resume(&mut self, force_disable: bool) {
        for device in self.devices.values_mut() {
            if let Err(err) = device.drm.activate(force_disable) {
                warn!("error activating DRM device: {err:?}");
            }
            for surface in device.surfaces.values_mut() {
                // The connector color state (max bpc, HDR signalling) re-asserts itself via the
                // compositor's pending state on the next commit.
                if let Some(gamma_props) = &surface.gamma_props {
                    if let Err(err) = gamma_props.restore_gamma(&device.drm) {
                        warn!("error restoring gamma: {err:?}");
                    }
                }
                if let Some(ctm_props) = &mut surface.ctm_props {
                    let res = match surface.pending_ctm.take() {
                        Some(ctm) => ctm_props.set_ctm(&device.drm, ctm.as_ref()),
                        None => ctm_props.restore_ctm(&device.drm),
                    };
                    if let Err(err) = res {
                        warn!("error restoring CTM: {err:?}");
                    }
                }
            }
        }
    }

    fn device(&mut self, dev: DevId) -> anyhow::Result<&mut Device> {
        self.devices.get_mut(&dev).context("unknown device")
    }

    fn surface(&mut self, output: OutputRef) -> anyhow::Result<(&mut Device, crtc::Handle)> {
        let crtc = crtc_handle(output.crtc)?;
        let device = self.device(output.dev)?;
        ensure!(device.surfaces.contains_key(&crtc), "output is not enabled");
        Ok((device, crtc))
    }

    /// Re-reads connectors. Disconnected outputs are torn down here; the core learns about
    /// them from the reply.
    pub fn rescan(&mut self, dev: DevId) -> anyhow::Result<Event> {
        let device = self.device(dev)?;
        let scan = device
            .drm_scanner
            .scan_connectors(&device.drm)
            .context("error scanning connectors")?;

        let mut connected = Vec::new();
        let mut changed = Vec::new();
        let mut disconnected = Vec::new();
        for event in scan {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => connected.push(device.connector_info(dev, &connector, crtc)),
                DrmScanEvent::Changed {
                    connector,
                    crtc: Some(crtc),
                } => changed.push(device.connector_info(dev, &connector, crtc)),
                DrmScanEvent::Disconnected {
                    crtc: Some(crtc), ..
                } => {
                    device.surfaces.remove(&crtc);
                    disconnected.push(output_ref(dev, crtc));
                }
                _ => (),
            }
        }
        Ok(Event::Scan {
            connected,
            changed,
            disconnected,
        })
    }

    pub fn cleanup(&mut self, dev: DevId, off: &[u32]) -> anyhow::Result<()> {
        let device = self.device(dev)?;
        let off: HashSet<u32> = off.iter().copied().collect();
        device.cleanup_mismatching_resources(&|crtc, _| off.contains(&u32::from(crtc)))?;
        for surface in device.surfaces.values_mut() {
            if let Err(err) = surface.compositor.reset_state() {
                warn!("error resetting DrmCompositor state: {err:?}");
            }
            surface.compositor.reset_buffers();
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn enable_output(
        &mut self,
        exec: &mut Executor,
        output: OutputRef,
        connector: u32,
        mode: &ModeDesc,
        vrr: bool,
        color: ColorState,
        clear: bool,
        prefer_10bit: bool,
    ) -> anyhow::Result<Event> {
        let debug_tint = self.debug_tint;
        let crtc = crtc_handle(output.crtc)?;
        let connector = connector_handle(connector)?;
        let renderer = exec.renderer()?;
        let device = self
            .devices
            .get_mut(&output.dev)
            .context("unknown device")?;
        ensure!(
            !device.surfaces.contains_key(&crtc),
            "output is already enabled"
        );
        let mode = DrmMode::from(mode);

        // The connector color state (HDR signalling, max bpc) is staged on the compositor
        // below and rides the initial modeset; committing it standalone hangs some drivers.

        let mut gamma_props = GammaProps::new(&device.drm, crtc)
            .map_err(|err| debug!("couldn't get gamma properties: {err:?}"))
            .ok();
        let res = if let Some(gamma_props) = &mut gamma_props {
            gamma_props.set_gamma(&device.drm, None)
        } else {
            set_gamma_for_crtc(&device.drm, crtc, None)
        };
        if let Err(err) = res {
            debug!("couldn't reset gamma: {err:?}");
        }

        let mut ctm_props = CtmProps::new(&device.drm, crtc)
            .map_err(|err| debug!("couldn't get CTM properties: {err:?}"))
            .ok();
        if let Some(ctm_props) = &mut ctm_props {
            if let Err(err) = ctm_props.set_ctm(&device.drm, None) {
                debug!("couldn't reset CTM: {err:?}");
            }
        }

        let surface = device
            .drm
            .create_surface(crtc, mode, &[connector])
            .context("error creating DRM surface")?;

        let vrr_supported = match surface.vrr_supported(connector) {
            Ok(VrrSupport::Supported | VrrSupport::RequiresModeset) => {
                if let Err(err) = surface.use_vrr(vrr) {
                    warn!("error setting VRR: {err:?}");
                }
                true
            }
            Ok(VrrSupport::NotSupported) => {
                let _ = surface.use_vrr(false);
                false
            }
            Err(err) => {
                warn!("error querying for VRR support: {err:?}");
                false
            }
        };

        let display_only = device.render_node.is_none();
        let render_formats = renderer
            .egl_context()
            .dmabuf_render_formats()
            .iter()
            .copied()
            .filter(|format| {
                if display_only {
                    return format.modifier == Modifier::Linear;
                }
                // CCS modifiers can't be scanned out.
                !matches!(
                    format.modifier,
                    Modifier::I915_y_tiled_ccs
                        | Modifier::Unrecognized(0x100000000000005)
                        | Modifier::I915_y_tiled_gen12_rc_ccs
                        | Modifier::I915_y_tiled_gen12_mc_ccs
                        | Modifier::Unrecognized(0x100000000000008)
                        | Modifier::Unrecognized(0x10000000000000a)
                        | Modifier::Unrecognized(0x10000000000000b)
                        | Modifier::Unrecognized(0x10000000000000c)
                )
            })
            .collect::<FormatSet>();
        let geometry = OutputGeometry {
            scale: 1.,
            transform: convert::transform(Transform::Normal),
        };

        // 10-bit formats are probed one by one with a throwaway compositor and render_frame:
        // some drivers can render into AR30 but not AB30 (or vice versa), so treating 10-bit
        // as a boolean would pick a broken format or fall back too far.
        let mut color_formats = Vec::new();
        if prefer_10bit {
            for format in TEN_BIT_COLOR_FORMATS {
                let surface = device
                    .drm
                    .create_surface(crtc, mode, &[connector])
                    .context("error creating DRM surface")?;
                let mut compositor: GbmDrmCompositor = match DrmCompositor::new(
                    mode_source(&mode, geometry),
                    surface,
                    None,
                    device.allocator.clone(),
                    GbmFramebufferExporter::new(device.gbm.clone(), device.render_node.into()),
                    std::iter::once(format),
                    render_formats.clone(),
                    device.drm.cursor_size(),
                    Some(device.gbm.clone()),
                ) {
                    Ok(x) => x,
                    Err(err) => {
                        debug!(?format, "10-bit format not usable for scanout: {err:?}");
                        continue;
                    }
                };
                let no_elements: [SolidColorRenderElement; 0] = [];
                match compositor.render_frame(renderer, &no_elements, [0.; 4], FrameFlags::empty())
                {
                    Ok(_) => color_formats.push(format),
                    Err(err) => warn!(?format, "10-bit format is not renderable: {err:?}"),
                }
                // The trial only rendered, never committed; drop what it left in the swapchain.
                compositor.reset_buffers();
            }
            if color_formats.is_empty() {
                warn!("no usable 10-bit scanout format; using an 8-bit framebuffer");
            }
        }
        color_formats.extend(SDR_COLOR_FORMATS);
        debug!(?color_formats, "creating DRM compositor");
        let color_formats = color_formats.into_iter();

        let res = DrmCompositor::new(
            mode_source(&mode, geometry),
            surface,
            None,
            device.allocator.clone(),
            GbmFramebufferExporter::new(device.gbm.clone(), device.render_node.into()),
            color_formats.clone(),
            render_formats.clone(),
            device.drm.cursor_size(),
            Some(device.gbm.clone()),
        );
        let mut compositor = match res {
            Ok(x) => x,
            Err(err) => {
                warn!("error creating DRM compositor, will try with invalid modifier: {err:?}");
                let render_formats = render_formats
                    .iter()
                    .copied()
                    .filter(|format| format.modifier == Modifier::Invalid)
                    .collect::<FormatSet>();
                let surface = device
                    .drm
                    .create_surface(crtc, mode, &[connector])
                    .context("error creating DRM surface")?;
                DrmCompositor::new(
                    mode_source(&mode, geometry),
                    surface,
                    None,
                    device.allocator.clone(),
                    GbmFramebufferExporter::new(device.gbm.clone(), device.render_node.into()),
                    color_formats,
                    render_formats,
                    device.drm.cursor_size(),
                    Some(device.gbm.clone()),
                )
                .context("error creating DRM compositor")?
            }
        };

        // Stage the initial connector color state so it rides the initial modeset.
        if let Err(err) = compositor.use_color_state(connector_color_state(color)) {
            warn!("error staging initial connector color state: {err:?}");
        }

        if debug_tint {
            compositor.set_debug_flags(DebugFlags::TINT);
        }
        if clear {
            if let Err(err) = compositor.clear() {
                warn!("error clearing drm surface: {err:?}");
            }
        }

        let vrr_enabled = compositor.vrr_enabled();
        device.surfaces.insert(
            crtc,
            Surface {
                connector,
                compositor,
                gamma_props,
                ctm_props,
                pending_ctm: None,
                last_blend: None,
                geometry,
                elements: ElementTracks::default(),
            },
        );

        Ok(Event::OutputState {
            output,
            mode: ModeDesc::from(mode),
            vrr_enabled,
            vrr_supported,
            max_bpc: read_max_bpc(&device.drm, connector),
        })
    }

    pub fn disable_output(&mut self, output: OutputRef) -> anyhow::Result<()> {
        let crtc = crtc_handle(output.crtc)?;
        let device = self.device(output.dev)?;
        device.surfaces.remove(&crtc);
        Ok(())
    }

    fn output_state(device: &Device, output: OutputRef, crtc: crtc::Handle) -> Event {
        let surface = &device.surfaces[&crtc];
        let vrr_supported = matches!(
            surface.compositor.vrr_supported(surface.connector),
            Ok(VrrSupport::Supported | VrrSupport::RequiresModeset)
        );
        Event::OutputState {
            output,
            mode: ModeDesc::from(surface.compositor.pending_mode()),
            vrr_enabled: surface.compositor.vrr_enabled(),
            vrr_supported,
            max_bpc: read_max_bpc(&device.drm, surface.connector),
        }
    }

    pub fn set_mode(&mut self, output: OutputRef, mode: &ModeDesc) -> anyhow::Result<Event> {
        let (device, crtc) = self.surface(output)?;
        let surface = device.surfaces.get_mut(&crtc).unwrap();
        let mode = DrmMode::from(mode);
        surface
            .compositor
            .use_mode(mode)
            .context("error changing mode")?;
        surface
            .compositor
            .set_output_mode_source(mode_source(&mode, surface.geometry));
        Ok(Self::output_state(device, output, crtc))
    }

    pub fn set_vrr(&mut self, output: OutputRef, enable: bool) -> anyhow::Result<Event> {
        let (device, crtc) = self.surface(output)?;
        let surface = device.surfaces.get_mut(&crtc).unwrap();
        if let Err(err) = surface.compositor.use_vrr(enable) {
            warn!("error setting VRR: {err:?}");
        }
        Ok(Self::output_state(device, output, crtc))
    }

    /// Stages the connector color state for the next commit (smithay tests it with a
    /// TEST_ONLY commit first, so a rejected state errors here).
    pub fn set_color_state(
        &mut self,
        output: OutputRef,
        state: ColorState,
    ) -> anyhow::Result<Event> {
        let (device, crtc) = self.surface(output)?;
        let surface = device.surfaces.get_mut(&crtc).unwrap();
        let desired = connector_color_state(state);
        if surface.compositor.pending_color_state() != desired {
            surface
                .compositor
                .use_color_state(desired)
                .map_err(|err| anyhow!("error setting connector color state: {err}"))?;
            debug!(hdr = state.hdr.is_some(), max_bpc = ?state.max_bpc, "staged color state");
        }
        Ok(Self::output_state(device, output, crtc))
    }

    pub fn set_ctm(&mut self, output: OutputRef, matrix: Option<[f64; 9]>) -> anyhow::Result<()> {
        let (device, crtc) = self.surface(output)?;
        let surface = device.surfaces.get_mut(&crtc).unwrap();
        if !device.drm.is_active() {
            surface.pending_ctm = Some(matrix);
            return Ok(());
        }
        match &mut surface.ctm_props {
            Some(ctm_props) => ctm_props.set_ctm(&device.drm, matrix.as_ref()),
            // No CTM support on this CRTC.
            None => Ok(()),
        }
    }

    pub fn set_geometry(
        &mut self,
        output: OutputRef,
        geometry: OutputGeometry,
    ) -> anyhow::Result<()> {
        let (device, crtc) = self.surface(output)?;
        let surface = device.surfaces.get_mut(&crtc).unwrap();
        if surface.geometry != geometry {
            surface.geometry = geometry;
            let mode = surface.compositor.pending_mode();
            surface
                .compositor
                .set_output_mode_source(mode_source(&mode, geometry));
            // Everything moved; the old damage history is meaningless.
            surface.elements.clear();
        }
        Ok(())
    }

    pub fn set_gamma(&mut self, output: OutputRef, ramp: Option<&[u16]>) -> anyhow::Result<()> {
        let (device, crtc) = self.surface(output)?;
        let surface = device.surfaces.get_mut(&crtc).unwrap();
        if let Some(gamma_props) = &mut surface.gamma_props {
            gamma_props.set_gamma(&device.drm, ramp)
        } else {
            set_gamma_for_crtc(&device.drm, crtc, ramp)
        }
    }

    pub fn clear_outputs(&mut self) {
        for device in self.devices.values_mut() {
            for surface in device.surfaces.values_mut() {
                if let Err(err) = surface.compositor.clear() {
                    warn!("error clearing drm surface: {err:?}");
                }
            }
        }
    }

    pub fn set_debug_tint(&mut self, enable: bool) {
        self.debug_tint = enable;
        for device in self.devices.values_mut() {
            for surface in device.surfaces.values_mut() {
                let mut flags = surface.compositor.debug_flags();
                flags.set(DebugFlags::TINT, enable);
                surface.compositor.set_debug_flags(flags);
            }
        }
    }

    /// Draws the frame the core recorded for `output` and queues it for scanout. Returns
    /// whether anything was submitted plus what happened to each element.
    /// Allocates a render buffer on the primary device (for screencast / capture targets).
    pub fn allocate_dmabuf(
        &self,
        width: u32,
        height: u32,
        format: u32,
        modifiers: &[u64],
    ) -> anyhow::Result<Dmabuf> {
        let gbm = self.primary_gbm().context("no primary device")?;
        let fourcc = Fourcc::try_from(format).map_err(|_| anyhow!("unknown fourcc {format:#x}"))?;
        allocate_gbm_dmabuf(&gbm, width, height, fourcc, modifiers)
    }

    /// GBM handle of the primary (rendering) device, for allocating outside `DrmState`.
    pub fn primary_gbm(&self) -> Option<GbmDevice<DeviceFd>> {
        let device = self.primary.and_then(|p| self.devices.get(&p))?;
        Some(device.gbm.clone())
    }

    pub fn present(
        &mut self,
        exec: &mut Executor,
        output: OutputRef,
        frame: u64,
        flags: PresentFlags,
    ) -> anyhow::Result<(bool, Vec<ElementState>)> {
        let _span = tracy_client::span!("DrmState::present");
        let (device, crtc) = self.surface(output)?;
        let surface = device.surfaces.get_mut(&crtc).unwrap();
        ensure!(device.drm.is_active(), "device is inactive");

        let frame_rec = exec
            .output_frames
            .remove(&output)
            .context("no frame recorded for this output")?;
        let commands = frame_rec.commands;
        let blend = frame_rec.blend;
        if surface.last_blend != Some(blend) {
            surface.last_blend = Some(blend);
            surface.compositor.reset_buffers();
        }
        let segments = split_elements(&commands);

        surface.elements.update(&segments);
        let id_map = surface.elements.id_map();
        let storages = scene::element_storages(&exec.tables.borrow(), &segments);
        // smithay wants elements top to bottom; the core recorded bottom to top.
        let elements = scene::scene_elements(&surface.elements, &segments, &storages, &exec.tables);

        let mut frame_flags = FrameFlags::empty();
        if flags.primary_scanout {
            frame_flags |= if flags.primary_scanout_any_format {
                FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT_ANY
            } else {
                FrameFlags::ALLOW_PRIMARY_PLANE_SCANOUT
            };
        }
        if flags.overlay_planes {
            frame_flags |= FrameFlags::ALLOW_OVERLAY_PLANE_SCANOUT;
        }
        if flags.cursor_plane {
            frame_flags |= FrameFlags::ALLOW_CURSOR_PLANE_SCANOUT;
        }
        if flags.skip_cursor_only_updates {
            frame_flags |= FrameFlags::SKIP_CURSOR_ONLY_UPDATES;
        }

        let renderer = exec.renderer.as_mut().context("no renderer yet")?;
        blend::apply(renderer, blend);
        let res = surface
            .compositor
            .render_frame(renderer, &elements, [0.; 4], frame_flags);
        if blend.is_some() {
            blend::apply(renderer, None);
        }
        let res = res.map_err(|err| anyhow!("error rendering frame: {err}"))?;

        let states = res
            .states
            .states
            .iter()
            .filter_map(|(id, state)| {
                Some(ElementState {
                    id: *id_map.get(id)?,
                    presentation: match state.presentation_state {
                        RenderElementPresentationState::Rendering { .. } => Presentation::Rendering,
                        RenderElementPresentationState::ZeroCopy => Presentation::ZeroCopy,
                        RenderElementPresentationState::Skipped => Presentation::Skipped,
                    },
                    visible_area: state.visible_area as u64,
                })
            })
            .collect();

        if res.needs_sync() {
            if let PrimaryPlaneElement::Swapchain(element) = &res.primary_element {
                let _span = tracy_client::span!("wait for completion");
                if let Err(err) = element.sync.wait() {
                    warn!("error waiting for frame completion: {err:?}");
                }
            }
        }
        if res.is_empty {
            return Ok((false, states));
        }
        drop(res);
        surface
            .compositor
            .queue_frame(frame)
            .map_err(|err| anyhow!("error queueing frame: {err}"))?;
        Ok((true, states))
    }

    /// Returns the event to forward to the core.
    pub fn on_vblank(
        &mut self,
        dev: DevId,
        crtc: crtc::Handle,
        meta: DrmEventMetadata,
    ) -> Option<Event> {
        let device = self.devices.get_mut(&dev)?;
        let surface = device.surfaces.get_mut(&crtc)?;
        let frame = match surface.compositor.frame_submitted() {
            Ok(frame) => frame,
            Err(err) => {
                warn!("error marking frame as submitted: {err}");
                None
            }
        };
        let time_ns = match meta.time {
            DrmEventTime::Monotonic(time) => Some(time.as_nanos() as u64),
            DrmEventTime::Realtime(_) => None,
        };
        Some(Event::Notify(super::protocol::GpuEvent::VBlank {
            output: output_ref(dev, crtc),
            sequence: meta.sequence as u64,
            time_ns,
            frame,
        }))
    }
}

impl Device {
    fn connector_info(
        &self,
        dev: DevId,
        connector: &connector::Info,
        crtc: crtc::Handle,
    ) -> ConnectorInfo {
        let name = format_connector_name(connector);
        let edid = get_edid_info(&self.drm, connector.handle())
            .map_err(|err| warn!("error getting EDID info for {name}: {err:?}"))
            .ok();
        let props = ConnectorProperties::try_new(&self.drm, connector.handle()).ok();
        let panel_orientation = props
            .as_ref()
            .and_then(|p| p.get_panel_orientation().ok())
            .map(convert::transform);
        let max_bpc = read_max_bpc(&self.drm, connector.handle());
        let max_bpc_range = props.as_ref().and_then(|p| p.max_bpc_range());
        let hdr = hdr_caps(props.as_ref(), edid.as_ref());
        let non_desktop = find_drm_property(&self.drm, connector.handle(), "non-desktop")
            .and_then(|(_, info, value)| info.value_type().convert_value(value).as_boolean())
            .unwrap_or(false);
        let gamma_size = match GammaProps::new(&self.drm, crtc) {
            Ok(props) => props.gamma_size(&self.drm).unwrap_or(0),
            Err(_) => self
                .drm
                .get_crtc(crtc)
                .map(|c| c.gamma_length())
                .unwrap_or(0),
        };
        ConnectorInfo {
            output: output_ref(dev, crtc),
            connector: connector.handle().into(),
            name,
            make: edid.as_ref().and_then(|e| e.make()),
            model: edid.as_ref().and_then(|e| e.model()),
            serial: edid.as_ref().and_then(|e| e.serial()),
            physical_size_mm: connector.size(),
            modes: connector
                .modes()
                .iter()
                .map(|m| ModeDesc::from(*m))
                .collect(),
            vrr_capable: is_vrr_capable(&self.drm, connector.handle()),
            non_desktop,
            panel_orientation,
            max_bpc,
            max_bpc_range,
            hdr,
            gamma_size,
        }
    }

    fn cleanup_mismatching_resources(
        &self,
        should_be_off: &dyn Fn(crtc::Handle, &connector::Info) -> bool,
    ) -> anyhow::Result<()> {
        let _span = tracy_client::span!("Device::cleanup_mismatching_resources");

        let res_handles = self
            .drm
            .resource_handles()
            .context("error getting plane handles")?;
        let plane_handles = self
            .drm
            .plane_handles()
            .context("error getting plane handles")?;

        let mut req = AtomicModeReq::new();

        // CRTCs we will disable; connectors that keep their CRTC are removed below.
        let mut cleanup = HashSet::<crtc::Handle>::new();
        cleanup.extend(res_handles.crtcs());

        for (conn, info) in self.drm_scanner.connectors() {
            if let Some(crtc) = self.drm_scanner.crtc_for_connector(conn) {
                let mut has_different_crtc = false;
                if let Some(enc) = info.current_encoder() {
                    match self.drm.get_encoder(enc) {
                        Ok(enc) => {
                            if let Some(current_crtc) = enc.crtc() {
                                if current_crtc != crtc {
                                    has_different_crtc = true;
                                }
                            }
                        }
                        Err(err) => {
                            debug!("couldn't get encoder: {err:?}");
                            has_different_crtc = true;
                        }
                    }
                }

                if !has_different_crtc && !should_be_off(crtc, info) {
                    cleanup.remove(&crtc);
                    continue;
                }
            }

            let Some((crtc_id, _, _)) = find_drm_property(&self.drm, *conn, "CRTC_ID") else {
                debug!("couldn't find connector CRTC_ID property");
                continue;
            };
            req.add_property(*conn, crtc_id, property::Value::CRTC(None));
        }

        if !self.drm.is_atomic() {
            for crtc in res_handles.crtcs() {
                #[allow(deprecated)]
                let _ = self.drm.set_cursor(*crtc, Option::<&DumbBuffer>::None);
            }
            for crtc in cleanup {
                let _ = self.drm.set_crtc(crtc, None, (0, 0), &[], None);
            }
            return Ok(());
        }

        let is_primary = |plane: plane::Handle| {
            if let Some((_, info, value)) = find_drm_property(&self.drm, plane, "type") {
                match info.value_type().convert_value(value) {
                    property::Value::Enum(Some(val)) => val.value() == PlaneType::Primary as u64,
                    _ => false,
                }
            } else {
                debug!("couldn't find plane type property");
                false
            }
        };

        for plane in plane_handles {
            let info = match self.drm.get_plane(plane) {
                Ok(x) => x,
                Err(err) => {
                    debug!("error getting plane: {err:?}");
                    continue;
                }
            };
            let Some(crtc) = info.crtc() else {
                continue;
            };
            if !cleanup.contains(&crtc) && is_primary(plane) {
                continue;
            }
            let Some((crtc_id, _, _)) = find_drm_property(&self.drm, plane, "CRTC_ID") else {
                debug!("couldn't find plane CRTC_ID property");
                continue;
            };
            let Some((fb_id, _, _)) = find_drm_property(&self.drm, plane, "FB_ID") else {
                debug!("couldn't find plane FB_ID property");
                continue;
            };
            req.add_property(plane, crtc_id, property::Value::CRTC(None));
            req.add_property(plane, fb_id, property::Value::Framebuffer(None));
        }

        for crtc in cleanup {
            let Some((mode_id, _, _)) = find_drm_property(&self.drm, crtc, "MODE_ID") else {
                debug!("couldn't find CRTC MODE_ID property");
                continue;
            };
            let Some((active, _, _)) = find_drm_property(&self.drm, crtc, "ACTIVE") else {
                debug!("couldn't find CRTC ACTIVE property");
                continue;
            };
            req.add_property(crtc, mode_id, property::Value::Unknown(0));
            req.add_property(crtc, active, property::Value::Boolean(false));
        }

        self.drm
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
            .context("error doing atomic commit")?;
        Ok(())
    }
}

/// Allocates a GBM render buffer and exports it as a dmabuf. A lone `Invalid` modifier means
/// "no modifiers" (implicit layout).
pub fn allocate_gbm_dmabuf(
    gbm: &GbmDevice<DeviceFd>,
    width: u32,
    height: u32,
    fourcc: Fourcc,
    modifiers: &[u64],
) -> anyhow::Result<Dmabuf> {
    let flags = GbmBufferFlags::RENDERING;
    let buffer = if modifiers.len() == 1 && Modifier::from(modifiers[0]) == Modifier::Invalid {
        let bo = gbm
            .create_buffer_object::<()>(width, height, fourcc, flags)
            .context("error creating GBM buffer object")?;
        GbmBuffer::from_bo(bo, true)
    } else {
        let modifiers = modifiers
            .iter()
            .map(|m| Modifier::from(*m))
            .filter(|m| *m != Modifier::Invalid);
        let bo = gbm
            .create_buffer_object_with_modifiers2::<()>(width, height, fourcc, modifiers, flags)
            .context("error creating GBM buffer object")?;
        GbmBuffer::from_bo(bo, false)
    };
    buffer
        .export()
        .context("error exporting GBM buffer object as dmabuf")
}

impl GammaProps {
    fn new(device: &DrmDevice, crtc: crtc::Handle) -> anyhow::Result<Self> {
        let mut gamma_lut = None;
        let mut gamma_lut_size = None;

        let props = device
            .get_properties(crtc)
            .context("error getting properties")?;
        for (prop, _) in props {
            let Ok(info) = device.get_property(prop) else {
                continue;
            };
            let Ok(name) = info.name().to_str() else {
                continue;
            };
            match name {
                "GAMMA_LUT" => {
                    ensure!(
                        matches!(info.value_type(), property::ValueType::Blob),
                        "wrong GAMMA_LUT value type"
                    );
                    gamma_lut = Some(prop);
                }
                "GAMMA_LUT_SIZE" => {
                    ensure!(
                        matches!(info.value_type(), property::ValueType::UnsignedRange(_, _)),
                        "wrong GAMMA_LUT_SIZE value type"
                    );
                    gamma_lut_size = Some(prop);
                }
                _ => (),
            }
        }

        Ok(Self {
            crtc,
            gamma_lut: gamma_lut.context("missing GAMMA_LUT property")?,
            gamma_lut_size: gamma_lut_size.context("missing GAMMA_LUT_SIZE property")?,
            previous_blob: None,
        })
    }

    fn gamma_size(&self, device: &DrmDevice) -> anyhow::Result<u32> {
        let value = get_drm_property(device, self.crtc, self.gamma_lut_size)
            .context("missing GAMMA_LUT_SIZE property")?;
        Ok(value as u32)
    }

    fn set_gamma(&mut self, device: &DrmDevice, gamma: Option<&[u16]>) -> anyhow::Result<()> {
        let _span = tracy_client::span!("GammaProps::set_gamma");

        let blob = if let Some(gamma) = gamma {
            let gamma_size = self
                .gamma_size(device)
                .context("error getting gamma size")? as usize;
            ensure!(gamma.len() == gamma_size * 3, "wrong gamma length");

            #[allow(non_camel_case_types)]
            #[repr(C)]
            #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
            pub struct drm_color_lut {
                pub red: u16,
                pub green: u16,
                pub blue: u16,
                pub reserved: u16,
            }

            let (red, rest) = gamma.split_at(gamma_size);
            let (blue, green) = rest.split_at(gamma_size);
            let mut data = zip(zip(red, blue), green)
                .map(|((&red, &green), &blue)| drm_color_lut {
                    red,
                    green,
                    blue,
                    reserved: 0,
                })
                .collect::<Vec<_>>();
            let data = cast_slice_mut(&mut data);

            let blob = drm_ffi::mode::create_property_blob(device.as_fd(), data)
                .context("error creating property blob")?;
            NonZeroU64::new(u64::from(blob.blob_id))
        } else {
            None
        };

        {
            let blob = blob.map(NonZeroU64::get).unwrap_or(0);
            device
                .set_property(
                    self.crtc,
                    self.gamma_lut,
                    property::Value::Blob(blob).into(),
                )
                .context("error setting GAMMA_LUT")
                .inspect_err(|_| {
                    if blob != 0 {
                        if let Err(err) = device.destroy_property_blob(blob) {
                            warn!("error destroying GAMMA_LUT property blob: {err:?}");
                        }
                    }
                })?;
        }

        if let Some(blob) = mem::replace(&mut self.previous_blob, blob) {
            if let Err(err) = device.destroy_property_blob(blob.get()) {
                warn!("error destroying previous GAMMA_LUT blob: {err:?}");
            }
        }
        Ok(())
    }

    fn restore_gamma(&self, device: &DrmDevice) -> anyhow::Result<()> {
        let blob = self.previous_blob.map(NonZeroU64::get).unwrap_or(0);
        device
            .set_property(
                self.crtc,
                self.gamma_lut,
                property::Value::Blob(blob).into(),
            )
            .context("error setting GAMMA_LUT")?;
        Ok(())
    }
}

fn find_drm_property(
    drm: &DrmDevice,
    resource: impl ResourceHandle,
    name: &str,
) -> Option<(property::Handle, property::Info, property::RawValue)> {
    let props = match drm.get_properties(resource) {
        Ok(props) => props,
        Err(err) => {
            warn!("error getting properties: {err:?}");
            return None;
        }
    };
    props.into_iter().find_map(|(handle, value)| {
        let info = drm.get_property(handle).ok()?;
        let n = info.name().to_str().ok()?;
        (n == name).then_some((handle, info, value))
    })
}

fn get_drm_property(
    drm: &DrmDevice,
    resource: impl ResourceHandle,
    prop: property::Handle,
) -> Option<property::RawValue> {
    let props = match drm.get_properties(resource) {
        Ok(props) => props,
        Err(err) => {
            warn!("error getting properties: {err:?}");
            return None;
        }
    };
    props
        .into_iter()
        .find_map(|(handle, value)| (handle == prop).then_some(value))
}

fn get_edid_info(
    device: &DrmDevice,
    connector: connector::Handle,
) -> anyhow::Result<libdisplay_info::info::Info> {
    let (_, info, value) =
        find_drm_property(device, connector, "EDID").context("no EDID property")?;
    let blob = info
        .value_type()
        .convert_value(value)
        .as_blob()
        .context("EDID was not blob type")?;
    let data = device
        .get_property_blob(blob)
        .context("error getting EDID blob value")?;
    libdisplay_info::info::Info::parse_edid(&data).context("error parsing EDID")
}

impl ConnectorProperties {
    fn try_new(device: &DrmDevice, connector: connector::Handle) -> anyhow::Result<Self> {
        let prop_vals = device
            .get_properties(connector)
            .context("error getting properties")?;
        let mut properties = Vec::new();
        for (prop, value) in prop_vals {
            let info = device
                .get_property(prop)
                .context("error getting property")?;
            properties.push((info, value));
        }
        Ok(Self { properties })
    }

    fn find(&self, name: &std::ffi::CStr) -> anyhow::Result<&(property::Info, property::RawValue)> {
        self.properties
            .iter()
            .find(|prop| prop.0.name() == name)
            .ok_or_else(|| anyhow!("couldn't find property: {name:?}"))
    }

    fn get_panel_orientation(&self) -> anyhow::Result<Transform> {
        let (info, value) = self.find(c"panel orientation")?;
        match info.value_type().convert_value(*value) {
            property::Value::Enum(Some(val)) => match val.value() {
                0 => Ok(Transform::Normal),
                1 => Ok(Transform::_180),
                2 => Ok(Transform::_90),
                3 => Ok(Transform::_270),
                _ => bail!("panel orientation has invalid value: {:?}", val),
            },
            _ => bail!("panel orientation has wrong value type"),
        }
    }

    fn max_bpc_range(&self) -> Option<(u32, u32)> {
        let (info, _) = self.find(c"max bpc").ok()?;
        match info.value_type() {
            property::ValueType::UnsignedRange(min, max) => Some((min as u32, max as u32)),
            _ => None,
        }
    }

    /// Whether the driver exposes `Colorspace` with a BT2020_RGB choice.
    fn supports_bt2020_rgb(&self) -> bool {
        let Ok((info, _)) = self.find(c"Colorspace") else {
            return false;
        };
        match info.value_type() {
            property::ValueType::Enum(values) => values
                .values()
                .1
                .iter()
                .any(|v| v.name().to_bytes() == b"BT2020_RGB"),
            _ => false,
        }
    }

    fn supports_hdr_metadata(&self) -> bool {
        matches!(
            self.find(c"HDR_OUTPUT_METADATA")
                .map(|(info, _)| info.value_type()),
            Ok(property::ValueType::Blob)
        )
    }
}

/// HDR signalling needs `Colorspace` (with BT2020_RGB) and `HDR_OUTPUT_METADATA` from the
/// driver, plus a sink that accepts the PQ EOTF per its EDID.
fn hdr_caps(
    props: Option<&ConnectorProperties>,
    edid: Option<&libdisplay_info::info::Info>,
) -> HdrCaps {
    let Some(edid) = edid else {
        return HdrCaps::default();
    };
    let hdr = edid.hdr_static_metadata();
    let lum_u16 = |v: f32| v.clamp(0.0, u16::MAX as f32).round() as u16;
    let driver_ok = props.is_some_and(|p| p.supports_bt2020_rgb() && p.supports_hdr_metadata());
    HdrCaps {
        supported: driver_ok && hdr.pq,
        max_luminance: lum_u16(hdr.desired_content_max_luminance),
        // EDID reports cd/m²; the infoframe field is in 0.0001 cd/m² units.
        min_luminance: lum_u16(hdr.desired_content_min_luminance * 10000.),
        max_frame_avg_luminance: lum_u16(hdr.desired_content_max_frame_avg_luminance),
    }
}

fn connector_color_state(state: ColorState) -> ConnectorColorState {
    match state.hdr {
        Some(meta) => ConnectorColorState {
            colorspace: Colorspace::Bt2020Rgb,
            hdr_metadata: Some(HdrOutputMetadata::pq_bt2020(
                meta.max_luminance,
                meta.min_luminance,
                meta.max_cll,
                meta.max_fall,
            )),
            max_bpc: state.max_bpc,
        },
        None => ConnectorColorState {
            colorspace: Colorspace::Default,
            hdr_metadata: None,
            max_bpc: state.max_bpc,
        },
    }
}

impl CtmProps {
    fn new(device: &DrmDevice, crtc: crtc::Handle) -> anyhow::Result<Self> {
        let props = device
            .get_properties(crtc)
            .context("error getting properties")?;
        let mut ctm = None;
        for (prop, _) in props {
            let Ok(info) = device.get_property(prop) else {
                continue;
            };
            if info.name().to_bytes() == b"CTM" {
                ensure!(
                    matches!(info.value_type(), property::ValueType::Blob),
                    "wrong CTM value type"
                );
                ctm = Some(prop);
                break;
            }
        }
        Ok(Self {
            crtc,
            ctm: ctm.context("missing CTM property")?,
            previous_blob: None,
        })
    }

    fn set_ctm(&mut self, device: &DrmDevice, ctm: Option<&[f64; 9]>) -> anyhow::Result<()> {
        let _span = tracy_client::span!("CtmProps::set_ctm");

        let blob = if let Some(matrix) = ctm {
            // The kernel wants S31.32 fixed point with a sign bit.
            fn to_s3132(val: f64) -> u64 {
                let magnitude = (val.abs() * (1u64 << 32) as f64) as u64;
                if val < 0.0 {
                    magnitude | (1u64 << 63)
                } else {
                    magnitude
                }
            }

            #[allow(non_camel_case_types)]
            #[repr(C)]
            #[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
            pub struct drm_color_ctm {
                pub matrix: [u64; 9],
            }

            let mut data = drm_color_ctm {
                matrix: matrix.map(to_s3132),
            };
            let blob = drm_ffi::mode::create_property_blob(
                device.as_fd(),
                bytemuck::bytes_of_mut(&mut data),
            )
            .context("error creating CTM property blob")?;
            NonZeroU64::new(u64::from(blob.blob_id))
        } else {
            None
        };

        let blob_id = blob.map(NonZeroU64::get).unwrap_or(0);
        device
            .set_property(self.crtc, self.ctm, property::Value::Blob(blob_id).into())
            .context("error setting CTM")
            .inspect_err(|_| {
                if blob_id != 0 {
                    if let Err(err) = device.destroy_property_blob(blob_id) {
                        warn!("error destroying CTM property blob: {err:?}");
                    }
                }
            })?;

        if let Some(blob) = mem::replace(&mut self.previous_blob, blob) {
            if let Err(err) = device.destroy_property_blob(blob.get()) {
                warn!("error destroying previous CTM blob: {err:?}");
            }
        }
        Ok(())
    }

    fn restore_ctm(&self, device: &DrmDevice) -> anyhow::Result<()> {
        let blob = self.previous_blob.map(NonZeroU64::get).unwrap_or(0);
        device
            .set_property(self.crtc, self.ctm, property::Value::Blob(blob).into())
            .context("error restoring CTM")?;
        Ok(())
    }
}

fn is_vrr_capable(device: &DrmDevice, connector: connector::Handle) -> Option<bool> {
    let (_, info, value) = find_drm_property(device, connector, "vrr_capable")?;
    info.value_type().convert_value(value).as_boolean()
}

fn set_gamma_for_crtc(
    device: &DrmDevice,
    crtc: crtc::Handle,
    ramp: Option<&[u16]>,
) -> anyhow::Result<()> {
    let info = device.get_crtc(crtc).context("error getting crtc info")?;
    let gamma_length = info.gamma_length() as usize;
    ensure!(gamma_length != 0, "setting gamma is not supported");

    let mut temp;
    let ramp = if let Some(ramp) = ramp {
        ensure!(ramp.len() == gamma_length * 3, "wrong gamma length");
        ramp
    } else {
        temp = vec![0u16; gamma_length * 3];
        let (red, rest) = temp.split_at_mut(gamma_length);
        let (green, blue) = rest.split_at_mut(gamma_length);
        let denom = gamma_length as u64 - 1;
        for (i, ((r, g), b)) in zip(zip(red, green), blue).enumerate() {
            let value = (0xFFFFu64 * i as u64 / denom) as u16;
            *r = value;
            *g = value;
            *b = value;
        }
        &temp
    };

    let (red, ramp) = ramp.split_at(gamma_length);
    let (green, blue) = ramp.split_at(gamma_length);
    device
        .set_gamma(crtc, red, green, blue)
        .context("error setting gamma")?;
    Ok(())
}

fn format_connector_name(connector: &connector::Info) -> String {
    format!(
        "{}-{}",
        connector.interface().as_str(),
        connector.interface_id(),
    )
}
