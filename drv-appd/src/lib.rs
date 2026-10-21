//! drv-appd: the launcher for untrusted things, unprivileged. Knows the apps (a static manifest
//! generated from the system configuration: every app has a fixed UID), answers policy
//! lookups, and turns `Launch { app }` into a request to drv-forker, its privileged helper,
//! over the channel the supervisor made for the two of them. Launch is not on the public
//! socket: it is served on launch channels, socketpairs the supervisor made between us and
//! a launcher it started (the compositor). Apps get nothing but their manifest entry.
//! Android's PackageManager plus ActivityManager, in one small process with its own UID.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::{fmt, io};

use drv_policy::daemon::{self, Handler};
use drv_policy::forker::{self, Launch};
use drv_policy::rpc::Delivery;
use drv_policy::{rpc, seq, AppPolicy, Global, Grant};
use serde::{Deserialize, Serialize};

/// `appd.json`.
///
/// ```json
/// {
///   "wayland-socket": "/run/drv/wayland",
///   "env": { "PIPEWIRE_RUNTIME_DIR": "/run/pipewire" },
///   "app": [
///     { "name": "compositor", "uid": 901, "grants": ["lookup"] },
///     { "name": "firefox", "uid": 100042, "exec": ["firefox"], "gpu": true, "network": true, "autostart": true }
///   ]
/// }
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

fn yes() -> bool {
    true
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
    #[serde(default)]
    pub gpu: bool,
    /// PipeWire and a PulseAudio server of its own.
    #[serde(default)]
    pub audio: bool,
    /// Keep the host network; off means an empty network namespace.
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub globals: Vec<Global>,
    #[serde(default)]
    pub grants: Vec<Grant>,
    #[serde(default)]
    pub icon: Option<String>,
    /// May use the ssh agent; drv-agent asks on every connection.
    #[serde(default)]
    pub agent: bool,
    /// The origins it may claim at the FIDO door (`app:dev.rho.Gui`).
    #[serde(default)]
    pub fido: Vec<String>,
    /// Extra environment for this app only.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Started by the daemon once the compositor's socket exists, in manifest order.
    #[serde(default)]
    pub autostart: bool,
    /// Listed by the menu. Off for daemons and probes that autostart.
    #[serde(default = "yes")]
    pub menu: bool,
    /// URI schemes this app opens (`"https"`), lowercase. One handler per scheme: the
    /// OpenURI portal starts it with the URI as its last argument.
    #[serde(default)]
    pub opens: Vec<String>,
    /// Runs nix: the daemon's socket in its root (and, from drv-init, the whole store).
    #[serde(default)]
    pub nix: bool,
    /// Makes code at runtime (a JIT): the forker leaves W^X memory to it.
    #[serde(default)]
    pub jit: bool,
    /// May make user namespaces (a browser's own sandbox).
    #[serde(default)]
    pub userns: bool,
    /// Folders of the person's files the app has as its own (`~/<name>`), drv-files' on disk.
    #[serde(default)]
    pub folders: Vec<String>,
    /// One instance at most, which owns this bus name (its desktop app id, `dev.rho.Gui`)
    /// on its private bus. Once it runs, URIs and launches are not new processes: its shim
    /// passes them on as `org.freedesktop.Application` calls (Android's `onNewIntent`).
    /// The app needs the shim (a bus).
    #[serde(default)]
    pub single: Option<String>,
}

impl AppConfig {
    fn policy(&self) -> AppPolicy {
        AppPolicy {
            name: self.name.clone(),
            gpu: self.gpu,
            globals: self.globals.clone(),
            grants: self.grants.clone(),
            icon: self.icon.clone(),
            agent: self.agent,
            fido: self.fido.clone(),
        }
    }
}

#[derive(Debug)]
pub enum Error {
    Io(PathBuf, std::io::Error),
    Parse(PathBuf, serde_json::Error),
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
    let config: Config =
        serde_json::from_str(&text).map_err(|e| Error::Parse(path.to_owned(), e))?;
    check_config(&config).map_err(Error::Config)?;
    Ok(config)
}

