//! Core-side handle of a screencast stream.
//!
//! PipeWire itself lives in the GPU process (`gpu::cast`). The core keeps what needs the
//! compositor's knowledge: the target, frame pacing, and recording the target's elements into a
//! `Target::Cast` frame. Stream state is mirrored from `CastEvent`s.

use std::time::Duration;

use calloop::timer::{TimeoutAction, Timer};
use calloop::{LoopHandle, RegistrationToken};
use smithay::backend::renderer::element::utils::{Relocate, RelocateRenderElement};
use smithay::backend::renderer::element::{Element, RenderElement};
use smithay::output::Output;
use smithay::utils::{Logical, Physical, Point, Scale, Size, Transform};
use super::CursorMode;
use crate::gpu::protocol::{CastCursorMode, CastInfo, CursorMeta, Request};
use crate::gpu::record::Recorder;
use crate::gpu::remote::RemoteRenderer;
use crate::niri::{CastTarget, State};
use crate::render_helpers::encompassing_geo;
use crate::utils::{get_monotonic_time, CastSessionId, CastStreamId};

// Give a 0.1 ms allowance for presentation time errors.
const CAST_DELAY_ALLOWANCE: Duration = Duration::from_micros(100);

pub struct Cast {
    event_loop: LoopHandle<'static, State>,
    pub session_id: CastSessionId,
    pub stream_id: CastStreamId,
    pub target: CastTarget,
    pub dynamic_target: bool,
    /// Effective cursor mode (the GPU may downgrade metadata to embedded).
    pub cursor_mode: CursorMode,
    /// drv-cast's id for this cast.
    pub portal_cast: u64,
    pub node_id: Option<u32>,
    /// Presentation time of the last frame the GPU actually sent.
    pub last_frame_time: Duration,
    /// Target time of the newest recorded frame the GPU hasn't reported on yet. Pacing counts
    /// it as sent until `Skipped` says otherwise, so redraws before the report don't pile on.
    pending_frame_time: Option<Duration>,
    scheduled_redraw: Option<RegistrationToken>,
    active: bool,
    /// Size the stream can currently take frames at (mirrored from the GPU).
    ready_size: Option<Size<i32, Physical>>,
    /// Refresh rate the target wants.
    refresh: u32,
    /// Size and refresh last sent with `CastConfigure`.
    configured: Option<(Size<i32, Physical>, u32)>,
    min_time_between_frames: Duration,
    recorder: Recorder,
    cursor_recorder: Recorder,
}

#[derive(PartialEq, Eq)]
pub enum CastSizeChange {
    Ready,
    Pending,
}

/// Data for drawing a cursor either as metadata or embedded.
///
/// The cursor elements are expected to be at the start of the main elements slice. `elem_count` is
/// the count of the pointer elements. This way, the full slice includes both main and cursor
/// elements for embedded mode, and `&elements[elem_count..]` gives just the main elements for
/// metadata mode.
#[derive(Debug)]
pub struct CursorData<'a, E> {
    /// Count of the pointer elements in the slice (index of the first non-pointer element).
    elem_count: usize,
    /// Cursor elements relocated to (0, 0).
    relocated: Vec<RelocateRenderElement<&'a E>>,
    /// Location of the cursor's hotspot in the video buffer.
    location: Point<i32, Physical>,
    /// Location of the cursor's hotspot on the cursor bitmap.
    hotspot: Point<i32, Physical>,
    /// Size of the elements' encompassing geo.
    size: Size<i32, Physical>,
}

impl<'a, E: Element> CursorData<'a, E> {
    pub fn compute(
        elements: &'a [E],
        elem_count: usize,
        location: Point<f64, Logical>,
        scale: Scale<f64>,
    ) -> Self {
        let pointer_elements = &elements[..elem_count];
        let location = location.to_physical_precise_round(scale);

        let geo = encompassing_geo(scale, pointer_elements.iter());
        let relocated = Vec::from_iter(pointer_elements.iter().map(|elem| {
            RelocateRenderElement::from_element(elem, geo.loc.upscale(-1), Relocate::Relative)
        }));

        Self {
            elem_count,
            relocated,
            location,
            hotspot: location - geo.loc,
            size: geo.size,
        }
    }
}

