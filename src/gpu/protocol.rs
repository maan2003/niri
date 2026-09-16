//! Wire protocol between the compositor core and the GPU process.
//!
//! The core records renderer calls as [`Command`]s and ships them in batches with
//! [`Request::Execute`]. Every request gets exactly one [`Event`] reply. Commands that
//! carry file descriptors get them attached to the same transport frame, in command order.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 2;

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
    Mat2x2 { matrices: Vec<[f32; 4]>, transpose: bool },
    Mat2x3 { matrices: Vec<[f32; 6]>, transpose: bool },
    Mat2x4 { matrices: Vec<[f32; 8]>, transpose: bool },
    Mat3x2 { matrices: Vec<[f32; 6]>, transpose: bool },
    Mat3x3 { matrices: Vec<[f32; 9]>, transpose: bool },
    Mat3x4 { matrices: Vec<[f32; 12]>, transpose: bool },
    Mat4x2 { matrices: Vec<[f32; 8]>, transpose: bool },
    Mat4x3 { matrices: Vec<[f32; 12]>, transpose: bool },
    Mat4x4 { matrices: Vec<[f32; 16]>, transpose: bool },
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    Texture(TexId),
    Output(OutputId),
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
    End,
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
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Caps {
    pub renderer: String,
    pub mem_formats: Vec<u32>,
    pub dmabuf_formats: Vec<(u32, u64)>,
    pub shaders: ShaderSupport,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    Execute { commands: Vec<Command> },
    ReadTexture { id: TexId, region: Rect<i32>, format: u32 },
    SetCustomShader { kind: ShaderKind, src: Option<String> },
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Event {
    Ready { version: u32, caps: Caps },
    Ack,
    Image(Image),
    Error { message: String },
    Done,
}
