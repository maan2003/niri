//! Composition runs on the compositor thread; VP9 runs on a subscription-owned
//! thread in this same process. No screenshot client or external encoder exists.
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::Result;
use calloop::channel;
use calloop::timer::{TimeoutAction, Timer};
use rho_desktop_media::codec::{Encoder, Image};
use rho_desktop_media::{media, FrameKind};
use rho_desktop_proto::Feedback;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use tokio::sync::mpsc;

use crate::niri::Niri;

pub struct Frame {
    image: Image,
    settled: bool,
    timestamp_us: u64,
    interaction: u64,
}
pub struct Video {
    frames: Arc<RawFrames>,
    next: Instant,
    scheduled: bool,
    recovery_scheduled: bool,
    capture_pending: bool,
    refinement: bool,
    revision: u64,
    born: Instant,
    interaction: u64,
    quality: Arc<Quality>,
}
pub enum Command {
    Start(Arc<RawFrames>, Arc<Quality>),
    Stop,
}
pub struct Quality {
    pub keyframe: AtomicBool,
    pub composed: AtomicU64,
    pub encoded: AtomicU64,
    pub origin: Instant,
    pub interaction: AtomicU64,
    capture_us: AtomicU64,
    encode_us: AtomicU64,
    last_key_us: AtomicU64,
    next_viewer: AtomicU64,
    sender: Mutex<rho_desktop_media::sender::Sender>,
    wake: Mutex<Weak<RawFrames>>,
}

