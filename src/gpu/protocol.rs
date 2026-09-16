//! Wire protocol between the compositor core and the GPU process.
//!
//! The core records renderer calls as [`Command`]s and ships them in batches with
//! [`Request::Execute`]. Every request gets exactly one [`Event`] reply. Commands that
//! carry file descriptors get them attached to the same transport frame, in command order.

use serde::{Deserialize, Serialize};
use smithay::reexports::drm::control::Mode as DrmMode;

pub const PROTOCOL_VERSION: u32 = 8;

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
    /// `DmabufFlags` bits (y-invert, interlaced, ...).
    pub flags: u32,
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
    /// A dmabuf previously imported under this id; bound directly (not via its texture).
    Dmabuf(TexId),
    /// Recorded frames for outputs are kept by the GPU process and drawn by [`Request::Present`].
    Output(OutputRef),
    /// A screencast stream's next PipeWire buffer; rendered when the frame ends.
    Cast(u64),
    /// The cursor bitmap for a screencast stream in metadata cursor mode; must be recorded
    /// right before that stream's `Cast` frame.
    CastCursor(u64),
}

/// Cursor placement for a screencast frame in metadata cursor mode.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CursorMeta {
    /// Hotspot location in the video buffer.
    pub location: (i32, i32),
    /// Hotspot location on the cursor bitmap.
    pub hotspot: (i32, i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CastCursorMode {
    Hidden,
    Embedded,
    Metadata,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElementKind {
    Cursor,
    ScanoutCandidate,
    Unspecified,
}

/// What the GPU-side damage tracker needs to know about one element.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ElementMeta {
    /// Stable across frames for the same element; chosen by the core.
    pub id: u64,
    pub src: Rect<f64>,
    /// In output coordinates.
    pub geometry: Rect<i32>,
    /// Element-relative damage since the previous frame this id was sent in. `None` means
    /// everything (new element or unknown).
    pub damage: Option<Vec<Rect<i32>>>,
    /// Element-relative.
    pub opaque: Vec<Rect<i32>>,
    pub kind: ElementKind,
    /// Buffer transform, for direct scanout.
    pub transform: Transform,
    pub framebuffer_effect: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Presentation {
    Rendering,
    ZeroCopy,
    Skipped,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElementState {
    pub id: u64,
    pub presentation: Presentation,
    pub visible_area: u64,
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
    /// Parameters for the next `Begin { Target::Cast(stream) }` frame.
    CastFrameInfo {
        stream: u64,
        scale: f64,
        /// The presentation time this frame was rendered for; echoed in `CastEvent::Rendered`.
        target_time_ns: u64,
        /// `None` in embedded/hidden cursor modes or when the pointer is not over the target.
        cursor: Option<CursorMeta>,
    },
    /// Shm buffer contents by pool fd (attached), so the core never maps client memory. With
    /// `damage` set, `id` is an existing texture to update in those regions; otherwise a new
    /// texture is created.
    ImportShm {
        id: TexId,
        format: u32,
        width: i32,
        height: i32,
        stride: i32,
        offset: i32,
        damage: Option<Vec<Rect<i32>>>,
    },
    /// `data` holds tightly packed rows covering `region` (only the damaged rows travel).
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
    /// Starts one scene element inside an output frame (`Begin { Target::Output }`). Commands
    /// up to `BeginElementDraw` are its framebuffer capture, the rest up to `EndElement` its
    /// draw. The GPU turns each element into a real smithay element for its DRM compositor.
    BeginElement(ElementMeta),
    BeginElementDraw,
    EndElement,
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

impl Request {
    /// One-way requests get no reply; failures surface as `GpuEvent::Error`. Used on the
    /// per-frame path so the core never blocks on the GPU process.
    pub fn is_oneway(&self) -> bool {
        matches!(
            self,
            Request::Execute { .. }
                | Request::Present { .. }
                | Request::CastClear { .. }
                | Request::CastStop { .. }
        )
    }
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
    /// Replied to (Ack) once everything sent before it has executed and finished on the GPU.
    Sync,
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
    /// One-way; the outcome arrives as `GpuEvent::Presented`, then `frame` comes back in the
    /// matching `VBlank`.
    Present {
        output: OutputRef,
        frame: u64,
        flags: PresentFlags,
    },
    /// Creates a PipeWire screencast stream. Reply: `CastStarted` with the effective cursor
    /// mode (metadata needs a recent PipeWire).
    CastStart {
        stream: u64,
        width: i32,
        height: i32,
        refresh: u32,
        alpha: bool,
        cursor_mode: CastCursorMode,
        allow_dmabuf: bool,
        force_invalid_modifier: bool,
    },
    /// Renegotiates the stream size / frame rate. Reply: Ack.
    CastConfigure {
        stream: u64,
        width: i32,
        height: i32,
        refresh: u32,
    },
    /// Sends one cleared frame (dynamic cast without a target). One-way; reported like a
    /// recorded frame with `Rendered` / `Skipped`.
    CastClear {
        stream: u64,
        target_time_ns: u64,
    },
    /// One-way.
    CastStop {
        stream: u64,
    },
    /// Allocates a GBM buffer on the primary device. Reply: `Dmabuf` with fds attached.
    AllocateDmabuf {
        width: u32,
        height: u32,
        format: u32,
        modifiers: Vec<u64>,
    },
    Shutdown,
}

/// Which planes the DRM compositor may use; mirrors smithay's `FrameFlags`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PresentFlags {
    pub primary_scanout: bool,
    /// Allow primary-plane scanout of buffers whose format differs from the swapchain's.
    pub primary_scanout_any_format: bool,
    pub overlay_planes: bool,
    pub cursor_plane: bool,
    pub skip_cursor_only_updates: bool,
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
    /// Outcome of a `Present`. `submitted == false` means nothing changed on screen.
    Presented {
        output: OutputRef,
        frame: u64,
        submitted: bool,
        /// Per element, what the DRM compositor did with it (for presentation feedback).
        states: Vec<ElementState>,
    },
    /// A one-way request failed.
    Error {
        message: String,
    },
    VBlank {
        output: OutputRef,
        sequence: u64,
        /// CLOCK_MONOTONIC nanoseconds, if the kernel reported a time.
        time_ns: Option<u64>,
        frame: Option<u64>,
    },
    /// The kernel told us the device is gone or errored; the core should remove it.
    DeviceError {
        dev: DevId,
        message: String,
    },
    Cast(CastEvent),
}

/// Screencast stream events from the GPU process, which owns PipeWire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CastEvent {
    /// The stream got its PipeWire node id (to hand to the portal).
    NodeId { stream: u64, node_id: u32 },
    /// Negotiation state changed. `ready_size` is `Some` once buffers of that size can be
    /// rendered into.
    State {
        stream: u64,
        active: bool,
        ready_size: Option<(i32, i32)>,
        min_frame_time_ns: u64,
    },
    /// PipeWire wants a new frame (e.g. after a resize or on becoming active).
    Redraw { stream: u64 },
    /// A recorded frame was actually sent (it had damage and a buffer was free). The core uses
    /// this for frame pacing.
    Rendered { stream: u64, target_time_ns: u64 },
    /// A recorded frame was not sent (no damage, no free buffer, or an error).
    Skipped { stream: u64, target_time_ns: u64 },
    /// The stream failed; the core should stop the cast.
    Stop { stream: u64 },
    /// The PipeWire connection died; all streams are gone.
    PipeWireFatal,
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
    /// Fds attached.
    Dmabuf(DmabufDesc),
    CastStarted {
        cursor_mode: CastCursorMode,
    },
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
