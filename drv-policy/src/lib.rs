//! Per-UID policy for Wayland clients.
//!
//! Identity is the UID of the connecting process (`SO_PEERCRED`); the identity daemon
//! (`drv-identityd`) says what a UID may do and starts apps under their UIDs. Every process is
//! its own UID: the compositor, the bridge, each service and each app. Nothing is "trusted";
//! a record lists what its UID may do and nothing else. The compositor holds a
//! [`client::PolicyClient`] and asks it once per new connection (cached per UID).
//! [`PolicyStore`] is a file-backed answerer for tests and simple setups.
//!
//! The compositor applies the policy at one choke point: which optional globals a connection
//! is shown. Everything a plain app needs (`wl_compositor`, `xdg_wm_base`, `wl_shm`, input,
//! outputs, pointer constraints, idle inhibit, ...) is always advertised; [`Global`] lists the
//! rest.

use std::collections::HashMap;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub use client::PolicyClient;
use serde::{Deserialize, Serialize};

/// Optional globals the compositor may hide from a client. Anything not listed here is always
/// advertised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Global {
    /// `zwp_linux_dmabuf_v1`: GPU buffer sharing. Also granted by [`AppPolicy::gpu`].
    Dmabuf,
    /// `zwlr_layer_shell_v1`: bars, overlays, anything outside a window.
    LayerShell,
    /// `ext_session_lock_v1`.
    SessionLock,
    /// `zwlr_data_control_manager_v1` and `ext_data_control_manager_v1`: clipboard access
    /// without focus.
    DataControl,
    /// `ext_foreign_toplevel_list_v1` and friends: the list of other apps' windows.
    ForeignToplevel,
    /// `ext_workspace_manager_v1`.
    Workspaces,
    /// `zwlr_output_manager_v1` and `zwlr_output_power_manager_v1`.
    OutputManagement,
    /// `zwlr_gamma_control_manager_v1`.
    GammaControl,
    /// `zwlr_screencopy_manager_v1`.
    Screencopy,
    /// `ext_image_copy_capture_manager_v1` and `ext_output_image_capture_source_manager_v1`.
    ImageCopyCapture,
    /// `zwp_virtual_keyboard_manager_v1`.
    VirtualKeyboard,
    /// `zwlr_virtual_pointer_manager_v1`.
    VirtualPointer,
    /// `zwp_input_method_manager_v2`.
    InputMethod,
    /// `wp_security_context_manager_v1`.
    SecurityContext,
}

impl Global {
    pub const ALL: &'static [Global] = &[
        Global::Dmabuf,
        Global::LayerShell,
        Global::SessionLock,
        Global::DataControl,
        Global::ForeignToplevel,
        Global::Workspaces,
        Global::OutputManagement,
        Global::GammaControl,
        Global::Screencopy,
        Global::ImageCopyCapture,
        Global::VirtualKeyboard,
        Global::VirtualPointer,
        Global::InputMethod,
        Global::SecurityContext,
    ];
}

/// Things a UID may do beyond its Wayland globals. Each unlocks one service; none implies
/// another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Grant {
    /// Ask the identity daemon about UIDs other than its own (the compositor, the bridge).
    Lookup,
    /// Use the compositor's screencast, screenshot and service-channel D-Bus services (the
    /// portal backend). Apps never get this: they get one portal session at a time, with
    /// consent.
    Screencast,
}

/// What one UID is allowed. The identity daemon's record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct AppPolicy {
    /// Shown to the user by the compositor (decorations, notifications). Never app-supplied.
    pub name: String,
    /// May use the GPU (`linux-dmabuf`). Software-rendered clients need only `wl_shm`.
    #[serde(default)]
    pub gpu: bool,
    /// Globals granted on top of the untrusted baseline.
    #[serde(default)]
    pub globals: Vec<Global>,
    #[serde(default)]
    pub grants: Vec<Grant>,
    /// Icon name, for the same places as `name`.
    #[serde(default)]
    pub icon: Option<String>,
}

impl AppPolicy {
    /// The policy for a UID the store knows nothing about: nothing optional, no GPU.
    pub fn unknown() -> Self {
        Self {
            name: "unknown".to_owned(),
            gpu: false,
            globals: Vec::new(),
            grants: Vec::new(),
            icon: None,
        }
    }

    /// Every global and grant, for tests that want the compositor as it was without policy.
    pub fn everything(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            gpu: true,
            globals: Global::ALL.to_vec(),
            grants: vec![Grant::Lookup, Grant::Screencast],
            icon: None,
        }
    }

    pub fn allows(&self, global: Global) -> bool {
        if global == Global::Dmabuf && self.gpu {
            return true;
        }
        self.globals.contains(&global)
    }

    pub fn has(&self, grant: Grant) -> bool {
        self.grants.contains(&grant)
    }
}

/// One entry of the static policy file: a UID or an inclusive UID range with its policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct AppEntry {
    /// First UID, inclusive.
    pub uid: u32,
    /// Last UID, inclusive. Defaults to `uid`.
    #[serde(default)]
    pub uid_end: Option<u32>,
    #[serde(flatten)]
    pub policy: AppPolicy,
}

impl AppEntry {
    pub fn contains(&self, uid: u32) -> bool {
        self.uid <= uid && uid <= self.uid_end.unwrap_or(self.uid)
    }
}

/// The static policy file (`policy.toml`).
///
/// ```toml
/// [default]
/// name = "unknown"
///
/// [[app]]
/// uid = 901
/// name = "compositor"
/// grants = ["lookup"]
///
/// [[app]]
/// uid = 100000
/// uid-end = 199999
/// name = "sandboxed"
/// gpu = true
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct PolicyFile {
    /// For UIDs without an entry.
    #[serde(default = "AppPolicy::unknown")]
    pub default: AppPolicy,
    #[serde(default, rename = "app")]
    pub apps: Vec<AppEntry>,
}

