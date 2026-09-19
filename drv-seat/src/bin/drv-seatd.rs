//! Root seat daemon: the only process on the seat. Holds the libseat session and udev,
//! announces the seat's DRM and evdev nodes to its one client (the compositor, whose
//! connection is the fd `compositor` from the supervisor), opens those nodes and only those,
//! passes the fds, and forwards hotplug and enable/disable. When the compositor hangs up we
//! exit: the supervisor restarts the whole set. The compositor holds no device groups, no
//! udev socket, and never sees a VT. Both processes here are sealed with the shared seccomp
//! allowlist: libseat's builtin backend forks the seatd server (the one that opens the
//! devices and drives the VT), so the seal goes on before that fork and the child inherits
//! it. What stays: opening existing device nodes and ttys (never creating files), DRM, evdev
//! and VT ioctls, reading udev's database. The parent loses fork and netlink again once the
//! seat and udev are up.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::process::ExitCode;
use std::rc::Rc;

use clap::Parser;
use drv_seat::{Device, DeviceKind, Event, Request, Response, VERSION};
use libseat::{Seat, SeatEvent};
use rustix::event::{PollFd, PollFlags};
use udev::{EventType, MonitorBuilder, MonitorSocket};

#[derive(Parser)]
#[command(name = "drv-seatd", about = "Hand seat devices to the compositor")]
struct Args {
    /// Switch to this VT before taking the seat, so the desktop is not on a VT a getty owns
    /// (agetty resets its VT to mode 0620, which a non-root seat daemon cannot open).
    #[arg(long)]
    vt: Option<u32>,
}

/// `VT_ACTIVATE` and `VT_WAITACTIVE` on `/dev/tty0` (`CAP_SYS_TTY_CONFIG`).
fn activate_vt(vt: u32) -> Result<(), String> {
    const VT_ACTIVATE: libc::c_ulong = 0x5606;
    const VT_WAITACTIVE: libc::c_ulong = 0x5607;
    let tty0 = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty0")
        .map_err(|e| format!("open /dev/tty0: {e}"))?;
    for request in [VT_ACTIVATE, VT_WAITACTIVE] {
        // SAFETY: an ioctl with an integer argument on our own fd.
        if unsafe { libc::ioctl(tty0.as_raw_fd(), request as _, vt as libc::c_int) } < 0 {
            return Err(format!("switching to VT {vt}: {}", io::Error::last_os_error()));
        }
    }
    Ok(())
}

/// The seat's devices as udev sees them: what the client is told about and allowed to open.
struct Devices {
    seat: String,
    monitor: MonitorSocket,
    known: HashMap<u64, Device>,
}

impl Devices {
    fn new(seat: &str) -> io::Result<Self> {
        // Listen before enumerating so nothing slips between the two.
        let monitor = MonitorBuilder::new()?
            .match_subsystem("drm")?
            .match_subsystem("input")?
            .listen()?;
        let mut known = HashMap::new();
        for subsystem in ["drm", "input"] {
            let mut enumerator = udev::Enumerator::new()?;
            enumerator.match_subsystem(subsystem)?;
            for device in enumerator.scan_devices()? {
                if let Some(device) = describe(seat, &device) {
                    known.insert(device.dev, device);
                }
            }
        }
        Ok(Devices {
            seat: seat.to_owned(),
            monitor,
            known,
        })
    }

    fn is_announced(&self, path: &str) -> bool {
        self.known.values().any(|d| d.path == path)
    }

    /// The seat's devices, in a stable order.
    fn snapshot(&self) -> Vec<Device> {
        let mut devices: Vec<_> = self.known.values().cloned().collect();
        devices.sort_by(|a, b| a.path.cmp(&b.path));
        devices
    }

