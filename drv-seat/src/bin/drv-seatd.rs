//! Root seat daemon: the only process on the seat. Holds the libseat session, opens DRM and
//! evdev nodes for one client at a time (the compositor, whose connection the spawner hands
//! down the wire), passes the fds, and forwards enable/disable. The compositor holds no
//! device groups and never sees a VT.
//!
//! It also forks the GPU process on the compositor's request, as its own user, so the
//! compositor never execs anything and a Mesa exploit lands in a UID that holds no client
//! connection.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::rc::Rc;

use clap::Parser;
use drv_policy::wire::{self, Attach};
use drv_seat::{Event, Request, Response, VERSION};
use libseat::{Seat, SeatEvent};
use rustix::event::{PollFd, PollFlags};

#[derive(Parser)]
#[command(name = "drv-seatd", about = "Hand seat devices to the compositor")]
struct Args {
    /// The compositor binary, run as `<exec> gpu-process` for `StartGpu`. Without it the
    /// request is refused.
    #[arg(long)]
    gpu_exec: Option<PathBuf>,
    /// User the GPU process runs as.
    #[arg(long, default_value = "drv-gpu")]
    gpu_user: String,
    /// Supplementary group for the GPU process (`render` for the render nodes). Repeatable.
    #[arg(long = "gpu-group")]
    gpu_groups: Vec<String>,
}

/// Everything the GPU process gets, fixed at startup.
struct GpuSpawn {
    exec: PathBuf,
    uid: u32,
    gid: u32,
    groups: Vec<u32>,
}

impl GpuSpawn {
    /// Forks the GPU process with the socket on fd 3 and `devices` on 4, 5, ...; returns the
    /// core's end of the socket and the pid.
    fn start(
        &self,
        devices: &[(u64, OwnedFd)],
        render_node_hint: Option<u64>,
    ) -> io::Result<(OwnedFd, u32)> {
        let (ours, theirs) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::STREAM,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )?;
        // Everything the child inherits is first moved above the target numbers, so the dup2s
        // in the child never clobber each other (and a same-number dup2 would keep CLOEXEC).
        let mut high = vec![dup_high(theirs.as_fd())?];
        let mut cmd = Command::new(&self.exec);
        cmd.arg("gpu-process")
            .arg("--socket-fd")
            .arg("3")
            .arg("--mode")
            .arg("drm");
        if let Some(hint) = render_node_hint {
            cmd.arg("--render-node-hint").arg(hint.to_string());
        }
        for (i, (dev, fd)) in devices.iter().enumerate() {
            cmd.arg("--device").arg(format!("{dev}:{}", 4 + i));
            high.push(dup_high(fd.as_fd())?);
        }
        cmd.env_clear()
            // No home directory in the sandbox, so no shader cache on disk.
            .env("MESA_SHADER_CACHE_DISABLE", "true")
            .env("MESA_GLSL_CACHE_DISABLE", "true")
            .stdin(Stdio::null());
        for var in ["NIRI_GPU_SANDBOX", "RUST_LOG", "RUST_BACKTRACE"] {
            if let Some(value) = std::env::var_os(var) {
                cmd.env(var, value);
            }
        }
        let raw: Vec<i32> = high.iter().map(|fd| fd.as_raw_fd()).collect();
        let (uid, gid, groups) = (self.uid, self.gid, self.groups.clone());
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(move || {
                for (i, fd) in raw.iter().enumerate() {
                    if libc::dup2(*fd, 3 + i as i32) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                if libc::setgroups(groups.len(), groups.as_ptr()) < 0
                    || libc::setresgid(gid, gid, gid) < 0
                    || libc::setresuid(uid, uid, uid) < 0
                    || libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) < 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        drop(high);
        drop(theirs);
        // Reaped by SIGCHLD being ignored; the process ends when the core closes the socket.
        Ok((ours, child.id()))
    }
}

fn dup_high(fd: BorrowedFd<'_>) -> io::Result<OwnedFd> {
    let new = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if new < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(new) })
}

struct Daemon {
    seat: Seat,
    events: Rc<RefCell<VecDeque<SeatEvent>>>,
    active: bool,
    gpu: Option<GpuSpawn>,
}

