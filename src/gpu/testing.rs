//! Shared smoke scenario for the in-thread unit test and the spawned-process
//! integration test.

use std::os::fd::AsFd;

use anyhow::Context;
use smithay::backend::allocator::Fourcc;

use super::client::GpuClient;
use super::protocol::ShmDesc;
use super::scene::{BufferId, Kind, Node, NodeId, Rect, Scene, Transform};

pub const SURFACE: i32 = 64;
pub const CANVAS: i32 = 128;
/// Deliberately not `width * 4`, so row repacking is exercised.
pub const STRIDE: i32 = SURFACE * 4 + 16;

/// Left half red, right half blue, opaque. ARGB8888 little-endian is B,G,R,A.
pub fn make_pattern_pool() -> anyhow::Result<std::fs::File> {
    let size = (STRIDE * SURFACE) as usize;
    let fd = rustix::fs::memfd_create(
        "niri-gpu-test",
        rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
    )?;
    let file = std::fs::File::from(fd);
    file.set_len(size as u64)?;
    {
        let mut map = unsafe { memmap2::MmapOptions::new().len(size).map_mut(&file) }?;
        for y in 0..SURFACE as usize {
            for x in 0..SURFACE as usize {
                let i = y * STRIDE as usize + x * 4;
                let px: [u8; 4] = if x < SURFACE as usize / 2 {
                    [0, 0, 255, 255]
                } else {
                    [255, 0, 0, 255]
                };
                map[i..i + 4].copy_from_slice(&px);
            }
        }
        map.flush()?;
    }
    rustix::fs::fcntl_add_seals(file.as_fd(), rustix::fs::SealFlags::SHRINK)?;
    Ok(file)
}

pub fn smoke_scene() -> Scene {
    Scene {
        size: (CANVAS, CANVAS),
        scale: 1.0,
        transform: Transform::Normal,
        nodes: vec![
            Node::Surface {
                id: NodeId(2),
                commit: 0,
                buffer: BufferId(1),
                location: (32.0, 32.0),
                buffer_scale: 1,
                transform: Transform::Normal,
                alpha: 1.0,
                src: None,
                size: None,
                opaque: vec![Rect::new(0, 0, SURFACE, SURFACE)],
                kind: Kind::Unspecified,
            },
            Node::SolidColor {
                id: NodeId(1),
                commit: 0,
                geometry: Rect::new(0, 0, CANVAS, CANVAS),
                color: [0.0, 1.0, 0.0, 1.0],
            },
        ],
    }
}

/// Registers the pattern buffer, renders the smoke scene, checks three pixels.
pub fn run_smoke(client: &mut GpuClient) -> anyhow::Result<()> {
    let pool = make_pattern_pool()?;
    client.register_shm(
        BufferId(1),
        pool.as_fd(),
        ShmDesc {
            size: (STRIDE * SURFACE) as usize,
            offset: 0,
            stride: STRIDE,
            width: SURFACE,
            height: SURFACE,
            format: Fourcc::Argb8888 as u32,
        },
    )?;

    // Abgr8888 in memory is R,G,B,A.
    let image = client.render_to_image(smoke_scene(), Fourcc::Abgr8888)?;
    anyhow::ensure!(image.width == CANVAS && image.height == CANVAS);
    anyhow::ensure!(image.pixels.len() == (CANVAS * CANVAS * 4) as usize);

    let px = |x: i32, y: i32| -> [u8; 4] {
        let i = ((y * CANVAS + x) * 4) as usize;
        image.pixels[i..i + 4].try_into().unwrap()
    };
    let close = |a: [u8; 4], b: [u8; 4]| a.iter().zip(b).all(|(p, q)| (*p as i32 - q as i32).abs() <= 2);

    anyhow::ensure!(close(px(10, 10), [0, 255, 0, 255]), "background: {:?}", px(10, 10));
    anyhow::ensure!(close(px(40, 40), [255, 0, 0, 255]), "left half: {:?}", px(40, 40));
    anyhow::ensure!(close(px(80, 40), [0, 0, 255, 255]), "right half: {:?}", px(80, 40));
    anyhow::ensure!(close(px(120, 120), [0, 255, 0, 255]), "corner: {:?}", px(120, 120));

    // Repaint the pool and make sure UpdateShm picks it up.
    {
        let size = (STRIDE * SURFACE) as usize;
        let mut map = unsafe { memmap2::MmapOptions::new().len(size).map_mut(&pool) }?;
        for chunk in map.chunks_exact_mut(4) {
            chunk.copy_from_slice(&[0, 255, 255, 255]); // yellow in B,G,R,A
        }
        map.flush()?;
    }
    client.update_shm(BufferId(1), vec![Rect::new(0, 0, SURFACE, SURFACE)])?;
    let image = client.render_to_image(smoke_scene(), Fourcc::Abgr8888)?;
    let i = ((40 * CANVAS + 40) * 4) as usize;
    let p: [u8; 4] = image.pixels[i..i + 4].try_into().unwrap();
    anyhow::ensure!(close(p, [255, 255, 0, 255]), "after update: {p:?}");

    client.destroy_buffer(BufferId(1)).context("destroy")?;
    Ok(())
}
