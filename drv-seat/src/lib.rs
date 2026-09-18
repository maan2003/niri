//! Seat protocol: the compositor gets its DRM and evdev fds from a root daemon instead of
//! holding device groups itself. One `SOCK_SEQPACKET` connection, handed to both sides by the
//! spawner, requests in lock step, a reply may carry one fd. The daemon also owns udev: `Hello`
//! answers with the seat's current devices, and hotplug plus session enable/disable arrive on
//! a second socket handed over with `Hello`, so they never interleave with replies. Only an
//! announced device can be opened.

use serde::{Deserialize, Serialize};

/// Bumped on any incompatible change; the daemon answers `Hello` with its own version.
pub const VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Request {
    Hello { version: u32 },
    /// Open a device the daemon announced (in `Hello` or `Event::Added`).
    Open { path: String },
    /// Close a device opened here. The client drops its own fd itself.
    Close { id: u32 },
    SwitchVt { vt: i32 },
    /// Fork the GPU process as its own user with these DRM devices (one fd attached per
    /// entry, in order) and hand back the core's end of their socket. What runs is the
    /// daemon's configured executable, never the caller's choice.
    StartGpu {
        devices: Vec<u64>,
        render_node_hint: Option<u64>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    /// Carries the events socket as its fd. `devices` is the seat right now; changes follow
    /// as events.
    Hello {
        version: u32,
        seat: String,
        active: bool,
        devices: Vec<Device>,
    },
    /// Carries the device fd.
    Opened { id: u32 },
    /// Carries the core's socket to the GPU process.
    GpuStarted { pid: u32 },
    Done,
    Error(String),
}

/// Session state changes and hotplug, on the events socket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Event {
    /// Devices are live (again).
    Enable,
    /// The seat went elsewhere (VT switch): devices are revoked until `Enable`.
    Disable,
    Added(Device),
    /// A DRM device's connectors changed.
    Changed { dev: u64 },
    Removed { dev: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceKind {
    /// `/dev/dri/cardN`.
    Drm,
    /// `/dev/input/eventN`.
    Input,
}

/// A device on the seat, as udev describes it to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Device {
    pub kind: DeviceKind,
    /// `st_rdev` of the node.
    pub dev: u64,
    pub path: String,
    /// The card on the firmware's boot display adapter: where to render absent other
    /// preference.
    pub boot_vga: bool,
}

/// Only the seat's display and input nodes, by their canonical names: no render nodes, no
/// symlinks, nothing else under `/dev`.
pub fn is_allowed_device(path: &str) -> bool {
    let number = path
        .strip_prefix("/dev/dri/card")
        .or_else(|| path.strip_prefix("/dev/input/event"));
    matches!(number, Some(n) if !n.is_empty() && n.len() <= 4 && n.bytes().all(|b| b.is_ascii_digit()))
}

pub use drv_policy::seq::{pair, recv, send};

#[cfg(test)]
mod tests {
    use std::io;
    use std::os::fd::AsFd;

    use super::*;

    #[test]
    fn device_allowlist() {
        assert!(is_allowed_device("/dev/dri/card0"));
        assert!(is_allowed_device("/dev/input/event12"));
        assert!(!is_allowed_device("/dev/dri/renderD128"));
        assert!(!is_allowed_device("/dev/dri/card"));
        assert!(!is_allowed_device("/dev/dri/card0/../renderD128"));
        assert!(!is_allowed_device("/dev/input/mice"));
        assert!(!is_allowed_device("/dev/tty0"));
    }

    #[test]
    fn roundtrip_with_fd() {
        let (a, b) = pair().unwrap();
        let (x, _y) = pair().unwrap();
        send(&a, &Response::Opened { id: 7 }, &[x.as_fd()]).unwrap();
        let (msg, fds): (Response, _) = recv(&b).unwrap();
        assert_eq!(msg, Response::Opened { id: 7 });
        assert_eq!(fds.len(), 1);
        drop(a);
        assert_eq!(
            recv::<Response>(&b).unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
