//! Xcursor icon parsing for the GPU process. The core finds and opens the theme file (this
//! process has no filesystem access) and only learns frame sizes, hotspots and delays back.

use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;

use anyhow::{anyhow, ensure, Context};
use xcursor::parser::{parse_xcursor, Image};

use super::protocol::MAX_CURSOR_FRAMES;

/// Some default looking `left_ptr` icon.
static FALLBACK_CURSOR_DATA: &[u8] = include_bytes!("../../resources/cursor.rgba");

/// Parses `icon`, keeping the frames closest to `size`.
pub fn load_cursor(icon: Option<OwnedFd>, size: i32, fallback: bool) -> anyhow::Result<Vec<Image>> {
    let res = match icon {
        Some(fd) => parse_icon(File::from(fd), size),
        None => Err(anyhow!("no icon file")),
    };

    match res {
        Ok(images) => Ok(images),
        Err(err) if fallback => {
            warn!("error loading xcursor @{size}, using fallback: {err:?}");
            Ok(fallback_cursor())
        }
        Err(err) => Err(err),
    }
}

fn parse_icon(mut file: File, size: i32) -> anyhow::Result<Vec<Image>> {
    let _span = tracy_client::span!("parse_icon");

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
