//! The one root piece of app launching, kept dumb on purpose: it takes `{uid, argv, env}` from
//! an allowed peer, checks the UID is in that peer's range, becomes the UID and execs. No config
//! files, no policy, no idea what an "app" is. The identity daemon is the brain; a bug here is
//! reachable only through it.
//!
//! Zygote on Android has the same shape: root, forks on command, only `system` may connect.

use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{io, thread};

use niri_policy::rpc::{read_msg, write_msg};
use rustix::fs::{Mode, OFlags};
use rustix::net::UCred;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub uid: u32,
    /// `argv[0]` is looked up in `PATH` from `env`.
    pub argv: Vec<String>,
    /// The child's whole environment, plus `HOME` and `XDG_RUNTIME_DIR` which the forker sets
    /// for range UIDs.
    pub env: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Response {
    Forked { pid: u32 },
    Error(String),
}

/// One connection, one request. The identity daemon calls this per launch.
pub fn fork(socket: &Path, request: &Request) -> io::Result<u32> {
    let stream = UnixStream::connect(socket)?;
    write_msg(&stream, request)?;
    match read_msg::<Response>(&stream)? {
        Response::Forked { pid } => Ok(pid),
        Response::Error(err) => Err(io::Error::other(err)),
    }
}

/// Who may ask, and for which UIDs. A peer may always fork as itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allowed {
    pub peer: u32,
    pub start: u32,
    pub count: u32,
}

impl Allowed {
    /// `peer:start:count`, e.g. `1000:100000:65536`.
    pub fn parse(s: &str) -> Result<Self, String> {
        let parts: Vec<_> = s.split(':').collect();
        let [peer, start, count] = parts.as_slice() else {
            return Err(format!("expected peer:start:count, got {s:?}"));
        };
        let num = |x: &str| x.parse::<u32>().map_err(|e| format!("{x:?}: {e}"));
        Ok(Self {
            peer: num(peer)?,
            start: num(start)?,
            count: num(count)?,
        })
    }

    fn covers(&self, uid: u32) -> bool {
        self.start <= uid && (uid as u64) < self.start as u64 + self.count as u64
    }
}

pub struct Server {
    pub allowed: Vec<Allowed>,
    /// `<runtime_base>/<uid>` becomes the child's `XDG_RUNTIME_DIR`.
    pub runtime_base: PathBuf,
    /// `<home_base>/<uid>` becomes the child's `HOME` and working directory.
    pub home_base: PathBuf,
}

impl Server {
    pub fn serve(self, listener: UnixListener) -> io::Result<()> {
        let server = std::sync::Arc::new(self);
        loop {
            let (stream, _) = listener.accept()?;
            let server = server.clone();
            thread::spawn(move || {
                let _ = server.handle(stream);
            });
        }
    }

    pub fn handle(&self, stream: UnixStream) -> io::Result<()> {
        let peer = rustix::net::sockopt::socket_peercred(&stream)?;
        let request: Request = read_msg(&stream)?;
        let response = match self.launch(&peer, &request) {
            Ok(pid) => Response::Forked { pid },
            Err(err) => {
                eprintln!(
                    "niri-forker: refused uid {} from peer uid {}: {err}",
                    request.uid,
                    peer.uid.as_raw()
                );
                Response::Error(err)
            }
        };
        write_msg(&stream, &response)
    }

