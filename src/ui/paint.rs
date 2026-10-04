//! Core-side text shaping and logical paint recording. Rasterization lives in the GPU process.
use std::cell::RefCell;
use std::ops::Range;

use anyhow::{ensure, Result};
use parley::{
    Alignment, AlignmentOptions, FontContext, FontFamily, FontStyle, FontWeight, LayoutContext,
    PositionedLayoutItem, StyleProperty,
};
use smithay::utils::Transform;

use crate::gpu::protocol::{UiFont, UiGlyph, UiOp, UiScene};
use crate::gpu::remote::{RemoteRenderer, RemoteTexture};
use crate::render_helpers::texture::TextureBuffer;

#[derive(Clone, Copy, Default)]
pub enum TextAlign {
    #[default]
    Left,
    Center,
}

pub struct TextOptions {
    pub font_size: f32,
    pub family: String,
    pub bold: bool,
    pub align: TextAlign,
    pub single_line: bool,
    pub line_spacing: f32,
}
impl Default for TextOptions {
    fn default() -> Self {
        Self {
            font_size: 14.,
            family: "sans-serif".into(),
            bold: false,
            align: TextAlign::Left,
            single_line: false,
            line_spacing: 0.,
        }
    }
}
#[derive(Clone, Debug, PartialEq)]
struct Brush {
    foreground: [f32; 4],
    background: Option<[f32; 4]>,
}
impl Default for Brush {
    fn default() -> Self {
        Self {
            foreground: [1.; 4],
            background: None,
        }
    }
}
#[derive(Clone, Default)]
struct SpanStyle {
    brush: Brush,
    family: Option<String>,
    bold: bool,
    italic: bool,
    spacing: f32,
    underline: bool,
    strike: bool,
}
type Span = (Range<usize>, SpanStyle);

fn color(value: &str) -> Option<[f32; 4]> {
    if let Ok(color) = value.parse::<niri_config::Color>() {
        return Some(color.to_array_unpremul());
    }
    // Pango also accepts 16-bit RGB components (used by hotkey keycaps).
    let hex = value.strip_prefix('#')?;
    if !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let digits = hex.len() / 3;
    if !matches!(digits, 1 | 2 | 4) || hex.len() != digits * 3 {
        return None;
    }
    let max = ((1u32 << (digits * 4)) - 1) as f32;
    Some([
        u32::from_str_radix(&hex[..digits], 16).ok()? as f32 / max,
        u32::from_str_radix(&hex[digits..digits * 2], 16).ok()? as f32 / max,
        u32::from_str_radix(&hex[digits * 2..], 16).ok()? as f32 / max,
        1.,
    ])
}

fn parse_markup(input: &str) -> Result<(String, Vec<Span>)> {
    let wrapped = format!("<root>{input}</root>");
    let doc = roxmltree::Document::parse(&wrapped)?;
    fn walk(
        node: roxmltree::Node<'_, '_>,
        inherited: &SpanStyle,
        out: &mut String,
        spans: &mut Vec<Span>,
    ) {
        if node.is_text() {
            let start = out.len();
            out.push_str(node.text().unwrap_or_default());
            spans.push((start..out.len(), inherited.clone()));
            return;
        }
        let mut style = inherited.clone();
        match node.tag_name().name() {
            "b" | "strong" => style.bold = true,
            "i" | "em" => style.italic = true,
            "tt" => style.family = Some("monospace".into()),
            "u" => style.underline = true,
            "s" => style.strike = true,
            _ => {}
        }
        for attr in node.attributes() {
            match attr.name() {
                "foreground" | "fgcolor" | "color" => {
                    if let Some(c) = color(attr.value()) {
                        style.brush.foreground = c;
                    }
                }
                "background" | "bgcolor" => style.brush.background = color(attr.value()),
                "font_family" | "face" => {
                    style.family = Some(
                        if attr.value() == "mono" {
                            "monospace"
                        } else {
                            attr.value()
                        }
                        .into(),
                    )
                }
                "weight" => style.bold = matches!(attr.value(), "bold" | "heavy" | "700"),
                "style" => style.italic = matches!(attr.value(), "italic" | "oblique"),
                "letter_spacing" => {
                    style.spacing = attr.value().parse::<f32>().unwrap_or(0.) / 1024.
                }
                "underline" => style.underline = attr.value() != "none",
                "strikethrough" => style.strike = attr.value() == "true",
                _ => {}
            }
        }
        for child in node.children() {
            walk(child, &style, out, spans);
        }
    }
    let mut text = String::new();
    let mut spans = Vec::new();
    walk(
        doc.root_element(),
        &SpanStyle::default(),
        &mut text,
        &mut spans,
    );
    Ok((text, spans))
}
pub fn plain_text(markup: &str) -> String {
    parse_markup(markup)
        .map(|(text, _)| text)
        .unwrap_or_else(|_| markup.into())
}

