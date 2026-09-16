use std::collections::HashMap;
use std::rc::Rc;

use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::Uniform;
use smithay::backend::renderer::utils::{CommitCounter, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size};

use super::blend::FrameBlendState;
use super::shaders::{ProgramType, Shaders};
use crate::gpu::remote::{RemoteError, RemoteFrame, RemoteRenderer, RemoteTexture};

/// Renders a shader with optional texture input, on the primary GPU.
#[derive(Debug, Clone)]
pub struct ShaderRenderElement {
    program: ProgramType,
    id: Id,
    commit_counter: CommitCounter,
    area: Rectangle<f64, Logical>,
    opaque_regions: Vec<Rectangle<f64, Logical>>,
    // Should only be used for visual improvements, i.e. corner radius anti-aliasing.
    scale: f32,
    alpha: f32,
    additional_uniforms: Rc<[Uniform<'static>]>,
    textures: HashMap<String, RemoteTexture>,
    kind: Kind,
}

/// Marker for a compiled program in the GPU process; see [`Shaders::program`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShaderProgram(pub ProgramType);

impl ShaderRenderElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        program: ProgramType,
        size: Size<f64, Logical>,
        opaque_regions: Option<Vec<Rectangle<f64, Logical>>>,
        // Should only be used for visual improvements, i.e. corner radius anti-aliasing.
        scale: f32,
        alpha: f32,
        additional_uniforms: Rc<[Uniform<'static>]>,
        textures: HashMap<String, RemoteTexture>,
        kind: Kind,
    ) -> Self {
        Self {
            program,
            id: Id::new(),
            commit_counter: CommitCounter::default(),
            area: Rectangle::from_size(size),
            opaque_regions: opaque_regions.unwrap_or_default(),
            scale,
            alpha,
            additional_uniforms,
            textures,
            kind,
        }
    }

    pub fn empty(program: ProgramType, kind: Kind) -> Self {
        Self {
            program,
            id: Id::new(),
            commit_counter: CommitCounter::default(),
            area: Rectangle::default(),
            opaque_regions: vec![],
            scale: 1.,
            alpha: 1.,
            additional_uniforms: Rc::new([]),
            textures: HashMap::new(),
            kind,
        }
    }

    pub fn damage_all(&mut self) {
        self.commit_counter.increment();
    }

    pub fn update(
        &mut self,
        size: Size<f64, Logical>,
        opaque_regions: Option<Vec<Rectangle<f64, Logical>>>,
        scale: f32,
        alpha: f32,
        uniforms: Rc<[Uniform<'static>]>,
        textures: HashMap<String, RemoteTexture>,
    ) {
        self.area.size = size;
        self.opaque_regions = opaque_regions.unwrap_or_default();
        self.scale = scale;
        self.alpha = alpha;
        self.additional_uniforms = uniforms;
        self.textures = textures;

        self.commit_counter.increment();
    }

    pub fn with_location(mut self, location: Point<f64, Logical>) -> Self {
        self.area.loc = location;
        self
    }

    pub fn with_alpha(mut self, alpha: f32) -> Self {
        self.alpha = alpha;
        self
    }
}

impl Element for ShaderRenderElement {
    fn id(&self) -> &Id {
        &self.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.commit_counter
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        Rectangle::from_size(Size::from((1., 1.)))
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.area.to_physical_precise_round(scale)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        self.opaque_regions
            .iter()
            .map(|region| region.to_physical_precise_down(scale))
            .collect()
    }

    fn alpha(&self) -> f32 {
        self.alpha
    }

    fn kind(&self) -> Kind {
        self.kind
    }
}

impl RenderElement<RemoteRenderer> for ShaderRenderElement {
    fn draw(
        &self,
        frame: &mut RemoteFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), RemoteError> {
        if damage.is_empty() {
            return Ok(());
        }
        // The GPU process may have failed to compile this program; skip silently like before.
        if Shaders::from_renderer(frame.renderer())
            .program(self.program)
            .is_none()
        {
            return Ok(());
        }
        let textures: Vec<(String, RemoteTexture)> = self
            .textures
            .iter()
            .map(|(name, tex)| (name.clone(), tex.clone()))
            .collect();
        // Uniform values persist in the GPU-side program object, so the blend uniforms go
        // with every draw.
        let mut uniforms = self.additional_uniforms.to_vec();
        uniforms.extend(FrameBlendState::uniforms(frame));
        frame.draw_shader(
            self.program.into(),
            src,
            dst,
            damage,
            self.scale,
            self.alpha,
            &uniforms,
            &textures,
        );
        Ok(())
    }

    fn underlying_storage(&self, _renderer: &mut RemoteRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
