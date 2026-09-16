//! Per-output blend space for windowed HDR and wide-gamut (Display P3) support.
//!
//! An output is either SDR (electrical sRGB, the default), HDR (the framebuffer holds
//! PQ/BT.2020 electrical values and the connector is signalled accordingly), or Display P3
//! (the framebuffer holds P3 electrical values, for panels that scan out in their native
//! wide gamut, like Apple panels on the Asahi DCP driver). On non-SDR outputs, sRGB content
//! is encoded into the blend space at draw time by the shaders' `niri_blend` stage; surfaces
//! that already carry a matching image description pass through numerically.
//!
//! The core only decides: the blend space travels in `Command::Begin`, the GPU process
//! installs the blend texture shader frame-wide and encodes solid colors on the CPU. niri's
//! own shader draws carry the blend uniforms explicitly (see [`FrameBlendState`]).
//!
//! Blending happens directly in encoded space. Alpha blending in an encoded space is an
//! approximation (the same class of error as regular sRGB-space blending).

use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::Uniform;
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::backend::renderer::{ImportAll, Renderer};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Transform};
use smithay::wayland::color::management::{ImageDescription, Primaries};

use crate::gpu::protocol::BlendParams;
use crate::gpu::remote::{RemoteError, RemoteFrame, RemoteRenderer};

/// Default SDR reference white in cd/m² (BT.2408).
pub const DEFAULT_REFERENCE_LUMINANCE: f64 = 203.;

/// The blend space an output is composited in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BlendSpace {
    /// PQ/BT.2020 with the given SDR reference luminance in cd/m².
    HdrPq { reference_luminance: f64 },
    /// Display P3 with an SDR (2.2) transfer.
    DisplayP3,
}

impl BlendSpace {
    /// The protocol form sent to the GPU process.
    pub fn params(self) -> BlendParams {
        match self {
            BlendSpace::HdrPq {
                reference_luminance,
            } => BlendParams::HdrPq {
                ref_lum_scale: (reference_luminance / 10000.) as f32,
            },
            BlendSpace::DisplayP3 => BlendParams::DisplayP3,
        }
    }
}

/// What color encoding a surface's content carries, per its color-management image
/// description. Content matching the frame's blend space passes through numerically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BlendContent {
    /// No image description, or an sRGB-like one: encode into the blend space.
    #[default]
    Sdr,
    /// A Display P3 (SDR) image description.
    DisplayP3,
    /// An HDR (PQ and/or BT.2020) image description.
    Hdr,
}

impl BlendContent {
    /// Classifies a surface's image description (if any).
    pub fn from_description(desc: Option<&ImageDescription>) -> Self {
        match desc {
            Some(desc) if desc.is_hdr() => BlendContent::Hdr,
            Some(desc) if desc.primaries == Primaries::DisplayP3 => BlendContent::DisplayP3,
            _ => BlendContent::Sdr,
        }
    }

    fn in_blend_space(self, blend: Option<BlendParams>) -> bool {
        match self {
            BlendContent::Sdr => false,
            BlendContent::DisplayP3 => matches!(blend, Some(BlendParams::DisplayP3)),
            BlendContent::Hdr => matches!(blend, Some(BlendParams::HdrPq { .. })),
        }
    }
}

/// Blend uniforms for draws with niri's own shader programs, from the frame's blend space.
///
/// Shader uniform values persist in GL program objects across draws, so on non-SDR frames
/// every draw sets the blend uniforms, and on SDR frames sets them back to zero.
pub struct FrameBlendState;

impl FrameBlendState {
    /// Whether the given content encoding matches the blend space of this frame (and thus
    /// passes through numerically).
    pub fn content_in_blend_space(frame: &RemoteFrame<'_, '_>, content: BlendContent) -> bool {
        content.in_blend_space(frame.blend())
    }

    /// The `niri_blend` uniform values for a draw of SDR content in this frame.
    pub fn uniforms(frame: &RemoteFrame<'_, '_>) -> [Uniform<'static>; 2] {
        Self::uniforms_for_content(frame, false)
    }

    /// The `niri_blend` uniform values for a draw in this frame; `in_blend_space` exempts
    /// content already encoded in the blend space.
    pub fn uniforms_for_content(
        frame: &RemoteFrame<'_, '_>,
        in_blend_space: bool,
    ) -> [Uniform<'static>; 2] {
        let blend = frame.blend();
        let mode = match blend {
            Some(params) if !in_blend_space => params.mode(),
            _ => 0.,
        };
        let scale = blend.map_or(0., |p| p.ref_lum_scale());
        [
            Uniform::new("niri_blend_mode", mode),
            Uniform::new("niri_ref_lum_scale", scale),
        ]
    }
}

/// A surface-tree render element that knows what color encoding its content carries (from
/// its image description).
///
/// For content matching the frame's blend space, the frame-wide blend-space texture program
/// is suspended around the draw, so the client's values pass through numerically. Underlying
/// storage is delegated, so direct scanout keeps working.
#[derive(Debug)]
pub struct BlendSurfaceRenderElement<R: Renderer> {
    inner: WaylandSurfaceRenderElement<R>,
    content: BlendContent,
}

impl<R: Renderer> BlendSurfaceRenderElement<R> {
    pub fn new(inner: WaylandSurfaceRenderElement<R>, content: BlendContent) -> Self {
        Self { inner, content }
    }

    pub fn inner(&self) -> &WaylandSurfaceRenderElement<R> {
        &self.inner
    }

    pub fn into_inner(self) -> WaylandSurfaceRenderElement<R> {
        self.inner
    }

    pub fn content(&self) -> BlendContent {
        self.content
    }
}

impl<R: Renderer + ImportAll> Element for BlendSurfaceRenderElement<R>
where
    R::TextureId: Clone + 'static,
{
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn location(&self, scale: Scale<f64>) -> Point<i32, Physical> {
        self.inner.location(scale)
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        self.inner.damage_since(scale, commit)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.inner.opaque_regions(scale)
    }

    fn alpha(&self) -> f32 {
        self.inner.alpha()
    }

    fn kind(&self) -> Kind {
        self.inner.kind()
    }
}

impl RenderElement<RemoteRenderer> for BlendSurfaceRenderElement<RemoteRenderer> {
    fn draw(
        &self,
        frame: &mut RemoteFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), RemoteError> {
        let suspend = FrameBlendState::content_in_blend_space(frame, self.content);
        if suspend {
            frame.suspend_tex_program_override();
        }
        let res = RenderElement::<RemoteRenderer>::draw(
            &self.inner,
            frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        );
        if suspend {
            frame.restore_tex_program_override();
        }
        res
    }

    fn underlying_storage(&self, renderer: &mut RemoteRenderer) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}