/// A UID is one identity, a name is one app, and an autostart entry must be launchable.
fn check_config(config: &Config) -> Result<(), String> {
    let mut names = HashSet::new();
    let mut uids = HashSet::new();
    let mut schemes = HashSet::new();
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
        if let Some(name) = &app.single {
            if app.exec.is_none() {
                return Err(format!("app {:?} is single but has no exec", app.name));
            }
            if !bus_name(name) {
                return Err(format!(
                    "app {:?} is single as {name:?}, which is not a well-known bus name",
                    app.name
                ));
            }
        }
        for scheme in &app.opens {
            if app.exec.is_none() {
                return Err(format!(
                    "app {:?} opens {scheme:?} but has no exec",
                    app.name
                ));
            }
            if rpc::uri_scheme(&format!("{scheme}:")).as_deref() != Some(scheme.as_str()) {
                return Err(format!(
                    "app {:?} opens {scheme:?}, which is not a lowercase scheme",
                    app.name
                ));
            }
            if !schemes.insert(scheme) {
                return Err(format!("scheme {scheme:?} has more than one handler"));
            }
        }
    }
    Ok(())
}

/// A well-known D-Bus name of two or more dot-separated elements of `[A-Za-z0-9_-]`, none
/// starting with a digit: what the shim calls and what its object path is made from.
fn bus_name(name: &str) -> bool {
    name.len() <= 255
        && name.split('.').count() >= 2
        && name.split('.').all(|e| {
            e.bytes().next().is_some_and(|b| !b.is_ascii_digit())
                && e.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
}

/// What forks for us: drv-forker over the channel, or a recorder in tests. `deliveries` is
/// the app's end of its deliveries line, which `launch.deliveries` says it carries.
pub trait Forker: Send + Sync {
    fn launch(&self, launch: &Launch, deliveries: Option<BorrowedFd<'_>>) -> Result<u32, String>;
}

impl Forker for forker::Channel {
    fn launch(&self, launch: &Launch, deliveries: Option<BorrowedFd<'_>>) -> Result<u32, String> {
        forker::Channel::launch(self, launch, deliveries)
            .map(|_| launch.uid)
            .map_err(|e| format!("forker: {e}"))
    }
}

pub struct Appd {
    config: Config,
    forker: Arc<dyn Forker>,
    /// Environment every launched app gets first: `PATH` and friends from our own environment.
    base_env: Vec<(String, String)>,
    autostarted: std::sync::Once,
    /// Our ends of the deliveries lines of the `single` apps started, by uid: the app holds
    /// the other end for as long as its namespace lives. Non-blocking: a delivery never
    /// waits on an app. Under this lock too: starting one, so two opens start it once.
    lines: Mutex<HashMap<u32, OwnedFd>>,
}

impl Appd {
    pub fn new(config: Config, forker: Arc<dyn Forker>, base_env: Vec<(String, String)>) -> Self {
        Self {
            config,
            forker,
            base_env,
            autostarted: std::sync::Once::new(),
            lines: Mutex::new(HashMap::new()),
        }
    }

    fn app(&self, name: &str) -> Option<&AppConfig> {
        self.config.apps.iter().find(|a| a.name == name)
    }

    /// The manifest's handler for `uri`'s scheme opens it: started with the URI as its last
    /// argument, or handed it if it is `single` and runs. `who` is for the log.
    pub fn open_for(&self, who: &str, uri: &str, token: Option<&str>) -> Result<u32, String> {
        let scheme = rpc::uri_scheme(uri).ok_or_else(|| format!("{uri:?} is not a URI"))?;
        let app = self
            .config
            .apps
            .iter()
            .find(|a| a.opens.contains(&scheme))
            .ok_or_else(|| format!("no app opens {scheme}: URIs"))?;
        let uid = self.run(app, Some(uri), token)?;
        drv_os::say!(
            "drv-appd: {:?} (uid {uid}) opens {uri:?} for {who}",
            app.name
        );
        Ok(uid)
    }

    /// Starts `app`, unless it is `single` and runs: then the URI (or, without one, an
    /// activation) goes down its line, and its shim passes it on. A line that hung up is an
    /// app that exited: it is started again.
    fn run(&self, app: &AppConfig, uri: Option<&str>, token: Option<&str>) -> Result<u32, String> {
        let token = token.filter(|t| rpc::activation_token(t));
        if app.single.is_none() {
            return self.start(app, uri, token, None);
        }
        if let Some(uri) = uri {
            check_opens(app, uri)?;
        }
        let mut lines = self.lines.lock().unwrap();
        if let Some(line) = lines.get(&app.uid) {
            let delivery = match uri {
                Some(uri) => Delivery::Open {
                    uris: vec![uri.to_owned()],
                    token: token.map(str::to_owned),
                },
                None => Delivery::Activate {
                    token: token.map(str::to_owned),
                },
            };
            match seq::send(line, &delivery, &[]) {
                Ok(()) => {
                    drv_os::say!("drv-appd: {:?} runs: delivered {delivery:?}", app.name);
                    return Ok(app.uid);
                }
                Err(err) if gone(&err) => {
                    drv_os::say!("drv-appd: {:?} has exited; starting it", app.name);
                    lines.remove(&app.uid);
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    return Err(format!(
                        "{:?} runs but has not read what it was sent; not sending more",
                        app.name
                    ));
                }
                Err(err) => return Err(format!("delivering to {:?}: {err}", app.name)),
            }
        }
        let (ours, theirs) =
            drv_os::fds::seqpacket_pair().map_err(|e| format!("a deliveries line: {e}"))?;
        rustix::io::ioctl_fionbio(&ours, true).map_err(|e| format!("a deliveries line: {e}"))?;
        let uid = self.start(app, uri, token, Some(theirs.as_fd()))?;
        // Ours only once it started: a failed start leaves nothing to deliver to.
        lines.insert(uid, ours);
        Ok(uid)
    }

    fn by_uid(&self, uid: u32) -> Option<&AppConfig> {
        self.config.apps.iter().find(|a| a.uid == uid)
    }

    /// What a menu may offer: every entry with an `exec`, in manifest order.
    fn launchable(&self) -> Vec<String> {
        self.config
            .apps
            .iter()
            .filter(|a| a.exec.is_some() && a.menu)
            .map(|a| a.name.clone())
            .collect()
    }

    /// The child's environment: ours (`PATH`..), the config's `[env]`, the app's own, then
    /// the compositor's apps socket as `WAYLAND_DISPLAY`. drv-init sets `HOME` and
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

    /// Starts `app`; with `uri`, as the handler of its scheme, which the manifest must say
    /// it is. The URI is the one argument that ever comes from outside the manifest; an
    /// activation token (`XDG_ACTIVATION_TOKEN`, checked to be one) the one variable.
    fn start(
        &self,
        app: &AppConfig,
        uri: Option<&str>,
        token: Option<&str>,
        deliveries: Option<BorrowedFd<'_>>,
    ) -> Result<u32, String> {
        let mut argv = app
            .exec
            .clone()
            .ok_or_else(|| format!("{:?} is a service, not a launchable app", app.name))?;
        if let Some(uri) = uri {
            check_opens(app, uri)?;
            argv.push(uri.to_owned());
        }
        let mut env = self.env_for(app);
        if let Some(token) = token.filter(|t| rpc::activation_token(t)) {
            env.push(("XDG_ACTIVATION_TOKEN".to_owned(), token.to_owned()));
        }
        if let Some(name) = &app.single {
            env.push((drv_os::fds::SINGLE_ENV.to_owned(), name.clone()));
        }
        let launch = Launch {
            uid: app.uid,
            argv,
            env,
            network: app.network,
            gpu: app.gpu,
            nix: app.nix,
            folders: app.folders.clone(),
            jit: app.jit,
            userns: app.userns,
            deliveries: deliveries.is_some(),
        };
        self.forker.launch(&launch, deliveries)?;
        Ok(app.uid)
    }

    /// Serves launches on `sock` (a stream socket speaking `rpc`) for `who`, on a thread,
    /// until the other end closes. Lookups are refused there: the public socket answers them.
    /// With `autostart`, the peer's `Hello` starts the autostart apps: the compositor says it
    /// once its apps socket listens.
    pub fn serve_launcher(self: &Arc<Self>, who: String, sock: OwnedFd, autostart: bool) {
        let launcher = Launcher {
            appd: self.clone(),
            who,
            autostart,
        };
        std::thread::spawn(move || {
            let stream = UnixStream::from(sock);
            if let Err(err) = daemon::serve_connection(stream, 0, &launcher) {
                drv_os::say!("drv-appd: {}'s launch channel: {err}", launcher.who);
            }
        });
    }

    /// Starts every `autostart` app in manifest order. Once per start of the set: the apps
    /// die with it.
    pub fn autostart(self: &Arc<Self>) {
        self.autostarted.call_once(|| {
            for app in self.config.apps.iter().filter(|a| a.autostart) {
                match self.run(app, None, None) {
                    Ok(uid) => drv_os::say!("drv-appd: autostarted {:?} as uid {uid}", app.name),
                    Err(err) => drv_os::say!("drv-appd: autostart {:?}: {err}", app.name),
                }
            }
        });
    }
}

/// A send that found the app's end closed: the app is gone.
fn gone(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset | io::ErrorKind::NotConnected
    )
}

/// `app` is its scheme's handler, as the manifest says.
fn check_opens(app: &AppConfig, uri: &str) -> Result<(), String> {
    let scheme = rpc::uri_scheme(uri).ok_or_else(|| "not a URI".to_owned())?;
    if !app.opens.contains(&scheme) {
        return Err(format!("{:?} does not open {scheme}: URIs", app.name));
    }
    Ok(())
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
    fn launch(&self, peer: u32, name: &str, _token: Option<&str>) -> Result<u32, String> {
        Err(format!(
            "uid {peer} asked to launch {name:?} on the public socket; only a launch channel may"
        ))
    }

    fn apps(&self, peer: u32) -> Result<Vec<String>, String> {
        Err(format!(
            "uid {peer} asked for the app list on the public socket; only a launch channel may"
        ))
    }

    /// An app opening a URI (the shim's `OpenURI`): the manifest handler starts for it.
    /// Anything not in the manifest is refused.
    fn open(&self, peer: u32, uri: &str, token: Option<&str>) -> Result<u32, String> {
        let Some(app) = self.by_uid(peer) else {
            return Err(format!("uid {peer} is not an app; it opens nothing"));
        };
        self.open_for(&format!("{} (uid {peer})", app.name), uri, token)
    }
}

/// One launch channel's handler: launches for `who`, nothing else.
pub struct Launcher {
    appd: Arc<Appd>,
    who: String,
    autostart: bool,
}

impl Handler for Launcher {
    fn lookup(&self, _peer: u32, uid: u32) -> Result<AppPolicy, String> {
        Err(format!(
            "lookup of uid {uid} on a launch channel; the public socket answers those"
        ))
    }

    /// A `single` app that runs is activated instead.
    fn launch(&self, _peer: u32, name: &str, token: Option<&str>) -> Result<u32, String> {
        let app = self
            .appd
            .app(name)
            .ok_or_else(|| format!("unknown app {name:?}; add it to appd.json"))?;
        let uid = self.appd.run(app, None, token)?;
        drv_os::say!("drv-appd: launched {name:?} as uid {uid} for {}", self.who);
        Ok(uid)
    }

    fn apps(&self, _peer: u32) -> Result<Vec<String>, String> {
        Ok(self.appd.launchable())
    }

    fn open(&self, _peer: u32, uri: &str, token: Option<&str>) -> Result<u32, String> {
        self.appd.open_for(&self.who, uri, token)
    }

    fn hello(&self, _peer: u32) {
        if self.autostart {
            self.appd.autostart();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// The launches, and each launched app's end of its deliveries line (the app's
    /// namespace, as far as drv-appd can tell: dropping it is the app exiting).
    struct Recorder(Mutex<Vec<Launch>>, Mutex<Vec<Option<OwnedFd>>>);

    impl Forker for Recorder {
        fn launch(
            &self,
            launch: &Launch,
            deliveries: Option<BorrowedFd<'_>>,
        ) -> Result<u32, String> {
            assert_eq!(launch.deliveries, deliveries.is_some());
            self.0.lock().unwrap().push(launch.clone());
            self.1
                .lock()
                .unwrap()
                .push(deliveries.map(|fd| fd.try_clone_to_owned().unwrap()));
            Ok(1)
        }
    }

    fn identity() -> (Arc<Appd>, Arc<Recorder>) {
        let config: Config = serde_json::from_str(
            r#"{
              "wayland-socket": "/run/drv/wayland",
              "env": { "PIPEWIRE_RUNTIME_DIR": "/run/pipewire" },
              "app": [
                { "name": "compositor", "uid": 901, "grants": ["lookup"] },
                { "name": "firefox", "uid": 100042, "exec": ["firefox"], "gpu": true,
                  "env": { "MOZ_ENABLE_WAYLAND": "1" }, "opens": ["https"] },
                { "name": "rho", "uid": 100102, "exec": ["rho-gui"], "opens": ["rho"],
                  "single": "dev.rho.Gui" }
              ]
            }"#,
        )
        .unwrap();
        check_config(&config).unwrap();
        let recorder = Arc::new(Recorder(Mutex::new(Vec::new()), Mutex::new(Vec::new())));
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
            autostart: false,
        }
    }

    #[test]
    fn launch_builds_env_from_manifest_only() {
        let (id, recorder) = identity();
        assert_eq!(launcher(&id).launch(5, "firefox", None), Ok(100042));
        let requests = recorder.0.lock().unwrap();
        let req = &requests[0];
        assert_eq!(req.argv, vec!["firefox"]);
        assert!(req.gpu && !req.nix && !req.network);
        let get = |k: &str| {
            req.env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("PATH"), Some("/bin"));
        assert_eq!(get("PIPEWIRE_RUNTIME_DIR"), Some("/run/pipewire"));
        assert_eq!(get("MOZ_ENABLE_WAYLAND"), Some("1"));
        assert_eq!(get("WAYLAND_DISPLAY"), Some("/run/drv/wayland"));
    }

    #[test]
    fn the_public_socket_launches_nothing() {
        let (id, recorder) = identity();
        let err = id.launch(5, "firefox", None).unwrap_err();
        assert!(err.contains("public socket"), "{err}");
        assert!(recorder.0.lock().unwrap().is_empty());
        assert!(launcher(&id).lookup(5, 5).is_err());
    }

    #[test]
    fn services_are_not_launchable() {
        let (id, _) = identity();
        let err = launcher(&id).launch(5, "compositor", None).unwrap_err();
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
                serde_json::from_str(&format!(r#"{{"wayland-socket": "/x", "app": [{apps}]}}"#))
                    .unwrap();
            check_config(&config).unwrap_err()
        };
        assert!(bad(r#"{"name": "a", "uid": 12}, {"name": "b", "uid": 12}"#).contains("uid 12"));
        assert!(bad(r#"{"name": "a", "uid": 12}, {"name": "a", "uid": 13}"#).contains("twice"));
        assert!(bad(r#"{"name": "a", "uid": 12, "autostart": true}"#).contains("autostart"));
    }

    #[test]
    fn open_appends_the_uri_for_the_handler_only() {
        let (id, recorder) = identity();
        let launcher = launcher(&id);
        assert_eq!(
            launcher.open(5, "HTTPS://example.com/a?b=c#d", None),
            Ok(100042)
        );
        assert_eq!(
            recorder.0.lock().unwrap()[0].argv,
            vec!["firefox", "HTTPS://example.com/a?b=c#d"]
        );
        for bad in [
            "http://example.com",
            "-https://x",
            "https://a b",
            "https",
            "https:\u{e9}",
            "mailto:x@y",
        ] {
            assert!(launcher.open(5, bad, None).is_err(), "{bad}");
        }
        // The public socket: an app may open, a uid that is no app may not.
        assert!(id
            .open(5, "https://example.com", None)
            .unwrap_err()
            .contains("not an app"));
        assert_eq!(recorder.0.lock().unwrap().len(), 1);
        assert_eq!(id.open(100042, "https://example.com", None), Ok(100042));
        assert_eq!(recorder.0.lock().unwrap().len(), 2);
    }

    #[test]
    fn one_handler_per_scheme() {
        let config: Config = serde_json::from_str(
            r#"{"wayland-socket": "/x", "app": [
                {"name": "a", "uid": 12, "exec": ["a"], "opens": ["https"]},
                {"name": "b", "uid": 13, "exec": ["b"], "opens": ["https"]}]}"#,
        )
        .unwrap();
        assert!(check_config(&config)
            .unwrap_err()
            .contains("more than one handler"));
        let config: Config = serde_json::from_str(
            r#"{"wayland-socket": "/x", "app": [{"name": "a", "uid": 12, "exec": ["a"], "opens": ["HTTPS"]}]}"#,
        )
        .unwrap();
        assert!(check_config(&config).unwrap_err().contains("lowercase"));
    }

    fn open(uri: &str, token: Option<&str>) -> Delivery {
        Delivery::Open {
            uris: vec![uri.to_owned()],
            token: token.map(str::to_owned),
        }
    }

    /// What is waiting on launch `i`'s line, without blocking: the app has not read it.
    fn waiting(recorder: &Recorder, i: usize) -> Vec<Delivery> {
        let ends = recorder.1.lock().unwrap();
        let end = ends[i].as_ref().expect("a single app gets a line");
        rustix::io::ioctl_fionbio(end, true).unwrap();
        std::iter::from_fn(|| seq::recv::<Delivery>(end).ok().map(|(d, _)| d)).collect()
    }

    #[test]
    fn a_single_app_that_runs_is_handed_the_uri_not_started_again() {
        let (id, recorder) = identity();
        let launches = || recorder.0.lock().unwrap().len();
        // Not running: started with the URI, as any handler, and with its line.
        assert_eq!(id.open(100042, "rho://a", None), Ok(100102));
        assert_eq!(
            recorder.0.lock().unwrap()[0].argv,
            vec!["rho-gui", "rho://a"]
        );
        let get = |k: &str| {
            recorder.0.lock().unwrap()[0]
                .env
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
        };
        assert_eq!(get("DRV_SINGLE").as_deref(), Some("dev.rho.Gui"));
        // Right after the start, before anything in the app reads: no second process, and
        // the line holds it until the shim does.
        assert_eq!(id.open(100042, "rho://b", Some("t1")), Ok(100102));
        assert_eq!(launcher(&id).launch(5, "rho", Some("t2")), Ok(100102));
        assert_eq!(launches(), 1);
        assert_eq!(
            waiting(&recorder, 0),
            vec![
                open("rho://b", Some("t1")),
                Delivery::Activate {
                    token: Some("t2".to_owned())
                }
            ]
        );
        // Still the scheme check: rho opens rho: URIs only, and firefox gets no line.
        assert!(id.open(100042, "https://x", None).is_ok());
        assert_eq!(recorder.0.lock().unwrap()[1].argv[0], "firefox");
        assert!(recorder.1.lock().unwrap()[1].is_none());
        assert!(waiting(&recorder, 0).is_empty());
        // The app exits (its namespace, and the line with it): the next open starts it again,
        // with that URI and a new line.
        recorder.1.lock().unwrap()[0] = None;
        assert_eq!(id.open(100042, "rho://d", None), Ok(100102));
        assert_eq!(launches(), 3);
        assert_eq!(
            recorder.0.lock().unwrap()[2].argv,
            vec!["rho-gui", "rho://d"]
        );
        assert_eq!(id.open(100042, "rho://e", None), Ok(100102));
        assert_eq!(waiting(&recorder, 2), vec![open("rho://e", None)]);
        assert_eq!(launches(), 3);
    }

    #[test]
    fn an_app_that_is_not_single_starts_every_time() {
        let (id, recorder) = identity();
        assert_eq!(id.open(100102, "https://a", None), Ok(100042));
        assert_eq!(id.open(100102, "https://b", None), Ok(100042));
        assert_eq!(launcher(&id).launch(5, "firefox", None), Ok(100042));
        assert_eq!(recorder.0.lock().unwrap().len(), 3);
        assert!(recorder.1.lock().unwrap().iter().all(Option::is_none));
        assert!(recorder
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|l| !l.deliveries && !l.env.iter().any(|(k, _)| k == "DRV_SINGLE")));
    }

    #[test]
    fn a_single_app_whose_start_failed_is_started_by_the_next_open() {
        struct Failing(Mutex<u32>);
        impl Forker for Failing {
            fn launch(&self, _: &Launch, _: Option<BorrowedFd<'_>>) -> Result<u32, String> {
                *self.0.lock().unwrap() += 1;
                Err("forker: down".to_owned())
            }
        }
        let (id, _) = identity();
        let failing = Arc::new(Failing(Mutex::new(0)));
        let id = Appd::new(id.config.clone(), failing.clone(), Vec::new());
        assert!(id.open(100042, "rho://a", None).is_err());
        assert!(id.open(100042, "rho://b", None).is_err());
        assert_eq!(*failing.0.lock().unwrap(), 2);
    }

    #[test]
    fn an_app_that_does_not_read_is_not_sent_more_forever() {
        let (id, _recorder) = identity();
        assert!(id.open(100042, "rho://a", None).is_ok());
        let long = format!("rho://{}", "x".repeat(8000));
        let mut refused = None;
        for i in 0..10_000 {
            if let Err(err) = id.open(100042, &long, None) {
                refused = Some((i, err));
                break;
            }
        }
        let (_, err) = refused.expect("a full line is refused, not waited on");
        assert!(err.contains("has not read"), "{err}");
    }

    #[test]
    fn the_token_of_a_first_start_is_its_environment() {
        let (id, recorder) = identity();
        assert!(id.open(100102, "https://a", Some("tok-1")).is_ok());
        assert!(id.open(100102, "https://b", Some("bad token")).is_ok());
        let requests = recorder.0.lock().unwrap();
        let token = |i: usize| {
            requests[i]
                .env
                .iter()
                .find(|(k, _)| k == "XDG_ACTIVATION_TOKEN")
                .map(|(_, v)| v.clone())
        };
        assert_eq!(token(0).as_deref(), Some("tok-1"));
        assert_eq!(token(1), None);
    }

    #[test]
    fn single_needs_a_bus_name() {
        let bad = |single: &str| {
            let config: Config = serde_json::from_str(&format!(
                r#"{{"wayland-socket": "/x", "app": [{{"name": "a", "uid": 12, "exec": ["a"], "single": {single:?}}}]}}"#
            ))
            .unwrap();
            check_config(&config)
        };
        assert!(bad("dev.rho.Gui").is_ok());
        assert!(bad("org.example.my-app_2").is_ok());
        for name in ["Gui", "dev..Gui", "dev.2rho", "dev.rho/Gui", ""] {
            assert!(bad(name).is_err(), "{name}");
        }
    }

    #[test]
    fn unknown_app_is_refused() {
        let (id, _) = identity();
        let err = launcher(&id).launch(5, "nope", None).unwrap_err();
        assert!(err.contains("unknown app"));
    }

    #[test]
    fn unknown_field_is_an_error() {
        assert!(serde_json::from_str::<Config>(r#"{"wayland-socket": "/x", "foo": 1}"#).is_err());
    }
}
