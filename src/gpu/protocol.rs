//! Wire protocol between the compositor core and the GPU process.
//!
//! The core records renderer calls as [`Command`]s and ships them in batches with
//! [`Request::Execute`]. Every request gets exactly one [`Event`] reply. Commands that
//! carry file descriptors get them attached to the same transport frame, in command order.

use serde::{Deserialize, Serialize};
use smithay::reexports::drm::control::Mode as DrmMode;

pub const PROTOCOL_VERSION: u32 = 19;

/// A cached compositor UI surface. Text is shaped in the core; only paint and positioned
/// glyphs cross the wire. Colors are straight sRGB RGBA; the result is premultiplied RGBA8.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiScene {
    pub width: u32,
    pub height: u32,
    pub fonts: Vec<UiFont>,
    pub ops: Vec<UiOp>,
}

/// Scene-local font data. `index` selects a face in a font collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiFont {
    pub index: u32,
    #[serde(with = "serde_bytes")]
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct UiGlyph {
    pub id: u32,
    /// Absolute pixel position, with y at the glyph baseline.
    pub x: f32,
    pub y: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UiOp {
    Rect {
        rect: [f32; 4],
        color: [f32; 4],
    },
    Circle {
        center: [f32; 2],
        radius: f32,
        color: [f32; 4],
    },
    GlyphRun {
        /// Index into `UiScene::fonts`.
        font: u32,
        font_size: f32,
        /// Normalized F2Dot14 variation coordinates, in font axis order.
        coords: Vec<i16>,
        glyphs: Vec<UiGlyph>,
        color: [f32; 4],
    },
}

/// Texture ids a `LoadCursor` request reserves for its frames (`first_id..first_id + N`).
pub const MAX_CURSOR_FRAMES: u64 = 256;

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

/// Source encoding, independent of any rendering effect. `Target` denotes pixels
/// already composited in the current target's encoded blend space.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceColor {
    #[default]
    Srgb,
    DisplayP3,
    Hdr,
    Target,
}

impl SourceColor {
    /// Preserve the compositor's encoded-space policy: matching content passes through;
    /// other content follows the existing SDR-to-target conversion (no new tone mapping).
    pub fn conversion_to(self, target: Option<BlendParams>) -> Option<BlendParams> {
        match (self, target) {
            (Self::Target, _)
            | (Self::DisplayP3, Some(BlendParams::DisplayP3))
            | (Self::Hdr, Some(BlendParams::HdrPq { .. })) => None,
            (_, target) => target,
        }
    }
}

/// A rounded shape in geometry units; the column-major matrix maps normalized
/// effect input coordinates into those units.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RoundedGeometry {
    pub size: [f32; 2],
    pub radii: [f32; 4],
    pub input_to_geo: [f32; 9],
}

