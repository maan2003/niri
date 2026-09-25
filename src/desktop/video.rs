//! Composition runs on the compositor thread; VP9 runs on a subscription-owned
//! thread in this same process. No screenshot client or external encoder exists.
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use calloop::channel;
use calloop::timer::{TimeoutAction, Timer};
use rho_desktop_media::codec::{Encoder, Image};
use rho_desktop_media::media;
use rho_desktop_proto::Feedback;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use tokio::sync::mpsc;

use crate::niri::Niri;

pub struct Frame {
    image: Image,
    settled: bool,
    timestamp_us: u64,
}
pub struct Video {
    frames: Arc<RawFrames>,
    next: Instant,
    scheduled: bool,
    recovery_scheduled: bool,
    refinement: bool,
    revision: u64,
    born: Instant,
    quality: Arc<Quality>,
}
pub enum Command {
    Start(Arc<RawFrames>, Arc<Quality>),
    Stop,
}
pub struct Quality {
    pub bitrate: AtomicU32,
    pub keyframe: AtomicBool,
    pub composed: AtomicU64,
    pub encoded: AtomicU64,
    pub origin: Instant,
    capture_us: AtomicU64,
    encode_us: AtomicU64,
    latest_us: AtomicU64,
    last_key_us: AtomicU64,
    viewers: Mutex<BTreeMap<u64, (Feedback, Instant)>>,
    next_viewer: AtomicU64,
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
    fn take(&self) -> Option<Frame> {
        let mut state = self.state.lock().unwrap();
        loop {
            if let Some(frame) = state.0.take() {
                return Some(frame);
            }
            if state.1 {
                return None;
            }
            state = self.ready.wait(state).unwrap();
        }
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
            bitrate: AtomicU32::new(2_000_000),
            keyframe: AtomicBool::new(true),
            composed: AtomicU64::new(0),
            encoded: AtomicU64::new(0),
            origin: Instant::now(),
            capture_us: AtomicU64::new(0),
            encode_us: AtomicU64::new(0),
            latest_us: AtomicU64::new(0),
            last_key_us: AtomicU64::new(0),
            viewers: Mutex::new(BTreeMap::new()),
            next_viewer: AtomicU64::new(1),
        }
    }
}
impl Quality {
    pub fn feedback(&self, id: &mut Option<u64>, feedback: Feedback, now: Instant) {
        let id = *id.get_or_insert_with(|| self.next_viewer.fetch_add(1, Ordering::Relaxed));
        self.viewers.lock().unwrap().insert(id, (feedback, now));
        if feedback.recover {
            self.keyframe.store(true, Ordering::Release);
        }
    }
    pub fn remove_viewer(&self, id: Option<u64>) {
        if let Some(id) = id {
            self.viewers.lock().unwrap().remove(&id);
        }
    }
    fn interval(&self, now: Instant) -> Duration {
        let capture = self.capture_us.load(Ordering::Relaxed);
        let encode = self.encode_us.load(Ordering::Relaxed);
        let latest = self.latest_us.load(Ordering::Relaxed);
        // Target a 50% combined capture and encode duty budget.
        let local = capture.saturating_add(encode).saturating_mul(2);
        let mut receiver = 33_334_u64;
        let mut viewers = self.viewers.lock().unwrap();
        viewers.retain(|_, (_, seen)| {
            now.saturating_duration_since(*seen) < Duration::from_millis(600)
        });
        for (feedback, _) in viewers.values() {
            receiver = receiver.max(feedback.decode_us.saturating_mul(2));
            receiver = receiver.max((feedback.lag_us / 3).min(150_000));
            if latest
                > feedback
                    .presented
                    .map_or(0, |id| id.timestamp_us)
                    .saturating_add(500_000)
            {
                receiver = receiver.max(100_000);
            }
        }
        Duration::from_micros(local.max(receiver))
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
            refinement: false,
            revision: 0,
            born: Instant::now(),
            quality,
        }
    }
    /// Called only for compositor damage, initial demand, or one refinement.
    pub fn render(
        &mut self,
        niri: &mut Niri,
        renderer: &mut GlesRenderer,
        output: &Output,
    ) -> Result<()> {
        let now = Instant::now();
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
            let (encoded, mut packets) = mpsc::channel(2);
            let q = quality.clone();
            let encoding = tokio::task::spawn_blocking(move || -> Result<()> {
                let mut encoder = None;
                let mut previous: Option<Image> = None;
                let mut last_key = Instant::now();
                while let Some(frame) = raw.take() {
                    let started = Instant::now();
                    let image = frame.image;
                    let requested = q.keyframe.swap(false, Ordering::AcqRel);
                    let last_key_us = q.last_key_us.load(Ordering::Relaxed);
                    let force = recovery_due(
                        previous.is_none(),
                        requested,
                        frame.timestamp_us,
                        last_key_us,
                    );
                    if requested && !force {
                        q.keyframe.store(true, Ordering::Release);
                    }
                    if !force
                        && !frame.settled
                        && previous.as_ref().is_some_and(|p| p.bgra == image.bgra)
                    {
                        continue;
                    }
                    let size = (image.width, image.height);
                    let rate = q.bitrate.load(Ordering::Acquire) as usize;
                    if encoder.as_ref().is_none_or(|(old, _)| *old != size) {
                        encoder = Some((size, Encoder::new(size.0, size.1, rate)?));
                    }
                    let encoder = &mut encoder.as_mut().unwrap().1;
                    encoder.quality(rate, frame.settled)?;
                    for packet in encoder.encode(
                        &image.bgra,
                        force || last_key.elapsed() >= Duration::from_secs(2),
                    )? {
                        q.encoded.fetch_add(1, Ordering::Relaxed);
                        if packet.keyframe {
                            last_key = Instant::now();
                            q.last_key_us.store(frame.timestamp_us, Ordering::Relaxed);
                        }
                        q.latest_us.store(frame.timestamp_us, Ordering::Relaxed);
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
                        Some((time,packet))=>if let Err(error)=video.write(packet.keyframe,time,packet.data.into()) {break Err(error)},
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
    fn slow_viewer_controls_pacing_until_expired_or_disconnected() {
        let quality = Quality::default();
        let now = Instant::now();
        let mut fast = None;
        let mut slow = None;
        let frame = FrameId {
            group: 3,
            timestamp_us: 300_000,
        };
        quality.latest_us.store(310_000, Ordering::Relaxed);
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
        assert_eq!(quality.interval(now), Duration::from_micros(150_000));
        assert_eq!(
            quality.interval(now + Duration::from_millis(600)),
            Duration::from_micros(33_334)
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
        quality.latest_us.store(800_000, Ordering::Relaxed);
        let mut viewer = None;
        quality.feedback(
            &mut viewer,
            Feedback {
                presented: Some(FrameId {
                    group: 1,
                    timestamp_us: 10_000,
                }),
                ..Default::default()
            },
            now,
        );
        assert_eq!(quality.interval(now), Duration::from_micros(100_000));
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
        // Lag pressure caps at 150ms; local work still needs 206ms.
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
        };
        raw.put(frame(1));
        raw.put(frame(2));
        assert_eq!(raw.take().unwrap().timestamp_us, 2);
        raw.close();
        raw.put(frame(3));
        assert!(raw.take().is_none());
    }
}
