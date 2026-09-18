//! Who is calling one of our D-Bus services. The bus tells us the sender's UID, the identity
//! daemon tells us what that UID may do. Screencast, screenshot and the service channel are
//! for the portal backend, which the manifest marks with the `screencast` grant; an app that
//! reaches the bus anyway gets `AccessDenied`.

use std::sync::{Arc, Mutex};

use drv_policy::{AppPolicy, Grant, PolicyClient};
use zbus::message::Header;
use zbus::names::BusName;
use zbus::{fdo, Connection};

#[derive(Clone)]
pub struct Caller {
    policy: Arc<Mutex<PolicyClient>>,
}

impl Caller {
    pub fn new(policy: PolicyClient) -> Self {
        Self {
            policy: Arc::new(Mutex::new(policy)),
        }
    }

    /// The sender's UID from the bus daemon's credentials for that connection.
    pub async fn uid(conn: &Connection, hdr: &Header<'_>) -> fdo::Result<u32> {
        let sender = hdr
            .sender()
            .ok_or_else(|| fdo::Error::Failed("message has no sender".to_owned()))?;
        let creds = fdo::DBusProxy::new(conn)
            .await?
            .get_connection_credentials(BusName::Unique(sender.clone()))
            .await?;
        creds
            .unix_user_id()
            .ok_or_else(|| fdo::Error::AccessDenied("caller has no unix uid".to_owned()))
    }

    pub fn policy(&self, uid: u32) -> fdo::Result<Arc<AppPolicy>> {
        self.policy.lock().unwrap().lookup(uid).map_err(|err| {
            warn!("policy lookup for D-Bus caller uid {uid} failed: {err}");
            fdo::Error::Failed("identity daemon unavailable".to_owned())
        })
    }

    /// The caller's UID, if its policy has `grant`; `AccessDenied` otherwise.
    pub async fn require(
        &self,
        conn: &Connection,
        hdr: &Header<'_>,
        grant: Grant,
    ) -> fdo::Result<u32> {
        let uid = Self::uid(conn, hdr).await?;
        let policy = self.policy(uid)?;
        if !policy.has(grant) {
            warn!(
                "refusing {} from uid {uid} ({:?}): no {grant:?} grant",
                hdr.member().map(|m| m.as_str()).unwrap_or("?"),
                policy.name
            );
            return Err(fdo::Error::AccessDenied(format!(
                "uid {uid} has no {grant:?} grant"
            )));
        }
        Ok(uid)
    }
}
