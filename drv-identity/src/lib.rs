//! The brain of app launching, unprivileged. Knows the apps (static manifests generated from the
//! system configuration: every app has a fixed UID), answers policy lookups, and turns
//! `Launch { app }` into a request on the spawner's channel. Android's PackageManager plus
//! ActivityManager, in one small process with its own UID.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use drv_policy::daemon::Handler;
use drv_policy::{spawn, AppPolicy, Global, Grant};
use serde::{Deserialize, Serialize};

/// `identity.toml`.
///
/// ```toml
/// wayland-socket = "/run/drv-wayland/wayland"
///
/// [env]
/// PIPEWIRE_RUNTIME_DIR = "/run/pipewire"
///
/// [[app]]
/// name = "compositor"   # a service: identified, never launched
/// uid = 901
/// grants = ["lookup"]
///
/// [[app]]
/// name = "firefox"
/// uid = 100042
/// exec = ["firefox"]    # the only arguments the app ever gets
/// gpu = true
/// network = true
/// groups = ["render"]
/// autostart = true
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    /// The compositor's apps socket: every launched app's `WAYLAND_DISPLAY`. Autostart waits
    /// for it to exist.
    pub wayland_socket: PathBuf,
    /// Environment every app gets, from the system configuration: where the system put the
    /// PipeWire socket, for example. On top of `PATH` and friends.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default, rename = "app")]
    pub apps: Vec<AppConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct AppConfig {
    pub name: String,
    /// Fixed, from the system configuration. Identity never allocates.
    pub uid: u32,
    /// Program and its arguments. Absent means a service that is identified but not launched.
    #[serde(default)]
    pub exec: Option<Vec<String>>,
    /// Supplementary groups, e.g. `render` for the GPU. The spawner checks them against its
    /// list.
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub gpu: bool,
    /// Keep the host network; off means an empty network namespace.
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub globals: Vec<Global>,
    #[serde(default)]
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub icon: Option<String>,
    /// Extra environment for this app only.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Extra `/run` entries this app may see, on top of the spawner's default list.
    #[serde(default)]
    pub expose: Vec<String>,
    /// Started by the daemon once the compositor's socket exists, in manifest order.
    #[serde(default)]
    pub autostart: bool,
}

