//! The compositor's seat: devices come from `drv-seatd` over the connection the spawner
//! handed us, so this process holds no device groups, no udev socket and no VT. The daemon
//! tells us which devices exist (in `Hello`, then as events) and we open only those. Implements
//! smithay's `Session` for libinput and the DRM code.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::rc::Rc;

use anyhow::{bail, Context as _};
use calloop::generic::Generic;
use calloop::{EventSource, Interest, Mode, Poll, PostAction, Readiness, Token, TokenFactory};
use drv_seat::{Device, Event, Request, Response, VERSION};
use rustix::fs::OFlags;
use smithay::backend::session::{AsErrno, Session};

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
    /// `control` is the connection the spawner handed us. Also returns the seat's devices as
    /// of now; the notifier carries changes.
    pub fn attach(control: OwnedFd) -> anyhow::Result<(Self, DrvSeatNotifier, Vec<Device>)> {
        drv_seat::send(&control, &Request::Hello { version: VERSION }, &[])?;
        let (reply, mut fds): (Response, _) = drv_seat::recv(&control)?;
        let (seat, active, devices) = match reply {
            Response::Hello {
                version,
                seat,
                active,
                devices,
            } if version == VERSION => (seat, active, devices),
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
        Ok((Self { inner }, notifier, devices))
    }

    fn call(&self, request: &Request) -> Result<(Response, Vec<OwnedFd>), Error> {
        drv_seat::send(&self.inner.control, request, &[])?;
        Ok(drv_seat::recv(&self.inner.control)?)
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
    type Event = Event;
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
        F: FnMut(Event, &mut ()),
    {
        let inner = &self.inner;
        self.events.process_events(readiness, token, |_, fd| loop {
            match drv_seat::recv::<Event>(&*fd) {
                Ok((event, _)) => {
                    match event {
                        Event::Enable => inner.active.set(true),
                        Event::Disable => inner.active.set(false),
                        _ => {}
                    }
                    callback(event, &mut ());
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
