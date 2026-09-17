//! Scene nodes from core frames, with per-node damage tracking.
//!
//! The core describes every frame as a [`SceneFrame`]: nodes bottom to top, each carrying the
//! damage smithay computed for that element since the previous frame. Here each node becomes
//! a real smithay element, so `DrmCompositor` and `OutputDamageTracker` see per-element
//! damage, opaque regions, kinds and (for plain buffer copies) the underlying buffer for
//! direct scanout. The tracks keep enough damage history for buffer ages.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::mem;

use smithay::backend::allocator::dmabuf::Dmabuf;
use smithay::backend::renderer::element::memory::MemoryBuffer;
use smithay::backend::renderer::element::{Element, Id, Kind, RenderElement, UnderlyingStorage};
use smithay::backend::renderer::gles::{GlesError, GlesFrame, GlesRenderer};
use smithay::backend::renderer::utils::{CommitCounter, DamageSet, OpaqueRegions};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Buffer, Physical, Rectangle, Scale, Transform};

use super::convert;
use super::draw::draw_ops;
use super::exec::Tables;
use super::protocol::{ElementKind, Node, Op};

/// How many past frames of damage we remember for buffer ages.
pub const DAMAGE_HISTORY: usize = 8;

pub struct NodeTrack {
    pub id: Id,
    commit: CommitCounter,
    /// Element-relative damage of recent commits, newest last.
    history: VecDeque<Vec<Rectangle<i32, Physical>>>,
    seen: bool,
}

/// Damage tracking state for one render target, keyed by the core's node ids.
#[derive(Default)]
pub struct NodeTracks {
    generation: u64,
    tracks: HashMap<u64, NodeTrack>,
}

impl NodeTracks {
    /// Advances per-node commits by the damage the core reported and forgets nodes that are
    /// not in this frame. A new `generation` drops all history first.
    pub fn update(&mut self, generation: u64, nodes: &[Node]) {
        if generation != self.generation {
            self.generation = generation;
            self.tracks.clear();
        }
        for node in nodes {
            let track = self.tracks.entry(node.id).or_insert_with(|| NodeTrack {
                id: Id::new(),
                commit: CommitCounter::default(),
                history: VecDeque::new(),
                seen: false,
            });
            track.seen = true;
            let geometry = convert::to_rect::<Physical>(node.geometry);
            let damage = match &node.damage {
                None => Some(vec![Rectangle::from_size(geometry.size)]),
                Some(d) if d.is_empty() => None,
                Some(d) => Some(
                    d.iter()
                        .map(|r| {
                            let mut r = convert::to_rect::<Physical>(*r);
                            r.loc -= geometry.loc;
                            r
                        })
                        .collect(),
                ),
            };
            if let Some(damage) = damage {
                track.commit.increment();
                track.history.push_back(damage);
                while track.history.len() > DAMAGE_HISTORY {
                    track.history.pop_front();
                }
            }
        }
        self.tracks.retain(|_, t| mem::take(&mut t.seen));
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
    }

    /// smithay element id to core node id.
    pub fn id_map(&self) -> HashMap<Id, u64> {
        self.tracks
            .iter()
            .map(|(k, t)| (t.id.clone(), *k))
            .collect()
    }
}

/// Builds smithay elements for `nodes`, top to bottom (the core sends bottom to top).
/// `tracks` must have been updated with these nodes.
pub fn scene_elements<'a>(
    tracks: &'a NodeTracks,
    nodes: &'a [Node],
    storages: &'a [Option<Storage>],
    tables: &'a RefCell<Tables>,
) -> Vec<SceneElement<'a>> {
    let mut elements: Vec<SceneElement> = nodes
        .iter()
        .zip(storages)
        .map(|(node, storage)| SceneElement {
            track: &tracks.tracks[&node.id],
            node,
            storage: storage.as_ref(),
            tables,
        })
        .collect();
    elements.reverse();
    elements
}

pub fn node_storages(tables: &Tables, nodes: &[Node]) -> Vec<Option<Storage>> {
    nodes
        .iter()
        .map(|node| node_storage(tables, node))
        .collect()
}

/// What a node is made of, when it is a plain copy of one buffer. Lets the DRM compositor
/// scan the buffer out directly or copy it to the cursor plane.
pub enum Storage {
    Dmabuf(Dmabuf),
    Memory(MemoryBuffer),
}