impl AppConfig {
    fn policy(&self) -> AppPolicy {
        AppPolicy {
            name: self.name.clone(),
            gpu: self.gpu,
            globals: self.globals.clone(),
            grants: self.grants.clone(),
            icon: self.icon.clone(),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Io(PathBuf, std::io::Error),
    Parse(PathBuf, toml::de::Error),
    Config(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io(path, err) => write!(f, "{}: {err}", path.display()),
            Error::Parse(path, err) => write!(f, "{}: {err}", path.display()),
            Error::Config(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}

pub fn load_config(path: &Path) -> Result<Config, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io(path.to_owned(), e))?;
    let config: Config = toml::from_str(&text).map_err(|e| Error::Parse(path.to_owned(), e))?;
    check_config(&config).map_err(Error::Config)?;
    Ok(config)
}

/// A UID is one identity, a name is one app, and an autostart entry must be launchable.
fn check_config(config: &Config) -> Result<(), String> {
    let mut names = HashSet::new();
    let mut uids = HashSet::new();
    for app in &config.apps {
        if !names.insert(&app.name) {
            return Err(format!("app {:?} listed twice", app.name));
        }
        if !uids.insert(app.uid) {
            return Err(format!("uid {} used by more than one app", app.uid));
        }
        if app.exec.as_ref().is_some_and(|e| e.is_empty()) {
            return Err(format!("app {:?} has an empty exec", app.name));
        }
        if app.autostart && app.exec.is_none() {
            return Err(format!("app {:?} is autostart but has no exec", app.name));
        }
    }
    Ok(())
}

/// Whatever actually creates processes: the spawner's channel, or a stub in tests.
pub trait Spawner: Send + Sync {
    fn fork(&self, request: &spawn::Request) -> Result<u32, String>;
}

impl Spawner for spawn::Channel {
    fn fork(&self, request: &spawn::Request) -> Result<u32, String> {
        spawn::Channel::fork(self, request)
            .map(|_| request.uid)
            .map_err(|e| format!("spawner: {e}"))
    }
}

pub struct Identity {
    config: Config,
    spawner: Arc<dyn Spawner>,
    /// Environment every launched app gets first: `PATH` and friends from our own environment.
    base_env: Vec<(String, String)>,
}

impl Identity {
    pub fn new(config: Config, spawner: Arc<dyn Spawner>, base_env: Vec<(String, String)>) -> Self {
        Self {
            config,
            spawner,
            base_env,
        }
    }

    fn app(&self, name: &str) -> Option<&AppConfig> {
        self.config.apps.iter().find(|a| a.name == name)
    }

    fn by_uid(&self, uid: u32) -> Option<&AppConfig> {
        self.config.apps.iter().find(|a| a.uid == uid)
    }

    /// The child's environment: ours (`PATH`..), the config's `[env]`, the app's own, then
    /// the compositor's apps socket as `WAYLAND_DISPLAY`. The spawner sets `HOME` and
    /// `XDG_RUNTIME_DIR`.
    fn env_for(&self, app: &AppConfig) -> Vec<(String, String)> {
        let mut env = self.base_env.clone();
        env.extend(self.config.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        env.extend(app.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        env.push((
            "WAYLAND_DISPLAY".to_owned(),
            self.config.wayland_socket.to_string_lossy().into_owned(),
        ));
        env
    }

    fn start(&self, app: &AppConfig) -> Result<u32, String> {
        let argv = app
            .exec
            .clone()
            .ok_or_else(|| format!("{:?} is a service, not a launchable app", app.name))?;
        let request = spawn::Request {
            uid: app.uid,
            groups: app.groups.clone(),
            argv,
            env: self.env_for(app),
            network: app.network,
            expose: app.expose.clone(),
        };
        self.spawner.fork(&request)?;
        Ok(app.uid)
    }

    /// Starts every `autostart` app once the compositor's socket exists, in manifest order.
    /// Waits up to `timeout` for the socket; the seat daemon will own this ordering later.
    pub fn autostart(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !self.config.wayland_socket.exists() {
            if Instant::now() > deadline {
                eprintln!(
                    "drv-identityd: no compositor socket at {} after {timeout:?}; not autostarting",
                    self.config.wayland_socket.display()
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        for app in self.config.apps.iter().filter(|a| a.autostart) {
            match self.start(app) {
                Ok(uid) => eprintln!("drv-identityd: autostarted {:?} as uid {uid}", app.name),
                Err(err) => eprintln!("drv-identityd: autostart {:?}: {err}", app.name),
            }
        }
    }
}

impl Handler for Identity {
    fn lookup(&self, peer: u32, uid: u32) -> Result<AppPolicy, String> {
        if peer != uid {
            let asker = self.by_uid(peer).map(AppConfig::policy);
            if !asker.is_some_and(|a| a.has(Grant::Lookup)) {
                return Err(format!("uid {peer} may not look up other uids"));
            }
        }
        Ok(self
            .by_uid(uid)
            .map(AppConfig::policy)
            .unwrap_or_else(AppPolicy::unknown))
    }

    fn launch(&self, _peer: u32, name: &str) -> Result<u32, String> {
        let app = self
            .app(name)
            .ok_or_else(|| format!("unknown app {name:?}; add it to identity.toml"))?;
        self.start(app)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Recorder(Mutex<Vec<spawn::Request>>);

    impl Spawner for Recorder {
        fn fork(&self, request: &spawn::Request) -> Result<u32, String> {
            self.0.lock().unwrap().push(request.clone());
            Ok(1)
        }
    }

    fn identity() -> (Identity, Arc<Recorder>) {
        let config: Config = toml::from_str(
            r#"
            wayland-socket = "/run/drv-wayland/wayland"

            [env]
            PIPEWIRE_RUNTIME_DIR = "/run/pipewire"

            [[app]]
            name = "compositor"
            uid = 901
            grants = ["lookup"]

            [[app]]
            name = "firefox"
            uid = 100042
            exec = ["firefox"]
            gpu = true
            groups = ["render"]
            env = { MOZ_ENABLE_WAYLAND = "1" }
            expose = ["/run/drv-session"]
            "#,
        )
        .unwrap();
        check_config(&config).unwrap();
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new())));
        let id = Identity::new(
            config,
            recorder.clone(),
            vec![("PATH".to_owned(), "/bin".to_owned())],
        );
        (id, recorder)
    }

    #[test]
    fn launch_builds_env_from_manifest_only() {
        let (id, recorder) = identity();
        assert_eq!(id.launch(5, "firefox"), Ok(100042));
        let requests = recorder.0.lock().unwrap();
        let req = &requests[0];
        assert_eq!(req.argv, vec!["firefox"]);
        assert_eq!(req.groups, vec!["render"]);
        assert_eq!(req.expose, vec!["/run/drv-session"]);
        let get = |k: &str| {
            req.env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("PATH"), Some("/bin"));
        assert_eq!(get("PIPEWIRE_RUNTIME_DIR"), Some("/run/pipewire"));
        assert_eq!(get("MOZ_ENABLE_WAYLAND"), Some("1"));
        assert_eq!(get("WAYLAND_DISPLAY"), Some("/run/drv-wayland/wayland"));
    }

    #[test]
    fn services_are_not_launchable() {
        let (id, _) = identity();
        let err = id.launch(5, "compositor").unwrap_err();
        assert!(err.contains("service"), "{err}");
    }

    #[test]
    fn lookup_needs_the_grant_for_other_uids() {
        let (id, _) = identity();
        assert_eq!(id.lookup(100042, 100042).unwrap().name, "firefox");
        assert!(id.lookup(100042, 901).is_err());
        assert_eq!(id.lookup(901, 100042).unwrap().name, "firefox");
        assert!(id.lookup(901, 100042).unwrap().allows(Global::Dmabuf));
        assert_eq!(id.lookup(901, 5).unwrap().name, "unknown");
        assert!(id.lookup(5, 5).is_ok());
    }

    #[test]
    fn rejects_duplicate_uids_and_names_and_bad_autostart() {
        let bad = |apps: &str| {
            let config: Config =
                toml::from_str(&format!("wayland-socket = \"/x\"\n{apps}")).unwrap();
            check_config(&config).unwrap_err()
        };
        assert!(
            bad("[[app]]\nname = \"a\"\nuid = 12\n[[app]]\nname = \"b\"\nuid = 12\n")
                .contains("uid 12")
        );
        assert!(
            bad("[[app]]\nname = \"a\"\nuid = 12\n[[app]]\nname = \"a\"\nuid = 13\n")
                .contains("twice")
        );
        assert!(bad("[[app]]\nname = \"a\"\nuid = 12\nautostart = true\n").contains("autostart"));
    }

    #[test]
    fn unknown_app_is_refused() {
        let (id, _) = identity();
        let err = id.launch(5, "nope").unwrap_err();
        assert!(err.contains("unknown app"));
    }

    #[test]
    fn unknown_field_is_an_error() {
        assert!(toml::from_str::<Config>("wayland-socket = \"/x\"\nfoo = 1\n").is_err());
    }
}
