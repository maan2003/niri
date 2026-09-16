//! Wire protocol between the compositor core and the GPU process.
//!
//! The core records renderer calls as [`Command`]s and ships them in batches with
//! [`Request::Execute`]. Every request gets exactly one [`Event`] reply. Commands that
//! carry file descriptors get them attached to the same transport frame, in command order.

use serde::{Deserialize, Serialize};
use smithay::reexports::drm::control::Mode as DrmMode;

pub const PROTOCOL_VERSION: u32 = 3;

/// `dev_t` of a DRM device node, as reported by udev.
pub type DevId = u64;

/// Identifier of a texture living in the GPU process, allocated by the core.
pub type TexId = u64;
/// Identifier of a scanout target (an output) living in the GPU process.
pub type OutputId = u64;

#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub struct Rect<T> {
    pub x: T,
    pub y: T,
    pub w: T,
    pub h: T,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Transform {
    Normal,
    _90,
    _180,
    _270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum UniformVal {
    F1(f32),
    F2(f32, f32),
    F3(f32, f32, f32),
    F4(f32, f32, f32, f32),
    I1(i32),
    I2(i32, i32),
    I3(i32, i32, i32),
    I4(i32, i32, i32, i32),
    U1(u32),
    U2(u32, u32),
    U3(u32, u32, u32),
    U4(u32, u32, u32, u32),
    Mat2x2 {
        matrices: Vec<[f32; 4]>,
        transpose: bool,
    },
    Mat2x3 {
        matrices: Vec<[f32; 6]>,
        transpose: bool,
    },
    Mat2x4 {
        matrices: Vec<[f32; 8]>,
        transpose: bool,
    },
    Mat3x2 {
        matrices: Vec<[f32; 6]>,
        transpose: bool,
    },
    Mat3x3 {
        matrices: Vec<[f32; 9]>,
        transpose: bool,
    },
    Mat3x4 {
        matrices: Vec<[f32; 12]>,
        transpose: bool,
    },
    Mat4x2 {
        matrices: Vec<[f32; 8]>,
        transpose: bool,
    },
    Mat4x3 {
        matrices: Vec<[f32; 12]>,
        transpose: bool,
    },
    Mat4x4 {
        matrices: Vec<[f32; 16]>,
        transpose: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Uniform {
    pub name: String,
    pub value: UniformVal,
}

/// Texture shader programs built into the GPU process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TexProgram {
    ClippedSurface,
    PostprocessAndClip,
    GradientFade,
}

/// Pixel shader programs in the GPU process. Resize/Close/Open can be replaced by custom
/// sources from the config.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ShaderKind {
    Border,
    Shadow,
    Resize,
    Close,
    Open,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaneDesc {
    pub offset: u32,
    pub stride: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DmabufDesc {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub modifier: u64,
    pub planes: Vec<PlaneDesc>,
}

/// A scanout target: one CRTC of one DRM device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OutputRef {
    pub dev: DevId,
    pub crtc: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    Texture(TexId),
    /// Recorded frames for outputs are kept by the GPU process and drawn by [`Request::Present`].
    Output(OutputRef),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BlurParams {
    pub passes: u8,
    pub offset: f64,
}

/// `drm_mode_modeinfo`, field by field.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModeDesc {
    pub clock: u32,
    pub hdisplay: u16,
    pub hsync_start: u16,
    pub hsync_end: u16,
    pub htotal: u16,
    pub hskew: u16,
    pub vdisplay: u16,
    pub vsync_start: u16,
    pub vsync_end: u16,
    pub vtotal: u16,
    pub vscan: u16,
    pub vrefresh: u32,
    pub flags: u32,
    pub type_: u32,
    pub name: String,
}

/// Everything the core needs to know about a connected connector to make output decisions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConnectorInfo {
    pub output: OutputRef,
    pub connector: u32,
    /// Connector name like `eDP-1`.
    pub name: String,
    pub make: Option<String>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub physical_size_mm: Option<(u32, u32)>,
    pub modes: Vec<ModeDesc>,
    /// None if the connector has no VRR property.
    pub vrr_capable: Option<bool>,
    pub non_desktop: bool,
    pub panel_orientation: Option<Transform>,
    pub max_bpc: Option<u8>,
    pub gamma_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OutputGeometry {
    pub scale: f64,
    pub transform: Transform,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Command {
    CreateTexture {
        id: TexId,
        format: u32,
        width: i32,
        height: i32,
    },
    ImportMemory {
        id: TexId,
        format: u32,
        width: i32,
        height: i32,
        flipped: bool,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// `data` holds tightly packed rows covering `region`.
    UpdateMemory {
        id: TexId,
        region: Rect<i32>,
        #[serde(with = "serde_bytes")]
        data: Vec<u8>,
    },
    /// One fd per plane is attached to the transport frame.
    ImportDmabuf {
        id: TexId,
        desc: DmabufDesc,
        damage: Option<Vec<Rect<i32>>>,
    },
    DestroyTexture {
        id: TexId,
    },
    SetDebugFlags {
        flags: u32,
    },

    // Frame commands; only valid between Begin and End.
    Begin {
        target: Target,
        width: i32,
        height: i32,
        transform: Transform,
    },
    Clear {
        color: [f32; 4],
        at: Vec<Rect<i32>>,
    },
    DrawSolid {
        dst: Rect<i32>,
        damage: Vec<Rect<i32>>,
        color: [f32; 4],
    },
    DrawTexture {
        texture: TexId,
        src: Rect<f64>,
        dst: Rect<i32>,
        damage: Vec<Rect<i32>>,
        opaque: Vec<Rect<i32>>,
        transform: Transform,
        alpha: f32,
        program: Option<TexProgram>,
        uniforms: Vec<Uniform>,
    },
    OverrideTexProgram {
        program: TexProgram,
        uniforms: Vec<Uniform>,
    },
    ClearTexProgramOverride,
    /// niri's custom pixel shaders (borders, shadows, resize/open/close animations).
    DrawShader {
        program: ShaderKind,
        src: Rect<f64>,
        dst: Rect<i32>,
        damage: Vec<Rect<i32>>,
        scale: f32,
        alpha: f32,
        uniforms: Vec<Uniform>,
        textures: Vec<(String, TexId)>,
    },
    /// Snapshot the framebuffer under `dst` (blurred if requested) for a later `DrawCaptured`.
    CaptureFramebuffer {
        key: u64,
        src: Rect<f64>,
        dst: Rect<i32>,
        scale: f32,
        blur: Option<BlurParams>,
    },
    DrawCaptured {
        key: u64,
        dst: Rect<i32>,
        damage: Vec<Rect<i32>>,
        uniforms: Vec<Uniform>,
    },
    End,

    // Valid anywhere.
    DestroyCapture {
        key: u64,
    },
    /// Blur `src` into a new texture registered as `dst`. Pyramid textures are cached per `key`.
    Blur {
        key: u64,
        src: TexId,
        dst: TexId,
        params: BlurParams,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShaderSupport {
    pub border: bool,
    pub shadow: bool,
    pub resize: bool,
    pub clipped_surface: bool,
    pub postprocess_and_clip: bool,
    pub gradient_fade: bool,
    pub blur: bool,
    pub close: bool,
    pub open: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Caps {
    pub renderer: String,
    pub mem_formats: Vec<u32>,
    pub dmabuf_formats: Vec<(u32, u64)>,
    /// Formats the GPU can render into (for screencast / image-copy dmabuf buffers).
    pub dmabuf_render_formats: Vec<(u32, u64)>,
    pub shaders: ShaderSupport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Execute {
        commands: Vec<Command>,
    },
    /// Like `Command::ImportDmabuf` but with its own reply, so a rejected buffer doesn't
    /// abort a batch. Fds attached.
    ImportDmabuf {
        id: TexId,
        desc: DmabufDesc,
    },
    ReadTexture {
        id: TexId,
        region: Rect<i32>,
        format: u32,
    },
    /// Reply: `ShaderSet`.
    SetCustomShader {
        kind: ShaderKind,
        src: Option<String>,
    },

    // DRM/KMS. The core opens the device through libseat and hands over the fd.
    /// Fd attached. Reply: `DeviceAdded`.
    AddDevice {
        dev: DevId,
        path: String,
        primary: bool,
    },
    RemoveDevice {
        dev: DevId,
    },
    /// Session lost: stop touching the devices.
    PauseDevices,
    /// Session regained: re-activate every device.
    ResumeDevices {
        force_disable: bool,
    },
    /// Re-read the connectors. Reply: `Scan`.
    RescanDevice {
        dev: DevId,
    },
    /// Drop kernel state that doesn't match what we will use; `off` lists CRTCs the core
    /// intends to leave disabled.
    CleanupDevice {
        dev: DevId,
        off: Vec<u32>,
    },
    /// Reply: `OutputState`.
    EnableOutput {
        output: OutputRef,
        connector: u32,
        mode: ModeDesc,
        vrr: bool,
        max_bpc: Option<u8>,
        /// Show black right away (monitors are "off").
        clear: bool,
        allow_10bit: bool,
    },
    DisableOutput {
        output: OutputRef,
    },
    /// Reply: `OutputState`.
    SetMode {
        output: OutputRef,
        mode: ModeDesc,
    },
    /// Reply: `OutputState`.
    SetVrr {
        output: OutputRef,
        enable: bool,
    },
    SetMaxBpc {
        output: OutputRef,
        max_bpc: Option<u8>,
    },
    /// Scale and transform the core renders the output with; must match before `Present`.
    SetOutputGeometry {
        output: OutputRef,
        geometry: OutputGeometry,
    },
    SetGamma {
        output: OutputRef,
        ramp: Option<Vec<u16>>,
    },
    /// Show a black frame on every output (monitors off).
    ClearOutputs,
    SetDebugTint {
        enable: bool,
    },
    /// Scan out the frame most recently recorded for `output` (`Begin { Target::Output }`).
    /// Reply: `Presented`. `frame` comes back in the matching `VBlank`.
    Present {
        output: OutputRef,
        frame: u64,
        damage: Vec<Rect<i32>>,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

/// Unsolicited GPU-process events, delivered between replies as `Event::Notify`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum GpuEvent {
    VBlank {
        output: OutputRef,
        sequence: u64,
        /// CLOCK_MONOTONIC nanoseconds, if the kernel reported a time.
        time_ns: Option<u64>,
        frame: Option<u64>,
    },
    /// The kernel told us the device is gone or errored; the core should remove it.
    DeviceError { dev: DevId, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// `caps` is present once a renderer exists (immediately in headless mode).
    Ready {
        version: u32,
        caps: Option<Caps>,
    },
    Ack,
    Image(Image),
    ShaderSet {
        available: bool,
    },
    /// `caps` is set when this device brought up the renderer.
    DeviceAdded {
        render_node: Option<DevId>,
        caps: Option<Caps>,
    },
    Scan {
        connected: Vec<ConnectorInfo>,
        /// Already-known connectors whose EDID/modes/properties changed (e.g. DP-MST docks
        /// that report Connected before the EDID is readable).
        changed: Vec<ConnectorInfo>,
        disconnected: Vec<OutputRef>,
    },
    OutputState {
        output: OutputRef,
        mode: ModeDesc,
        vrr_enabled: bool,
        vrr_supported: bool,
        /// The "max bpc" property as actually committed, for IPC.
        max_bpc: Option<u8>,
    },
    Presented {
        submitted: bool,
    },
    Notify(GpuEvent),
    Error {
        message: String,
    },
    Done,
}

impl From<DrmMode> for ModeDesc {
    fn from(mode: DrmMode) -> Self {
        let raw: drm_ffi::drm_mode_modeinfo = mode.into();
        let name_len = raw
            .name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(raw.name.len());
        let name = raw.name[..name_len]
            .iter()
            .map(|&c| c as u8 as char)
            .collect();
        ModeDesc {
            clock: raw.clock,
            hdisplay: raw.hdisplay,
            hsync_start: raw.hsync_start,
            hsync_end: raw.hsync_end,
            htotal: raw.htotal,
            hskew: raw.hskew,
            vdisplay: raw.vdisplay,
            vsync_start: raw.vsync_start,
            vsync_end: raw.vsync_end,
            vtotal: raw.vtotal,
            vscan: raw.vscan,
            vrefresh: raw.vrefresh,
            flags: raw.flags,
            type_: raw.type_,
            name,
        }
    }
}

impl From<&ModeDesc> for DrmMode {
    fn from(desc: &ModeDesc) -> Self {
        let mut name = [0 as core::ffi::c_char; 32];
        for (dst, src) in name[..31].iter_mut().zip(desc.name.bytes()) {
            *dst = src as _;
        }
        let raw = drm_ffi::drm_mode_modeinfo {
            clock: desc.clock,
            hdisplay: desc.hdisplay,
            hsync_start: desc.hsync_start,
            hsync_end: desc.hsync_end,
            htotal: desc.htotal,
            hskew: desc.hskew,
            vdisplay: desc.vdisplay,
            vsync_start: desc.vsync_start,
            vsync_end: desc.vsync_end,
            vtotal: desc.vtotal,
            vscan: desc.vscan,
            vrefresh: desc.vrefresh,
            flags: desc.flags,
            type_: desc.type_,
            name,
        };
        DrmMode::from(raw)
    }
}