    /// Applies queued udev events, telling the client if there is one.
    fn dispatch(&mut self, client: Option<&OwnedFd>) {
        let mut out = Vec::new();
        for event in self.monitor.iter() {
            let device = event.device();
            match event.event_type() {
                EventType::Add => {
                    if let Some(device) = describe(&self.seat, &device) {
                        self.known.insert(device.dev, device.clone());
                        out.push(Event::Added(device));
                    }
                }
                EventType::Remove => {
                    if let Some(dev) = device.devnum() {
                        if self.known.remove(&dev).is_some() {
                            out.push(Event::Removed { dev });
                        }
                    }
                }
                EventType::Change => {
                    if let Some(dev) = device.devnum() {
                        if self.known.get(&dev).is_some_and(|d| d.kind == DeviceKind::Drm) {
                            out.push(Event::Changed { dev });
                        }
                    }
                }
                _ => {}
            }
        }
        for event in out {
            drv_os::say!("drv-seatd: {event:?}");
            if let Some(client) = client {
                if let Err(err) = drv_seat::send(client, &event, &[]) {
                    drv_os::say!("drv-seatd: sending {event:?}: {err}");
                }
            }
        }
    }
}

/// What a udev device is to us, if it is one of this seat's card or event nodes.
fn describe(seat: &str, device: &udev::Device) -> Option<Device> {
    let path = device.devnode()?.to_str()?;
    if !drv_seat::is_allowed_device(path) {
        return None;
    }
    let dev = device.devnum()?;
    let device_seat = device
        .property_value("ID_SEAT")
        .and_then(|s| s.to_str())
        .unwrap_or("seat0");
    if device_seat != seat {
        return None;
    }
    let kind = if path.starts_with("/dev/dri/") {
        DeviceKind::Drm
    } else {
        DeviceKind::Input
    };
    let boot_vga = kind == DeviceKind::Drm
        && device
            .parent_with_subsystem("pci")
            .ok()
            .flatten()
            .and_then(|pci| pci.attribute_value("boot_vga").map(|v| v == "1"))
            .unwrap_or(false);
    Some(Device {
        kind,
        dev,
        path: path.to_owned(),
        boot_vga,
    })
}

struct Daemon {
    seat: Seat,
    events: Rc<RefCell<VecDeque<SeatEvent>>>,
    active: bool,
    devices: Devices,
}

impl Daemon {
    fn open() -> Result<Self, String> {
        let events = Rc::new(RefCell::new(VecDeque::new()));
        let queue = events.clone();
        let mut seat = Seat::open(move |_, event| queue.borrow_mut().push_back(event))
            .map_err(|err| format!("opening the seat: {err:?}"))?;
        seat.dispatch(0)
            .map_err(|err| format!("dispatching the seat: {err:?}"))?;
        let devices = Devices::new(seat.name())
            .map_err(|err| format!("listing the seat's devices: {err}"))?;
        let mut daemon = Daemon {
            seat,
            events,
            active: false,
            devices,
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
            drv_os::say!("drv-seatd: seat {msg:?}");
            if let Some(client) = client {
                if let Err(err) = drv_seat::send(client, &msg, &[]) {
                    drv_os::say!("drv-seatd: sending {msg:?}: {err}");
                }
            }
            if !active {
                // Acknowledge at once, like smithay does; the kernel revoked the devices.
                if let Err(err) = self.seat.disable() {
                    drv_os::say!("drv-seatd: acknowledging disable: {err:?}");
                }
            }
        }
    }

    fn dispatch(&mut self, client: Option<&OwnedFd>) {
        if let Err(err) = self.seat.dispatch(0) {
            drv_os::say!("drv-seatd: dispatching the seat: {err:?}");
        }
        self.drain(client);
    }