fn node_storage(tables: &Tables, node: &Node) -> Option<Storage> {
    let [Op::Texture {
        texture,
        src,
        dst,
        transform,
        alpha,
        program,
        uniforms,
        ..
    }] = node.draw.as_slice()
    else {
        return None;
    };
    // Anything but an untinted 1:1 copy of the whole element must be rendered.
    if !node.capture.is_empty()
        || *alpha != 1.0
        || program.is_some()
        || !uniforms.is_empty()
        || *src != node.src
        || *dst != node.geometry
        || *transform != node.transform
    {
        return None;
    }
    if let Some(dmabuf) = tables.dmabufs.get(texture) {
        return Some(Storage::Dmabuf(dmabuf.clone()));
    }
    if let Some(mem) = tables.memory.get(texture) {
        return Some(Storage::Memory(mem.clone()));
    }
    None
}

/// One core node as a smithay element. Damage and opaque regions come from the core, so the
/// DRM compositor tracks and culls exactly like in-process smithay would.
pub struct SceneElement<'a> {
    track: &'a NodeTrack,
    node: &'a Node,
    storage: Option<&'a Storage>,
    tables: &'a RefCell<Tables>,
}

impl Element for SceneElement<'_> {
    fn id(&self) -> &Id {
        &self.track.id
    }

    fn current_commit(&self) -> CommitCounter {
        self.track.commit
    }

    fn src(&self) -> Rectangle<f64, Buffer> {
        convert::to_rect_f64(self.node.src)
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        convert::to_rect(self.node.geometry)
    }

    fn transform(&self) -> Transform {
        convert::to_transform(self.node.transform)
    }

    fn damage_since(
        &self,
        scale: Scale<f64>,
        commit: Option<CommitCounter>,
    ) -> DamageSet<i32, Physical> {
        let full = || DamageSet::from_slice(&[Rectangle::from_size(self.geometry(scale).size)]);
        let Some(distance) = self.track.commit.distance(commit) else {
            return full();
        };
        if distance == 0 {
            return DamageSet::default();
        }
        if distance > self.track.history.len() {
            return full();
        }
        let rects: Vec<_> = self
            .track
            .history
            .iter()
            .rev()
            .take(distance)
            .flatten()
            .copied()
            .collect();
        DamageSet::from_slice(&rects)
    }

    fn opaque_regions(&self, scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        let loc = self.geometry(scale).loc;
        let rects: Vec<_> = self
            .node
            .opaque
            .iter()
            .map(|r| {
                let mut r = convert::to_rect::<Physical>(*r);
                r.loc -= loc;
                r
            })
            .collect();
        OpaqueRegions::from_slice(&rects)
    }

    fn kind(&self) -> Kind {
        match self.node.kind {
            ElementKind::Cursor => Kind::Cursor,
            ElementKind::ScanoutCandidate => Kind::ScanoutCandidate,
            ElementKind::Unspecified => Kind::Unspecified,
        }
    }

    fn is_framebuffer_effect(&self) -> bool {
        !self.node.capture.is_empty()
    }
}

impl RenderElement<GlesRenderer> for SceneElement<'_> {
    fn draw(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        dst: Rectangle<i32, Physical>,
        damage: &[Rectangle<i32, Physical>],
        _opaque_regions: &[Rectangle<i32, Physical>],
        _cache: Option<&UserDataMap>,
    ) -> Result<(), GlesError> {
        // Ops are in frame coordinates; damage arrives element-relative.
        let clip: Vec<_> = damage
            .iter()
            .map(|d| {
                let mut d = *d;
                d.loc += dst.loc;
                d
            })
            .collect();
        if let Err(err) = draw_ops(frame, self.tables, &self.node.draw, Some(&clip)) {
            warn!("error drawing node: {err:#}");
        }
        Ok(())
    }

    fn capture_framebuffer(
        &self,
        frame: &mut GlesFrame<'_, '_>,
        _src: Rectangle<f64, Buffer>,
        _dst: Rectangle<i32, Physical>,
        _cache: &UserDataMap,
    ) -> Result<(), GlesError> {
        if let Err(err) = draw_ops(frame, self.tables, &self.node.capture, None) {
            warn!("error capturing for node: {err:#}");
        }
        Ok(())
    }

    fn underlying_storage(&self, _renderer: &mut GlesRenderer) -> Option<UnderlyingStorage<'_>> {
        match self.storage? {
            Storage::Dmabuf(dmabuf) => Some(UnderlyingStorage::Dmabuf(dmabuf)),
            Storage::Memory(mem) => Some(UnderlyingStorage::Memory(mem)),
        }
    }
}

#[cfg(test)]
mod tests {
    use smithay::backend::allocator::Fourcc;
    use smithay::backend::renderer::damage::OutputDamageTracker;
    use smithay::backend::renderer::{Bind as _, Color32F, ExportMem as _, Offscreen as _};
    use smithay::utils::Size;