impl Daemon {
    fn open(gpu: Option<GpuSpawn>) -> Result<Self, String> {
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
            gpu,
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

    /// Serves one client until it hangs up, or until the spawner attaches its successor
    /// (which is then returned).
    fn serve(&mut self, control: OwnedFd, wire: &OwnedFd) -> io::Result<Option<OwnedFd>> {
        let (hello, _): (Request, _) = drv_seat::recv(&control)?;
        match hello {
            Request::Hello { version } if version == VERSION => (),
            Request::Hello { version } => {
                let msg = format!("version {version} unsupported, want {VERSION}");
                drv_seat::send(&control, &Response::Error(msg), &[])?;
                return Ok(None);
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
                PollFd::new(wire, PollFlags::IN),
            ];
            rustix::event::poll(&mut fds, None)?;
            let (control_ready, seat_ready, wire_ready) = (
                !fds[0].revents().is_empty(),
                !fds[1].revents().is_empty(),
                !fds[2].revents().is_empty(),
            );
            drop(fds);
            if seat_ready {
                self.dispatch(Some(&events));
            }
            if wire_ready {
                match next_client(wire) {
                    Ok(Some(next)) => break Ok(Some(next)),
                    Ok(None) => {}
                    Err(err) => break Err(err),
                }
            }
            if !control_ready {
                continue;
            }
            let (request, received): (Request, Vec<OwnedFd>) = match drv_seat::recv(&control) {
                Ok(r) => r,
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break Ok(None),
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
                Request::StartGpu {
                    devices,
                    render_node_hint,
                } => match &self.gpu {
                    None => (Response::Error("no GPU process configured".into()), None),
                    Some(_) if devices.len() != received.len() => (
                        Response::Error(format!(
                            "{} devices but {} fds",
                            devices.len(),
                            received.len()
                        )),
                        None,
                    ),
                    Some(gpu) => {
                        let devices: Vec<_> = devices.into_iter().zip(received).collect();
                        match gpu.start(&devices, render_node_hint) {
                            Ok((socket, pid)) => {
                                eprintln!("drv-seatd: GPU process started, pid {pid}");
                                (Response::GpuStarted { pid }, Some(socket))
                            }
                            Err(err) => (Response::Error(format!("starting the GPU process: {err}")), None),
                        }
                    }
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

    /// Waits for the spawner to attach a compositor, keeping the seat serviced meanwhile.
    fn accept(&mut self, wire: &OwnedFd) -> io::Result<OwnedFd> {
        loop {
            let seat_fd = self
                .seat
                .get_fd()
                .map_err(|err| io::Error::other(format!("seat fd: {err:?}")))?
                .try_clone_to_owned()?;
            let mut fds = [
                PollFd::new(wire, PollFlags::IN),
                PollFd::new(&seat_fd, PollFlags::IN),
            ];
            rustix::event::poll(&mut fds, None)?;
            let (wire_ready, seat_ready) = (
                !fds[0].revents().is_empty(),
                !fds[1].revents().is_empty(),
            );
            drop(fds);
            if seat_ready {
                self.dispatch(None);
            }
            if wire_ready {
                if let Some(control) = next_client(wire)? {
                    return Ok(control);
                }
            }
        }
    }
}

/// One message off the wire: a compositor connection, or nothing worth having. A closed wire
/// means the spawner is gone, and so are we.
fn next_client(wire: &OwnedFd) -> io::Result<Option<OwnedFd>> {
    match wire::recv_attach(wire) {
        Ok((Attach::Compositor, control)) => Ok(Some(control)),
        Ok((other, _)) => {
            eprintln!("drv-seatd: ignoring {other:?} on the wire");
            Ok(None)
        }
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => {
            Err(io::Error::other("the spawner closed the wire"))
        }
        Err(err) => Err(err),
    }
}

fn group_id(name: &str) -> Result<u32, String> {
    let cname = CString::new(name).map_err(|_| format!("bad group name {name:?}"))?;
    let mut grp: libc::group = unsafe { std::mem::zeroed() };
    let mut buf = vec![0u8; 16 * 1024];
    let mut result: *mut libc::group = std::ptr::null_mut();
    // SAFETY: all pointers are valid for the call; buf outlives the use of `grp`.
    let rc = unsafe {
        libc::getgrnam_r(
            cname.as_ptr(),
            &mut grp,
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            &mut result,
        )
    };
    if rc != 0 {
        return Err(format!("getgrnam {name:?}: {}", io::Error::from_raw_os_error(rc)));
    }
    if result.is_null() {
        return Err(format!("no such group {name:?}"));
    }
    Ok(grp.gr_gid)
}

fn user_id(name: &str) -> Result<(u32, u32), String> {
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
    Ok((pwd.pw_uid, pwd.pw_gid))
}

fn run(args: Args) -> Result<(), String> {
    let wire = wire::take().ok_or("no wire on fd 3: drv-seatd runs under drv-spawnd")?;
    let gpu = match args.gpu_exec {
        Some(exec) => {
            let (uid, gid) = user_id(&args.gpu_user)?;
            if uid == 0 {
                return Err("the GPU user must not be root".into());
            }
            let groups = args
                .gpu_groups
                .iter()
                .map(|g| group_id(g))
                .collect::<Result<Vec<_>, _>>()?;
            Some(GpuSpawn {
                exec,
                uid,
                gid,
                groups,
            })
        }
        None => None,
    };
    // GPU processes are not waited for.
    unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) };
    let mut daemon = Daemon::open(gpu)?;
    eprintln!("drv-seatd: seat {} ready", daemon.seat.name());
    let mut next = None;
    loop {
        let control = match next.take() {
            Some(control) => control,
            None => daemon
                .accept(&wire)
                .map_err(|err| format!("waiting for a compositor: {err}"))?,
        };
        eprintln!("drv-seatd: compositor attached");
        match daemon.serve(control, &wire) {
            Ok(None) => eprintln!("drv-seatd: compositor hung up"),
            Ok(Some(successor)) => {
                eprintln!("drv-seatd: a new compositor was attached");
                next = Some(successor);
            }
            Err(err) => eprintln!("drv-seatd: compositor failed: {err}"),
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
