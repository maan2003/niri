//! The FIDO door, a supervisor service of its own uid. It parses what apps send (WebAuthn
//! JSON) and what the USB bus answers (CTAP2 CBOR, through hidapi's C), so it lives apart
//! from the ssh agent, which holds keys: a bug here costs a restart, nothing else. The
//! authenticators' hidraw nodes are our group's by udev rule; the ssh agent is in it too.
//! The door itself (`/run/drv/fido`, fd `listener`) is bound by the supervisor; every
//! connection is checked on `SO_PEERCRED` plus drv-appd's record for the uid (`ceremony`).
//! The person's part, a touch or a PIN, is asked at the shell (fd `shell`). Web origins need
//! the public suffix list (`DRV_FIDO_PSL`, a `public_suffix_list.dat`) to check their relying
//! party; without one they are refused.

mod ceremony;

use std::process;
use std::sync::Arc;

use drv_os::fds::Kind;
use drv_policy::door::Door as Appd;
use drv_shell::ask::Client as Shell;
use libwebauthn::ops::webauthn::psl::{DatFilePublicSuffixList, PublicSuffixList};

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
    let psl: Option<Arc<dyn PublicSuffixList>> = match std::env::var_os("DRV_FIDO_PSL") {
        Some(path) => Some(Arc::new(
            DatFilePublicSuffixList::from_path(&path)
                .map_err(|e| format!("the public suffix list {}: {e}", path.to_string_lossy()))?,
        )),
        None => {
            drv_os::say!("drv-fido: no public suffix list (DRV_FIDO_PSL): web origins are refused");
            None
        }
    };
    drv_os::say!("drv-fido: serving the door");
    ceremony::serve(listener, appd, shell, psl).map_err(|e| e.to_string())
}