    use super::*;
    use crate::gpu::protocol::{Rect, Transform as PTransform};
    use crate::gpu::server::new_surfaceless_renderer;

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect<i32> {
        convert::rect(Rectangle::<i32, Physical>::new(
            (x, y).into(),
            (w, h).into(),
        ))
    }

    fn solid_node(
        id: u64,
        geometry: Rect<i32>,
        damage: Option<Vec<Rect<i32>>>,
        color: [f32; 4],
    ) -> Node {
        Node {
            id,
            src: convert::rect_f64(Rectangle::<f64, Buffer>::from_size(Size::from((
                geometry.w as f64,
                geometry.h as f64,
            )))),
            geometry,
            damage,
            opaque: vec![geometry],
            kind: ElementKind::Unspecified,
            transform: PTransform::Normal,
            capture: Vec::new(),
            draw: vec![Op::Solid {
                dst: geometry,
                color,
            }],
        }
    }

    /// Frame-space node damage must reach the op clipped and dst-relative: a node that
    /// changes color with partial damage repaints only the damaged part.
    #[test]
    fn node_damage_clips_ops_in_frame_space() {
        let Ok(mut renderer) = new_surfaceless_renderer() else {
            eprintln!("no EGL (llvmpipe) available, skipping");
            return;
        };
        let tables = RefCell::new(Tables::default());
        let size = Size::<i32, Physical>::from((64, 64));
        let mut texture = renderer
            .create_buffer(Fourcc::Abgr8888, Size::<i32, Buffer>::from((64, 64)))
            .unwrap();
        let mut tracker = OutputDamageTracker::new(size, 1.0, Transform::Normal);
        let mut tracks = NodeTracks::default();

        let red = [1., 0., 0., 1.];
        let blue = [0., 0., 1., 1.];
        let geometry = rect(10, 10, 40, 40);

        let mut render = |renderer: &mut GlesRenderer,
                          texture: &mut smithay::backend::renderer::gles::GlesTexture,
                          tracks: &mut NodeTracks,
                          generation: u64,
                          nodes: Vec<Node>,
                          age: usize| {
            tracks.update(generation, &nodes);
            let storages = node_storages(&tables.borrow(), &nodes);
            let elements = scene_elements(tracks, &nodes, &storages, &tables);
            let mut fb = renderer.bind(texture).unwrap();
            tracker
                .render_output(renderer, &mut fb, age, &elements, Color32F::TRANSPARENT)
                .unwrap();
        };
        let pixel = |renderer: &mut GlesRenderer, texture: &_, x: i32, y: i32| -> [u8; 4] {
            let mapping = renderer
                .copy_texture(
                    texture,
                    Rectangle::new((x, y).into(), (1, 1).into()),
                    Fourcc::Abgr8888,
                )
                .unwrap();
            renderer.map_texture(&mapping).unwrap()[..4]
                .try_into()
                .unwrap()
        };

        // New node: everything red.
        render(
            &mut renderer,
            &mut texture,
            &mut tracks,
            1,
            vec![solid_node(1, geometry, None, red)],
            0,
        );
        assert_eq!(pixel(&mut renderer, &texture, 15, 15), [255, 0, 0, 255]);
        assert_eq!(pixel(&mut renderer, &texture, 5, 5), [0, 0, 0, 0]);

        // Same node turns blue, but only a frame-space sub-rectangle is damaged.
        let damage = Some(vec![rect(30, 30, 10, 10)]);
        render(
            &mut renderer,
            &mut texture,
            &mut tracks,
            1,
            vec![solid_node(1, geometry, damage, blue)],
            1,
        );
        assert_eq!(
            pixel(&mut renderer, &texture, 35, 35),
            [0, 0, 255, 255],
            "damaged part"
        );
        assert_eq!(
            pixel(&mut renderer, &texture, 15, 15),
            [255, 0, 0, 255],
            "undamaged part"
        );

        // No damage reported: nothing repaints even though the color changed.
        render(
            &mut renderer,
            &mut texture,
            &mut tracks,
            1,
            vec![solid_node(1, geometry, Some(vec![]), red)],
            1,
        );
        assert_eq!(pixel(&mut renderer, &texture, 35, 35), [0, 0, 255, 255]);

        // New generation: history dropped, the node is fully redrawn.
        render(
            &mut renderer,
            &mut texture,
            &mut tracks,
            2,
            vec![solid_node(1, geometry, Some(vec![]), red)],
            1,
        );
        assert_eq!(pixel(&mut renderer, &texture, 35, 35), [255, 0, 0, 255]);
    }
}