impl Cast {
    /// The stream's size in pixels, once configured.
    pub fn size(&self) -> Option<Size<i32, Physical>> {
        self.configured.map(|(size, _)| size)
    }
}

pub fn to_gpu_cursor_mode(mode: CursorMode) -> CastCursorMode {
    match mode {
        CursorMode::Hidden => CastCursorMode::Hidden,
        CursorMode::Embedded => CastCursorMode::Embedded,
        CursorMode::Metadata => CastCursorMode::Metadata,
    }
}

pub fn from_gpu_cursor_mode(mode: CastCursorMode) -> CursorMode {
    match mode {
        CastCursorMode::Hidden => CursorMode::Hidden,
        CastCursorMode::Embedded => CursorMode::Embedded,
        CastCursorMode::Metadata => CursorMode::Metadata,
    }
}

impl Cast {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        event_loop: LoopHandle<'static, State>,
        session_id: CastSessionId,
        stream_id: CastStreamId,
        target: CastTarget,
        size: Size<i32, Physical>,
        refresh: u32,
        cursor_mode: CursorMode,
        portal_cast: u64,
    ) -> Self {
        Self {
            event_loop,
            session_id,
            stream_id,
            target,
            dynamic_target: false,
            cursor_mode,
            portal_cast,
            node_id: None,
            last_frame_time: Duration::ZERO,
            pending_frame_time: None,
            scheduled_redraw: None,
            active: false,
            ready_size: None,
            refresh,
            configured: Some((size, refresh)),
            min_time_between_frames: Duration::ZERO,
            recorder: Recorder::default(),
            cursor_recorder: Recorder::default(),
        }
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    pub fn stream(&self) -> u64 {
        self.stream_id.get()
    }

    /// Mirrors a `CastEvent::State` from the GPU.
    pub fn set_state(
        &mut self,
        active: bool,
        ready_size: Option<(i32, i32)>,
        min_frame_time: Duration,
    ) {
        self.active = active;
        self.ready_size = ready_size.map(Size::from);
        self.min_time_between_frames = min_frame_time;
    }

    /// `CastEvent::Rendered`: the frame recorded for `target_time` went out.
    pub fn on_rendered(&mut self, target_time: Duration) {
        self.last_frame_time = self.last_frame_time.max(target_time);
        if self.pending_frame_time == Some(target_time) {
            self.pending_frame_time = None;
        }
    }

    /// `CastEvent::Skipped`: the frame recorded for `target_time` was dropped.
    pub fn on_skipped(&mut self, target_time: Duration) {
        if self.pending_frame_time == Some(target_time) {
            self.pending_frame_time = None;
        }
    }

    /// Asks the GPU for `size` at the wanted refresh if it doesn't have it yet. Returns whether
    /// the stream can take a frame of this size right now.
    pub fn ensure_size(
        &mut self,
        renderer: &RemoteRenderer,
        size: Size<i32, Physical>,
    ) -> anyhow::Result<CastSizeChange> {
        let wanted = (size, self.refresh);
        if self.configured != Some(wanted) {
            let _span = tracy_client::span!("Cast::ensure_size");
            debug!("cast size or FPS changed, updating stream");
            renderer
                .client()
                .cast_configure(self.stream(), (size.w, size.h), self.refresh)?;
            self.configured = Some(wanted);
            if self.ready_size != Some(size) {
                // Renegotiation is in flight; the GPU confirms with a State event.
                self.ready_size = None;
                self.recorder.clear();
            }
        }

        if self.ready_size == Some(size) {
            Ok(CastSizeChange::Ready)
        } else {
            debug!("stream size still hasn't changed, skipping frame");
            Ok(CastSizeChange::Pending)
        }
    }

    /// Applied on the next `ensure_size`.
    pub fn set_refresh(&mut self, refresh: u32) {
        self.refresh = refresh;
    }

    fn compute_extra_delay(&self, target_frame_time: Duration) -> Duration {
        let last = self
            .last_frame_time
            .max(self.pending_frame_time.unwrap_or(Duration::ZERO));
        let min = self.min_time_between_frames;

        if last.is_zero() {
            trace!(?target_frame_time, ?last, "last is zero, recording");
            return Duration::ZERO;
        }

        if target_frame_time < last {
            // Record frame with a warning; in case it was an overflow this will fix it.
            warn!(
                ?target_frame_time,
                ?last,
                "target frame time is below last, did it overflow or did we mispredict?"
            );
            return Duration::ZERO;
        }

        let diff = target_frame_time - last;
        if diff < min {
            let delay = min - diff;
            trace!(
                ?target_frame_time,
                ?last,
                "frame is too soon: min={min:?}, delay={:?}",
                delay
            );
            return delay;
        } else {
            trace!("overshoot={:?}", diff - min);
        }

        Duration::ZERO
    }

    fn schedule_redraw(&mut self, output: Output, target_time: Duration) {
        if self.scheduled_redraw.is_some() {
            return;
        }

        let now = get_monotonic_time();
        let duration = target_time.saturating_sub(now);
        let timer = Timer::from_duration(duration);
        let token = self
            .event_loop
            .insert_source(timer, move |_, _, state| {
                // Guard against output disconnecting before the timer has a chance to run.
                if state.niri.output_state.contains_key(&output) {
                    state.niri.queue_redraw(&output);
                }

                TimeoutAction::Drop
            })
            .unwrap();
        self.scheduled_redraw = Some(token);
    }

    fn remove_scheduled_redraw(&mut self) {
        if let Some(token) = self.scheduled_redraw.take() {
            self.event_loop.remove(token);
        }
    }

    /// Checks whether this frame should be skipped because it's too soon.
    ///
    /// If the frame should be skipped, schedules a redraw and returns `true`. Otherwise, removes a
    /// scheduled redraw, if any, and returns `false`.
    pub fn check_time_and_schedule(
        &mut self,
        output: &Output,
        target_frame_time: Duration,
    ) -> bool {
        let delay = self.compute_extra_delay(target_frame_time);
        if delay >= CAST_DELAY_ALLOWANCE {
            trace!("delay >= allowance, scheduling redraw");
            self.schedule_redraw(output.clone(), target_frame_time + delay);
            true
        } else {
            self.remove_scheduled_redraw();
            false
        }
    }

    /// Records the frame for the GPU to render into the next PipeWire buffer. The GPU skips it
    /// if nothing changed and reports sent frames with `CastEvent::Rendered`.
    pub fn record<E: RenderElement<RemoteRenderer>>(
        &mut self,
        renderer: &mut RemoteRenderer,
        elements: &[E],
        cursor_data: &CursorData<E>,
        size: Size<i32, Physical>,
        scale: Scale<f64>,
        target_frame_time: Duration,
    ) -> anyhow::Result<()> {
        let _span = tracy_client::span!("Cast::record");
        let stream = self.stream();

        let cursor = (self.cursor_mode == CursorMode::Metadata).then(|| CursorMeta {
            location: (cursor_data.location.x, cursor_data.location.y),
            hotspot: (cursor_data.hotspot.x, cursor_data.hotspot.y),
        });
        let info = CastInfo {
            scale: scale.x,
            target_time_ns: target_frame_time.as_nanos() as u64,
            cursor,
        };

        if self.cursor_mode == CursorMode::Metadata && !cursor_data.size.is_empty() {
            let target = renderer.cast_cursor_target(stream, cursor_data.size);
            self.cursor_recorder.record(
                renderer,
                target,
                cursor_data.size,
                Transform::Normal,
                scale,
                &cursor_data.relocated,
            )?;
        }

        // Embedded cursor: the pointer elements at the start of the slice are part of the frame.
        let elements = if self.cursor_mode == CursorMode::Embedded {
            elements
        } else {
            &elements[cursor_data.elem_count..]
        };
        let target = renderer.cast_target(stream, size, info);
        self.recorder
            .record(renderer, target, size, Transform::Normal, scale, elements)?;
        self.pending_frame_time = Some(target_frame_time);
        Ok(())
    }

    /// Sends a cleared frame (dynamic cast without a target).
    pub fn clear(
        &mut self,
        renderer: &RemoteRenderer,
        target_frame_time: Duration,
    ) -> anyhow::Result<()> {
        self.recorder.clear();
        self.cursor_recorder.clear();
        renderer.client().send_oneway(
            &Request::CastClear {
                stream: self.stream(),
                target_time_ns: target_frame_time.as_nanos() as u64,
            },
            &[],
        )?;
        self.pending_frame_time = Some(target_frame_time);
        Ok(())
    }
}
