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
use std::sync::{Arc, Mutex};
use std::time::Duration;

use drv_fido::wire::{Reply, Request};
use drv_policy::door::Door as Appd;
use drv_policy::{AppPolicy, seq};
use drv_shell::ask::Client as Shell;
use libwebauthn::UvUpdate;
use libwebauthn::ops::webauthn::idl::origin::{Origin, RequestOrigin};
use libwebauthn::ops::webauthn::{
    GetAssertionRequest, GetAssertionResponse, MakeCredentialRequest, MakeCredentialResponse,
    OriginValidation, RequestSettings, WebAuthnIDLResponse as _,
};
use libwebauthn::proto::CtapError;
use libwebauthn::transport::hid::channel::HidChannelHandle;
use libwebauthn::transport::hid::list_devices;
use libwebauthn::webauthn::error::PlatformError;
use libwebauthn::transport::{Channel as _, ChannelSettings, Device as _};
use libwebauthn::webauthn::WebAuthn as _;
use libwebauthn::webauthn::error::WebAuthnError;
use tokio::sync::broadcast;

/// Accepts forever on the door. Each connection is one request, answered when its ceremony
/// ends; ceremonies queue on `one`.
pub fn serve(listener: UnixListener, appd: Arc<Appd>, shell: Arc<Shell>) -> std::io::Result<()> {
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
            let _one = one.lock().unwrap_or_else(|p| p.into_inner());
            runtime.block_on(ceremony(&request, &policy.name, uid, &shell, &sock))
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

/// The manifest lists the origins the app may claim, exactly as it claims them.
fn allowed(policy: &AppPolicy, origin: &str) -> Result<(), String> {
    if policy.fido.iter().any(|o| o == origin) {
        Ok(())
    } else {
        Err(format!("{origin} is not an origin of this app"))
    }
}

/// The origin as WebAuthn sees it, and the host the relying party must be.
fn relying_party(origin: &str) -> Result<(RequestOrigin, String), String> {
    let host = if let Some(app_id) = origin.strip_prefix("app:") {
        let labels: Vec<&str> = app_id.split('.').collect();
        if labels.iter().any(|l| {
            l.is_empty()
                || !l
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        }) {
            return Err(format!("{origin}: not an app id"));
        }
        labels
            .iter()
            .rev()
            .map(|l| l.to_ascii_lowercase())
            .collect::<Vec<_>>()
            .join(".")
    } else if let Some(rest) = origin.strip_prefix("https://") {
        rest.to_owned()
    } else {
        return Err(format!("{origin}: neither app: nor https://"));
    };
    let parsed: Origin = format!("https://{host}")
        .parse()
        .map_err(|e| format!("{origin}: {e}"))?;
    let host = parsed.host.as_str().to_owned();
    Ok((RequestOrigin::new(parsed), host))
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
    sock: &OwnedFd,
) -> Result<String, String> {
    let (request_origin, host) = relying_party(request.origin())?;
    let settings = RequestSettings {
        origin: OriginValidation::Trust,
    };
    let op = match request {
        Request::Create { public_key, .. } => {
            let make = MakeCredentialRequest::prepare(&request_origin, public_key, &settings)
                .await
                .map_err(|e| format!("the request: {e}"))?;
            if make.relying_party.id != host {
                return Err(format!(
                    "relying party {} is not {host}",
                    make.relying_party.id
                ));
            }
            Op::Make(make)
        }
        Request::Get { public_key, .. } => {
            let get = GetAssertionRequest::prepare(&request_origin, public_key, &settings)
                .await
                .map_err(|e| format!("the request: {e}"))?;
            if get.relying_party_id != host {
                return Err(format!(
                    "relying party {} is not {host}",
                    get.relying_party_id
                ));
            }
            Op::Get(get)
        }
    };
    let devices = list_devices()
        .await
        .map_err(|e| format!("listing security keys: {e:?}"))?;
    let mut device = devices
        .into_iter()
        .next()
        .ok_or("no security key is plugged in")?;
    let name = device.to_string();
    let mut channel = device
        .channel(ChannelSettings::default())
        .await
        .map_err(|e| format!("{name}: {e:?}"))?;
    let handle = channel.get_handle();
    let prompts = tokio::spawn(prompt(
        channel.get_ux_update_receiver(),
        shell.clone(),
        app.to_owned(),
        uid,
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
            let assertion = response
                .assertions
                .into_iter()
                .next()
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
async fn prompt(
    mut updates: broadcast::Receiver<UvUpdate>,
    shell: Arc<Shell>,
    app: String,
    uid: u32,
    handle: HidChannelHandle,
) {
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
                touching = shell.touch(&app, uid, "Touch your security key").ok();
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
                let (shell, of) = (shell.clone(), app.clone());
                let prompt = format!("Enter your security key's PIN ({tries})");
                let answer =
                    tokio::task::spawn_blocking(move || shell.pin(&of, uid, &prompt)).await;
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
    use super::relying_party;

    #[test]
    fn app_ids_become_reversed_hosts() {
        let (origin, host) = relying_party("app:dev.rho.Gui").unwrap();
        assert_eq!(host, "gui.rho.dev");
        assert_eq!(origin.origin.to_string(), "https://gui.rho.dev");
        assert_eq!(relying_party("app:Gui").unwrap().1, "gui");
    }

    #[test]
    fn https_origins_are_their_host() {
        assert_eq!(
            relying_party("https://Example.com").unwrap().1,
            "example.com"
        );
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
}
