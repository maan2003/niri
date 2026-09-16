use std::error::Error;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::{
    Bind, ExportMem, ImportAll, ImportMem, Offscreen, Renderer, Texture,
};

use crate::gpu::remote::{RemoteError, RemoteFrame, RemoteRenderer, RemoteTexture};

/// Trait with our main renderer requirements to save on the typing.
pub trait NiriRenderer:
    ImportAll
    + ImportMem
    + ExportMem
    + Bind<Dmabuf>
    + Offscreen<RemoteTexture>
    + Renderer<TextureId = Self::NiriTextureId, Error = Self::NiriError>
    + AsRemoteRenderer
{
    // Associated types to work around the instability of associated type bounds.
    type NiriTextureId: Texture + Clone + Send + 'static;
    type NiriError: Error + Send + Sync + From<RemoteError> + 'static;
}

impl<R> NiriRenderer for R
where
    R: ImportAll + ImportMem + ExportMem + Bind<Dmabuf> + Offscreen<RemoteTexture>,
    R: AsRemoteRenderer,
    R::TextureId: Texture + Clone + Send + 'static,
    R::Error: Error + Send + Sync + From<RemoteError> + 'static,
{
    type NiriTextureId = R::TextureId;
    type NiriError = R::Error;
}

/// Trait for getting the underlying `RemoteRenderer`.
pub trait AsRemoteRenderer {
    fn as_remote_renderer(&mut self) -> &mut RemoteRenderer;
}

impl AsRemoteRenderer for RemoteRenderer {
    fn as_remote_renderer(&mut self) -> &mut RemoteRenderer {
        self
    }
}

/// Trait for getting the underlying `RemoteFrame`.
pub trait AsRemoteFrame<'frame, 'buffer>
where
    Self: 'frame,
{
    fn as_remote_frame(&mut self) -> &mut RemoteFrame<'frame, 'buffer>;
}

impl<'frame, 'buffer> AsRemoteFrame<'frame, 'buffer> for RemoteFrame<'frame, 'buffer> {
    fn as_remote_frame(&mut self) -> &mut RemoteFrame<'frame, 'buffer> {
        self
    }
}
