//! Shared smoke test for the remote renderer, run both in-thread and cross-process.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{Id, Kind, RenderElement};
use smithay::render_elements;
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{
    Bind as _, Color32F, ExportMem as _, Frame as _, ImportMem as _, Offscreen as _, Renderer as _,
};
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Size, Transform};

use super::client::GpuClient;
use super::remote::RemoteRenderer;

pub fn pattern(width: i32, height: i32, left: [u8; 4], right: [u8; 4]) -> Vec<u8> {
    let mut data = Vec::with_capacity((width * height * 4) as usize);
    for _y in 0..height {
        for x in 0..width {
            let px = if x < width / 2 { left } else { right };
            data.extend_from_slice(&px);
        }
    }
    data
}

/// Renders `elements` (front to back) into a fresh texture and reads it back as `fourcc`.
pub fn render_to_vec<E: RenderElement<RemoteRenderer>>(
    renderer: &mut RemoteRenderer,
    size: Size<i32, Physical>,
    elements: &[E],
    fourcc: Fourcc,
) -> anyhow::Result<Vec<u8>> {
    let buffer_size = Size::<i32, Buffer>::from((size.w, size.h));
    let mut texture = renderer.create_buffer(fourcc, buffer_size)?;
    {
        let mut target = renderer.bind(&mut texture)?;
        let mut frame = renderer.render(&mut target, size, Transform::Normal)?;
        frame.clear(Color32F::TRANSPARENT, &[Rectangle::from_size(size)])?;
        for element in elements.iter().rev() {
            let src = element.src();
            let dst = element.geometry(Scale::from(1.));
            let damage = Rectangle::from_size(dst.size);
            element.draw(&mut frame, src, dst, &[damage], &[], None)?;
        }
        let _sync = frame.finish()?;
    }
    let mapping = renderer.copy_texture(&texture, Rectangle::from_size(buffer_size), fourcc)?;
    Ok(renderer.map_texture(&mapping)?.to_vec())
}

render_elements! {
    SmokeElement<=RemoteRenderer>;
    Texture = TextureRenderElement<super::remote::RemoteTexture>,
    Solid = SolidColorRenderElement,
}

fn pixel(data: &[u8], width: i32, x: i32, y: i32) -> [u8; 4] {
    let i = ((y * width + x) * 4) as usize;
    data[i..i + 4].try_into().unwrap()
}

/// Draws a red/blue memory texture over a green background, reads back, checks pixels,
/// then updates the texture and checks again.
pub fn run_smoke(client: GpuClient) -> anyhow::Result<()> {
    let mut renderer = RemoteRenderer::new(client);
    let context_id = renderer.context_id();

    // Abgr8888 in memory is R, G, B, A bytes.
    let red = [255, 0, 0, 255];
    let blue = [0, 0, 255, 255];
    let yellow = [255, 255, 0, 255];
    let green = [0, 255, 0, 255];

    let tex = renderer.import_memory(&pattern(64, 64, red, blue), Fourcc::Abgr8888, (64, 64).into(), false)?;

    let make_elements = |tex| {
        let surface = TextureRenderElement::from_static_texture(
            Id::new(),
            context_id.clone(),
            Point::<f64, Physical>::from((32., 32.)),
            tex,
            1,
            Transform::Normal,
            None,
            None,
            None,
            None,
            Kind::Unspecified,
        );
        let background = SolidColorRenderElement::new(
            Id::new(),
            Rectangle::from_size((128, 128).into()),
            CommitCounter::default(),
            Color32F::new(0., 1., 0., 1.),
            Kind::Unspecified,
        );
        (surface, background)
    };

    let (surface, background) = make_elements(tex.clone());
    let size = Size::<i32, Physical>::from((128, 128));
    let elements = [SmokeElement::from(surface), SmokeElement::from(background)];
    let out = render_to_vec(&mut renderer, size, &elements, Fourcc::Abgr8888)?;
    assert_eq!(pixel(&out, 128, 10, 10), green, "background");
    assert_eq!(pixel(&out, 128, 40, 40), red, "left half");
    assert_eq!(pixel(&out, 128, 80, 40), blue, "right half");
    assert_eq!(pixel(&out, 128, 120, 120), green, "background bottom right");

    renderer.update_memory(&tex, &pattern(64, 64, yellow, yellow), Rectangle::from_size((64, 64).into()))?;
    let (surface, background) = make_elements(tex.clone());
    let elements = [SmokeElement::from(surface), SmokeElement::from(background)];
    let out = render_to_vec(&mut renderer, size, &elements, Fourcc::Abgr8888)?;
    assert_eq!(pixel(&out, 128, 40, 40), yellow, "after update");
    assert_eq!(pixel(&out, 128, 10, 10), green, "background after update");

    drop(tex);
    renderer.flush()?;
    Ok(())
}

