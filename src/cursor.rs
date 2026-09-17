use std::cell::RefCell;
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::rc::Rc;

use smithay::input::pointer::{CursorIcon, CursorImageStatus, CursorImageSurfaceData};
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Physical, Point, Transform};
use smithay::wayland::compositor::with_states;
use xcursor::CursorTheme;

use crate::gpu::protocol::CursorFrameDesc;
use crate::gpu::remote::{GpuHandle, RemoteTexture};
use crate::render_helpers::texture::TextureBuffer;

type XCursorCache = HashMap<(CursorIcon, i32), Option<Rc<XCursor>>>;

/// Named cursors, loaded and uploaded by the GPU process.
///
/// The core never reads theme files or pixels; it only keeps the frame geometry and a texture
/// handle per frame.
pub struct CursorManager {
    theme: String,
    /// Where icon files live; the core opens them and hands the fd to the GPU process.
    xcursor: CursorTheme,
    size: u8,
    gpu: Option<GpuHandle>,
    current_cursor: CursorImageStatus,
    named_cursor_cache: RefCell<XCursorCache>,
}

impl CursorManager {
    pub fn new(theme: &str, size: u8, gpu: Option<GpuHandle>) -> Self {
        Self::ensure_env(theme, size);

        Self {
            theme: theme.to_owned(),
            xcursor: CursorTheme::load(theme),
            size,
            gpu,
            current_cursor: CursorImageStatus::default_named(),
            named_cursor_cache: Default::default(),
        }
    }

    /// Reload the cursor theme.
    pub fn reload(&mut self, theme: &str, size: u8) {
        Self::ensure_env(theme, size);
        self.theme = theme.to_owned();
        self.xcursor = CursorTheme::load(theme);
        self.size = size;
        self.named_cursor_cache.get_mut().clear();
    }

    /// Forgets loaded cursors, e.g. after the GPU renderer was recreated.
    pub fn clear_cache(&mut self) {
        self.named_cursor_cache.get_mut().clear();
    }

    /// Checks if the cursor WlSurface is alive, and if not, cleans it up.
    pub fn check_cursor_image_surface_alive(&mut self) {
        if let CursorImageStatus::Surface(surface) = &self.current_cursor {
            if !surface.alive() {
                self.current_cursor = CursorImageStatus::default_named();
            }
        }
    }

    /// Get the current rendering cursor.
    pub fn get_render_cursor(&self, scale: i32) -> RenderCursor {
        match self.current_cursor.clone() {
            CursorImageStatus::Hidden => RenderCursor::Hidden,
            CursorImageStatus::Surface(surface) => {
                let hotspot = with_states(&surface, |states| {
                    states
                        .data_map
                        .get::<CursorImageSurfaceData>()
                        .unwrap()
                        .lock()
                        .unwrap()
                        .hotspot
                });

                RenderCursor::Surface { hotspot, surface }
            }
            CursorImageStatus::Named(icon) => self.get_render_cursor_named(icon, scale),
        }
    }

    fn get_render_cursor_named(&self, icon: CursorIcon, scale: i32) -> RenderCursor {
        if let Some(cursor) = self.get_cursor_with_name(icon, scale) {
            return RenderCursor::Named {
                icon,
                scale,
                cursor,
            };
        }

        match self.get_default_cursor(scale) {
            Some(cursor) => RenderCursor::Named {
                icon: Default::default(),
                scale,
                cursor,
            },
            // No GPU renderer yet; nothing to draw the cursor with anyway.
            None => RenderCursor::Hidden,
        }
    }

    pub fn is_current_cursor_animated(&self, scale: i32) -> bool {
        match &self.current_cursor {
            CursorImageStatus::Hidden => false,
            CursorImageStatus::Surface(_) => false,
            CursorImageStatus::Named(icon) => self
                .get_cursor_with_name(*icon, scale)
                .or_else(|| self.get_default_cursor(scale))
                .is_some_and(|cursor| cursor.is_animated_cursor()),
        }
    }