    fn launch(&self, peer: &UCred, request: &Request) -> Result<u32, String> {
        let peer_uid = peer.uid.as_raw();
        let allowed = self
            .allowed
            .iter()
            .find(|a| a.peer == peer_uid)
            .ok_or_else(|| format!("uid {peer_uid} may not use the forker"))?;
        if request.argv.is_empty() {
            return Err("empty argv".to_owned());
        }

        let uid = request.uid;
        let as_self = uid == peer_uid;
        if !as_self && !allowed.covers(uid) {
            return Err(format!("uid {uid} is outside the peer's range"));
        }
        let we_are_root = rustix::process::getuid().is_root();
        if !as_self && !we_are_root {
            return Err("forker is not root, can only fork as the peer itself".to_owned());
        }

        let mut command = Command::new(&request.argv[0]);
        command
            .args(&request.argv[1..])
            .env_clear()
            .envs(request.env.iter().cloned());
        command.stdin(Stdio::null());

        let gid = if as_self { peer.gid.as_raw() } else { uid };
        if !as_self {
            let runtime = self.owned_dir(&self.runtime_base, uid, gid)?;
            let home = self.owned_dir(&self.home_base, uid, gid)?;
            command.env("XDG_RUNTIME_DIR", &runtime).env("HOME", &home);
            command.current_dir(&home);
        }

        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(move || {
                if !as_self {
                    if libc::setgroups(1, &gid) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::setresgid(gid, gid, gid) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::setresuid(uid, uid, uid) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::getuid() != uid || libc::geteuid() != uid {
                        return Err(io::Error::other("uid did not change"));
                    }
                }
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }

        let mut child = command
            .spawn()
            .map_err(|err| format!("spawn {:?}: {err}", request.argv[0]))?;
        let pid = child.id();
        let name = request.argv[0].clone();
        // Reap it, or every launched app leaves a zombie under us.
        thread::spawn(move || match child.wait() {
            Ok(status) => eprintln!("niri-forker: {name} (pid {pid}, uid {uid}) exited: {status}"),
            Err(err) => eprintln!("niri-forker: waiting for {name} (pid {pid}): {err}"),
        });
        Ok(pid)
    }

    /// `<base>/<uid>`, mode 0700, owned by the UID. Created on first launch.
    fn owned_dir(&self, base: &Path, uid: u32, gid: u32) -> Result<PathBuf, String> {
        let dir = base.join(uid.to_string());
        let mkdir = |path: &Path, mode: Mode| match rustix::fs::mkdir(path, mode) {
            Ok(()) => Ok(()),
            Err(rustix::io::Errno::EXIST) => Ok(()),
            Err(err) => Err(format!("mkdir {}: {err}", path.display())),
        };
        mkdir(base, Mode::from_raw_mode(0o711))?;
        mkdir(&dir, Mode::from_raw_mode(0o700))?;
        let fd = rustix::fs::open(&dir, OFlags::DIRECTORY | OFlags::NOFOLLOW, Mode::empty())
            .map_err(|err| format!("open {}: {err}", dir.display()))?;
        rustix::fs::fchown(
            &fd,
            Some(rustix::process::Uid::from_raw(uid)),
            Some(rustix::process::Gid::from_raw(gid)),
        )
        .map_err(|err| format!("chown {}: {err}", dir.display()))?;
        rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700))
            .map_err(|err| format!("chmod {}: {err}", dir.display()))?;
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_for_us(dir: &Path) -> (PathBuf, thread::JoinHandle<()>) {
        let socket = dir.join("forker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = Server {
            allowed: vec![Allowed {
                peer: rustix::process::getuid().as_raw(),
                start: 0,
                count: 0,
            }],
            runtime_base: dir.join("run"),
            home_base: dir.join("home"),
        };
        let handle = thread::spawn(move || {
            let _ = server.serve(listener);
        });
        (socket, handle)
    }

    #[test]
    fn forks_as_ourselves_and_refuses_other_uids() {
        let dir = std::env::temp_dir().join(format!("niri-forker-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (socket, _server) = server_for_us(&dir);
        let uid = rustix::process::getuid().as_raw();
        let path = std::env::var("PATH").unwrap();

        let pid = fork(
            &socket,
            &Request {
                uid,
                argv: vec!["sh".into(), "-c".into(), "exit 0".into()],
                env: vec![("PATH".into(), path.clone())],
            },
        )
        .unwrap();
        assert!(pid > 0);

        let err = fork(
            &socket,
            &Request {
                uid: uid.wrapping_add(1),
                argv: vec!["sh".into()],
                env: vec![("PATH".into(), path)],
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("outside"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_allow() {
        assert_eq!(
            Allowed::parse("1000:100000:65536").unwrap(),
            Allowed {
                peer: 1000,
                start: 100000,
                count: 65536
            }
        );
        assert!(Allowed::parse("1000:100000").is_err());
    }
}
