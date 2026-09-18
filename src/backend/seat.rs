//! The compositor's seat: devices come from `drv-seatd` over the connection the spawner
//! handed us, so this process holds no device groups and no VT. Implements smithay's `Session` for libinput and the DRM code.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::Path;
use std::rc::Rc;

use anyhow::{bail, Context as _};
use calloop::generic::Generic;
use calloop::{EventSource, Interest, Mode, Poll, PostAction, Readiness, Token, TokenFactory};
use drv_seat::{Event, Request, Response, VERSION};
use rustix::fs::OFlags;
use smithay::backend::session::{AsErrno, Event as SessionEvent, Session};

struct Inner {
    control: OwnedFd,
    seat: String,
    active: Cell<bool>,
    /// Daemon-side id of each fd we were handed, for `Close`.
    devices: RefCell<HashMap<RawFd, u32>>,
}

#[derive(Clone)]
pub struct DrvSeatSession {
    inner: Rc<Inner>,
}

pub struct DrvSeatNotifier {
    events: Generic<OwnedFd>,
    inner: Rc<Inner>,
}

#[derive(Debug)]
pub struct Error(String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

impl AsErrno for Error {
    fn as_errno(&self) -> Option<i32> {
        None
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error(err.to_string())
    }
}

impl DrvSeatSession {
    /// `control` is the connection the spawner handed us.
    pub fn attach(control: OwnedFd) -> anyhow::Result<(Self, DrvSeatNotifier)> {
        drv_seat::send(&control, &Request::Hello { version: VERSION }, &[])?;
        let (reply, mut fds): (Response, _) = drv_seat::recv(&control)?;
        let (seat, active) = match reply {
            Response::Hello {
                version,
                seat,
                active,
            } if version == VERSION => (seat, active),
            Response::Hello { version, .. } => {
                bail!("seat daemon speaks version {version}, we speak {VERSION}")
            }
            other => bail!("unexpected reply to Hello: {other:?}"),
        };
        let events = fds.pop().context("seat daemon sent no events socket")?;
        rustix::io::ioctl_fionbio(&events, true)?;
        debug!("seat {seat} from the seat daemon, active: {active}");

        let inner = Rc::new(Inner {
            control,
            seat,
            active: Cell::new(active),
            devices: RefCell::new(HashMap::new()),
        });
        let notifier = DrvSeatNotifier {
            events: Generic::new(events, Interest::READ, Mode::Level),
            inner: inner.clone(),
        };
        Ok((Self { inner }, notifier))
    }

    fn call(&self, request: &Request) -> Result<(Response, Vec<OwnedFd>), Error> {
        drv_seat::send(&self.inner.control, request, &[])?;
        Ok(drv_seat::recv(&self.inner.control)?)
    }

    /// Has the seat daemon fork the GPU process with these DRM devices; returns the socket to
    /// it and its pid.
    pub fn start_gpu(
        &self,
        devices: &[(u64, BorrowedFd<'_>)],
        render_node_hint: Option<u64>,
    ) -> anyhow::Result<(OwnedFd, u32)> {
        let request = Request::StartGpu {
            devices: devices.iter().map(|(dev, _)| *dev).collect(),
            render_node_hint,
        };
        let fds: Vec<_> = devices.iter().map(|(_, fd)| *fd).collect();
        drv_seat::send(&self.inner.control, &request, &fds)?;
        let (reply, mut received): (Response, Vec<OwnedFd>) = drv_seat::recv(&self.inner.control)?;
        match reply {
            Response::GpuStarted { pid } => {
                let socket = received.pop().context("no socket with GpuStarted")?;
                Ok((socket, pid))
            }
            Response::Error(msg) => bail!("seat daemon refused to start the GPU process: {msg}"),
            other => bail!("unexpected reply to StartGpu: {other:?}"),
        }
    }

    fn expect_done(&self, request: &Request) -> Result<(), Error> {
        match self.call(request)? {
            (Response::Done, _) => Ok(()),
            (Response::Error(msg), _) => Err(Error(msg)),
            (other, _) => Err(Error(format!("unexpected reply: {other:?}"))),
        }
    }
}

impl Session for DrvSeatSession {
    type Error = Error;

    fn open(&mut self, path: &Path, _flags: OFlags) -> Result<OwnedFd, Self::Error> {
        let path = path
            .to_str()
            .ok_or_else(|| Error("non-UTF-8 path".into()))?;
        let request = Request::Open {
            path: path.to_owned(),
        };
        match self.call(&request)? {
            (Response::Opened { id }, mut fds) => {
                let fd = fds.pop().ok_or_else(|| Error("no fd with Opened".into()))?;
                self.inner.devices.borrow_mut().insert(fd.as_raw_fd(), id);
                Ok(fd)
            }
            (Response::Error(msg), _) => Err(Error(msg)),
            (other, _) => Err(Error(format!("unexpected reply: {other:?}"))),
        }
    }

    fn close(&mut self, fd: OwnedFd) -> Result<(), Self::Error> {
        let id = self.inner.devices.borrow_mut().remove(&fd.as_raw_fd());
        drop(fd);
        match id {
            Some(id) => self.expect_done(&Request::Close { id }),
            None => Ok(()),
        }
    }

    fn change_vt(&mut self, vt: i32) -> Result<(), Self::Error> {
        self.expect_done(&Request::SwitchVt { vt })
    }

    fn is_active(&self) -> bool {
        self.inner.active.get()
    }

    fn seat(&self) -> String {
        self.inner.seat.clone()
    }
}

impl EventSource for DrvSeatNotifier {
    type Event = SessionEvent;
    type Metadata = ();
    type Ret = ();
    type Error = io::Error;

    fn process_events<F>(
        &mut self,
        readiness: Readiness,
        token: Token,
        mut callback: F,
    ) -> io::Result<PostAction>
    where
        F: FnMut(SessionEvent, &mut ()),
    {
        let inner = &self.inner;
        self.events.process_events(readiness, token, |_, fd| loop {
            match drv_seat::recv::<Event>(&*fd) {
                Ok((Event::Enable, _)) => {
                    inner.active.set(true);
                    callback(SessionEvent::ActivateSession, &mut ());
                }
                Ok((Event::Disable, _)) => {
                    inner.active.set(false);
                    callback(SessionEvent::PauseSession, &mut ());
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(PostAction::Continue)
                }
                Err(err) => {
                    // Devices we hold keep working; only VT switching is gone.
                    error!("lost the seat daemon: {err}");
                    return Ok(PostAction::Remove);
                }
            }
        })
    }

    fn register(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        self.events.register(poll, factory)
    }

    fn reregister(&mut self, poll: &mut Poll, factory: &mut TokenFactory) -> calloop::Result<()> {
        self.events.reregister(poll, factory)
    }

    fn unregister(&mut self, poll: &mut Poll) -> calloop::Result<()> {
        self.events.unregister(poll)
    }
}
