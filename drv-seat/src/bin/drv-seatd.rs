//! Root seat daemon: the only process on the seat. Holds the libseat session, opens DRM and
//! evdev nodes for one client at a time (the compositor, checked by UID), passes the fds, and
//! forwards enable/disable. The compositor holds no device groups and never sees a VT.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;

use clap::Parser;
use drv_seat::{Event, Request, Response, VERSION};
use libseat::{Seat, SeatEvent};
use rustix::event::{PollFd, PollFlags};

#[derive(Parser)]
#[command(name = "drv-seatd", about = "Hand seat devices to the compositor")]
struct Args {
    #[arg(long, default_value = drv_seat::DEFAULT_SOCKET)]
    socket: PathBuf,
    /// The one user allowed to connect.
    #[arg(long)]
    client: String,
}

struct Daemon {
    seat: Seat,
    events: Rc<RefCell<VecDeque<SeatEvent>>>,
    active: bool,
}

impl Daemon {
    fn open() -> Result<Self, String> {
        let events = Rc::new(RefCell::new(VecDeque::new()));
        let queue = events.clone();
        let mut seat = Seat::open(move |_, event| queue.borrow_mut().push_back(event))
            .map_err(|err| format!("opening the seat: {err:?}"))?;
        seat.dispatch(0)
            .map_err(|err| format!("dispatching the seat: {err:?}"))?;
        let mut daemon = Daemon {
            seat,
            events,
            active: false,
        };
        daemon.drain(None);
        Ok(daemon)
    }

    /// Applies queued seat events, telling the client if there is one.
    fn drain(&mut self, client: Option<&OwnedFd>) {
        let queued: Vec<_> = self.events.borrow_mut().drain(..).collect();
        for event in queued {
            let (active, msg) = match event {
                SeatEvent::Enable => (true, Event::Enable),
                SeatEvent::Disable => (false, Event::Disable),
            };
            self.active = active;
            eprintln!("drv-seatd: seat {msg:?}");
            if let Some(client) = client {
                if let Err(err) = drv_seat::send(client, &msg, &[]) {
                    eprintln!("drv-seatd: sending {msg:?}: {err}");
                }
            }
            if !active {
                // Acknowledge at once, like smithay does; the kernel revoked the devices.
                if let Err(err) = self.seat.disable() {
                    eprintln!("drv-seatd: acknowledging disable: {err:?}");
                }
            }
        }
    }

    fn dispatch(&mut self, client: Option<&OwnedFd>) {
        if let Err(err) = self.seat.dispatch(0) {
            eprintln!("drv-seatd: dispatching the seat: {err:?}");
        }
        self.drain(client);
    }

    /// Serves one client until it hangs up.
    fn serve(&mut self, control: OwnedFd) -> io::Result<()> {
        let (hello, _): (Request, _) = drv_seat::recv(&control)?;
        match hello {
            Request::Hello { version } if version == VERSION => (),
            Request::Hello { version } => {
                let msg = format!("version {version} unsupported, want {VERSION}");
                drv_seat::send(&control, &Response::Error(msg), &[])?;
                return Ok(());
            }
            _ => return Err(io::Error::other("expected Hello")),
        }
        let (events, their_events) = drv_seat::pair()?;
        let hello = Response::Hello {
            version: VERSION,
            seat: self.seat.name().to_owned(),
            active: self.active,
        };
        drv_seat::send(&control, &hello, &[their_events.as_fd()])?;
        drop(their_events);

        let mut devices: HashMap<u32, libseat::Device> = HashMap::new();
        let mut next_id = 1u32;
        let result = loop {
            let seat_fd = match self.seat.get_fd() {
                Ok(fd) => fd.try_clone_to_owned()?,
                Err(err) => break Err(io::Error::other(format!("seat fd: {err:?}"))),
            };
            let mut fds = [
                PollFd::new(&control, PollFlags::IN),
                PollFd::new(&seat_fd, PollFlags::IN),
            ];
            rustix::event::poll(&mut fds, None)?;
            let (control_ready, seat_ready) = (
                !fds[0].revents().is_empty(),
                !fds[1].revents().is_empty(),
            );
            drop(fds);
            if seat_ready {
                self.dispatch(Some(&events));
            }
            if !control_ready {
                continue;
            }
            let (request, _): (Request, _) = match drv_seat::recv(&control) {
                Ok(r) => r,
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
                Err(err) => break Err(err),
            };
            let (reply, fd) = match request {
                Request::Open { path } => {
                    if !drv_seat::is_allowed_device(&path) {
                        (Response::Error(format!("{path} is not a seat device")), None)
                    } else {
                        match self.seat.open_device(&path) {
                            Ok(device) => {
                                let id = next_id;
                                next_id += 1;
                                let fd = device.as_fd().try_clone_to_owned()?;
                                devices.insert(id, device);
                                (Response::Opened { id }, Some(fd))
                            }
                            Err(err) => (Response::Error(format!("open {path}: {err:?}")), None),
                        }
                    }
                }
                Request::Close { id } => match devices.remove(&id) {
                    Some(device) => match self.seat.close_device(device) {
                        Ok(()) => (Response::Done, None),
                        Err(err) => (Response::Error(format!("close: {err:?}")), None),
                    },
                    None => (Response::Error(format!("no device {id}")), None),
                },
                Request::SwitchVt { vt } => match self.seat.switch_session(vt) {
                    Ok(()) => (Response::Done, None),
                    Err(err) => (Response::Error(format!("switch to vt {vt}: {err:?}")), None),
                },
                Request::Hello { .. } => (Response::Error("already said hello".into()), None),
            };
            let fds: Vec<_> = fd.iter().map(|fd| fd.as_fd()).collect();
            if let Err(err) = drv_seat::send(&control, &reply, &fds) {
                break Err(err);
            }
        };
        for (_, device) in devices.drain() {
            if let Err(err) = self.seat.close_device(device) {
                eprintln!("drv-seatd: closing a device after hangup: {err:?}");
            }
        }
        result
    }