impl Default for PolicyFile {
    fn default() -> Self {
        Self {
            default: AppPolicy::unknown(),
            apps: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Parse(toml::de::Error),
    /// Two entries claim the same UID.
    Overlap(u32),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(err) => write!(f, "error reading the policy file: {err}"),
            Error::Parse(err) => write!(f, "error parsing the policy file: {err}"),
            Error::Overlap(uid) => write!(f, "uid {uid} is covered by more than one app entry"),
        }
    }
}

impl std::error::Error for Error {}

/// Answers "what may this UID do", with the answers shared between connections of one UID.
pub struct PolicyStore {
    default: Arc<AppPolicy>,
    apps: Vec<(AppEntry, Arc<AppPolicy>)>,
    cache: Mutex<HashMap<u32, Arc<AppPolicy>>>,
}

impl PolicyStore {
    pub fn new(file: PolicyFile) -> Result<Self, Error> {
        let apps: Vec<_> = file.apps.into_iter().collect();
        for (i, a) in apps.iter().enumerate() {
            for b in &apps[i + 1..] {
                let (lo, hi) = (
                    a.uid.max(b.uid),
                    a.uid_end.unwrap_or(a.uid).min(b.uid_end.unwrap_or(b.uid)),
                );
                if lo <= hi {
                    return Err(Error::Overlap(lo));
                }
            }
        }
        Ok(Self {
            default: Arc::new(file.default),
            apps: apps
                .into_iter()
                .map(|entry| {
                    let policy = Arc::new(entry.policy.clone());
                    (entry, policy)
                })
                .collect(),
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(Error::Io)?;
        let file: PolicyFile = toml::from_str(&text).map_err(Error::Parse)?;
        Self::new(file)
    }

    pub fn lookup(&self, uid: u32) -> Arc<AppPolicy> {
        if let Some(policy) = self.cache.lock().unwrap().get(&uid) {
            return policy.clone();
        }
        let policy = self
            .apps
            .iter()
            .find(|(entry, _)| entry.contains(uid))
            .map(|(_, policy)| policy.clone())
            .unwrap_or_else(|| self.default.clone());
        self.cache.lock().unwrap().insert(uid, policy.clone());
        policy
    }

    /// For connections whose UID cannot be determined.
    pub fn default_policy(&self) -> Arc<AppPolicy> {
        self.default.clone()
    }
}

pub mod client;
pub mod daemon;
pub mod rpc;
pub mod spawn;
pub mod wire;

/// Environment names the pieces agree on.
pub mod env {
    /// Path of the identity daemon's socket.
    pub const IDENTITY_SOCKET: &str = "DRV_IDENTITY_SOCKET";
    /// Path the compositor listens on for apps (absolute; apps get it as `WAYLAND_DISPLAY`).
    pub const APPS_SOCKET: &str = "DRV_APPS_SOCKET";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_file() {
        let file: PolicyFile = toml::from_str(
            r#"
            [default]
            name = "unknown"

            [[app]]
            uid = 1000
            name = "me"
            globals = ["screencopy"]
            grants = ["lookup"]

            [[app]]
            uid = 100000
            uid-end = 199999
            name = "sandboxed"
            gpu = true
            globals = ["layer-shell"]
            "#,
        )
        .unwrap();
        let store = PolicyStore::new(file).unwrap();

        assert!(store.lookup(1000).allows(Global::Screencopy));
        assert!(store.lookup(1000).has(Grant::Lookup));
        assert!(!store.lookup(1000).has(Grant::Screencast));
        let sandboxed = store.lookup(150000);
        assert_eq!(sandboxed.name, "sandboxed");
        assert!(sandboxed.allows(Global::Dmabuf));
        assert!(sandboxed.allows(Global::LayerShell));
        assert!(!sandboxed.allows(Global::Screencopy));
        let unknown = store.lookup(5);
        assert_eq!(unknown.name, "unknown");
        assert!(!unknown.allows(Global::Dmabuf));
    }

    #[test]
    fn client_and_daemon_roundtrip() {
        let file: PolicyFile = toml::from_str(
            "[[app]]\nuid = 7\nname = \"seven\"\ngpu = true\ngrants = [\"lookup\"]\n",
        )
        .unwrap();
        let store = PolicyStore::new(file).unwrap();
        let (a, b) = std::os::unix::net::UnixStream::pair().unwrap();
        let server = std::thread::spawn(move || {
            daemon::serve_connection(b, 7, &store as &dyn daemon::Handler)
        });

        let mut client = PolicyClient::from_stream(a).unwrap();
        assert_eq!(client.lookup(7).unwrap().name, "seven");
        assert!(client.lookup(7).unwrap().allows(Global::Dmabuf));
        assert_eq!(client.lookup(8).unwrap().name, "unknown");
        assert!(client.launch("seven".to_owned()).is_err());
        drop(client);
        server.join().unwrap().unwrap();
    }

    #[test]
    fn rejects_overlap() {
        let file: PolicyFile = toml::from_str(
            r#"
            [[app]]
            uid = 10
            uid-end = 20
            name = "a"

            [[app]]
            uid = 20
            name = "b"
            "#,
        )
        .unwrap();
        assert!(matches!(PolicyStore::new(file), Err(Error::Overlap(20))));
    }

    #[test]
    fn unknown_field_is_an_error() {
        assert!(toml::from_str::<PolicyFile>("[default]\nname = \"x\"\nfoo = 1\n").is_err());
    }
}
