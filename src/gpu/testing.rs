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
use super::protocol::{
    BlendParams, GpuEvent, SourceColor, TextureOptions, UiFont, UiGlyph, UiOp, UiScene,
};
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
    run_ui_smoke(&mut renderer)?;

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

    // Explicit texture source colors select conversion independently of rendering effects.
    // Nested core draw context must restore on success and on a returned error.
    {
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
                let dst = Rectangle::from_size(Size::from((48, 48)));
                let draw = |frame: &mut RemoteFrame<'_, '_>, dst: Rectangle<i32, Physical>| {
                    let damage = Rectangle::from_size(dst.size);
                    smithay::backend::renderer::Frame::render_texture_from_to(
                        frame,
                        &tex,
                        src,
                        dst,
                        &[damage],
                        &[],
                        Transform::Normal,
                        1.,
                    )
                };
                let target_color = TextureOptions {
                    color: SourceColor::Target,
                    ..Default::default()
                };
                frame.with_texture_options(
                    target_color,
                    |frame| -> Result<(), super::remote::RemoteError> {
                        draw(frame, dst)?;
                        let result =
                            frame.with_texture_options(TextureOptions::default(), |frame| {
                                draw(frame, Rectangle::new((64, 0).into(), dst.size)).unwrap();
                                Err::<(), _>("intentional returned error")
                            });
                        assert!(result.is_err());
                        assert_eq!(frame.texture_options(), target_color);
                        draw(frame, Rectangle::new((0, 64).into(), dst.size))
                    },
                )?;
                assert_eq!(frame.texture_options(), TextureOptions::default());
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
            let pq = crate::gpu::vello::effects::srgb_to_pq(c, ref_lum_scale);
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
        assert_eq!(raw, yellow, "target-encoded content passes through");
        assert_eq!(
            pixel(&out, 128, 40, 100),
            yellow,
            "returned error restores outer source color"
        );
        assert!(
            close(pixel(&out, 128, 100, 40), encode(yellow)),
            "nested SDR source is encoded"
        );
        let enc = pixel(&out, 128, 100, 100);
        assert!(
            close(enc, encode(yellow)),
            "texture in PQ: {enc:?} vs {:?}",
            encode(yellow)
        );
    }

    // Cursor parsing happens GPU-side; without an icon file the built-in arrow is used.
    let gpu = renderer.gpu_handle();
    let frames = gpu.load_cursor(None, 24, true)?;
    assert_eq!(frames.len(), 1, "fallback cursor has one frame");
    let (desc, cursor_tex) = &frames[0];
    assert_eq!(
        (desc.width, desc.height, desc.xhot, desc.yhot),
        (64, 64, 1, 1)
    );
    assert_eq!(cursor_tex.size(), Size::from((64, 64)));
    assert!(
        gpu.load_cursor(None, 24, false).is_err(),
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

/// Exercises paint IPC, glyph positioning, premultiplied compositing and padded GPU readback.
fn run_ui_smoke(renderer: &mut RemoteRenderer) -> anyhow::Result<()> {
    use anyhow::Context as _;
    use skrifa::{FontRef, MetadataProvider};
    let mut font_context = parley::FontContext::new();
    let mut query = font_context
        .collection
        .query(&mut font_context.source_cache);
    query.set_families([parley::fontique::GenericFamily::SansSerif]);
    let mut font = None;
    query.matches_with(|candidate| {
        font = Some(UiFont {
            index: candidate.index,
            data: candidate.blob.data().to_vec(),
        });
        parley::fontique::QueryStatus::Stop
    });
    let font = font.context("UI GPU smoke needs a system sans-serif font")?;
    let face = FontRef::from_index(&font.data, font.index)?;
    let glyph = face
        .charmap()
        .map('H')
        .context("test font has no H glyph")?
        .to_u32();
    let scene = UiScene {
        width: 130,
        height: 96,
        fonts: vec![font],
        ops: vec![
            UiOp::Rect {
                rect: [1., 2., 23., 16.],
                color: [1., 0., 0., 0.5],
            },
            UiOp::Rect {
                rect: [15., 8., 18., 14.],
                color: [0., 0., 1., 0.5],
            },
            UiOp::Circle {
                center: [100., 20.],
                radius: 9.,
                color: [0., 1., 0., 1.],
            },
            UiOp::GlyphRun {
                font: 0,
                font_size: 32.,
                coords: vec![],
                color: [1., 1., 1., 1.],
                glyphs: vec![
                    UiGlyph {
                        id: glyph,
                        x: 12.,
                        y: 60.,
                    },
                    UiGlyph {
                        id: glyph,
                        x: 64.,
                        y: 85.,
                    },
                ],
            },
        ],
    };
    let texture = renderer.render_ui(scene.clone())?;
    assert_eq!(texture.size(), Size::from((130, 96)));
    let mapping = renderer.copy_texture(
        &texture,
        Rectangle::from_size((130, 96).into()),
        Fourcc::Abgr8888,
    )?;
    let pixels = renderer.map_texture(&mapping)?;
    assert_eq!(pixels.len(), 130 * 96 * 4);
    let close = |got: [u8; 4], want: [u8; 4]| {
        assert!(
            got.iter()
                .zip(want)
                .all(|(g, w)| (*g as i32 - w as i32).abs() <= 1),
            "pixel {got:?} != {want:?}"
        );
    };
    close(pixel(pixels, 130, 5, 5), [128, 0, 0, 128]);
    close(pixel(pixels, 130, 20, 10), [64, 0, 128, 191]);
    close(pixel(pixels, 130, 30, 20), [0, 0, 128, 128]);
    assert_eq!(
        pixel(pixels, 130, 100, 20),
        [0, 255, 0, 255],
        "circle center"
    );
    assert_eq!(pixel(pixels, 130, 111, 20), [0, 0, 0, 0], "outside circle");
    assert_eq!(
        pixel(pixels, 130, 129, 95),
        [0, 0, 0, 0],
        "transparent target/padded readback"
    );
    let mut glyph_pixels = 0;
    for y in 28..60 {
        for x in 12..44 {
            let first = pixel(pixels, 130, x, y);
            assert_eq!(
                first,
                pixel(pixels, 130, x + 52, y + 25),
                "absolute glyph positions"
            );
            glyph_pixels += usize::from(first[3] != 0);
        }
    }
    assert!(
        glyph_pixels > 100,
        "glyphs were actually painted ({glyph_pixels} pixels)"
    );
    if let Some(path) = std::env::var_os("NIRI_GPU_UI_SMOKE_PNG") {
        // PNG stores straight alpha, unlike the premultiplied render texture.
        let mut rgba = pixels.to_vec();
        for pixel in rgba.chunks_exact_mut(4) {
            let alpha = u32::from(pixel[3]);
            if alpha != 0 {
                for channel in &mut pixel[..3] {
                    *channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
                }
            }
        }
        let mut encoder = png::Encoder::new(std::fs::File::create(path)?, 130, 96);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.write_header()?.write_image_data(&rgba)?;
    }

    let mut bad = scene.clone();
    bad.width = 0;
    assert!(renderer.render_ui(bad).is_err(), "zero dimensions rejected");
    let mut bad = scene.clone();
    bad.ops = vec![UiOp::Circle {
        center: [f32::NAN, 0.],
        radius: 1.,
        color: [1.; 4],
    }];
    assert!(
        renderer.render_ui(bad).is_err(),
        "nonfinite geometry rejected"
    );
    let mut bad = scene.clone();
    bad.fonts[0].data = vec![0; 32];
    assert!(
        renderer.render_ui(bad).is_err(),
        "invalid font data rejected"
    );
    let mut bad = scene.clone();
    if let UiOp::GlyphRun { font, .. } = bad.ops.last_mut().unwrap() {
        *font = 1;
    }
    assert!(
        renderer.render_ui(bad).is_err(),
        "scene-local font index checked"
    );
    let mut bad = scene.clone();
    if let UiOp::GlyphRun { glyphs, .. } = bad.ops.last_mut().unwrap() {
        glyphs[0].id = u32::MAX;
    }
    assert!(
        renderer.render_ui(bad).is_err(),
        "font-local glyph index checked"
    );
    let mut bad = scene.clone();
    if let UiOp::Rect { color, .. } = &mut bad.ops[0] {
        color[3] = 1.5;
    }
    assert!(renderer.render_ui(bad).is_err(), "color bounds checked");

    let texture = renderer.render_ui(scene)?;
    let mapping = renderer.copy_texture(
        &texture,
        Rectangle::from_size((130, 96).into()),
        Fourcc::Abgr8888,
    )?;
    assert_eq!(
        pixel(renderer.map_texture(&mapping)?, 130, 100, 20),
        [0, 255, 0, 255],
        "worker still usable after malformed scenes"
    );
    Ok(())
}
