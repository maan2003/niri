//! The FIDO door: WebAuthn ceremonies (a credential made, an assertion signed) on the
//! authenticator plugged in, for the apps whose manifest lists the origin they claim. The
//! wire is [`drv_fido::wire`]. libwebauthn (linux-credentials') speaks CTAP2 to the hidraw
//! nodes udev gave our group; the person's part (a touch, a PIN) is asked at the shell, as
//! the ssh agent's is. One ceremony at a time: there is one key, and one person.
//!
//! An app's origin is `app:<id>` (`app:dev.rho.Gui`). WebAuthn has no such origin, so it
//! stands in as `https://` of the id's labels reversed and lowercased (`gui.rho.dev`), and
//! the request's relying party must be exactly that host. An `https://` origin is its own
//! host, and the relying party must equal it too: no registrable-suffix rule here yet, a
//! browser behind this door will need one.

use std::io::Read as _;
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::{Arc, Mutex, TryLockError};
use std::time::Duration;

use drv_fido::wire::{Reply, Request};
use drv_policy::door::Door as Appd;
use drv_policy::{AppPolicy, seq};
use drv_shell::ask::Client as Shell;
use libwebauthn::UvUpdate;
use libwebauthn::ops::webauthn::idl::origin::{Origin, RequestOrigin};
use libwebauthn::ops::webauthn::psl::PublicSuffixList;
use libwebauthn::ops::webauthn::{
    GetAssertionRequest, GetAssertionResponse, MakeCredentialRequest, MakeCredentialResponse,
    OriginValidation, RelatedOrigins, RequestSettings, WebAuthnIDLResponse as _,
};
use libwebauthn::proto::CtapError;
use libwebauthn::transport::hid::channel::HidChannelHandle;
use libwebauthn::transport::hid::list_devices;
use libwebauthn::transport::{Channel as _, ChannelSettings, Device as _};
use libwebauthn::webauthn::WebAuthn as _;
use libwebauthn::webauthn::error::{PlatformError, WebAuthnError};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use tokio::sync::broadcast;

/// Accepts forever on the door. Each connection is one request, answered when its ceremony
/// ends; ceremonies queue on `one`.
pub fn serve(
    listener: UnixListener,
    appd: Arc<Appd>,
    shell: Arc<Shell>,
    psl: Option<Arc<dyn PublicSuffixList>>,
) -> std::io::Result<()> {
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .worker_threads(2)
            .build()?,
    );
    let one = Arc::new(Mutex::new(()));
    appd.serve(listener, "drv-fido", move |sock, uid, policy| {
        let request: Request = match seq::recv(&sock) {
            Ok((request, _)) => request,
            Err(err) => {
                drv_os::say!("drv-fido: {} (uid {uid}): {err}", policy.name);
                return;
            }
        };
        let outcome = allowed(&policy, request.origin()).and_then(|()| {
            let _one = match one.try_lock() {
                Ok(one) => one,
                Err(TryLockError::Poisoned(p)) => p.into_inner(),
                Err(TryLockError::WouldBlock) => {
                    drv_os::say!(
                        "drv-fido: {} (uid {uid}): waiting for the ceremony in progress",
                        policy.name
                    );
                    one.lock().unwrap_or_else(|p| p.into_inner())
                }
            };
            if hung_up(&sock) {
                return Err("the app hung up while waiting".to_owned());
            }
            runtime.block_on(ceremony(
                &request,
                &policy.name,
                uid,
                &shell,
                psl.as_deref(),
                &sock,
            ))
        });
        let reply = match outcome {
            Ok(json) => Reply::Credential { json },
            Err(reason) => {
                drv_os::say!("drv-fido: {} (uid {uid}): {reason}", policy.name);
                Reply::Failed { reason }
            }
        };
        if let Err(err) = seq::send(&sock, &reply, &[]) {
            drv_os::say!("drv-fido: {} (uid {uid}): answering: {err}", policy.name);
        }
    })
}