    /// Serves the compositor until it hangs up. `events` is the pair made before the seal;
    /// the compositor gets one end with the Hello.
    fn serve(&mut self, control: OwnedFd, events: (OwnedFd, OwnedFd)) -> io::Result<()> {
        let (hello, _): (Request, _) = drv_seat::recv(&control)?;
        match hello {
            Request::Hello { version } if version == VERSION => (),
            Request::Hello { version } => {
                let msg = format!("version {version} unsupported, want {VERSION}");
                drv_seat::send(&control, &Response::Error(msg.clone()), &[])?;
                return Err(io::Error::other(msg));
            }
            _ => return Err(io::Error::other("expected Hello")),
        }
        let (events, their_events) = events;
        let hello = Response::Hello {
            version: VERSION,
            seat: self.seat.name().to_owned(),
            active: self.active,
            devices: self.devices.snapshot(),
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
                PollFd::new(&self.devices.monitor, PollFlags::IN),
            ];
            rustix::event::poll(&mut fds, None)?;
            let (control_ready, seat_ready, udev_ready) = (
                !fds[0].revents().is_empty(),
                !fds[1].revents().is_empty(),
                !fds[2].revents().is_empty(),
            );
            drop(fds);
            if seat_ready {
                self.dispatch(Some(&events));
            }
            if udev_ready {
                self.devices.dispatch(Some(&events));
            }
            if !control_ready {
                continue;
            }
            let (request, _received): (Request, Vec<OwnedFd>) = match drv_seat::recv(&control) {
                Ok(r) => r,
                Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => break Ok(()),
                Err(err) => break Err(err),
            };
            let (reply, fd) = match request {
                Request::Open { path } => {
                    if !drv_seat::is_allowed_device(&path) || !self.devices.is_announced(&path) {
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
                drv_os::say!("drv-seatd: closing a device after hangup: {err:?}");
            }
        }
        result
    }
}

fn run(args: Args) -> Result<(), String> {
    let control = drv_os::fds::take()
        .and_then(|mut fds| fds.socket("compositor", drv_os::fds::Kind::SeqPacket))
        .map_err(|e| format!("the compositor's connection from the supervisor: {e}"))?;
    if let Some(vt) = args.vt {
        activate_vt(vt)?;
        drv_os::say!("drv-seatd: on VT {vt}");
    }
    let events = drv_seat::pair().map_err(|e| format!("the events socket pair: {e}"))?;
    let sealed = drv_os::seccomp::enabled();
    if sealed {
        lockdown(true).map_err(|e| format!("sealing: {e}"))?;
    } else {
        drv_os::say!("drv-seatd: seccomp disabled ({}=0)", drv_os::seccomp::DISABLE_ENV);
    }
    let mut daemon = Daemon::open()?;
    drv_os::say!(
        "drv-seatd: seat {} ready with {} devices",
        daemon.seat.name(),
        daemon.devices.known.len()
    );
    if sealed {
        lockdown(false).map_err(|e| format!("sealing: {e}"))?;
        drv_os::say!("drv-seatd: seccomp: syscall allowlist applied");
    }
    match daemon.serve(control, events) {
        Ok(()) => Err("the compositor hung up".to_owned()),
        Err(err) => Err(format!("compositor failed: {err}")),
    }
}

/// Seccomp. `opening` is the first, wider seal: libseat still has to fork its server and
/// udev to make its netlink socket. The second one, on the parent alone, drops those (filters
/// stack, the strictest verdict wins). Both keep opening the announced device nodes (and
/// ttys on VT switches), the ioctls on them, and reading udev's database.
fn lockdown(opening: bool) -> io::Result<()> {
    let mut allow = drv_os::seccomp::Allowlist::base()?;
    allow.open_existing()?;
    // 'd' is DRM (set/drop master), 'E' evdev (revoke), 'K' and 'V' the console: KD* keyboard
    // mode and VT_* switching.
    for ty in *b"dEKV" {
        allow.ioctl_type(ty)?;
    }
    if opening {
        // libseat forks its server with a socketpair to it; udev binds its netlink socket.
        allow.allow(&[libc::SYS_clone, libc::SYS_wait4, libc::SYS_socketpair, libc::SYS_bind]);
        #[cfg(target_arch = "x86_64")]
        allow.allow(&[libc::SYS_fork]);
        allow.socket(libc::AF_NETLINK)?;
        allow.reseal()?;
    }
    allow.apply("drv-seatd")
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            drv_os::say!("drv-seatd: {err}");
            ExitCode::FAILURE
        }
    }
}
