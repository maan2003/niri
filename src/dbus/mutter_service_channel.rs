use std::os::unix::net::UnixStream;

use drv_policy::Grant;
use zbus::message::Header;
use zbus::{fdo, interface, zvariant, Connection};

use super::caller::Caller;
use super::Start;
use crate::niri::NewClient;

pub struct ServiceChannel {
    to_niri: calloop::channel::Sender<NewClient>,
    caller: Caller,
}

#[interface(name = "org.gnome.Mutter.ServiceChannel")]
impl ServiceChannel {
    async fn open_wayland_service_connection(
        &mut self,
        #[zbus(connection)] conn: &Connection,
        #[zbus(header)] hdr: Header<'_>,
        service_client_type: u32,
    ) -> fdo::Result<zvariant::OwnedFd> {
        if service_client_type != 1 {
            return Err(fdo::Error::InvalidArgs(
                "Invalid service client type".to_owned(),
            ));
        }
        // The socket we hand back has no peer credentials of its own (both ends are ours), so
        // the client is identified by who asked on the bus.
        let uid = self.caller.require(conn, &hdr, Grant::Screencast).await?;

        let (sock1, sock2) = UnixStream::pair().unwrap();
        let client = NewClient {
            client: sock2,
            restricted: false,
            identity: Some(uid),
        };
        if let Err(err) = self.to_niri.send(client) {
            warn!("error sending message to niri: {err:?}");
            return Err(fdo::Error::Failed("internal error".to_owned()));
        }

        Ok(zvariant::OwnedFd::from(std::os::fd::OwnedFd::from(sock1)))
    }
}

impl ServiceChannel {
    pub fn new(to_niri: calloop::channel::Sender<NewClient>, caller: Caller) -> Self {
        Self { to_niri, caller }
    }
}

impl Start for ServiceChannel {
    fn start(self, _monitor: bool) -> anyhow::Result<zbus::blocking::Connection> {
        let conn = zbus::blocking::connection::Builder::session()?
            .name("org.gnome.Mutter.ServiceChannel")?
            .serve_at("/org/gnome/Mutter/ServiceChannel", self)?
            .build()?;
        Ok(conn)
    }
}
