//! Blur options; the blur itself runs in the GPU process (see `gpu::gl::blur`).

use crate::gpu::protocol::BlurParams;

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct BlurOptions {
    pub passes: u8,
    pub offset: f64,
}

impl From<niri_config::Blur> for BlurOptions {
    fn from(config: niri_config::Blur) -> Self {
        Self {
            passes: config.passes,
            offset: config.offset,
        }
    }
}

impl From<BlurOptions> for BlurParams {
    fn from(options: BlurOptions) -> Self {
        Self {
            passes: options.passes,
            offset: options.offset,
        }
    }
}