thread_local! {
    static SHAPER: RefCell<(FontContext, LayoutContext<Brush>)> =
        RefCell::new((FontContext::new(), LayoutContext::new()));
}
#[derive(Clone)]
pub struct Text {
    paint: Paint,
}
impl Text {
    pub fn new(text: &str, font_size: f32) -> Result<Self> {
        Self::with_options(
            text,
            TextOptions {
                font_size,
                ..Default::default()
            },
            false,
        )
    }
    pub fn markup(text: &str, font_size: f32) -> Result<Self> {
        Self::with_options(
            text,
            TextOptions {
                font_size,
                ..Default::default()
            },
            true,
        )
    }
    pub fn with_options(input: &str, options: TextOptions, markup: bool) -> Result<Self> {
        ensure!(
            options.font_size.is_finite() && options.font_size > 0.,
            "invalid font size"
        );
        let (mut text, spans) = if markup {
            parse_markup(input)?
        } else {
            (input.to_owned(), Vec::new())
        };
        // Preserve byte offsets for styled ranges.
        if options.single_line {
            text = text.replace(['\n', '\r'], " ");
        }
        // Parley gives an empty line a synthetic advance; empty labels have no paint.
        if text.is_empty() {
            return Ok(Self {
                paint: Paint::new(0, 0),
            });
        }
        SHAPER.with_borrow_mut(|(fonts, context)| {
            let mut builder = context.ranged_builder(fonts, &text, 1., true);
            builder.push_default(FontFamily::from(options.family.as_str()));
            builder.push_default(StyleProperty::FontSize(options.font_size));
            if options.bold {
                builder.push_default(StyleProperty::FontWeight(FontWeight::BOLD));
            }
            for (range, style) in &spans {
                builder.push(StyleProperty::Brush(style.brush.clone()), range.clone());
                if let Some(family) = &style.family {
                    builder.push(FontFamily::from(family.as_str()), range.clone());
                }
                if style.bold {
                    builder.push(StyleProperty::FontWeight(FontWeight::BOLD), range.clone());
                }
                if style.italic {
                    builder.push(StyleProperty::FontStyle(FontStyle::Italic), range.clone());
                }
                builder.push(StyleProperty::LetterSpacing(style.spacing), range.clone());
                builder.push(StyleProperty::Underline(style.underline), range.clone());
                builder.push(StyleProperty::Strikethrough(style.strike), range.clone());
            }
            let mut layout = builder.build(&text);
            layout.break_all_lines(None);
            layout.align(
                match options.align {
                    TextAlign::Left => Alignment::Start,
                    TextAlign::Center => Alignment::Center,
                },
                AlignmentOptions::default(),
            );
            let extra = options.line_spacing * (layout.len().saturating_sub(1) as f32);
            let mut paint = Paint::new(
                layout.full_width().ceil() as i32,
                (layout.height() + extra).ceil() as i32,
            );
            let mut font_ids = Vec::new();
            for (line_index, line) in layout.lines().enumerate() {
                let dy = line_index as f32 * options.line_spacing;
                for item in line.items() {
                    let PositionedLayoutItem::GlyphRun(glyph_run) = item else {
                        continue;
                    };
                    let run = glyph_run.run();
                    let font = run.font();
                    let key = (font.data.id(), font.index);
                    let index = font_ids.iter().position(|k| *k == key).unwrap_or_else(|| {
                        font_ids.push(key);
                        paint.scene.fonts.push(UiFont {
                            index: font.index,
                            data: font.data.data().to_vec(),
                        });
                        font_ids.len() - 1
                    });
                    let style = glyph_run.style();
                    let metrics = run.metrics();
                    let baseline = glyph_run.baseline() + dy;
                    if let Some(bg) = style.brush.background {
                        paint.rect(
                            glyph_run.offset(),
                            baseline - metrics.ascent,
                            glyph_run.advance(),
                            metrics.ascent + metrics.descent,
                            bg,
                        );
                    }
                    let glyphs = glyph_run
                        .positioned_glyphs()
                        .map(|g| UiGlyph {
                            id: g.id,
                            x: g.x,
                            y: g.y + dy,
                        })
                        .collect();
                    paint.scene.ops.push(UiOp::GlyphRun {
                        font: index as u32,
                        font_size: run.font_size(),
                        coords: run.normalized_coords().to_vec(),
                        glyphs,
                        color: style.brush.foreground,
                    });
                    if style.underline.is_some() {
                        paint.rect(
                            glyph_run.offset(),
                            baseline - metrics.underline_offset,
                            glyph_run.advance(),
                            metrics.underline_size,
                            style.brush.foreground,
                        );
                    }
                    if style.strikethrough.is_some() {
                        paint.rect(
                            glyph_run.offset(),
                            baseline - metrics.strikethrough_offset,
                            glyph_run.advance(),
                            metrics.strikethrough_size,
                            style.brush.foreground,
                        );
                    }
                }
            }
            Ok(Self { paint })
        })
    }
    pub fn size(&self) -> (i32, i32) {
        (
            self.paint.scene.width as i32,
            self.paint.scene.height as i32,
        )
    }
}