/// A single raw slot: replacing it is safe before the encoder has seen the image.
/// In contrast, packets already encoded are always sent in dependency order.
pub struct RawFrames {
    state: Mutex<(Option<Frame>, bool)>,
    ready: Condvar,
}
impl RawFrames {
    fn new() -> Self {
        Self {
            state: Mutex::new((None, false)),
            ready: Condvar::new(),
        }
    }
    fn put(&self, frame: Frame) {
        let mut state = self.state.lock().unwrap();
        if !state.1 {
            state.0 = Some(frame);
            self.ready.notify_one();
        }
    }
    fn take(&self, quality: Option<&Quality>) -> Option<Frame> {
        let mut state = self.state.lock().unwrap();
        loop {
            if state.1 {
                return None;
            }
            if state.0.as_ref().is_some_and(|frame| {
                frame.settled
                    && quality
                        .is_some_and(|q| q.interaction.load(Ordering::Acquire) != frame.interaction)
            }) {
                state.0 = None;
            }
            if state
                .0
                .as_ref()
                .is_some_and(|frame| quality.is_none_or(|q| q.admit(frame.settled)))
            {
                return state.0.take();
            }
            state = self.ready.wait(state).unwrap();
        }
    }
    fn wake(&self) {
        // Pair notification with the mailbox mutex so feedback cannot arrive
        // between the admission check and entering the condition-variable wait.
        let _state = self.state.lock().unwrap();
        self.ready.notify_all();
    }
    fn close(&self) {
        let mut state = self.state.lock().unwrap();
        state.0 = None;
        state.1 = true;
        self.ready.notify_one();
    }
}
impl Drop for Video {
    fn drop(&mut self) {
        self.frames.close();
    }
}
impl Default for Quality {
    fn default() -> Self {
        Self {
            keyframe: AtomicBool::new(true),
            composed: AtomicU64::new(0),
            encoded: AtomicU64::new(0),
            origin: Instant::now(),
            interaction: AtomicU64::new(0),
            capture_us: AtomicU64::new(0),
            encode_us: AtomicU64::new(0),
            last_key_us: AtomicU64::new(0),
            next_viewer: AtomicU64::new(1),
            sender: Mutex::new(Default::default()),
            wake: Mutex::new(Weak::new()),
        }
    }
}
impl Quality {
    pub fn feedback(&self, id: &mut Option<u64>, feedback: Feedback, now: Instant) -> bool {
        let id = *id.get_or_insert_with(|| self.next_viewer.fetch_add(1, Ordering::Relaxed));
        let recover = self.sender.lock().unwrap().feedback(id, feedback, now);
        if recover {
            self.keyframe.store(true, Ordering::Release);
        }
        if let Some(frames) = self.wake.lock().unwrap().upgrade() {
            frames.wake();
        }
        recover
    }
    pub fn set_bitrate(&self, bitrate: u32) {
        self.sender.lock().unwrap().set_bitrate(bitrate);
    }
    pub(super) fn admit(&self, settled: bool) -> bool {
        if self.keyframe.load(Ordering::Acquire) {
            return true;
        }
        let budget = self.sender.lock().unwrap().budget(Instant::now());
        if settled {
            budget.refine
        } else {
            budget.ready
        }
    }
    pub fn remove_viewer(&self, id: Option<u64>) {
        if let Some(id) = id {
            self.sender.lock().unwrap().remove(id);
            if let Some(frames) = self.wake.lock().unwrap().upgrade() {
                frames.wake();
            }
        }
    }
    fn interval(&self, now: Instant) -> Duration {
        let capture = self.capture_us.load(Ordering::Relaxed);
        let encode = self.encode_us.load(Ordering::Relaxed);
        // Network pressure gates admission. Only CPU cost controls this interval.
        let local = capture.saturating_add(encode).saturating_mul(2);
        let decode = self
            .sender
            .lock()
            .unwrap()
            .budget(now)
            .decode_us
            .saturating_mul(2);
        Duration::from_micros(local.max(decode).max(33_334))
    }
}
/// Request a keyframe even if the output is idle, retrying when group spacing permits.
pub fn request_recovery(state: &mut crate::niri::State, quality: &Quality) -> Result<()> {
    quality.keyframe.store(true, Ordering::Release);
    state.niri.queue_redraw_all();
    if let Some(video) = state.backend.headless().video.as_mut() {
        if !video.recovery_scheduled {
            video.recovery_scheduled = true;
            let born = video.born;
            state
                .niri
                .event_loop
                .insert_source(
                    Timer::from_duration(Duration::from_millis(260)),
                    move |_, _, state| {
                        if let Some(video) = state.backend.headless().video.as_mut() {
                            if video.born != born {
                                return TimeoutAction::Drop;
                            }
                            if video.quality.keyframe.load(Ordering::Acquire) {
                                state.niri.queue_redraw_all();
                                return TimeoutAction::ToDuration(Duration::from_millis(260));
                            }
                            video.recovery_scheduled = false;
                        }
                        TimeoutAction::Drop
                    },
                )
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
    }
    Ok(())
}
impl Video {
    pub fn new(frames: Arc<RawFrames>, quality: Arc<Quality>) -> Self {
        Self {
            frames,
            next: Instant::now(),
            scheduled: false,
            recovery_scheduled: false,
            capture_pending: false,
            refinement: false,
            revision: 0,
            born: Instant::now(),
            interaction: 0,
            quality,
        }
    }
    pub(super) fn capture_ready(&self) -> bool {
        self.capture_pending && self.admission_ready()
    }
    fn admission_ready(&self) -> bool {
        self.quality.admit(self.refinement)
    }
    /// Called only for compositor damage, initial demand, or one refinement.
    pub fn render(
        &mut self,
        niri: &mut Niri,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Result<()> {
        let now = Instant::now();
        let interaction = self.quality.interaction.load(Ordering::Acquire);
        if interaction != self.interaction {
            self.interaction = interaction;
            self.refinement = false;
            self.revision += 1;
        }
        if !self.admission_ready() {
            // Remember the deferred capture: readiness can change on the encoder
            // thread before feedback arrives, so testing only an edge loses wakes.
            self.capture_pending = true;
            return Ok(());
        }
        if now < self.next {
            if !self.scheduled {
                self.scheduled = true;
                let output = output.clone();
                niri.event_loop
                    .insert_source(Timer::from_duration(self.next - now), move |_, _, state| {
                        if let Some(video) = state.backend.headless().video.as_mut() {
                            video.scheduled = false;
                            state.niri.queue_redraw(&output);
                        }
                        TimeoutAction::Drop
                    })
                    .map_err(|e| anyhow::anyhow!("{e}"))?;
            }
            return Ok(());
        }
        self.capture_pending = false;
        let captured = Instant::now();
        let timestamp_us = captured.duration_since(self.quality.origin).as_micros() as u64;
        self.quality.composed.fetch_add(1, Ordering::Relaxed);
        let image = super::capture_pixels(niri, renderer, output)?;
        let settled = std::mem::take(&mut self.refinement);
        self.quality
            .capture_us
            .store(captured.elapsed().as_micros() as u64, Ordering::Relaxed);
        self.frames.put(Frame {
            image,
            settled,
            timestamp_us,
            interaction,
        });
        self.next = Instant::now() + self.quality.interval(Instant::now());
        if !settled {
            self.revision += 1;
            let revision = self.revision;
            let born = self.born;
            let output = output.clone();
            niri.event_loop
                .insert_source(
                    Timer::from_duration(Duration::from_millis(180)),
                    move |_, _, state| {
                        if let Some(video) = state.backend.headless().video.as_mut() {
                            if video.revision == revision && video.born == born {
                                video.refinement = true;
                                state.niri.queue_redraw(&output);
                            }
                        }
                        TimeoutAction::Drop
                    },
                )
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        Ok(())
    }
}

/// Tracks demand across all MoQ viewers. A late viewer requests a fresh keyframe.
pub async fn run(
    listener: tokio::net::UnixListener,
    commands: channel::Sender<Command>,
    quality: Arc<Quality>,
) -> Result<()> {
    let origin = media::origin();
    let broadcast = origin.create_broadcast("app")?;
    broadcast.announce(Default::default())?;
    let mut video = media::Video::new(&broadcast)?;
    let used = video.track.clone();
    let listening = async {
        let slots = Arc::new(tokio::sync::Semaphore::new(8));
        loop {
            let (stream, _) = listener.accept().await?;
            if stream.peer_cred()?.uid() != unsafe { libc::geteuid() } {
                continue;
            }
            let Ok(slot) = slots.clone().try_acquire_owned() else {
                continue;
            };
            let origin = origin.clone();
            tokio::spawn(async move {
                let _slot = slot;
                if let Ok(session) = media::local_server(stream, &origin).await {
                    let guard = media::SessionGuard(session);
                    guard.0.closed().await;
                }
            });
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let production = async {
        loop {
            used.used().await?;
            quality.keyframe.store(true, Ordering::Release);
            let frames = Arc::new(RawFrames::new());
            let raw = frames.clone();
            quality.sender.lock().unwrap().reset();
            *quality.wake.lock().unwrap() = Arc::downgrade(&frames);
            let (encoded, mut packets) = mpsc::channel(2);
            let q = quality.clone();
            let encoding = tokio::task::spawn_blocking(move || -> Result<()> {
                let mut encoder = None;
                let mut previous: Option<Image> = None;
                let mut checkpoint_us = 0;
                let mut checkpoint_bytes = 0usize;
                let mut checkpoints = 0usize;
                while let Some(frame) = raw.take(Some(&q)) {
                    let started = Instant::now();
                    let image = frame.image;
                    let requested = q.keyframe.swap(false, Ordering::AcqRel);
                    let last_key_us = q.last_key_us.load(Ordering::Relaxed);
                    let force = checkpoint_bytes >= 8 * 1024 * 1024
                        || checkpoints >= 4096
                        || recovery_due(
                            previous.is_none(),
                            requested,
                            frame.timestamp_us,
                            last_key_us,
                        );
                    if requested && !force {
                        q.keyframe.store(true, Ordering::Release);
                        // The admission bypass is for one replacement keyframe,
                        // not more dependencies on the failed group while waiting
                        // for minimum group spacing.
                        continue;
                    }
                    if !force
                        && !frame.settled
                        && previous.as_ref().is_some_and(|p| p.bgra == image.bgra)
                    {
                        continue;
                    }
                    let size = (image.width, image.height);
                    let budget = q.sender.lock().unwrap().budget(Instant::now());
                    let rate = budget.bitrate as usize;
                    if encoder.as_ref().is_none_or(|(old, _)| *old != size) {
                        encoder = Some((size, Encoder::new(size.0, size.1, rate)?));
                    }
                    let encoder = &mut encoder.as_mut().unwrap().1;
                    encoder.quality(rate, frame.settled)?;
                    let kind = if force {
                        FrameKind::Key
                    } else if frame.timestamp_us.saturating_sub(checkpoint_us) >= 2_000_000 {
                        // Promote only real captures, never wake an idle desktop for a timer.
                        FrameKind::Checkpoint
                    } else {
                        FrameKind::State
                    };
                    for packet in encoder.encode(&image.bgra, kind)? {
                        q.encoded.fetch_add(1, Ordering::Relaxed);
                        if packet.kind == FrameKind::Key {
                            checkpoint_bytes = 0;
                            checkpoints = 0;
                            q.last_key_us.store(frame.timestamp_us, Ordering::Relaxed);
                        }
                        if packet.kind != FrameKind::State {
                            checkpoint_us = frame.timestamp_us;
                            checkpoint_bytes += packet.data.len();
                            checkpoints += 1;
                        }
                        q.sender.lock().unwrap().encoded(
                            frame.timestamp_us,
                            packet.data.len(),
                            packet.kind,
                            frame.settled,
                            Instant::now(),
                        );
                        tracing::debug!(
                            timestamp_us = frame.timestamp_us,
                            bytes = packet.data.len(),
                            kind = ?packet.kind,
                            settled = frame.settled,
                            bitrate = rate,
                            outstanding_bytes = budget.outstanding_bytes,
                            window_bytes = budget.window_bytes,
                            oldest_us = budget.oldest_us,
                            encode_us = started.elapsed().as_micros() as u64,
                            "desktop encoded"
                        );
                        if encoded.blocking_send((frame.timestamp_us, packet)).is_err() {
                            return Ok(());
                        }
                    }
                    q.encode_us
                        .store(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                    previous = Some(image);
                }
                Ok(())
            });
            commands.send(Command::Start(frames.clone(), quality.clone()))?;
            struct Stop(channel::Sender<Command>);
            impl Drop for Stop {
                fn drop(&mut self) {
                    let _ = self.0.send(Command::Stop);
                }
            }
            let stop = Stop(commands.clone());
            let result = loop {
                tokio::select! {
                    _=used.unused()=>break Ok(()),
                    packet=packets.recv()=>match packet {
                        Some((time,packet))=>if let Err(error)=video.write(packet.kind,time,packet.data.into()) {break Err(error)},
                        None=>break Err(anyhow::anyhow!("VP9 encoder stopped")),
                    }
                }
            };
            frames.close();
            drop(stop);
            drop(packets);
            encoding.await??;
            result?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! {result=listening=>result,result=production=>result}
}

/// Recovery requests stay pending until another capture can safely open a new group.
fn recovery_due(first: bool, requested: bool, timestamp_us: u64, last_key_us: u64) -> bool {
    first || requested && timestamp_us.saturating_sub(last_key_us) >= 250_000
}

#[cfg(test)]
mod tests {
    use rho_desktop_proto::FrameId;

    use super::*;

    #[test]
    fn slow_decoder_controls_pacing_until_disconnected() {
        let quality = Quality::default();
        let now = Instant::now();
        let mut fast = None;
        let mut slow = None;
        let frame = FrameId {
            epoch: 3,
            timestamp_us: 300_000,
        };
        quality.feedback(
            &mut fast,
            Feedback {
                presented: Some(frame),
                decode_us: 3_000,
                lag_us: 6_000,
                ..Default::default()
            },
            now,
        );
        quality.feedback(
            &mut slow,
            Feedback {
                presented: Some(frame),
                decode_us: 54_000,
                lag_us: 12_000,
                ..Default::default()
            },
            now,
        );
        assert_eq!(quality.interval(now), Duration::from_micros(108_000));
        quality.remove_viewer(slow);
        assert_eq!(quality.interval(now), Duration::from_micros(33_334));
        quality.feedback(
            &mut slow,
            Feedback {
                lag_us: 450_000,
                ..Default::default()
            },
            now,
        );
        // Network lag no longer masquerades as capture/decoder CPU cost.
        assert_eq!(quality.interval(now), Duration::from_micros(33_334));
        assert!(
            !quality
                .sender
                .lock()
                .unwrap()
                .budget(now + Duration::from_secs(2))
                .ready
        );
    }

    #[test]
    fn capture_and_encode_cost_and_progress_limit_work() {
        let quality = Quality::default();
        let now = Instant::now();
        quality.capture_us.store(36_000, Ordering::Relaxed);
        quality.encode_us.store(18_000, Ordering::Relaxed);
        assert_eq!(quality.interval(now), Duration::from_micros(108_000));
        quality.capture_us.store(0, Ordering::Relaxed);
        quality.encode_us.store(0, Ordering::Relaxed);
        let mut viewer = None;
        quality.feedback(
            &mut viewer,
            Feedback {
                presented: Some(FrameId {
                    epoch: 1,
                    timestamp_us: 10_000,
                }),
                ..Default::default()
            },
            now,
        );
        assert_eq!(quality.interval(now), Duration::from_micros(33_334));
        quality.capture_us.store(61_000, Ordering::Relaxed);
        quality.encode_us.store(42_000, Ordering::Relaxed);
        quality.feedback(
            &mut viewer,
            Feedback {
                lag_us: 900_000,
                ..Default::default()
            },
            now,
        );
        // Network lag does not replace the local duty budget.
        assert_eq!(quality.interval(now), Duration::from_micros(206_000));
        quality.feedback(
            &mut viewer,
            Feedback {
                decode_us: 190_000,
                ..Default::default()
            },
            now,
        );
        // A slow decoder must lower frame rate beyond the lag cap.
        assert_eq!(quality.interval(now), Duration::from_micros(380_000));
    }

    #[test]
    fn recovery_waits_for_group_spacing_and_is_not_lost() {
        assert!(recovery_due(true, false, 10, 0));
        assert!(!recovery_due(false, true, 349_999, 100_000));
        assert!(recovery_due(false, true, 350_000, 100_000));
        assert!(!recovery_due(false, false, 800_000, 100_000));
        let quality = Quality::default();
        quality.keyframe.store(false, Ordering::Relaxed);
        let mut viewer = None;
        quality.feedback(
            &mut viewer,
            Feedback {
                recover: true,
                ..Default::default()
            },
            Instant::now(),
        );
        assert!(quality.keyframe.load(Ordering::Relaxed));
        let video = Video::new(Arc::new(RawFrames::new()), Arc::new(quality));
        assert!(!video.recovery_scheduled);
        // Requests are held until the next capture reaches the keyframe boundary.
        assert!(video.quality.keyframe.load(Ordering::Relaxed));
    }

    #[test]
    fn refinement_waits_for_receipt_and_new_input_replaces_it_before_encoding() {
        let quality = Quality::default();
        quality.keyframe.store(false, Ordering::Release);
        let now = Instant::now();
        let mut viewer = None;
        quality.feedback(
            &mut viewer,
            Feedback {
                rtt_us: 200_000,
                delivery_bps: 2_000_000,
                ..Default::default()
            },
            now,
        );
        quality
            .sender
            .lock()
            .unwrap()
            .encoded(1, 30_000, FrameKind::State, false, now);
        assert!(quality.admit(false));
        assert!(!quality.admit(true));

        let raw = RawFrames::new();
        raw.put(Frame {
            image: Image {
                width: 1,
                height: 1,
                bgra: vec![0; 4],
            },
            settled: true,
            timestamp_us: 2,
            interaction: 0,
        });
        quality.interaction.fetch_add(1, Ordering::AcqRel);
        raw.put(Frame {
            image: Image {
                width: 1,
                height: 1,
                bgra: vec![1; 4],
            },
            settled: false,
            timestamp_us: 3,
            interaction: 1,
        });
        let frame = raw.take(Some(&quality)).unwrap();
        assert_eq!(frame.timestamp_us, 3);
        assert!(!frame.settled);
        quality.feedback(
            &mut viewer,
            Feedback {
                received: Some(FrameId {
                    epoch: 1,
                    timestamp_us: 1,
                }),
                rtt_us: 200_000,
                delivery_bps: 2_000_000,
                ..Default::default()
            },
            now,
        );
        assert!(quality.admit(true));
    }

    #[test]
    fn deferred_capture_retries_even_if_readiness_changed_before_feedback() {
        let quality = Arc::new(Quality::default());
        quality.keyframe.store(false, Ordering::Release);
        let mut video = Video::new(Arc::new(RawFrames::new()), quality.clone());
        assert!(!video.capture_ready(), "idle feedback must not capture");
        video.capture_pending = true;
        quality.sender.lock().unwrap().encoded(
            10,
            100_000,
            FrameKind::State,
            false,
            Instant::now(),
        );
        assert!(!video.capture_ready());
        let mut viewer = None;
        quality.feedback(
            &mut viewer,
            Feedback {
                received: Some(FrameId {
                    epoch: 1,
                    timestamp_us: 10,
                }),
                ..Default::default()
            },
            Instant::now(),
        );
        // Readiness is already true before the next control callback checks it.
        assert!(quality.admit(false));
        assert!(video.capture_ready());
        video.capture_pending = false;
        assert!(!video.capture_ready());
    }

    #[test]
    fn mailbox_replaces_only_unencoded_images() {
        let raw = RawFrames::new();
        let frame = |n| Frame {
            image: Image {
                width: 1,
                height: 1,
                bgra: vec![n; 4],
            },
            settled: false,
            timestamp_us: n as u64,
            interaction: 0,
        };
        raw.put(frame(1));
        raw.put(frame(2));
        assert_eq!(raw.take(None).unwrap().timestamp_us, 2);
        raw.close();
        raw.put(frame(3));
        assert!(raw.take(None).is_none());
    }
}