    /// Waits for a connection, keeping the seat serviced meanwhile.
    fn accept(&mut self, listener: &OwnedFd) -> io::Result<OwnedFd> {
        loop {
            let seat_fd = self
                .seat
                .get_fd()
                .map_err(|err| io::Error::other(format!("seat fd: {err:?}")))?
                .try_clone_to_owned()?;
            let mut fds = [
                PollFd::new(listener, PollFlags::IN),
                PollFd::new(&seat_fd, PollFlags::IN),
            ];
            rustix::event::poll(&mut fds, None)?;
            let (listener_ready, seat_ready) = (
                !fds[0].revents().is_empty(),
                !fds[1].revents().is_empty(),
            );
            drop(fds);
            if seat_ready {
                self.dispatch(None);
            }
            if listener_ready {
                return Ok(rustix::net::accept(listener)?);
            }
        }
    }
}

fn user_id(name: &str) -> Result<u32, String> {
    let cname = CString::new(name).map_err(|_| format!("bad user name {name:?}"))?;
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8; 16 * 1024];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the call; buf outlives the use of `pwd`.
    let rc = unsafe {
        libc::getpwnam_r(
            cname.as_ptr(),
            &mut pwd,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 {
        return Err(format!("getpwnam {name:?}: {}", io::Error::from_raw_os_error(rc)));
    }
    if result.is_null() {
        return Err(format!("no such user {name:?}"));
    }
    Ok(pwd.pw_uid)
}

fn run(args: Args) -> Result<(), String> {
    let client_uid = user_id(&args.client)?;
    let mut daemon = Daemon::open()?;
    let listener = drv_seat::listen(&args.socket)
        .map_err(|err| format!("listening on {:?}: {err}", args.socket))?;
    // The directory is what keeps others away from the socket; the UID check is the guard.
    std::fs::set_permissions(&args.socket, std::fs::Permissions::from_mode(0o666))
        .map_err(|err| format!("chmod {:?}: {err}", args.socket))?;
    eprintln!(
        "drv-seatd: seat {} ready, serving uid {client_uid}",
        daemon.seat.name()
    );
    loop {
        let control = daemon
            .accept(&listener)
            .map_err(|err| format!("accepting: {err}"))?;
        let peer = match rustix::net::sockopt::socket_peercred(&control) {
            Ok(cred) => cred.uid.as_raw(),
            Err(err) => {
                eprintln!("drv-seatd: no peer credentials: {err}");
                continue;
            }
        };
        if peer != client_uid {
            eprintln!("drv-seatd: refusing uid {peer}");
            continue;
        }
        eprintln!("drv-seatd: client connected");
        match daemon.serve(control) {
            Ok(()) => eprintln!("drv-seatd: client hung up"),
            Err(err) => eprintln!("drv-seatd: client failed: {err}"),
        }
    }
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("drv-seatd: {err}");
            ExitCode::FAILURE
        }
    }
}
