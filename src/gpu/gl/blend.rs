//! Blend-space setup for a frame: the renderer-level texture program override and the CPU
//! encode for solid colors. The core decides the blend space per frame (`Command::Begin`).

use smithay::backend::renderer::gles::{GlesRenderer, Uniform};
use smithay::backend::renderer::Color32F;

use super::shaders::Shaders;
use crate::gpu::protocol::BlendParams;

/// Configures `renderer` for frames in the given blend space; `None` = SDR passthrough. Call
/// with `None` after the frame so later frames (casts, screenshots) stay SDR.
pub fn apply(renderer: &mut GlesRenderer, blend: Option<BlendParams>) {
    match blend {
        Some(params) => {
            let program = Shaders::get(renderer).texture_hdr.clone();
            if let Some(program) = program {
                renderer.set_default_tex_program_override(Some((program, uniforms(params))));
            } else {
                warn!("blend-space texture shader missing; SDR content will render raw");
            }
            renderer.set_solid_color_transform(Some(Box::new(move |color| match params {
                BlendParams::HdrPq { ref_lum_scale } => srgb_to_pq(color, ref_lum_scale),
                BlendParams::DisplayP3 => srgb_to_p3(color),
            })));
        }
        None => {
            renderer.set_default_tex_program_override(None);
            renderer.set_solid_color_transform(None);
        }
    }
}

fn uniforms(params: BlendParams) -> Vec<Uniform<'static>> {
    vec![
        Uniform::new("niri_blend_mode", params.mode()),
        Uniform::new("niri_ref_lum_scale", params.ref_lum_scale()),
    ]
}

/// CPU counterpart of the shaders' `niri_blend`: encodes an electrical sRGB premultiplied
/// color into PQ/BT.2020 for the given SDR reference luminance scale (reference / 10000).
#[allow(clippy::excessive_precision)] // the ST 2084 constants, as specified
pub fn srgb_to_pq(color: Color32F, ref_lum_scale: f32) -> Color32F {
    let a = color.a();
    let unpremul = |c: f32| if a > 0. { c / a } else { c };

    let pq = |lin: f32| {
        const M1: f32 = 0.1593017578125;
        const M2: f32 = 78.84375;
        const C1: f32 = 0.8359375;
        const C2: f32 = 18.8515625;
        const C3: f32 = 18.6875;
        let y = lin.clamp(0., 1.).powf(M1);
        ((C1 + C2 * y) / (1. + C3 * y)).powf(M2)
    };

    let r = unpremul(color.r()).max(0.).powf(2.2);
    let g = unpremul(color.g()).max(0.).powf(2.2);
    let b = unpremul(color.b()).max(0.).powf(2.2);

    // BT.709 -> BT.2020, linear light, D65.
    let r2020 = 0.627404 * r + 0.329283 * g + 0.043313 * b;
    let g2020 = 0.069097 * r + 0.919540 * g + 0.011362 * b;
    let b2020 = 0.016391 * r + 0.088013 * g + 0.895595 * b;

    Color32F::new(
        pq(r2020 * ref_lum_scale) * a,
        pq(g2020 * ref_lum_scale) * a,
        pq(b2020 * ref_lum_scale) * a,
        a,
    )
}

/// CPU counterpart of the shaders' `niri_blend` P3 mode: gamut-maps an electrical sRGB
/// premultiplied color into Display P3 with the same 2.2 decode/encode.
pub fn srgb_to_p3(color: Color32F) -> Color32F {
    let a = color.a();
    let unpremul = |c: f32| if a > 0. { c / a } else { c };

    let r = unpremul(color.r()).max(0.).powf(2.2);
    let g = unpremul(color.g()).max(0.).powf(2.2);
    let b = unpremul(color.b()).max(0.).powf(2.2);

    // BT.709 -> Display P3, linear light, D65.
    let rp3 = 0.822462 * r + 0.177538 * g;
    let gp3 = 0.033194 * r + 0.966806 * g;
    let bp3 = 0.017083 * r + 0.072397 * g + 0.910520 * b;

    let enc = |lin: f32| lin.max(0.).powf(1. / 2.2);
    Color32F::new(enc(rp3) * a, enc(gp3) * a, enc(bp3) * a, a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srgb_to_pq_reference_values() {
        let scale = (203. / 10000.) as f32;

        // Opaque white at reference luminance 203 cd/m²: PQ(0.0203) ≈ 0.5806.
        let white = srgb_to_pq(Color32F::new(1., 1., 1., 1.), scale);
        assert!((white.r() - 0.5806).abs() < 0.002, "got {}", white.r());
        // BT.709 white maps to BT.2020 white (rows sum to 1) => neutral stays neutral.
        assert!((white.r() - white.g()).abs() < 0.0005);
        assert!((white.g() - white.b()).abs() < 0.0005);

        // Black stays (essentially) black (PQ(0) is ~4e-7) and alpha is preserved.
        let black = srgb_to_pq(Color32F::new(0., 0., 0., 0.5), scale);
        assert!(black.r() < 1e-6, "got {}", black.r());
        assert_eq!(black.a(), 0.5);

        // Premultiplied 50% white: unpremultiplied value is 1.0, so the encoded result is
        // the white point rescaled by alpha.
        let half = srgb_to_pq(Color32F::new(0.5, 0.5, 0.5, 0.5), scale);
        assert!((half.r() - white.r() * 0.5).abs() < 0.0005);
    }

    #[test]
    fn srgb_to_p3_reference_values() {
        // Neutrals are untouched: the matrix rows sum to 1 and decode/encode cancel out.
        for v in [0., 0.25, 0.5, 1.] {
            let c = srgb_to_p3(Color32F::new(v, v, v, 1.));
            assert!((c.r() - v).abs() < 0.001, "got {} for {}", c.r(), v);
            assert!((c.r() - c.g()).abs() < 0.001);
            assert!((c.g() - c.b()).abs() < 0.001);
        }

        // Pure sRGB red is desaturated into P3: linear 0.822462 -> 0.9151 encoded, with a
        // little green and blue mixed in.
        let red = srgb_to_p3(Color32F::new(1., 0., 0., 1.));
        assert!((red.r() - 0.9151).abs() < 0.001, "got {}", red.r());
        assert!(red.g() > 0.1 && red.g() < 0.3, "got {}", red.g());
        assert!(red.b() > 0.1 && red.b() < 0.3, "got {}", red.b());

        // Alpha is preserved and premultiplication round-trips.
        let half = srgb_to_p3(Color32F::new(0.5, 0.5, 0.5, 0.5));
        assert!((half.r() - 0.5).abs() < 0.001, "got {}", half.r());
        assert_eq!(half.a(), 0.5);
    }
}
