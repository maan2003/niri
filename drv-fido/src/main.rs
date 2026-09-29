//! The FIDO door, a supervisor service of its own uid. It parses what apps send (WebAuthn
//! JSON) and what the USB bus answers (CTAP2 CBOR, through hidapi's C), so it lives apart
//! from the ssh agent, which holds keys: a bug here costs a restart, nothing else. The
//! authenticators' hidraw nodes are our group's by udev rule; the ssh agent is in it too.
//! The door itself (`/run/drv/fido`, fd `listener`) is bound by the supervisor; every
//! connection is checked on `SO_PEERCRED` plus drv-appd's record for the uid (`ceremony`).
//! The person's part, a touch or a PIN, is asked at the shell (fd `shell`).

mod ceremony;

use std::process;
use std::sync::Arc;

use drv_os::fds::Kind;
use drv_policy::door::Door as Appd;
use drv_shell::ask::Client as Shell;

fn main() {
    if let Err(err) = serve() {
        drv_os::say!("drv-fido: {err}");
        process::exit(1);
    }
}

fn serve() -> Result<(), String> {
    let mut fds = drv_os::fds::take().map_err(|e| format!("fds from the supervisor: {e}"))?;
    let shell = fds
        .socket("shell", Kind::SeqPacket)
        .map_err(|e| e.to_string())?;
    let listener = fds
        .listener_of("listener", Kind::SeqPacket)
        .map_err(|e| e.to_string())?;
    let appd = Arc::new(Appd::open().map_err(|e| format!("drv-appd: {e}"))?);
    let shell = Shell::start(shell, "drv-fido")?;
    drv_os::say!("drv-fido: serving the door");
    ceremony::serve(listener, appd, shell).map_err(|e| e.to_string())
}
