//! drv-appd: the launcher for untrusted things, unprivileged. Knows the apps (a static manifest
//! generated from the system configuration: every app has a fixed UID), answers policy
//! lookups, and turns `Launch { app }` into a request to drv-forker, its privileged helper,
//! over the channel the supervisor made for the two of them. Launch is not on the public
//! socket: it is served on launch channels, socketpairs the supervisor made between us and
//! a launcher it started (the compositor). Apps get nothing but their manifest entry.
//! Android's PackageManager plus ActivityManager, in one small process with its own UID.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use drv_policy::daemon::{self, Handler};
use drv_policy::forker::{self, Launch};
use drv_policy::{AppPolicy, Global, Grant};
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
/// What forks for us: drv-forker over the channel, or a recorder in tests.
pub trait Forker: Send + Sync {
    fn launch(&self, launch: &Launch) -> Result<u32, String>;
}

impl Forker for forker::Channel {
    fn launch(&self, launch: &Launch) -> Result<u32, String> {
        forker::Channel::launch(self, launch)
            .map(|_| launch.uid)
            .map_err(|e| format!("forker: {e}"))
    }
}

pub struct Appd {
    config: Config,
    forker: Arc<dyn Forker>,
    /// Environment every launched app gets first: `PATH` and friends from our own environment.
    base_env: Vec<(String, String)>,
}

impl Appd {
    pub fn new(config: Config, forker: Arc<dyn Forker>, base_env: Vec<(String, String)>) -> Self {
        Self {
            config,
            forker,
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
    /// the compositor's apps socket as `WAYLAND_DISPLAY`. The forker sets `HOME` and
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

    fn start(self: &Arc<Self>, app: &AppConfig) -> Result<u32, String> {
        let argv = app
            .exec
            .clone()
            .ok_or_else(|| format!("{:?} is a service, not a launchable app", app.name))?;
        let launch = Launch {
            uid: app.uid,
            groups: app.groups.clone(),
            argv,
            env: self.env_for(app),
            network: app.network,
            expose: app.expose.clone(),
        };
        self.forker.launch(&launch)?;
        Ok(app.uid)
    }

    /// Serves launches on `sock` (a stream socket speaking `rpc`) for `who`, on a thread,
    /// until the other end closes. Lookups are refused there: the public socket answers them.
    pub fn serve_launcher(self: &Arc<Self>, who: String, sock: OwnedFd) {
        let launcher = Launcher {
            appd: self.clone(),
            who,
        };
        std::thread::spawn(move || {
            let stream = UnixStream::from(sock);
            if let Err(err) = daemon::serve_connection(stream, 0, &launcher) {
                eprintln!("drv-appd: {}'s launch channel: {err}", launcher.who);
            }
        });
    }

    /// Starts every `autostart` app in manifest order, once the compositor's socket is
    /// listening (the file alone may be a dead compositor's). Waits up to `timeout` for that.
    /// Once per start of the set: the apps die with it.
    pub fn autostart(self: &Arc<Self>, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while !socket_listening(&self.config.wayland_socket) {
            if Instant::now() > deadline {
                eprintln!(
                    "drv-appd: nothing listening at {} after {timeout:?}; not autostarting",
                    self.config.wayland_socket.display()
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        for app in self.config.apps.iter().filter(|a| a.autostart) {
            match self.start(app) {
                Ok(uid) => eprintln!("drv-appd: autostarted {:?} as uid {uid}", app.name),
                Err(err) => eprintln!("drv-appd: autostart {:?}: {err}", app.name),
            }
        }
    }
}

/// Whether some process is listening on the unix socket at `path`, per `/proc/net/unix`
/// (no connection is made, so the compositor never sees us).
fn socket_listening(path: &Path) -> bool {
    let Ok(table) = std::fs::read_to_string("/proc/net/unix") else {
        return false;
    };
    let path = path.to_string_lossy();
    table.lines().skip(1).any(|line| {
        let mut fields = line.split_whitespace();
        // Num RefCount Protocol Flags Type St Inode Path
        let flags = fields.nth(3);
        let sock_path = fields.nth(3);
        flags == Some("00010000") && sock_path == Some(path.as_ref())
    })
}

impl Handler for Appd {
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

    /// The public socket: reachable by every app, so it launches nothing.
    fn launch(&self, peer: u32, name: &str) -> Result<u32, String> {
        Err(format!(
            "uid {peer} asked to launch {name:?} on the public socket; only a launch channel may"
        ))
    }
}

/// One launch channel's handler: launches for `who`, nothing else.
pub struct Launcher {
    appd: Arc<Appd>,
    who: String,
}

impl Handler for Launcher {
    fn lookup(&self, _peer: u32, uid: u32) -> Result<AppPolicy, String> {
        Err(format!(
            "lookup of uid {uid} on a launch channel; the public socket answers those"
        ))
    }

    fn launch(&self, _peer: u32, name: &str) -> Result<u32, String> {
        let app = self
            .appd
            .app(name)
            .ok_or_else(|| format!("unknown app {name:?}; add it to appd.toml"))?;
        let uid = self.appd.start(app)?;
        eprintln!("drv-appd: launched {name:?} as uid {uid} for {}", self.who);
        Ok(uid)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    struct Recorder(Mutex<Vec<Launch>>);

    impl Forker for Recorder {
        fn launch(&self, launch: &Launch) -> Result<u32, String> {
            self.0.lock().unwrap().push(launch.clone());
            Ok(1)
        }
    }

    fn identity() -> (Arc<Appd>, Arc<Recorder>) {
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
        let id = Arc::new(Appd::new(
            config,
            recorder.clone(),
            vec![("PATH".to_owned(), "/bin".to_owned())],
        ));
        (id, recorder)
    }

    fn launcher(id: &Arc<Appd>) -> Launcher {
        Launcher {
            appd: id.clone(),
            who: "test".to_owned(),
        }
    }

    #[test]
    fn launch_builds_env_from_manifest_only() {
        let (id, recorder) = identity();
        assert_eq!(launcher(&id).launch(5, "firefox"), Ok(100042));
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
    fn the_public_socket_launches_nothing() {
        let (id, recorder) = identity();
        let err = id.launch(5, "firefox").unwrap_err();
        assert!(err.contains("public socket"), "{err}");
        assert!(recorder.0.lock().unwrap().is_empty());
        assert!(launcher(&id).lookup(5, 5).is_err());
    }

    #[test]
    fn services_are_not_launchable() {
        let (id, _) = identity();
        let err = launcher(&id).launch(5, "compositor").unwrap_err();
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
        let err = launcher(&id).launch(5, "nope").unwrap_err();
        assert!(err.contains("unknown app"));
    }

    #[test]
    fn unknown_field_is_an_error() {
        assert!(toml::from_str::<Config>("wayland-socket = \"/x\"\nfoo = 1\n").is_err());
    }
}
