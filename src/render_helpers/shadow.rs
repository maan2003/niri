use glam::{Mat3, Vec2};
use niri_config::{Color, CornerRadius};
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Logical, Physical, Point, Rectangle, Scale, Size, Transform};

use super::paint_element::PaintRenderElement;
use crate::gpu::protocol::{Paint, RoundedGeometry, ShadowParams};
use crate::gpu::remote::{RemoteError, RemoteFrame, RemoteRenderer};

/// Renders a rounded rectangle shadow.
#[derive(Debug, Clone)]
pub struct ShadowRenderElement {
    inner: PaintRenderElement,
    params: Parameters,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Parameters {
    size: Size<f64, Logical>,
    geometry: Rectangle<f64, Logical>,
    color: Color,
    sigma: f32,
    corner_radius: CornerRadius,
    alpha: f32,

    window_geometry: Rectangle<f64, Logical>,
    window_corner_radius: CornerRadius,
}

impl ShadowRenderElement {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        size: Size<f64, Logical>,
        geometry: Rectangle<f64, Logical>,
        color: Color,
        sigma: f32,
        corner_radius: CornerRadius,
        window_geometry: Rectangle<f64, Logical>,
        window_corner_radius: CornerRadius,
        alpha: f32,
    ) -> Self {
        let inner = PaintRenderElement::empty(Kind::Unspecified);
        let mut rv = Self {
            inner,
            params: Parameters {
                size,
                geometry,
                color,
                sigma,
                corner_radius,
                alpha,
                window_geometry,
                window_corner_radius,
            },
        };
        rv.update_inner();
        rv
    }

    pub fn empty() -> Self {
        let inner = PaintRenderElement::empty(Kind::Unspecified);
        Self {
            inner,
            params: Parameters {
                size: Default::default(),
                geometry: Default::default(),
                color: Default::default(),
                sigma: 0.,
                corner_radius: Default::default(),
                alpha: 1.,
                window_geometry: Default::default(),
                window_corner_radius: Default::default(),
            },
        }
    }

    pub fn damage_all(&mut self) {
        self.inner.damage_all();
    }

    #[allow(clippy::too_many_arguments)]
    pub fn update(
        &mut self,
        size: Size<f64, Logical>,
        geometry: Rectangle<f64, Logical>,
        color: Color,
        sigma: f32,
        corner_radius: CornerRadius,
        window_geometry: Rectangle<f64, Logical>,
        window_corner_radius: CornerRadius,
        alpha: f32,
    ) {
        let params = Parameters {
            size,
            geometry,
            color,
            sigma,
            alpha,
            corner_radius,
            window_geometry,
            window_corner_radius,
        };
        if self.params == params {
            return;
        }

        self.params = params;
        self.update_inner();
    }

    fn update_inner(&mut self) {
        let Parameters {
            size,
            geometry,
            color,
            sigma,
            alpha,
            corner_radius,
            window_geometry,
            window_corner_radius,
        } = self.params;

        let area_size = Vec2::new(size.w as f32, size.h as f32);

        let geo_loc = Vec2::new(geometry.loc.x as f32, geometry.loc.y as f32);
        let geo_size = Vec2::new(geometry.size.w as f32, geometry.size.h as f32);

        let input_to_geo =
            Mat3::from_scale(area_size) * Mat3::from_translation(-geo_loc / area_size);

        let window_geo_loc = Vec2::new(window_geometry.loc.x as f32, window_geometry.loc.y as f32);
        let window_geo_size =
            Vec2::new(window_geometry.size.w as f32, window_geometry.size.h as f32);

        let window_input_to_geo =
            Mat3::from_scale(area_size) * Mat3::from_translation(-window_geo_loc / area_size);

        self.inner.update(
            size,
            None,
            alpha,
            Paint::Shadow(ShadowParams {
                geometry: RoundedGeometry {
                    size: geo_size.to_array(),
                    radii: corner_radius.into(),
                    input_to_geo: input_to_geo.to_cols_array(),
                },
                window: (window_geo_size.x > 0. && window_geo_size.y > 0.).then_some(
                    RoundedGeometry {
                        size: window_geo_size.to_array(),
                        radii: window_corner_radius.into(),
                        input_to_geo: window_input_to_geo.to_cols_array(),
                    },
                ),
                color: color.to_array_premul(),
                sigma,
            }),
        );
    }

    pub fn with_location(mut self, location: Point<f64, Logical>) -> Self {
        self.inner = self.inner.with_location(location);
        self
    }

    pub fn with_alpha(mut self, alpha: f32) -> Self {
        self.inner = self.inner.with_alpha(alpha);
        self
    }
}

impl Default for ShadowRenderElement {
    fn default() -> Self {
        Self::empty()
    }
}

impl Element for ShadowRenderElement {
    fn id(&self) -> &Id {
        self.inner.id()
    }

    fn current_commit(&self) -> CommitCounter {
        self.inner.current_commit()
    }

    fn geometry(&self, scale: Scale<f64>) -> Rectangle<i32, Physical> {
        self.inner.geometry(scale)
    }

    fn transform(&self) -> Transform {
        self.inner.transform()
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        self.inner.src()
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

impl RenderElement<RemoteRenderer> for ShadowRenderElement {
    fn draw(
        &self,
        frame: &mut RemoteFrame<'_, '_>,
        src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        opaque_regions: &[Rectangle<i32, Physical>],
        cache: Option<&UserDataMap>,
    ) -> Result<(), RemoteError> {
        let _span = tracy_client::span!("ShadowRenderElement::draw");
        RenderElement::<RemoteRenderer>::draw(
            &self.inner,
            frame,
            src,
            dst,
            damage,
            opaque_regions,
            cache,
        )
    }

    fn underlying_storage(&self, renderer: &mut RemoteRenderer) -> Option<UnderlyingStorage<'_>> {
        self.inner.underlying_storage(renderer)
    }
}
