//! Shared smoke test for the remote renderer, run both in-thread and cross-process.

use smithay::backend::allocator::Fourcc;
use smithay::backend::renderer::element::solid::SolidColorRenderElement;
use smithay::backend::renderer::element::texture::TextureRenderElement;
use smithay::backend::renderer::element::{Id, Kind, RenderElement};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{
    Bind as _, Color32F, ExportMem as _, Frame as _, ImportMem as _, Offscreen as _, Renderer as _,
    Texture as _,
};
use smithay::render_elements;
use smithay::utils::{Buffer, Physical, Point, Rectangle, Scale, Size, Transform};

use super::client::GpuClient;
use super::protocol::{BlendParams, GpuEvent};
use super::remote::{RemoteFrame, RemoteRenderer, RemoteTexture};

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
    let texture = render_to_texture(renderer, size, elements, fourcc)?;
    let mapping = renderer.copy_texture(&texture, Rectangle::from_size(buffer_size), fourcc)?;
    Ok(renderer.map_texture(&mapping)?.to_vec())
}

/// Renders `elements` (front to back) into a fresh texture.
pub fn render_to_texture<E: RenderElement<RemoteRenderer>>(
    renderer: &mut RemoteRenderer,
    size: Size<i32, Physical>,
    elements: &[E],
    fourcc: Fourcc,
) -> anyhow::Result<RemoteTexture> {
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
    Ok(texture)
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

    let tex = renderer.import_memory(
        &pattern(64, 64, red, blue),
        Fourcc::Abgr8888,
        (64, 64).into(),
        false,
    )?;

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

    renderer.update_memory(
        &tex,
        &pattern(64, 64, yellow, yellow),
        Rectangle::from_size((64, 64).into()),
    )?;
    let (surface, background) = make_elements(tex.clone());
    let elements = [SmokeElement::from(surface), SmokeElement::from(background)];
    let out = render_to_vec(&mut renderer, size, &elements, Fourcc::Abgr8888)?;
    assert_eq!(pixel(&out, 128, 40, 40), yellow, "after update");
    assert_eq!(pixel(&out, 128, 10, 10), green, "background after update");

    // HDR blend space: solid colors are encoded on the CPU, default-program texture draws by
    // the blend texture shader, and a suspended override passes content through raw.
    if renderer.shaders().texture_hdr {
        let ref_lum_scale = 203. / 10000.;
        renderer.set_frame_blend(Some(BlendParams::HdrPq { ref_lum_scale }));
        let out = {
            let buffer_size = Size::<i32, Buffer>::from((128, 128));
            let mut texture = renderer.create_buffer(Fourcc::Abgr8888, buffer_size)?;
            {
                let mut target = renderer.bind(&mut texture)?;
                let mut frame = renderer.render(&mut target, size, Transform::Normal)?;
                let all = Rectangle::from_size(size);
                frame.clear(Color32F::TRANSPARENT, &[all])?;
                frame.draw_solid(all, &[all], Color32F::new(0., 1., 0., 1.))?;
                let src = Rectangle::from_size(Size::<f64, Buffer>::from((64., 64.)));
                let dst = Rectangle::from_size(Size::from((64, 64)));
                let draw = |frame: &mut RemoteFrame<'_, '_>, dst: Rectangle<i32, Physical>| {
                    let damage = Rectangle::from_size(dst.size);
                    frame.render_texture_from_to(
                        &tex,
                        src,
                        dst,
                        &[damage],
                        &[],
                        Transform::Normal,
                        1.,
                        None,
                        &[],
                    )
                };
                frame.suspend_tex_program_override();
                draw(&mut frame, dst)?;
                frame.restore_tex_program_override();
                draw(&mut frame, Rectangle::new((64, 64).into(), dst.size))?;
                let _sync = frame.finish()?;
            }
            let mapping = renderer.copy_texture(
                &texture,
                Rectangle::from_size(buffer_size),
                Fourcc::Abgr8888,
            )?;
            renderer.map_texture(&mapping)?.to_vec()
        };
        renderer.set_frame_blend(None);

        let encode = |c: [u8; 4]| {
            let c = Color32F::new(
                c[0] as f32 / 255.,
                c[1] as f32 / 255.,
                c[2] as f32 / 255.,
                c[3] as f32 / 255.,
            );
            let pq = crate::gpu::gl::blend::srgb_to_pq(c, ref_lum_scale);
            [pq.r(), pq.g(), pq.b(), pq.a()].map(|v| (v * 255.).round() as u8)
        };
        let close = |got: [u8; 4], want: [u8; 4]| {
            got.iter()
                .zip(want)
                .all(|(g, w)| (*g as i32 - w as i32).abs() <= 3)
        };
        let bg = pixel(&out, 128, 10, 120);
        assert!(
            close(bg, encode(green)),
            "solid in PQ: {bg:?} vs {:?}",
            encode(green)
        );
        let raw = pixel(&out, 128, 40, 40);
        assert_eq!(raw, yellow, "suspended override passes content through");
        let enc = pixel(&out, 128, 100, 100);
        assert!(
            close(enc, encode(yellow)),
            "texture in PQ: {enc:?} vs {:?}",
            encode(yellow)
        );
    }

    // Cursor loading happens GPU-side; a missing theme yields the built-in arrow.
    let gpu = renderer.gpu_handle();
    let names = ["default".to_owned()];
    let frames = gpu.load_cursor("niri-no-such-theme", &names, 24, true)?;
    assert_eq!(frames.len(), 1, "fallback cursor has one frame");
    let (desc, cursor_tex) = &frames[0];
    assert_eq!(
        (desc.width, desc.height, desc.xhot, desc.yhot),
        (64, 64, 1, 1)
    );
    assert_eq!(cursor_tex.size(), Size::from((64, 64)));
    assert!(
        gpu.load_cursor("niri-no-such-theme", &names, 24, false)
            .is_err(),
        "no fallback requested"
    );
    // The fallback arrow is opaque at its hotspot corner region.
    let cursor_px = renderer.copy_texture(
        cursor_tex,
        Rectangle::from_size((64, 64).into()),
        Fourcc::Abgr8888,
    )?;
    let cursor_px = renderer.map_texture(&cursor_px)?.to_vec();
    assert_eq!(cursor_px.len(), 64 * 64 * 4);
    assert_ne!(
        pixel(&cursor_px, 64, 2, 2)[3],
        0,
        "arrow tip is not transparent"
    );

    // PNG encoding happens GPU-side too; the bytes come back as an event.
    let (surface, background) = make_elements(tex.clone());
    let elements = [SmokeElement::from(surface), SmokeElement::from(background)];
    let texture = render_to_texture(&mut renderer, size, &elements, Fourcc::Abgr8888)?;
    let region = Rectangle::from_size((128, 128).into());
    renderer.encode_png(&texture, region, 7)?;
    let png = loop {
        let mut client = renderer.client();
        let found = client
            .take_events()
            .into_iter()
            .find_map(|event| match event {
                GpuEvent::Png { token, data } => Some((token, data)),
                _ => None,
            });
        if let Some((token, data)) = found {
            assert_eq!(token, 7);
            break data.expect("PNG encoding failed in the GPU process");
        }
        client.recv_event()?;
    };
    let decoder = png::Decoder::new(std::io::Cursor::new(&png));
    let mut reader = decoder.read_info()?;
    let mut decoded = vec![0; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut decoded)?;
    assert_eq!((info.width, info.height), (128, 128));
    assert_eq!(info.color_type, png::ColorType::Rgba);
    decoded.truncate(info.buffer_size());
    assert_eq!(pixel(&decoded, 128, 40, 40), yellow, "png left half");
    assert_eq!(pixel(&decoded, 128, 10, 10), green, "png background");

    drop(tex);
    renderer.flush()?;
    Ok(())
}
