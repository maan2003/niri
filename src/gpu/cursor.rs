//! Xcursor theme loading for the GPU process. Theme files come from disk and are parsed here,
//! so the core never touches them; it only learns frame sizes, hotspots and delays.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;

use anyhow::{anyhow, ensure, Context};
use xcursor::parser::{parse_xcursor, Image};
use xcursor::CursorTheme;

use super::protocol::MAX_CURSOR_FRAMES;

/// Some default looking `left_ptr` icon.
static FALLBACK_CURSOR_DATA: &[u8] = include_bytes!("../../resources/cursor.rgba");

#[derive(Default)]
pub struct CursorThemes {
    themes: HashMap<String, CursorTheme>,
}

impl CursorThemes {
    /// Loads the first of `names` that exists, picking the frames closest to `size`.
    pub fn load(
        &mut self,
        theme: &str,
        names: &[String],
        size: i32,
        fallback: bool,
    ) -> anyhow::Result<Vec<Image>> {
        let theme = self
            .themes
            .entry(theme.to_owned())
            .or_insert_with(|| CursorTheme::load(theme));

        let mut res = Err(anyhow!("no cursor names given"));
        for name in names {
            res = load_xcursor(theme, name, size);
            if res.is_ok() {
                break;
            }
        }

        match res {
            Ok(images) => Ok(images),
            Err(err) if fallback => {
                warn!("error loading xcursor {names:?}@{size}, using fallback: {err:?}");
                Ok(fallback_cursor())
            }
            Err(err) => Err(err),
        }
    }
}

fn load_xcursor(theme: &CursorTheme, name: &str, size: i32) -> anyhow::Result<Vec<Image>> {
    let _span = tracy_client::span!("load_xcursor");

    let path = theme
        .load_icon(name)
        .ok_or_else(|| anyhow!("no icon {name}"))?;

    let mut file = File::open(path).context("error opening cursor icon file")?;
    let mut buf = vec![];
    file.read_to_end(&mut buf)
        .context("error reading cursor icon file")?;

    let mut images = parse_xcursor(&buf).context("error parsing cursor icon file")?;
    ensure!(!images.is_empty(), "cursor file has no images");

    let (width, height) = images
        .iter()
        .min_by_key(|image| (size - image.size as i32).abs())
        .map(|image| (image.width, image.height))
        .unwrap();

    images.retain(move |image| image.width == width && image.height == height);
    images.truncate(MAX_CURSOR_FRAMES as usize);

    Ok(images)
}

fn fallback_cursor() -> Vec<Image> {
    vec![Image {
        size: 32,
        width: 64,
        height: 64,
        xhot: 1,
        yhot: 1,
        delay: 0,
        pixels_rgba: Vec::from(FALLBACK_CURSOR_DATA),
        pixels_argb: vec![],
    }]
}