    /// Get named cursor for the given `icon` and `scale`.
    ///
    /// Returns `None` (without caching) while the GPU process has no renderer.
    pub fn get_cursor_with_name(&self, icon: CursorIcon, scale: i32) -> Option<Rc<XCursor>> {
        let gpu = self.gpu.as_ref().filter(|gpu| gpu.is_ready())?;

        self.named_cursor_cache
            .borrow_mut()
            .entry((icon, scale))
            .or_insert_with_key(|(icon, scale)| {
                let size = self.size as i32 * scale;

                // Alternative names account for non-compliant themes.
                let mut names = vec![icon.name().to_owned()];
                names.extend(icon.alt_names().iter().map(|name| name.to_string()));

                // The default cursor must always have a fallback.
                let fallback = *icon == CursorIcon::Default;

                let file = names.iter().find_map(|name| {
                    let path = self.xcursor.load_icon(name)?;
                    match File::open(&path) {
                        Ok(file) => Some(file),
                        Err(err) => {
                            warn!("error opening cursor icon {path:?}: {err}");
                            None
                        }
                    }
                });
                if file.is_none() && !fallback {
                    warn!("no icon {} in cursor theme {}", icon.name(), self.theme);
                    return None;
                }

                match gpu.load_cursor(file.as_ref(), size, fallback) {
                    Ok(frames) => Some(Rc::new(XCursor::new(gpu, frames, *scale))),
                    Err(err) => {
                        warn!("error loading xcursor {}@{size}: {err:?}", icon.name());
                        None
                    }
                }
            })
            .clone()
    }

    /// Get default cursor. `None` only while the GPU process has no renderer.
    pub fn get_default_cursor(&self, scale: i32) -> Option<Rc<XCursor>> {
        self.get_cursor_with_name(CursorIcon::Default, scale)
    }

    /// Currently used cursor_image as a cursor provider.
    pub fn cursor_image(&self) -> &CursorImageStatus {
        &self.current_cursor
    }

    /// Set new cursor image provider.
    pub fn set_cursor_image(&mut self, cursor: CursorImageStatus) {
        self.current_cursor = cursor;
    }

    /// Set the common XCURSOR env variables.
    fn ensure_env(theme: &str, size: u8) {
        env::set_var("XCURSOR_THEME", theme);
        env::set_var("XCURSOR_SIZE", size.to_string());
    }
}

/// The cursor prepared for renderer.
pub enum RenderCursor {
    Hidden,
    Surface {
        hotspot: Point<i32, Logical>,
        surface: WlSurface,
    },
    Named {
        icon: CursorIcon,
        scale: i32,
        cursor: Rc<XCursor>,
    },
}

/// One frame of a named cursor: geometry here, pixels in the GPU process.
pub struct CursorFrame {
    pub width: u32,
    pub height: u32,
    pub xhot: u32,
    pub yhot: u32,
    /// Milliseconds this frame is shown.
    pub delay: u32,
    pub buffer: TextureBuffer<RemoteTexture>,
}

// The XCursorBuffer implementation is inspired by `wayland-rs`, thus provided under MIT license.

/// The state of the `NamedCursor`.
pub struct XCursor {
    frames: Vec<CursorFrame>,
    /// The total duration of the animation.
    animation_duration: u32,
}

impl XCursor {
    fn new(gpu: &GpuHandle, frames: Vec<(CursorFrameDesc, RemoteTexture)>, scale: i32) -> Self {
        let frames: Vec<_> = frames
            .into_iter()
            .map(|(desc, texture)| CursorFrame {
                width: desc.width,
                height: desc.height,
                xhot: desc.xhot,
                yhot: desc.yhot,
                delay: desc.delay,
                buffer: TextureBuffer::from_remote_texture(
                    gpu.context_id(),
                    texture,
                    f64::from(scale),
                    Transform::Normal,
                    Vec::new(),
                ),
            })
            .collect();
        let animation_duration = frames.iter().fold(0, |acc, frame| acc + frame.delay);
        Self {
            frames,
            animation_duration,
        }
    }

    /// Given a time, calculate which frame to show, and how much time remains until the next frame.
    ///
    /// Time will wrap, so if for instance the cursor has an animation lasting 100ms,
    /// then calling this function with 5ms and 105ms as input gives the same output.
    pub fn frame(&self, mut millis: u32) -> (usize, &CursorFrame) {
        if self.animation_duration == 0 {
            return (0, &self.frames[0]);
        }

        millis %= self.animation_duration;

        let mut res = 0;
        for (i, img) in self.frames.iter().enumerate() {
            if millis < img.delay {
                res = i;
                break;
            }
            millis -= img.delay;
        }

        (res, &self.frames[res])
    }

    /// Get the frames for the given `XCursor`.
    pub fn frames(&self) -> &[CursorFrame] {
        &self.frames
    }

    /// Check whether the cursor is animated.
    pub fn is_animated_cursor(&self) -> bool {
        self.frames.len() > 1
    }

    /// Get hotspot for the given `frame`.
    pub fn hotspot(frame: &CursorFrame) -> Point<i32, Physical> {
        (frame.xhot as i32, frame.yhot as i32).into()
    }
}
