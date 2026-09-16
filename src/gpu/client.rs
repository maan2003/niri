//! Core side handle to the GPU process.
//!
//! Synchronous request/reply for now. The frame path will move to an event
//! loop source once the core sends real frames.

use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;

use anyhow::{anyhow, bail, Context};
use serde::Serialize;
use smithay::backend::allocator::Fourcc;

use super::protocol::{DmabufDesc, Event, Image, Request, ShmDesc, PROTOCOL_VERSION};
use super::scene::{BufferId, Rect, Scene};
use super::transport::Channel;

/// Fd number the child finds its socket on, like Chromium's fixed IPC fd.
pub const CHILD_SOCKET_FD: i32 = 3;

pub struct GpuClient {
    chan: Channel,
    child: Option<Child>,
    thread: Option<JoinHandle<anyhow::Result<()>>>,
    pub renderer: String,
}

impl GpuClient {
    /// Spawns `exe gpu-process --socket-fd 3`.
    pub fn spawn_process(exe: &Path) -> anyhow::Result<Self> {
        let (ours, theirs) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )
        .context("socketpair")?;

        let theirs_raw = theirs.as_raw_fd();
        let mut cmd = Command::new(exe);
        cmd.arg("gpu-process")
            .arg("--socket-fd")
            .arg(CHILD_SOCKET_FD.to_string())
            .stdin(Stdio::null());
        unsafe {
            cmd.pre_exec(move || {
                // dup2 clears CLOEXEC on the target, so the child keeps only fd 3.
                if libc::dup2(theirs_raw, CHILD_SOCKET_FD) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn().context("spawning gpu process")?;
        drop(theirs);

        let mut client = Self {
            chan: Channel::new(ours),
            child: Some(child),
            thread: None,
            renderer: String::new(),
        };
        client.handshake()?;
        Ok(client)
    }

    /// Runs the server on a thread in this process. For tests only: it gives
    /// none of the isolation, just the same protocol path.
    pub fn spawn_thread() -> anyhow::Result<Self> {
        let (ours, theirs) = Channel::pair()?;
        let thread = std::thread::Builder::new()
            .name("gpu-server".into())
            .spawn(move || super::server::run(theirs.into_fd()))?;
        let mut client = Self {
            chan: ours,
            child: None,
            thread: Some(thread),
            renderer: String::new(),
        };
        client.handshake()?;
        Ok(client)
    }

    fn handshake(&mut self) -> anyhow::Result<()> {
        let (event, _): (Event, _) = self.chan.recv().context("waiting for gpu process")?;
        match event {
            Event::Ready { version, renderer } if version == PROTOCOL_VERSION => {
                self.renderer = renderer;
                Ok(())
            }
            Event::Ready { version, .. } => bail!("gpu process protocol version {version}"),
            other => bail!("unexpected first event from gpu process: {other:?}"),
        }
    }

    fn request<T: Serialize>(&mut self, req: &T, fds: &[BorrowedFd<'_>]) -> anyhow::Result<Event> {
        self.chan.send(req, fds)?;
        let (event, _): (Event, _) = self.chan.recv()?;
        if let Event::Error { message } = &event {
            bail!("gpu process: {message}");
        }
        Ok(event)
    }

    fn expect_ack(event: Event) -> anyhow::Result<()> {
        match event {
            Event::Ack => Ok(()),
            other => Err(anyhow!("expected Ack, got {other:?}")),
        }
    }

    pub fn register_shm(&mut self, id: BufferId, fd: BorrowedFd<'_>, desc: ShmDesc) -> anyhow::Result<()> {
        Self::expect_ack(self.request(&Request::RegisterShm { id, desc }, &[fd])?)
    }

    pub fn register_dmabuf(
        &mut self,
        id: BufferId,
        fds: &[BorrowedFd<'_>],
        desc: DmabufDesc,
    ) -> anyhow::Result<()> {
        Self::expect_ack(self.request(&Request::RegisterDmabuf { id, desc }, fds)?)
    }

    pub fn update_shm(&mut self, id: BufferId, damage: Vec<Rect<i32>>) -> anyhow::Result<()> {
        Self::expect_ack(self.request(&Request::UpdateShm { id, damage }, &[])?)
    }

    pub fn destroy_buffer(&mut self, id: BufferId) -> anyhow::Result<()> {
        Self::expect_ack(self.request(&Request::DestroyBuffer { id }, &[])?)
    }

    pub fn render_to_image(&mut self, scene: Scene, format: Fourcc) -> anyhow::Result<Image> {
        match self.request(
            &Request::RenderToImage {
                scene,
                format: format as u32,
            },
            &[],
        )? {
            Event::Image(image) => Ok(image),
            other => Err(anyhow!("expected Image, got {other:?}")),
        }
    }

    pub fn shutdown(mut self) -> anyhow::Result<()> {
        let _ = self.request(&Request::Shutdown, &[]);
        self.join()
    }

    fn join(&mut self) -> anyhow::Result<()> {
        if let Some(mut child) = self.child.take() {
            let status = child.wait().context("waiting for gpu process")?;
            if !status.success() {
                bail!("gpu process exited with {status}");
            }
        }
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow!("gpu server thread panicked"))??;
        }
        Ok(())
    }
}

impl Drop for GpuClient {
    fn drop(&mut self) {
        let _ = self.chan.send(&Request::Shutdown, &[]);
        let _ = self.join();
    }
}

/// Adopts the socket the parent left on `CHILD_SOCKET_FD`.
pub fn inherited_socket(fd: i32) -> OwnedFd {
    unsafe { OwnedFd::from_raw_fd(fd) }
}
