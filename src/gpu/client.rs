//! Core side of the GPU-process connection: spawning and request/reply plumbing.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread::JoinHandle;

use anyhow::{anyhow, bail, Context};
use serde::Serialize;

use super::protocol::{self, Caps, Event, Image, Rect, Request, ShaderKind, TexId, PROTOCOL_VERSION};
use super::transport::Channel;

pub const CHILD_SOCKET_FD: i32 = 3;

pub struct GpuClient {
    chan: Channel,
    child: Option<Child>,
    thread: Option<JoinHandle<anyhow::Result<()>>>,
    caps: Caps,
}

impl GpuClient {
    /// Spawns `exe gpu-process` with the socket on fd 3.
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
            caps: Caps::default(),
        };
        client.handshake()?;
        Ok(client)
    }

    /// Runs the GPU server on a thread in this process. For tests and debugging only.
    pub fn spawn_thread() -> anyhow::Result<Self> {
        let (ours, theirs) = Channel::pair()?;
        let thread = std::thread::Builder::new()
            .name("gpu-server".into())
            .spawn(move || super::server::run(theirs.into_fd()))?;
        let mut client = Self {
            chan: ours,
            child: None,
            thread: Some(thread),
            caps: Caps::default(),
        };
        client.handshake()?;
        Ok(client)
    }

    pub fn caps(&self) -> &Caps {
        &self.caps
    }

    fn handshake(&mut self) -> anyhow::Result<()> {
        let (event, _): (Event, _) = self.chan.recv().context("waiting for gpu process")?;
        match event {
            Event::Ready { version, caps } if version == PROTOCOL_VERSION => {
                self.caps = caps;
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
            bail!("{message}");
        }
        Ok(event)
    }

    fn expect_ack(event: Event) -> anyhow::Result<()> {
        match event {
            Event::Ack => Ok(()),
            other => Err(anyhow!("expected Ack, got {other:?}")),
        }
    }

    pub fn execute(&mut self, commands: Vec<protocol::Command>, fds: &[OwnedFd]) -> anyhow::Result<()> {
        let fds: Vec<BorrowedFd<'_>> = fds.iter().map(|fd| fd.as_fd()).collect();
        Self::expect_ack(self.request(&Request::Execute { commands }, &fds)?)
    }

    pub fn read_texture(&mut self, id: TexId, region: Rect<i32>, format: u32) -> anyhow::Result<Image> {
        match self.request(&Request::ReadTexture { id, region, format }, &[])? {
            Event::Image(image) => Ok(image),
            other => Err(anyhow!("expected Image, got {other:?}")),
        }
    }

    /// Returns whether the shader is now available.
    pub fn set_custom_shader(&mut self, kind: ShaderKind, src: Option<&str>) -> anyhow::Result<bool> {
        let src = src.map(str::to_owned);
        Self::expect_ack(self.request(&Request::SetCustomShader { kind, src }, &[])?)?;
        Ok(true)
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

/// Wraps the socket fd inherited from the parent (see [`CHILD_SOCKET_FD`]).
pub fn inherited_socket(fd: i32) -> OwnedFd {
    unsafe { OwnedFd::from_raw_fd(fd) }
}
