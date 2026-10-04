use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size};

use crate::gpu::protocol::Paint;
use crate::gpu::remote::{RemoteError, RemoteFrame, RemoteRenderer, RemoteTexture};

/// Renders a typed procedural paint on the primary GPU.
#[derive(Debug, Clone)]
pub struct PaintRenderElement {
    paint: Option<Paint<RemoteTexture>>,
    id: Id,
    commit_counter: CommitCounter,
    area: Rectangle<f64, Logical>,
    opaque_regions: Vec<Rectangle<f64, Logical>>,
    alpha: f32,
    kind: Kind,
}

impl PaintRenderElement {
    pub fn new(
        paint: Paint<RemoteTexture>,
        size: Size<f64, Logical>,
        opaque_regions: Option<Vec<Rectangle<f64, Logical>>>,
        alpha: f32,
        kind: Kind,
    ) -> Self {
        let mut element = Self::empty(kind);
        element.paint = Some(paint);
        element.area.size = size;
        element.opaque_regions = opaque_regions.unwrap_or_default();
        element.alpha = alpha;
        element
    }

    pub fn empty(kind: Kind) -> Self {
        Self {
            paint: None,
            id: Id::new(),
            commit_counter: CommitCounter::default(),
            area: Rectangle::default(),
            opaque_regions: vec![],
            alpha: 1.,
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
        alpha: f32,
        paint: Paint<RemoteTexture>,
    ) {
        self.area.size = size;
        self.opaque_regions = opaque_regions.unwrap_or_default();
        self.alpha = alpha;
        self.paint = Some(paint);
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

impl Element for PaintRenderElement {
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

impl RenderElement<RemoteRenderer> for PaintRenderElement {
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
        if let Some(paint) = &self.paint {
            frame.draw_paint(paint.clone(), src, dst, damage, self.alpha);
        }
        Ok(())
    }

    fn underlying_storage(&self, _renderer: &mut RemoteRenderer) -> Option<UnderlyingStorage<'_>> {
        None
    }
}
