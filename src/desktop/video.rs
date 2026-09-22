//! Composition runs on the compositor thread; VP9 runs on a subscription-owned
//! thread in this same process. No screenshot client or external encoder exists.
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use calloop::channel;
use calloop::timer::{TimeoutAction, Timer};
use rho_desktop_media::codec::{Encoder, Image};
use rho_desktop_media::media;
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::output::Output;
use tokio::sync::mpsc;

use crate::niri::Niri;

pub struct Frame {
    image: Image,
    settled: bool,
}
pub struct Video {
    frames: mpsc::Sender<Frame>,
    next: Instant,
    scheduled: bool,
    refinement: bool,
    revision: u64,
    born: Instant,
    quality: Arc<Quality>,
}
pub enum Command {
    Start(mpsc::Sender<Frame>, Arc<Quality>),
    Stop,
}
pub struct Quality {
    pub bitrate: AtomicU32,
    pub keyframe: AtomicBool,
    pub composed: AtomicU64,
    pub encoded: AtomicU64,
}
impl Default for Quality {
    fn default() -> Self {
        Self {
            bitrate: AtomicU32::new(2_000_000),
            keyframe: AtomicBool::new(true),
            composed: AtomicU64::new(0),
            encoded: AtomicU64::new(0),
        }
    }
}
impl Video {
    pub fn new(frames: mpsc::Sender<Frame>, quality: Arc<Quality>) -> Self {
        Self {
            frames,
            next: Instant::now(),
            scheduled: false,
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
        if self.frames.capacity() == 0 {
            self.next = self.next.max(now + Duration::from_millis(33));
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
        self.quality.composed.fetch_add(1, Ordering::Relaxed);
        let image = super::capture_pixels(niri, renderer, output)?;
        let settled = std::mem::take(&mut self.refinement);
        if self.frames.try_send(Frame { image, settled }).is_err() {
            // No codec dependency was created: only raw images can be dropped.
            return Ok(());
        }
        self.next = now + Duration::from_millis(33);
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
        let epoch = Instant::now();
        loop {
            used.used().await?;
            quality.keyframe.store(true, Ordering::Release);
            let (frames, mut raw) = mpsc::channel::<Frame>(1);
            let (encoded, mut packets) = mpsc::channel(2);
            let q = quality.clone();
            let encoding = tokio::task::spawn_blocking(move || -> Result<()> {
                let mut encoder = None;
                let mut previous: Option<Image> = None;
                let mut last_key = Instant::now();
                while let Some(frame) = raw.blocking_recv() {
                    let image = frame.image;
                    let force = q.keyframe.swap(false, Ordering::AcqRel);
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
                        force || last_key.elapsed() >= Duration::from_millis(500),
                    )? {
                        q.encoded.fetch_add(1, Ordering::Relaxed);
                        if packet.keyframe {
                            last_key = Instant::now();
                        }
                        if encoded
                            .blocking_send((epoch.elapsed().as_micros() as u64, packet))
                            .is_err()
                        {
                            return Ok(());
                        }
                    }
                    previous = Some(image);
                }
                Ok(())
            });
            commands.send(Command::Start(frames, quality.clone()))?;
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
