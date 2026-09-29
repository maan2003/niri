//! The FIDO door: WebAuthn ceremonies (a credential made, an assertion signed) on the
//! authenticator plugged in, for the apps whose manifest lists the origin they claim. The
//! wire is [`drv_agent::fido`]. libwebauthn (linux-credentials') speaks CTAP2 to the hidraw
//! nodes udev gave our group; the person's part (a touch, a PIN) is asked at the shell, as
//! ssh-agent's is. One ceremony at a time: there is one key, and one person.
//!
//! An app's origin is `app:<id>` (`app:dev.rho.Gui`). WebAuthn has no such origin, so it
//! stands in as `https://` of the id's labels reversed and lowercased (`gui.rho.dev`), and
//! the request's relying party must be exactly that host. An `https://` origin is its own
//! host, and the relying party must equal it too: no registrable-suffix rule here yet, a
//! browser behind this door will need one.

use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};

use drv_agent::fido::{Reply, Request};
use drv_policy::door::Door as Appd;
use drv_policy::{AppPolicy, seq};
use libwebauthn::UvUpdate;
use libwebauthn::ops::webauthn::idl::origin::{Origin, RequestOrigin};
use libwebauthn::ops::webauthn::{
    GetAssertionRequest, GetAssertionResponse, MakeCredentialRequest, MakeCredentialResponse,
    OriginValidation, RequestSettings, WebAuthnIDLResponse as _,
};
use libwebauthn::transport::hid::list_devices;
use libwebauthn::transport::{Channel as _, ChannelSettings, Device as _};
use libwebauthn::webauthn::WebAuthn as _;
use libwebauthn::webauthn::error::WebAuthnError;
use tokio::sync::broadcast;

use crate::Shell;

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
    appd.serve(listener, "drv-agent: fido", move |sock, uid, policy| {
        let request: Request = match seq::recv(&sock) {
            Ok((request, _)) => request,
            Err(err) => {
                drv_os::say!("drv-agent: fido: {} (uid {uid}): {err}", policy.name);
                return;
            }
        };
        let outcome = allowed(&policy, request.origin()).and_then(|()| {
            let _one = one.lock().unwrap_or_else(|p| p.into_inner());
            runtime.block_on(ceremony(&request, &policy.name, uid, &shell))
        });
        let reply = match outcome {
            Ok(json) => Reply::Credential { json },
            Err(reason) => {
                drv_os::say!("drv-agent: fido: {} (uid {uid}): {reason}", policy.name);
                Reply::Failed { reason }
            }
        };
        if let Err(err) = seq::send(&sock, &reply, &[]) {
            drv_os::say!(
                "drv-agent: fido: {} (uid {uid}): answering: {err}",
                policy.name
            );
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

async fn ceremony(
    request: &Request,
    app: &str,
    uid: u32,
    shell: &Arc<Shell>,
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
    let prompts = tokio::spawn(prompt(
        channel.get_ux_update_receiver(),
        shell.clone(),
        app.to_owned(),
        uid,
    ));
    let done = loop {
        let result = match &op {
            Op::Make(make) => channel
                .webauthn_make_credential(make)
                .await
                .map(|response| Done::Make(Box::new(response))),
            Op::Get(get) => channel.webauthn_get_assertion(get).await.map(Done::Get),
        };
        match result {
            Err(WebAuthnError::Ctap(err)) if err.is_retryable_user_error() => continue,
            other => break other,
        }
    };
    prompts.abort();
    let done = done.map_err(|err| format!("{name}: {err:?}"))?;
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

/// The authenticator's asks, at the shell: a touch is shown until the next word from it
/// or the end; a PIN is typed there and handed back.
async fn prompt(
    mut updates: broadcast::Receiver<UvUpdate>,
    shell: Arc<Shell>,
    app: String,
    uid: u32,
) {
    let mut touching = None;
    loop {
        match updates.recv().await {
            Ok(UvUpdate::PresenceRequired) => {
                touching = shell.touch(&app, uid, "Touch your security key").ok();
            }
            Ok(UvUpdate::PinRequired(pin)) => {
                touching = None;
                let (shell, of) = (shell.clone(), app.clone());
                let answer = tokio::task::spawn_blocking(move || {
                    shell.pin(&of, uid, "Enter your security key's PIN")
                })
                .await;
                match answer {
                    Ok(Ok(Some(typed))) => {
                        if let Err(err) = pin.send_pin(&typed) {
                            drv_os::say!("drv-agent: fido: the PIN: {err}");
                        }
                    }
                    // Nothing sent: the operation waits out its timeout and fails.
                    Ok(Ok(None)) => {
                        drv_os::say!("drv-agent: fido: {app} (uid {uid}): the PIN was refused")
                    }
                    Ok(Err(err)) => drv_os::say!("drv-agent: fido: {err}"),
                    Err(err) => drv_os::say!("drv-agent: fido: {err}"),
                }
            }
            Ok(UvUpdate::PinNotSet(_)) => {
                drv_os::say!("drv-agent: fido: {app} (uid {uid}): the security key has no PIN")
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