#[derive(Clone)]
pub struct Paint {
    scene: UiScene,
}
impl Paint {
    pub fn new(width: i32, height: i32) -> Self {
        Self {
            scene: UiScene {
                width: width.max(0) as u32,
                height: height.max(0) as u32,
                fonts: Vec::new(),
                ops: Vec::new(),
            },
        }
    }
    pub fn fill(&mut self, color: [f32; 4]) {
        self.rect(
            0.,
            0.,
            self.scene.width as f32,
            self.scene.height as f32,
            color,
        );
    }
    pub fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: [f32; 4]) {
        self.scene.ops.push(UiOp::Rect {
            rect: [x, y, w, h],
            color,
        });
    }
    pub fn circle(&mut self, x: f32, y: f32, r: f32, color: [f32; 4]) {
        self.scene.ops.push(UiOp::Circle {
            center: [x, y],
            radius: r,
            color,
        });
    }
    pub fn border(&mut self, width: f32, color: [f32; 4]) {
        let w = self.scene.width as f32;
        let h = self.scene.height as f32;
        let b = width / 2.;
        self.rect(0., 0., w, b, color);
        self.rect(0., h - b, w, b, color);
        self.rect(0., b, b, h - 2. * b, color);
        self.rect(w - b, b, b, h - 2. * b, color);
    }
    pub fn text(&mut self, text: &Text, x: f32, y: f32) {
        let fonts: Vec<_> = text
            .paint
            .scene
            .fonts
            .iter()
            .map(|font| {
                self.scene
                    .fonts
                    .iter()
                    .position(|existing| existing.index == font.index && existing.data == font.data)
                    .unwrap_or_else(|| {
                        self.scene.fonts.push(font.clone());
                        self.scene.fonts.len() - 1
                    }) as u32
            })
            .collect();
        for mut op in text.paint.scene.ops.clone() {
            match &mut op {
                UiOp::Rect { rect, .. } => {
                    rect[0] += x;
                    rect[1] += y;
                }
                UiOp::Circle { center, .. } => {
                    center[0] += x;
                    center[1] += y;
                }
                UiOp::GlyphRun { font, glyphs, .. } => {
                    *font = fonts[*font as usize];
                    for g in glyphs {
                        g.x += x;
                        g.y += y;
                    }
                }
            }
            self.scene.ops.push(op);
        }
    }
    pub fn render(
        &self,
        renderer: &mut RemoteRenderer,
        scale: f64,
    ) -> Result<TextureBuffer<RemoteTexture>> {
        ensure!(scale.is_finite() && scale > 0., "invalid UI scale");
        let mut scene = self.scene.clone();
        let s = scale as f32;
        scene.width = (scene.width as f64 * scale).ceil() as u32;
        scene.height = (scene.height as f64 * scale).ceil() as u32;
        for op in &mut scene.ops {
            match op {
                UiOp::Rect { rect, .. } => {
                    for v in rect {
                        *v *= s;
                    }
                }
                UiOp::Circle { center, radius, .. } => {
                    center[0] *= s;
                    center[1] *= s;
                    *radius *= s;
                }
                UiOp::GlyphRun {
                    font_size, glyphs, ..
                } => {
                    *font_size *= s;
                    for g in glyphs {
                        g.x *= s;
                        g.y *= s;
                    }
                }
            }
        }
        let texture = renderer.render_ui(scene)?;
        Ok(TextureBuffer::from_texture(
            renderer,
            texture,
            scale,
            Transform::Normal,
            Vec::new(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyphs(text: &Text) -> Vec<&UiGlyph> {
        text.paint
            .scene
            .ops
            .iter()
            .flat_map(|op| match op {
                UiOp::GlyphRun { glyphs, .. } => glyphs.iter().collect::<Vec<_>>(),
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn nested_markup_ranges_are_utf8_bytes_and_restore_parent_style() {
        let (text, spans) = parse_markup(
            "é<span fgcolor='#336699' bgcolor='#2C2C2C' face='mono' letter_spacing='5000'>界<b>ß</b>z</span>!"
        ).unwrap();
        assert_eq!(text, "é界ßz!");
        let expected = [0..2, 2..5, 5..7, 7..8, 8..9];
        assert_eq!(
            spans
                .iter()
                .map(|(range, _)| range.clone())
                .collect::<Vec<_>>(),
            expected
        );
        let parent = &spans[1].1;
        assert_eq!(parent.brush.foreground, [0.2, 0.4, 0.6, 1.]);
        assert_eq!(
            parent.brush.background,
            Some([44. / 255., 44. / 255., 44. / 255., 1.])
        );
        assert_eq!(parent.family.as_deref(), Some("monospace"));
        assert_eq!(parent.spacing, 5000. / 1024.);
        assert!(!parent.bold);
        assert!(spans[2].1.bold);
        assert_eq!(spans[2].1.brush, parent.brush);
        assert!(!spans[3].1.bold);
        assert_eq!(spans[4].1.brush, Brush::default());
        assert_eq!(spans[4].1.family, None);
    }

    #[test]
    fn markup_entities_and_invalid_markup_match_accessible_text_contract() {
        assert_eq!(plain_text("<b>A &amp; é</b>&lt;"), "A & é<");
        assert_eq!(plain_text("<b>unclosed"), "<b>unclosed");
        assert!(Text::markup("<b>unclosed", 14.).is_err());
        assert_eq!(
            color("#2EE02EE02EE0"),
            Some([12000. / 65535., 12000. / 65535., 12000. / 65535., 1.])
        );
    }

    #[test]
    fn text_contains_real_font_data_and_positioned_nonmissing_glyphs() {
        let text = Text::new("AV office café", 14.).unwrap();
        assert!(text.size().0 > text.size().1);
        assert!(!text.paint.scene.fonts.is_empty());
        for font in &text.paint.scene.fonts {
            assert!(font.data.len() > 1024);
            assert!(matches!(&font.data[..4], b"\0\x01\0\0" | b"OTTO" | b"ttcf"));
        }
        let glyphs = glyphs(&text);
        assert!(!glyphs.is_empty());
        assert!(glyphs
            .iter()
            .all(|g| g.id != 0 && g.x.is_finite() && g.y.is_finite()));
        assert!(glyphs.windows(2).any(|g| g[0].x != g[1].x));
    }

    #[test]
    fn line_spacing_moves_later_baselines_and_centering_moves_short_lines() {
        let left = Text::new("ii\nWWWWWW\nabc", 14.).unwrap();
        let spaced = Text::with_options(
            "ii\nWWWWWW\nabc",
            TextOptions {
                line_spacing: 7.,
                ..Default::default()
            },
            false,
        )
        .unwrap();
        assert_eq!(spaced.size().0, left.size().0);
        assert_eq!(spaced.size().1, left.size().1 + 14);
        let left_g = glyphs(&left);
        let spaced_g = glyphs(&spaced);
        assert_eq!(left_g.len(), spaced_g.len());
        let baseline = left_g[0].y;
        let mut last_y = baseline;
        let mut line = 0;
        for (a, b) in left_g.iter().zip(spaced_g) {
            if a.y != last_y {
                line += 1;
                last_y = a.y;
            }
            assert!((b.y - a.y - line as f32 * 7.).abs() < 0.001);
            assert_eq!(a.x, b.x);
        }
        assert_eq!(line, 2);
        let center = Text::with_options(
            "ii\nWWWWWW\nabc",
            TextOptions {
                align: TextAlign::Center,
                ..Default::default()
            },
            false,
        )
        .unwrap();
        assert_eq!(center.size(), left.size());
        assert!(glyphs(&center)[0].x > left_g[0].x + 10.);
    }

    #[test]
    fn single_line_preserves_styled_unicode_ranges() {
        let one = Text::with_options(
            "é\n<b>界</b>\rß",
            TextOptions {
                single_line: true,
                ..Default::default()
            },
            true,
        )
        .unwrap();
        let expected = Text::markup("é <b>界</b> ß", 14.).unwrap();
        assert_eq!(one.size(), expected.size());
        let a = glyphs(&one);
        let b = glyphs(&expected);
        assert_eq!(a.len(), b.len());
        for (a, b) in a.iter().zip(b) {
            assert_eq!((a.id, a.x, a.y), (b.id, b.x, b.y));
        }
        assert!(one.size().1 < Text::markup("é\n<b>界</b>\rß", 14.).unwrap().size().1);
    }

    #[test]
    fn repeated_text_placements_share_fonts_and_remap_glyphs() {
        let first = Text::new("ordinary", 14.).unwrap();
        let second = Text::with_options(
            "bold",
            TextOptions {
                bold: true,
                ..Default::default()
            },
            false,
        )
        .unwrap();
        let mut paint = Paint::new(200, 100);
        paint.text(&first, 3., 5.);
        let fonts = paint.scene.fonts.len();
        let ops = paint.scene.ops.len();
        paint.text(&first, 13., 25.);
        assert_eq!(paint.scene.fonts.len(), fonts);
        for (a, b) in paint.scene.ops[..ops].iter().zip(&paint.scene.ops[ops..]) {
            if let (
                UiOp::GlyphRun {
                    font: fa,
                    glyphs: ga,
                    ..
                },
                UiOp::GlyphRun {
                    font: fb,
                    glyphs: gb,
                    ..
                },
            ) = (a, b)
            {
                assert_eq!(fa, fb);
                for (a, b) in ga.iter().zip(gb) {
                    assert_eq!(a.id, b.id);
                    assert!((b.x - a.x - 10.).abs() < 0.001);
                    assert!((b.y - a.y - 20.).abs() < 0.001);
                }
            }
        }
        paint.text(&second, 0., 50.);
        assert!(paint.scene.ops.iter().all(|op| match op {
            UiOp::GlyphRun { font, .. } => (*font as usize) < paint.scene.fonts.len(),
            _ => true,
        }));
        assert_eq!(
            paint
                .scene
                .fonts
                .iter()
                .enumerate()
                .filter(|(i, f)| {
                    paint.scene.fonts[..*i]
                        .iter()
                        .any(|old| old.index == f.index && old.data == f.data)
                })
                .count(),
            0
        );
    }

    #[test]
    #[ignore = "requires Vulkan and system sans/mono fonts; set NIRI_UI_GRID_PNG to capture"]
    fn affected_ui_visual_grid() -> anyhow::Result<()> {
        use niri_config::{Config, ModKey};
        use smithay::backend::allocator::Fourcc;
        use smithay::backend::renderer::{ExportMem as _, Texture as _};
        use smithay::utils::{Buffer, Rectangle};

        use crate::gpu::client::{GpuClient, Mode};

        let scale = std::env::var("NIRI_UI_GRID_SCALE")
            .ok()
            .map(|s| s.parse::<f64>().unwrap())
            .unwrap_or(1.);
        // This rendering test runs with a full DejaVu font configuration; verify our
        // helper actually resolves the generic family, rather than naming it literally.
        let mono = |text| {
            Text::with_options(
                text,
                TextOptions {
                    family: "monospace".to_owned(),
                    ..Default::default()
                },
                false,
            )
            .unwrap()
        };
        assert_eq!(mono("iiii").size().0, mono("WWWW").size().0);
        let config = Config::parse_mem(
            r##"binds {
            Mod+H hotkey-overlay-title="<span fgcolor='#80C0FF'><b>Show</b></span> <i>Hotkeys</i> &amp; Help" {
                show-hotkey-overlay;
            }
        }"##,
        )?;
        let scenes = [
            (
                "Exit confirmation",
                super::super::exit_confirm_dialog::test_paint()?,
            ),
            (
                "Configuration error",
                super::super::config_error_notification::test_paint(scale, None)?,
            ),
            (
                "Created configuration / Unicode path",
                super::super::config_error_notification::test_paint(
                    scale,
                    Some(std::path::Path::new("/home/é/config.kdl")),
                )?,
            ),
            (
                "Screen / microphone / camera sharing",
                super::super::cast_indicator::test_paint(
                    scale,
                    "Screen is being shared\nMicrophone: browser\nCamera: meeting",
                )?,
            ),
            (
                "Screenshot / pointer shown",
                super::super::screenshot_ui::test_paint(scale, true)?,
            ),
            (
                "Screenshot / pointer hidden",
                super::super::screenshot_ui::test_paint(scale, false)?,
            ),
            (
                "MRU scope / output selected",
                super::super::mru::test_paint(scale)?,
            ),
            (
                "Hotkey overlay",
                super::super::hotkey_overlay::test_paint(&config, ModKey::Super, scale)?,
            ),
        ];
        let col_width = scenes.iter().map(|(_, p)| p.scene.width).max().unwrap() as i32 + 32;
        let mut heights = [16i32; 2];
        let mut locations = Vec::new();
        for (i, (_, p)) in scenes.iter().enumerate() {
            let col = i % 2;
            locations.push((16 + col as i32 * col_width, heights[col] + 26));
            heights[col] += p.scene.height as i32 + 58;
        }
        let mut grid = Paint::new(col_width * 2, *heights.iter().max().unwrap());
        grid.fill([0.04, 0.04, 0.04, 1.]);
        for ((label, paint), &(x, y)) in scenes.iter().zip(&locations) {
            grid.text(&Text::new(label, 14.)?, x as f32, (y - 22) as f32);
            // Merge only in this diagnostic test, keeping production scene builders private.
            let offset = grid.scene.fonts.len() as u32;
            grid.scene.fonts.extend(paint.scene.fonts.clone());
            for mut op in paint.scene.ops.clone() {
                match &mut op {
                    UiOp::Rect { rect, .. } => {
                        rect[0] += x as f32;
                        rect[1] += y as f32;
                    }
                    UiOp::Circle { center, .. } => {
                        center[0] += x as f32;
                        center[1] += y as f32;
                    }
                    UiOp::GlyphRun { font, glyphs, .. } => {
                        *font += offset;
                        for g in glyphs {
                            g.x += x as f32;
                            g.y += y as f32;
                        }
                    }
                }
                grid.scene.ops.push(op);
            }
        }
        let mut renderer = RemoteRenderer::new(GpuClient::spawn_thread(Mode::Headless)?);
        let buffer = grid.render(&mut renderer, scale)?;
        let texture = buffer.texture();
        let size = texture.size();
        let mapping = renderer.copy_texture(
            texture,
            Rectangle::<i32, Buffer>::from_size(size),
            Fourcc::Abgr8888,
        )?;
        let pixels = renderer.map_texture(&mapping)?.to_vec();
        assert_eq!(pixels.len(), size.w as usize * size.h as usize * 4);
        let pixel = |x: i32, y: i32| -> [u8; 4] {
            let x = (x as f64 * scale).floor() as i32;
            let y = (y as f64 * scale).floor() as i32;
            let i = ((y * size.w + x) * 4) as usize;
            pixels[i..i + 4].try_into().unwrap()
        };
        // Independent palette/geometry checks on actual owner-generated scenes.
        let (ex, ey) = locations[0];
        assert_eq!(pixel(ex + 1, ey + 1), [255, 77, 77, 255], "exit border");
        let (cx, cy) = locations[3];
        assert_eq!(
            pixel(cx + 13, cy + scenes[3].1.scene.height as i32 / 2),
            [255, 64, 64, 255],
            "red capture dot"
        );
        let (sx, sy) = locations[4];
        let circle_y = sy + scenes[4].1.scene.height as i32 / 2;
        assert_eq!(pixel(sx + 24, circle_y), [255; 4], "capture button center");
        let ring = pixel(sx + 37, circle_y);
        // The two-pixel circular ring is antialiased at its curved boundaries.
        assert!(
            ring[0] < 80 && ring[0] == ring[1] && ring[1] == ring[2] && ring[3] == 255,
            "capture button dark ring: {ring:?}"
        );
        // Actual text must paint appreciable white foreground, not just panel shapes.
        let mut white = 0;
        for y in ey + 16..ey + scenes[0].1.scene.height as i32 - 16 {
            for x in ex + 16..ex + scenes[0].1.scene.width as i32 - 16 {
                let p = pixel(x, y);
                white += usize::from(p[0] > 220 && p[1] > 220 && p[2] > 220 && p[3] == 255);
            }
        }
        assert!(white > 100, "exit text glyphs are visible: {white}");
        if let Some(path) = std::env::var_os("NIRI_UI_GRID_PNG") {
            // Every panel and the diagnostic background are opaque.
            let mut encoder =
                png::Encoder::new(std::fs::File::create(path)?, size.w as u32, size.h as u32);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.write_header()?.write_image_data(&pixels)?;
        }
        Ok(())
    }
    #[test]
    fn styled_trailing_spaces_fit_the_measured_text_width() {
        assert_eq!(Text::new("", 14.).unwrap().size().0, 0);
        let text = Text::markup("<span face='mono' bgcolor='#2C2C2C'> key </span>", 14.).unwrap();
        let background_right = text
            .paint
            .scene
            .ops
            .iter()
            .filter_map(|op| match op {
                UiOp::Rect { rect, .. } => Some(rect[0] + rect[2]),
                _ => None,
            })
            .fold(0f32, f32::max);
        assert!(background_right > 0.);
        assert!(
            Text::markup("<span face='mono' bgcolor='#2C2C2C'>key</span>", 14.)
                .unwrap()
                .size()
                .0
                < text.size().0
        );
        assert!(
            background_right <= text.size().0 as f32,
            "styled trailing space extends past measured width: {background_right} > {}",
            text.size().0
        );
    }
}
