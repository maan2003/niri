//! An app-facing service's door: a world-connectable `SOCK_SEQPACKET` socket (the
//! supervisor made and bound it) where every connection is keyed on the peer UID
//! (`SO_PEERCRED`) and on what drv-appd says that UID is. The app never names itself, and a
//! UID drv-appd does not know is nobody: refused at accept. Each service checks here for
//! itself; there is no proxy in between.

use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::{AppPolicy, PolicyClient};

/// The uid on the other end of a connected Unix socket.
pub fn peer_uid(sock: impl AsFd) -> io::Result<u32> {
    Ok(rustix::net::sockopt::socket_peercred(sock)?.uid.as_raw())
}

pub struct Door {
    appd: Mutex<PolicyClient>,
}

impl Door {
    /// Connects to drv-appd's public socket (`DRV_APPD_SOCKET`, else `/run/drv/appd.sock`);
    /// the service's own record needs the `lookup` grant. The supervisor bound the socket
    /// before anyone started, but drv-appd may still be coming up: the hello is retried for
    /// a while.
    pub fn open() -> io::Result<Self> {
        let path = std::env::var_os(crate::env::APPD_SOCKET)
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/run/drv/appd.sock"));
        let mut tries = 0;
        let appd = loop {
            match PolicyClient::connect(path.clone()) {
                Ok(appd) => break appd,
                Err(err) if tries < 60 => {
                    tries += 1;
                    eprintln!("drv-appd not answering yet ({err}); retrying");
                    thread::sleep(std::time::Duration::from_millis(500));
                }
                Err(err) => return Err(err),
            }
        };
        Ok(Self {
            appd: Mutex::new(appd),
        })
    }

    /// Who `uid` is, by drv-appd. A UID it does not know is an error: not an app.
    pub fn who(&self, uid: u32) -> io::Result<Arc<AppPolicy>> {
        let policy = self.appd.lock().unwrap().lookup(uid)?;
        if *policy == AppPolicy::unknown() {
            return Err(io::Error::other(format!("uid {uid} is not an app")));
        }
        Ok(policy)
    }

    /// Accepts forever: `on` runs on its own thread for each connection that is an app,
    /// with the socket, the uid and the record. `tag` prefixes the log.
    pub fn serve(
        self: Arc<Self>,
        listener: UnixListener,
        tag: &'static str,
        on: impl Fn(OwnedFd, u32, Arc<AppPolicy>) + Send + Sync + 'static,
    ) -> io::Result<()> {
        let on = Arc::new(on);
        loop {
            let (stream, _) = listener.accept()?;
            let uid = match peer_uid(&stream) {
                Ok(uid) => uid,
                Err(err) => {
                    eprintln!("{tag}: no peer credentials: {err}");
                    continue;
                }
            };
            let policy = match self.who(uid) {
                Ok(policy) => policy,
                Err(err) => {
                    eprintln!("{tag}: refused uid {uid}: {err}");
                    continue;
                }
            };
            let on = on.clone();
            thread::spawn(move || on(OwnedFd::from(stream), uid, policy));
        }
    }
}