/// Texture clipping uses normalized geometry coordinates, unlike decoration geometry.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ClipParams {
    pub size: [f32; 2],
    pub radii: [f32; 4],
    pub input_to_geo: [f32; 9],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GradientSpace {
    Srgb,
    LinearSrgb,
    Oklab,
    Oklch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum GradientHue {
    Shorter,
    Longer,
    Increasing,
    Decreasing,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BorderParams {
    pub geometry: RoundedGeometry,
    pub width: f32,
    /// Straight sRGB endpoint colors.
    pub color_from: [f32; 4],
    pub color_to: [f32; 4],
    pub gradient_offset: [f32; 2],
    pub gradient_width: f32,
    pub gradient_vector: [f32; 2],
    pub gradient_space: GradientSpace,
    pub gradient_hue: GradientHue,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ShadowParams {
    pub geometry: RoundedGeometry,
    pub window: Option<RoundedGeometry>,
    /// Premultiplied sRGB.
    pub color: [f32; 4],
    pub sigma: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ResizeParams {
    pub input_to_geometry: [f32; 9],
    pub geometry_size: [f32; 2],
    pub previous_from_geometry: [f32; 9],
    pub next_from_geometry: [f32; 9],
    pub progress: f32,
    pub radii: [f32; 4],
    pub clip_to_geometry: bool,
}

/// Typed built-in paints. Core elements hold texture handles; the wire holds IDs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Paint<T = TexId> {
    Border(BorderParams),
    Shadow(ShadowParams),
    Resize {
        params: ResizeParams,
        previous: T,
        next: T,
    },
}

impl<T> Paint<T> {
    pub fn map_textures<U>(self, mut map: impl FnMut(T) -> U) -> Paint<U> {
        match self {
            Self::Border(params) => Paint::Border(params),
            Self::Shadow(params) => Paint::Shadow(params),
            Self::Resize {
                params,
                previous,
                next,
            } => Paint::Resize {
                params,
                previous: map(previous),
                next: map(next),
            },
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PostprocessParams {
    pub clip: ClipParams,
    pub saturation: f32,
    pub noise: f32,
    pub background: [f32; 4],
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum TextureEffect {
    Clip(ClipParams),
    Postprocess(PostprocessParams),
    Fade { cutoff: [f32; 2] },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct TextureOptions {
    pub color: SourceColor,
    pub effect: Option<TextureEffect>,
}

/// The blend space a frame is composited in; `None` in `Begin` means SDR (electrical sRGB).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BlendParams {
    /// PQ/BT.2020; `ref_lum_scale` is the SDR reference luminance / 10000.
    HdrPq { ref_lum_scale: f32 },
    /// Display P3 with a 2.2 transfer.
    DisplayP3,
}

/// HDR static metadata to signal on a connector (PQ, BT.2020 mastering primaries). Luminances
/// in cd/m², except `min_luminance` in 0.0001 cd/m².
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HdrMetadataDesc {
    pub max_luminance: u16,
    pub min_luminance: u16,
    pub max_cll: u16,
    pub max_fall: u16,
}

/// Connector color state, staged so it rides the DRM compositor's next atomic commit.
/// `hdr = Some` selects the BT2020_RGB colorspace with that infoframe; `None` is the default
/// (SDR) colorspace with no metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColorState {
    pub hdr: Option<HdrMetadataDesc>,
    /// The `max bpc` to request; `None` leaves the property alone.
    pub max_bpc: Option<u32>,
}

/// What the connector and its sink can do for HDR (from DRM properties and the EDID).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HdrCaps {
    /// The connector has `Colorspace` (with BT2020_RGB) and `HDR_OUTPUT_METADATA`, and the
    /// sink accepts the PQ EOTF.
    pub supported: bool,
    /// EDID desired content max luminance, cd/m² (0 = not provided).
    pub max_luminance: u16,
    /// EDID desired content min luminance, 0.0001 cd/m² (0 = not provided).
    pub min_luminance: u16,
    /// EDID desired content max frame-average luminance, cd/m² (0 = not provided).
    pub max_frame_avg_luminance: u16,
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
    /// Frames for outputs are kept by the GPU process and drawn by [`Request::Present`].
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
    /// Valid range of the `max bpc` property, if the connector has one.
    pub max_bpc_range: Option<(u32, u32)>,
    pub hdr: HdrCaps,
    pub gamma_size: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ElementKind {
    Cursor,
    ScanoutCandidate,
    Unspecified,
}

/// One frame of a render target, described as a scene: nodes bottom to top, each made of
/// draw ops. Every coordinate in a frame (node geometry, damage, opaque regions, op `dst`) is
/// in frame coordinates: the untransformed `width` x `height` buffer. The GPU derives
/// element-relative values where smithay wants them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SceneFrame {
    pub target: Target,
    pub width: i32,
    pub height: i32,
    pub transform: Transform,
    /// Blend space of this frame. When set, default-program texture draws go through
    /// Texture source encodings determine conversion; solids and built-in paints are SDR.
    pub blend: Option<BlendParams>,
    /// Offscreen targets are cleared to this before drawing; outputs and casts clear
    /// themselves to transparent.
    pub clear: Option<[f32; 4]>,
    /// Bumped whenever the core forgot its per-target history (new output geometry, failed
    /// send). The GPU forgets its history too, so node damage is never applied to a stale
    /// baseline.
    pub generation: u64,
    /// Present for `Target::Cast` frames.
    pub cast: Option<CastInfo>,
    pub nodes: Vec<Node>,
}

impl SceneFrame {
    pub fn size(&self) -> (i32, i32) {
        (self.width, self.height)
    }
}

/// Per-frame parameters of a screencast frame.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CastInfo {
    pub scale: f64,
    /// The presentation time this frame was rendered for; echoed in `CastEvent::Rendered`.
    pub target_time_ns: u64,
    /// `None` in embedded/hidden cursor modes or when the pointer is not over the target.
    pub cursor: Option<CursorMeta>,
}

impl Default for CastInfo {
    fn default() -> Self {
        Self {
            scale: 1.0,
            target_time_ns: 0,
            cursor: None,
        }
    }
}

/// One scene element: what the GPU's damage tracker needs to know, plus how to draw it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// Stable across frames of one target for the same element; chosen by the core. Ids with
    /// [`ANONYMOUS_NODE`] set are one-off nodes for draws outside any element.
    pub id: u64,
    pub src: Rect<f64>,
    pub geometry: Rect<i32>,
    /// Damage since the previous frame this id was in. `None` means everything (new element
    /// or unknown).
    pub damage: Option<Vec<Rect<i32>>>,
    pub opaque: Vec<Rect<i32>>,
    pub kind: ElementKind,
    /// Buffer transform, for direct scanout.
    pub transform: Transform,
    /// Framebuffer-effect ops (backdrop capture) run before the node is drawn.
    pub capture: Vec<Op>,
    pub draw: Vec<Op>,
}

/// High bit of a node id: the node was synthesized for draws outside any element and has no
/// history.
pub const ANONYMOUS_NODE: u64 = 1 << 63;

/// A draw inside a node. Ops carry no damage: the GPU clips every op to the damage the
/// tracker assigns its node.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Op {
    Solid {
        dst: Rect<i32>,
        color: [f32; 4],
    },
    Texture {
        texture: TexId,
        src: Rect<f64>,
        dst: Rect<i32>,
        opaque: Vec<Rect<i32>>,
        transform: Transform,
        alpha: f32,
        options: TextureOptions,
    },
    /// Built-in paint, evaluated as SDR content in the target's blend space.
    Paint {
        paint: Paint,
        src: Rect<f64>,
        dst: Rect<i32>,
        alpha: f32,
    },
    /// Snapshot the framebuffer under `dst` (blurred if requested) for a later `Captured`.
    Capture {
        key: u64,
        src: Rect<f64>,
        dst: Rect<i32>,
        scale: f32,
        blur: Option<BlurParams>,
    },
    /// A backdrop is already encoded in the target space; never encode it again.
    Captured {
        key: u64,
        dst: Rect<i32>,
        postprocess: PostprocessParams,
    },
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

    /// One rendered frame of a target. Output frames are kept for `Request::Present`, cast
    /// frames go to their stream, texture/dmabuf frames render right away.
    Frame(Box<SceneFrame>),

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Caps {
    pub renderer: String,
    pub mem_formats: Vec<u32>,
    pub dmabuf_formats: Vec<(u32, u64)>,
    /// Formats the GPU can render into (for screencast / image-copy dmabuf buffers).
    pub dmabuf_render_formats: Vec<(u32, u64)>,
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
                | Request::EncodePng { .. }
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    /// The first message to a process the supervisor started (no `--device` on its command
    /// line): the DRM devices to add before the sandbox seals, one fd attached per entry, in
    /// order. Answered by `Ready`. Never valid afterwards.
    Start {
        devices: Vec<DevId>,
        render_node_hint: Option<DevId>,
    },
    Execute {
        commands: Vec<Command>,
    },
    /// Like `Command::ImportDmabuf` but with its own reply, so a rejected buffer doesn't
    /// abort a batch. Fds attached.
    ImportDmabuf {
        id: TexId,
        desc: DmabufDesc,
    },
    /// Paints a UI scene into a Vulkan texture. Reply: Ack.
    RenderUi {
        id: TexId,
        scene: UiScene,
    },
    ReadTexture {
        id: TexId,
        region: Rect<i32>,
        format: u32,
    },
    /// Replied to (Ack) once everything sent before it has executed and finished on the GPU.
    Sync,

    // DRM/KMS. The core opens the device through libseat and hands over the fd.
    /// Fd attached. Reply: `DeviceAdded`. Devices may be added in any order.
    ///
    /// The GPU process owns the device model: the renderer is created on the first
    /// Vulkan-capable card matching `render_node_hint`, when set. Every other device is
    /// display-only and scans out buffers allocated on the rendering device.
    ///
    /// Hot-plug only: the initial devices come with the process's command line or in
    /// `Start`, so Mesa can initialize before the sandbox seals. A sealed process cannot bring
    /// up a renderer.
    AddDevice {
        dev: DevId,
        path: String,
        render_node_hint: Option<DevId>,
    },
    /// Reply: `DeviceRemoved`.
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
    /// Draw and commit every output's next frame in full: the kernel put something else on
    /// screen meanwhile (black after resume), and an undamaged scene would never be committed.
    RedrawAll,
    /// Reply: `OutputState`.
    EnableOutput {
        output: OutputRef,
        connector: u32,
        mode: ModeDesc,
        vrr: bool,
        /// Initial connector color state, staged to ride the modeset.
        color: ColorState,
        /// Show black right away (monitors are "off").
        clear: bool,
        /// Offer 10-bit scanout formats (each probed for renderability) before 8-bit ones.
        /// Off, the output stays 8-bit like upstream niri.
        prefer_10bit: bool,
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
    /// Stages connector color state (HDR signalling, max bpc) for the next commit. Reply:
    /// `OutputState`; fails if the driver rejects the state (TEST_ONLY commit).
    SetColorState {
        output: OutputRef,
        state: ColorState,
    },
    /// CRTC color transform matrix (row-major 3x3), `None` resets. Reply: Ack.
    SetCtm {
        output: OutputRef,
        matrix: Option<[f64; 9]>,
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
    /// Creates a PipeWire screencast stream. A connected PipeWire socket may be attached as
    /// fd; the GPU process uses it if it has no connection yet (it cannot connect itself).
    /// Reply: `CastStarted` with the effective cursor mode (metadata needs a recent PipeWire).
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
    /// Parses the Xcursor icon file attached as fd (the core finds and opens it; the GPU
    /// process has no filesystem), keeps the frames closest to `size` and uploads them as
    /// Argb8888 textures `first_id`, `first_id + 1`, … Reply: `Cursor`. With `fallback`, a
    /// built-in arrow is used when there is no fd or it does not parse.
    LoadCursor {
        size: i32,
        fallback: bool,
        first_id: TexId,
    },
    /// Encodes `region` of a texture as PNG (RGBA) on a GPU-process thread. One-way; the
    /// result arrives as `GpuEvent::Png { token }`.
    EncodePng {
        token: u64,
        id: TexId,
        region: Rect<i32>,
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
    /// Result of `Request::EncodePng`; `None` if reading or encoding failed.
    Png {
        token: u64,
        data: Option<Vec<u8>>,
    },
}

/// One frame of an Xcursor animation; the texture id is `first_id + index`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorFrameDesc {
    pub width: u32,
    pub height: u32,
    pub xhot: u32,
    pub yhot: u32,
    /// Milliseconds this frame is shown.
    pub delay: u32,
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

/// Outcome of adding a device given at startup.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceResult {
    pub dev: DevId,
    /// Set when this device brought up the renderer.
    pub render_node: Option<DevId>,
    /// Set when the device could not be added (it should be closed and not retried).
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    /// Sent once the devices given at startup are added and the sandbox is sealed. `caps` is
    /// present when a renderer exists (always in headless mode); `devices` reports on every
    /// startup device.
    Ready {
        version: u32,
        caps: Option<Caps>,
        devices: Vec<DeviceResult>,
    },
    Ack,
    Image(Image),
    /// Fds attached.
    Dmabuf(DmabufDesc),
    CastStarted {
        cursor_mode: CastCursorMode,
    },
    Cursor {
        frames: Vec<CursorFrameDesc>,
    },
    /// `render_node` and `caps` are set when this device brought up the renderer.
    DeviceAdded {
        render_node: Option<DevId>,
        caps: Option<Caps>,
    },
    /// `renderer_dropped` is set when the removed device owned the renderer.
    DeviceRemoved {
        renderer_dropped: bool,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_encoding_selects_only_existing_target_conversion() {
        let pq = Some(BlendParams::HdrPq {
            ref_lum_scale: 0.0203,
        });
        let p3 = Some(BlendParams::DisplayP3);
        for target in [None, pq, p3] {
            assert_eq!(SourceColor::Srgb.conversion_to(target), target);
            assert_eq!(SourceColor::Target.conversion_to(target), None);
        }
        assert_eq!(SourceColor::Hdr.conversion_to(pq), None);
        assert_eq!(SourceColor::DisplayP3.conversion_to(p3), None);
        // Nonmatching descriptions retain the existing SDR-conversion fallback.
        assert_eq!(SourceColor::Hdr.conversion_to(p3), p3);
        assert_eq!(SourceColor::DisplayP3.conversion_to(pq), pq);
        assert_eq!(SourceColor::Hdr.conversion_to(None), None);
        assert_eq!(SourceColor::DisplayP3.conversion_to(None), None);
    }

    #[test]
    fn typed_texture_intent_round_trips_without_effect_color_coupling() {
        let options = TextureOptions {
            color: SourceColor::DisplayP3,
            effect: Some(TextureEffect::Fade {
                cutoff: [0.17, 0.81],
            }),
        };
        let bytes = postcard::to_stdvec(&options).unwrap();
        let decoded: TextureOptions = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(decoded, options);
    }
}