/// Whether the app closed its end while its request waited its turn.
fn hung_up(sock: &OwnedFd) -> bool {
    let mut fds = [PollFd::new(sock, PollFlags::RDHUP)];
    matches!(poll(&mut fds, Some(&Timespec::default())), Ok(n) if n > 0)
        && fds[0]
            .revents()
            .intersects(PollFlags::RDHUP | PollFlags::HUP | PollFlags::ERR)
}

/// The manifest lists the origins the app may claim, exactly as it claims them; `https://*`
/// is every web origin, for the one app that is a browser and vouches for what it claims.
fn allowed(policy: &AppPolicy, origin: &str) -> Result<(), String> {
    if policy
        .fido
        .iter()
        .any(|o| o == origin || (o == "https://*" && origin.starts_with("https://")))
    {
        Ok(())
    } else {
        Err(format!("{origin} is not an origin of this app"))
    }
}

/// The origin as WebAuthn sees it and, for an `app:` origin, the one host its relying party
/// must be. A web origin's relying party is checked the web's way instead (a registrable
/// suffix of the origin's host, by the public suffix list), as a browser would.
fn relying_party(origin: &str) -> Result<(RequestOrigin, Option<String>), String> {
    if let Some(app_id) = origin.strip_prefix("app:") {
        let labels: Vec<&str> = app_id.split('.').collect();
        if labels.iter().any(|l| {
            l.is_empty()
                || !l
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        }) {
            return Err(format!("{origin}: not an app id"));
        }
        let host = labels
            .iter()
            .rev()
            .map(|l| l.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(".");
        let parsed: Origin = format!("https://{host}")
            .parse()
            .map_err(|e| format!("{origin}: {e}"))?;
        let host = parsed.host.as_str().to_owned();
        Ok((RequestOrigin::new(parsed), Some(host)))
    } else if origin.starts_with("https://") {
        let parsed: Origin = origin.parse().map_err(|e| format!("{origin}: {e}"))?;
        Ok((RequestOrigin::new(parsed), None))
    } else {
        Err(format!("{origin}: neither app: nor https://"))
    }
}

enum Op {
    Make(MakeCredentialRequest),
    Get(GetAssertionRequest),
}

/// The ceremony ends early when the person refuses at the shell (a touch or the PIN) or
/// the app hangs up (`sock` reads end): the operation on the key is cancelled
/// (CTAPHID_CANCEL) and the reply says so. Nothing waits for the key's own timeout.
async fn ceremony(
    request: &Request,
    app: &str,
    uid: u32,
    shell: &Arc<Shell>,
    psl: Option<&dyn PublicSuffixList>,
    sock: &OwnedFd,
) -> Result<String, String> {
    let (request_origin, app_host) = relying_party(request.origin())?;
    let settings = RequestSettings {
        origin: match (&app_host, psl) {
            (Some(_), _) => OriginValidation::Trust,
            (None, Some(public_suffix_list)) => OriginValidation::Validate {
                public_suffix_list,
                related_origins: RelatedOrigins::Disabled,
            },
            (None, None) => {
                return Err("no public suffix list: web origins are refused".to_owned());
            }
        },
    };
    let op = match request {
        Request::Create { public_key, .. } => {
            let make = MakeCredentialRequest::prepare(&request_origin, public_key, &settings)
                .await
                .map_err(|e| format!("the request: {e}"))?;
            if app_host
                .as_ref()
                .is_some_and(|host| make.relying_party.id != *host)
            {
                return Err(format!(
                    "relying party {} is not {}",
                    make.relying_party.id,
                    app_host.unwrap_or_default()
                ));
            }
            Op::Make(make)
        }
        Request::Get { public_key, .. } => {
            let get = GetAssertionRequest::prepare(&request_origin, public_key, &settings)
                .await
                .map_err(|e| format!("the request: {e}"))?;
            if app_host
                .as_ref()
                .is_some_and(|host| get.relying_party_id != *host)
            {
                return Err(format!(
                    "relying party {} is not {}",
                    get.relying_party_id,
                    app_host.unwrap_or_default()
                ));
            }
            Op::Get(get)
        }
    };
    // What the dialog says after "<app> wants to": the operation and the relying party, which
    // the manifest check above vouches for; the account only on registration, where the app
    // chose it.
    let what = match &op {
        Op::Make(make) => {
            let rp = &make.relying_party.id;
            match make
                .user
                .name
                .as_deref()
                .or(make.user.display_name.as_deref())
            {
                Some(user) => format!("register a security key with {rp} as \"{user}\""),
                None => format!("register a security key with {rp}"),
            }
        }
        Op::Get(get) => format!("sign in to {}", get.relying_party_id),
    };
    // No key yet: ask for one and wait for it, until the person refuses or the app hangs up.
    let mut inserting = None;
    let mut device = loop {
        let devices = list_devices()
            .await
            .map_err(|e| format!("listing security keys: {e:?}"))?;
        if let Some(device) = devices.into_iter().next() {
            break device;
        }
        match &inserting {
            None => {
                drv_os::say!("drv-fido: {app} (uid {uid}): no security key: waiting for one");
                inserting = Some(shell.touch(app, uid, &what, "Insert your security key")?);
            }
            Some(touching) if touching.refused() => return Err("refused".to_owned()),
            Some(_) if hung_up(sock) => return Err("the app hung up".to_owned()),
            Some(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    };
    drop(inserting);
    let name = device.to_string();
    // The key by its product name, when it has one.
    let key = match &device.backend {
        libwebauthn::transport::hid::device::HidBackendDevice::HidApiDevice(info) => info
            .product_string()
            .map(str::trim)
            .filter(|p| !p.is_empty()),
        #[allow(unreachable_patterns)]
        _ => None,
    }
    .map_or_else(|| "security key".to_owned(), str::to_owned);
    let mut channel = device
        .channel(ChannelSettings::default())
        .await
        .map_err(|e| format!("{name}: {e:?}"))?;
    let handle = channel.get_handle();
    let prompts = tokio::spawn(prompt(
        channel.get_ux_update_receiver(),
        shell.clone(),
        Asking {
            app: app.to_owned(),
            uid,
            what: what.clone(),
            key,
        },
        handle.clone(),
    ));
    // The app gone: one blocking read on its socket ends when it closes (or when we answer
    // and close it ourselves, when the cancel goes nowhere).
    let hangup = {
        let mut sock = UnixStream::from(sock.try_clone().map_err(|e| format!("dup: {e}"))?);
        let (app, handle) = (app.to_owned(), handle);
        tokio::spawn(async move {
            let _ = tokio::task::spawn_blocking(move || {
                let mut buf = [0u8; 64];
                while matches!(sock.read(&mut buf), Ok(n) if n > 0) {}
            })
            .await;
            drv_os::say!("drv-fido: {app} (uid {uid}): the app hung up");
            handle.cancel_ongoing_operation().await;
        })
    };
    let done = loop {
        let result = match &op {
            Op::Make(make) => channel
                .webauthn_make_credential(make)
                .await
                .map(|response| Done::Make(Box::new(response))),
            Op::Get(get) => channel.webauthn_get_assertion(get).await.map(Done::Get),
        };
        match result {
            // A wrong PIN is asked again (the prompt says how many tries are left); a touch
            // not given in the key's time is not: the person had their say.
            Err(WebAuthnError::Ctap(CtapError::PINInvalid | CtapError::UVInvalid)) => continue,
            other => break other,
        }
    };
    prompts.abort();
    hangup.abort();
    let done = done.map_err(|err| match err {
        WebAuthnError::Platform(PlatformError::Cancelled) => "cancelled".to_owned(),
        err => format!("{name}: {err:?}"),
    })?;
    let json = match (&op, done) {
        (Op::Make(make), Done::Make(response)) => {
            let mut json = response
                .to_idl_model(make)
                .map_err(|e| format!("{name}: the response: {e}"))?;
            json.response.transports = vec!["usb".to_owned()];
            json.authenticator_attachment = Some("cross-platform".to_owned());
            serde_json::to_string(&json)
        }
        (Op::Get(get), Done::Get(response)) => {
            let mut assertions = response.assertions;
            // Several accounts on the key for this relying party: the person picks one at
            // the shell; the browser sees one credential, as WebAuthn wants.
            let chosen = if assertions.len() > 1 {
                let choices = assertions
                    .iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let user = a.user.as_ref();
                        let name = user.and_then(|u| u.display_name.clone().or(u.name.clone()));
                        let detail = user
                            .and_then(|u| u.name.clone())
                            .filter(|n| Some(n) != name.as_ref());
                        drv_shell::ask::Choice {
                            key: i.to_string(),
                            name: name.unwrap_or_else(|| format!("account {}", i + 1)),
                            detail: detail.unwrap_or_default(),
                        }
                    })
                    .collect();
                let (shell, of, what) = (shell.clone(), app.to_owned(), what.clone());
                let picked = tokio::task::spawn_blocking(move || {
                    shell.pick(&of, uid, &what, "Which account?", choices)
                })
                .await
                .map_err(|e| e.to_string())??;
                match picked {
                    Some(key) => key.parse::<usize>().ok().filter(|i| *i < assertions.len()),
                    None => {
                        drv_os::say!("drv-fido: {app} (uid {uid}): the account pick was refused");
                        return Err("refused".to_owned());
                    }
                }
            } else {
                Some(0)
            };
            let assertion = chosen
                .filter(|i| *i < assertions.len())
                .map(|i| assertions.swap_remove(i))
                .ok_or_else(|| format!("{name}: no assertion"))?;
            let mut json = assertion
                .to_idl_model(get)
                .map_err(|e| format!("{name}: the response: {e}"))?;
            json.authenticator_attachment = Some("cross-platform".to_owned());
            serde_json::to_string(&json)
        }
        _ => unreachable!("the answer is to the question"),
    };
    json.map_err(|e| format!("{name}: the response: {e}"))
}

enum Done {
    Make(Box<MakeCredentialResponse>),
    Get(GetAssertionResponse),
}

/// A PIN with this many tries left is not asked for: the next miss but one blocks the key
/// for good (a reset, every credential gone). Type it somewhere that shows the count.
const LAST_TRIES: u32 = 1;

/// The authenticator's asks, at the shell: a touch is shown until the next word from it,
/// the end, or the person's refusal, which cancels the operation on the key; a PIN is
/// typed there and handed back, its refusal cancels the operation too.
/// Who asks, for what, on which key: the dialogs' words.
struct Asking {
    app: String,
    uid: u32,
    what: String,
    key: String,
}

async fn prompt(
    mut updates: broadcast::Receiver<UvUpdate>,
    shell: Arc<Shell>,
    asking: Asking,
    handle: HidChannelHandle,
) {
    let Asking {
        app,
        uid,
        what,
        key,
    } = asking;
    let mut touching: Option<drv_shell::ask::Touching> = None;
    loop {
        let update = if touching.is_some() {
            tokio::select! {
                update = updates.recv() => update,
                _ = tokio::time::sleep(Duration::from_millis(200)) => {
                    if touching.as_ref().is_some_and(|t| t.refused()) {
                        drv_os::say!("drv-fido: {app} (uid {uid}): the touch was refused");
                        touching = None;
                        handle.cancel_ongoing_operation().await;
                    }
                    continue;
                }
            }
        } else {
            updates.recv().await
        };
        match update {
            Ok(UvUpdate::PresenceRequired) => {
                touching = shell
                    .touch(&app, uid, &what, &format!("Touch your {key}"))
                    .ok();
            }
            Ok(UvUpdate::PinRequired(pin)) => {
                touching = None;
                let tries = match pin.attempts_left {
                    Some(n) => format!("{n} attempts left"),
                    None => "attempts left unknown".to_owned(),
                };
                if pin.attempts_left.is_some_and(|n| n <= LAST_TRIES) {
                    drv_os::say!(
                        "drv-fido: {app} (uid {uid}): not asking the PIN ({tries}): another miss would block the key"
                    );
                    pin.cancel();
                    continue;
                }
                drv_os::say!("drv-fido: {app} (uid {uid}): asking the PIN ({tries})");
                let (shell, of, what) = (shell.clone(), app.clone(), what.clone());
                // The count only once it matters: the key blocks after eight misses.
                let prompt = match pin.attempts_left {
                    Some(n) if n <= 3 => format!("PIN for your {key} ({n} tries left)"),
                    _ => format!("PIN for your {key}"),
                };
                let answer =
                    tokio::task::spawn_blocking(move || shell.pin(&of, uid, &what, &prompt)).await;
                match answer {
                    Ok(Ok(Some(typed))) => {
                        if let Err(err) = pin.send_pin(&typed) {
                            drv_os::say!("drv-fido: the PIN: {err}");
                        }
                    }
                    Ok(Ok(None)) => {
                        drv_os::say!("drv-fido: {app} (uid {uid}): the PIN was refused");
                        pin.cancel();
                    }
                    Ok(Err(err)) => drv_os::say!("drv-fido: {err}"),
                    Err(err) => drv_os::say!("drv-fido: {err}"),
                }
            }
            Ok(UvUpdate::PinNotSet(_)) => {
                drv_os::say!("drv-fido: {app} (uid {uid}): the security key has no PIN")
            }
            Ok(UvUpdate::UvRetry { .. }) => {}
            Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    drop(touching);
}

#[cfg(test)]
mod tests {
    use drv_policy::AppPolicy;

    use super::{OwnedFd, UnixStream, allowed, hung_up, relying_party};

    #[test]
    fn app_ids_become_reversed_hosts() {
        let (origin, host) = relying_party("app:dev.rho.Gui").unwrap();
        assert_eq!(host.as_deref(), Some("gui.rho.dev"));
        assert_eq!(origin.origin.to_string(), "https://gui.rho.dev");
        assert_eq!(relying_party("app:Gui").unwrap().1.as_deref(), Some("gui"));
    }

    #[test]
    fn https_origins_are_their_host() {
        let (origin, host) = relying_party("https://Example.com").unwrap();
        assert_eq!(origin.origin.host.as_str(), "example.com");
        assert_eq!(host, None);
        assert!(relying_party("https://example.com/path").is_err());
    }

    #[test]
    fn other_schemes_and_odd_ids_are_refused() {
        assert!(relying_party("http://localhost").is_err());
        assert!(relying_party("app:").is_err());
        assert!(relying_party("app:a..b").is_err());
        assert!(relying_party("app:a/b").is_err());
        assert!(relying_party("rho").is_err());
    }

    #[test]
    fn hung_up_sees_the_peer_close_and_only_that() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let ours = OwnedFd::from(ours);
        assert!(!hung_up(&ours));
        // Data waiting is not a hangup.
        std::io::Write::write_all(&mut &theirs, b"x").unwrap();
        assert!(!hung_up(&ours));
        drop(theirs);
        assert!(hung_up(&ours));
    }

    #[test]
    fn the_web_grant_is_https_only_and_exact_grants_stay_exact() {
        let mut policy = AppPolicy::unknown();
        policy.fido = vec!["https://*".to_owned(), "app:dev.rho.Gui".to_owned()];
        assert!(allowed(&policy, "https://login.example.com").is_ok());
        assert!(allowed(&policy, "https://*").is_ok());
        assert!(allowed(&policy, "http://login.example.com").is_err());
        assert!(allowed(&policy, "app:dev.rho.Gui").is_ok());
        assert!(allowed(&policy, "app:dev.rho.Other").is_err());
        policy.fido = vec!["app:dev.rho.Gui".to_owned()];
        assert!(allowed(&policy, "https://login.example.com").is_err());
    }
}
