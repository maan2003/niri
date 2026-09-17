//! Core side of scene recording: turns a frame's smithay elements into scene nodes for the
//! GPU process, each with the damage since the last frame that element was recorded in.
//!
//! One `Recorder` per render target (output or screencast stream), because damage is relative
//! to what that target last saw. The recorder only maps ids and computes damage through
//! smithay's element model; the GPU keeps the history and does the tracking.

use std::collections::HashMap;
use std::mem;

use smithay::backend::renderer::element::{
    Id, Kind, RenderElement, RenderElementPresentationState, RenderElementState,
    RenderElementStates,
};
use smithay::backend::renderer::utils::CommitCounter;
use smithay::backend::renderer::{Frame as _, Renderer as _};
use smithay::utils::user_data::UserDataMap;
use smithay::utils::{Physical, Rectangle, Scale, Size, Transform};

use super::convert;
use super::protocol::{ElementKind, ElementState, Node, Presentation};
use super::remote::{RemoteRenderer, RemoteTarget};

struct ElementTrack {
    remote_id: u64,
    commit: CommitCounter,
    seen: bool,
}

pub struct Recorder {
    /// Per-element state for framebuffer effects, keyed like smithay's damage tracker does it.
    effects_cache: HashMap<Id, UserDataMap>,
    /// Elements sent to the GPU last frame, so we can send only their damage since then.
    tracks: HashMap<Id, ElementTrack>,
    next_id: u64,
    /// Bumped by `clear`; travels with every frame so the GPU drops its history in step.
    generation: u64,
}

impl Default for Recorder {
    fn default() -> Self {
        Self {
            effects_cache: HashMap::new(),
            tracks: HashMap::new(),
            next_id: 1,
            generation: 1,
        }
    }
}

impl Recorder {
    /// Forgets the element history; the next frame sends full damage for everything and
    /// makes the GPU forget too.
    pub fn clear(&mut self) {
        self.tracks.clear();
        self.generation += 1;
    }

    /// Records `elements` (top to bottom) into `target` as one scene frame. `size` is the
    /// untransformed buffer size.
    pub fn record<E: RenderElement<RemoteRenderer>>(
        &mut self,
        renderer: &mut RemoteRenderer,
        mut target: RemoteTarget<'static>,
        size: Size<i32, Physical>,
        transform: Transform,
        scale: Scale<f64>,
        elements: &[E],
    ) -> anyhow::Result<()> {
        let _span = tracy_client::span!("Recorder::record");

        // Output transform is in surface-rotation terms; inverting gives the render transform.
        let mut frame = renderer.render(&mut target, size, transform.invert())?;
        frame.set_generation(self.generation);

        for element in elements.iter().rev() {
            let id = element.id();
            let geometry = element.geometry(scale);
            let src = element.src();
            let commit = element.current_commit();
            let (remote_id, damage) = match self.tracks.get_mut(id) {
                Some(track) => {
                    let damage = element.damage_since(scale, Some(track.commit));
                    track.commit = commit;
                    track.seen = true;
                    (track.remote_id, Some(to_frame_coords(&damage, geometry)))
                }
                None => {
                    let remote_id = self.next_id;
                    self.next_id += 1;
                    self.tracks.insert(
                        id.clone(),
                        ElementTrack {
                            remote_id,
                            commit,
                            seen: true,
                        },
                    );
                    (remote_id, None)
                }
            };

            frame.begin_node(Node {
                id: remote_id,
                src: convert::rect_f64(src),
                geometry: convert::rect(geometry),
                damage,
                opaque: to_frame_coords(&element.opaque_regions(scale), geometry),
                kind: match element.kind() {
                    Kind::Cursor => ElementKind::Cursor,
                    Kind::ScanoutCandidate => ElementKind::ScanoutCandidate,
                    Kind::Unspecified => ElementKind::Unspecified,
                },
                transform: convert::transform(element.transform()),
                capture: Vec::new(),
                draw: Vec::new(),
            });
            if element.is_framebuffer_effect() {
                let cache = self.effects_cache.entry(id.clone()).or_default();
                element.capture_framebuffer(&mut frame, src, geometry, cache)?;
            }
            frame.begin_node_draw();
            element.draw(
                &mut frame,
                src,
                geometry,
                &[Rectangle::from_size(geometry.size)],
                &[],
                self.effects_cache.get(id),
            )?;
            frame.end_node();
        }
        self.tracks.retain(|_, t| mem::take(&mut t.seen));
        let tracks = &self.tracks;
        self.effects_cache.retain(|id, _| tracks.contains_key(id));

        // No fence to wait on: the GPU process syncs when it renders.
        let _ = frame.finish()?;
        Ok(())
    }

    /// Maps the GPU's per-node results back onto smithay ids for presentation feedback.
    pub fn element_states(&self, states: &[ElementState]) -> RenderElementStates {
        let by_remote: HashMap<u64, &ElementState> = states.iter().map(|s| (s.id, s)).collect();
        let states = self
            .tracks
            .iter()
            .filter_map(|(id, track)| {
                let s = by_remote.get(&track.remote_id)?;
                Some((
                    id.clone(),
                    RenderElementState {
                        visible_area: s.visible_area as usize,
                        presentation_state: match s.presentation {
                            Presentation::Rendering => {
                                RenderElementPresentationState::Rendering { reason: None }
                            }
                            Presentation::ZeroCopy => RenderElementPresentationState::ZeroCopy,
                            Presentation::Skipped => RenderElementPresentationState::Skipped,
                        },
                        needs_capture: false,
                    },
                ))
            })
            .collect();
        RenderElementStates { states }
    }
}

/// Element-relative rectangles to frame coordinates.
fn to_frame_coords(
    rects: &[Rectangle<i32, Physical>],
    geometry: Rectangle<i32, Physical>,
) -> Vec<super::protocol::Rect<i32>> {
    let rects: Vec<_> = rects
        .iter()
        .map(|r| {
            let mut r = *r;
            r.loc += geometry.loc;
            r
        })
        .collect();
    convert::rects(&rects)
}
