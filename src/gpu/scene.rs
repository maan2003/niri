//! Scene elements replayed from core recordings, with per-element damage tracking.
//!
//! The core wraps every element it records in `BeginElement … EndElement` markers. Here each
//! segment becomes a real smithay element, so `DrmCompositor` and `OutputDamageTracker` see
//! per-element damage, opaque regions, kinds and (for plain buffer copies) the underlying
//! buffer for direct scanout.

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
use super::exec::{run_frame, Tables};
use super::protocol::{Command, ElementKind, ElementMeta};

/// How many past frames of damage we remember for buffer ages.
pub const DAMAGE_HISTORY: usize = 8;

pub struct ElementTrack {
    pub id: Id,
    commit: CommitCounter,
    /// Element-relative damage of recent commits, newest last.
    history: VecDeque<Vec<Rectangle<i32, Physical>>>,
    seen: bool,
}

/// Damage tracking state for one render target, keyed by the core's element ids.
#[derive(Default)]
pub struct ElementTracks {
    tracks: HashMap<u64, ElementTrack>,
}

impl ElementTracks {
    /// Advances per-element commits by the damage the core reported and forgets elements that
    /// are not in this frame.
    pub fn update(&mut self, segments: &[Segment<'_>]) {
        for seg in segments {
            let track = self
                .tracks
                .entry(seg.meta.id)
                .or_insert_with(|| ElementTrack {
                    id: Id::new(),
                    commit: CommitCounter::default(),
                    history: VecDeque::new(),
                    seen: false,
                });
            track.seen = true;
            let damage = match &seg.meta.damage {
                None => {
                    let size = convert::to_rect::<Physical>(seg.meta.geometry).size;
                    Some(vec![Rectangle::from_size(size)])
                }
                Some(d) if d.is_empty() => None,
                Some(d) => Some(convert::to_rects(d)),
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

    /// smithay element id to core element id.
    pub fn id_map(&self) -> HashMap<Id, u64> {
        self.tracks
            .iter()
            .map(|(k, t)| (t.id.clone(), *k))
            .collect()
    }
}

/// Builds smithay elements for `segments`, top to bottom (the core records bottom to top).
/// `tracks` must have been updated with these segments.
pub fn scene_elements<'a>(
    tracks: &'a ElementTracks,
    segments: &'a [Segment<'a>],
    storages: &'a [Option<Storage>],
    tables: &'a RefCell<Tables>,
) -> Vec<SceneElement<'a>> {
    let mut elements: Vec<SceneElement> = segments
        .iter()
        .zip(storages)
        .map(|(seg, storage)| SceneElement {
            track: &tracks.tracks[&seg.meta.id],
            meta: seg.meta,
            capture: seg.capture,
            draw: seg.draw,
            storage: storage.as_ref(),
            tables,
        })
        .collect();
    elements.reverse();
    elements
}

pub fn element_storages(tables: &Tables, segments: &[Segment<'_>]) -> Vec<Option<Storage>> {
    segments
        .iter()
        .map(|seg| element_storage(tables, seg))
        .collect()
}

pub struct Segment<'a> {
    pub meta: &'a ElementMeta,
    pub capture: &'a [Command],
    pub draw: &'a [Command],
}

/// What an element is made of, when it is a plain copy of one buffer. Lets the DRM compositor
/// scan the buffer out directly or copy it to the cursor plane.
pub enum Storage {
    Dmabuf(Dmabuf),
    Memory(MemoryBuffer),
}

fn element_storage(tables: &Tables, seg: &Segment<'_>) -> Option<Storage> {
    let [Command::DrawTexture {
        texture,
        src,
        dst,
        transform,
        alpha,
        program,
        uniforms,
        ..
    }] = seg.draw
    else {
        return None;
    };
    // Anything but an untinted 1:1 copy of the whole element must be rendered.
    if *alpha != 1.0
        || program.is_some()
        || !uniforms.is_empty()
        || *src != seg.meta.src
        || *dst != seg.meta.geometry
        || *transform != seg.meta.transform
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

/// Splits an output frame recording into its `BeginElement … EndElement` segments.
pub fn split_elements(commands: &[Command]) -> Vec<Segment<'_>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < commands.len() {
        let Command::BeginElement(meta) = &commands[i] else {
            debug!("ignoring command outside an element: {:?}", commands[i]);
            i += 1;
            continue;
        };
        let start = i + 1;
        let mut draw_start = None;
        let mut j = start;
        loop {
            match commands.get(j) {
                None => {
                    warn!("unterminated element in frame recording");
                    return out;
                }
                Some(Command::BeginElementDraw) => draw_start = Some(j),
                Some(Command::EndElement) => break,
                Some(_) => (),
            }
            j += 1;
        }
        let (capture, draw) = match draw_start {
            Some(k) => (&commands[start..k], &commands[k + 1..j]),
            None => (&commands[start..start], &commands[start..j]),
        };
        out.push(Segment {
            meta,
            capture,
            draw,
        });
        i = j + 1;
    }
    out
}

/// One core element, replayed from its recorded commands. Damage and opaque regions come from
/// the core, so the DRM compositor tracks and culls exactly like in-process smithay would.
pub struct SceneElement<'a> {
    track: &'a ElementTrack,
    meta: &'a ElementMeta,
    capture: &'a [Command],
    draw: &'a [Command],
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
        convert::to_rect_f64(self.meta.src)
    }

    fn geometry(&self, _scale: Scale<f64>) -> Rectangle<i32, Physical> {
        convert::to_rect(self.meta.geometry)
    }

    fn transform(&self) -> Transform {
        convert::to_transform(self.meta.transform)
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

    fn opaque_regions(&self, _scale: Scale<f64>) -> OpaqueRegions<i32, Physical> {
        OpaqueRegions::from_slice(&convert::to_rects(&self.meta.opaque))
    }

    fn kind(&self) -> Kind {
        match self.meta.kind {
            ElementKind::Cursor => Kind::Cursor,
            ElementKind::ScanoutCandidate => Kind::ScanoutCandidate,
            ElementKind::Unspecified => Kind::Unspecified,
        }
    }

    fn is_framebuffer_effect(&self) -> bool {
        self.meta.framebuffer_effect
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
        // The recording used output coordinates; damage arrives element-relative.
        let clip: Vec<_> = damage
            .iter()
            .map(|d| {
                let mut d = *d;
                d.loc += dst.loc;
                d
            })
            .collect();
        let mut iter = self
            .draw
            .iter()
            .cloned()
            .chain(std::iter::once(Command::End));
        if let Err(err) = run_frame(
            frame,
            self.tables,
            &mut iter,
            Some(&clip),
            &mut VecDeque::new(),
        ) {
            warn!("error replaying element: {err:#}");
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
        let mut iter = self
            .capture
            .iter()
            .cloned()
            .chain(std::iter::once(Command::End));
        if let Err(err) = run_frame(frame, self.tables, &mut iter, None, &mut VecDeque::new()) {
            warn!("error replaying element capture: {err:#}");
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
