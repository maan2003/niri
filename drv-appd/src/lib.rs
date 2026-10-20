//! drv-appd: the launcher for untrusted things, unprivileged. Knows the apps (a static manifest
//! generated from the system configuration: every app has a fixed UID), answers policy
//! lookups, and turns `Launch { app }` into a request to drv-forker, its privileged helper,
//! over the channel the supervisor made for the two of them. Launch is not on the public
//! socket: it is served on launch channels, socketpairs the supervisor made between us and
//! a launcher it started (the compositor). Apps get nothing but their manifest entry.
//! Android's PackageManager plus ActivityManager, in one small process with its own UID.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use std::{fmt, io};

use drv_policy::daemon::{self, Handler};
use drv_policy::forker::{self, Launch};
use drv_policy::rpc::{Delivery, Response};
use drv_policy::{rpc, AppPolicy, Global, Grant};
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

/// How long a `single` app may take from its start to its shim's `Receive`. Opens meanwhile
/// wait for it; after this, the start is taken to have failed and the next open starts it
/// again.
const STARTUP: Duration = Duration::from_secs(30);

/// How long a shim may take to take a delivery on. It answers once queued, not once the app
/// has it, so this is short: the openers' own clients give up after two seconds.
const DELIVERY: Duration = Duration::from_secs(1);

/// A `single` app's instance, from its start to its shim's hang-up.
enum Instance {
    /// Started then; its shim has not asked for deliveries yet. What comes meanwhile waits.
    Starting {
        since: Instant,
        waiting: Vec<Delivery>,
    },
    /// Its shim's connection: deliveries go down it one at a time, each answered. Its
    /// closing is the app's exit.
    Running(Arc<Mutex<UnixStream>>),
}

/// Why a delivery did not get through.
enum Undelivered {
    /// The shim is gone (so the app is): start it instead.
    Gone(io::Error),
    /// The shim refused it, or did not answer in time.
    Failed(String),
}

/// One delivery down a shim's connection, and its answer.
fn deliver(stream: &UnixStream, delivery: &Delivery) -> Result<(), Undelivered> {
    let gone = |err: io::Error| match err.kind() {
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
            // A late answer would be taken for the next one's: this line is done.
            let _ = stream.shutdown(std::net::Shutdown::Both);
            Undelivered::Failed(format!("the shim did not answer within {DELIVERY:?}"))
        }
        _ => Undelivered::Gone(err),
    };
    stream.set_write_timeout(Some(DELIVERY)).map_err(gone)?;
    stream.set_read_timeout(Some(DELIVERY)).map_err(gone)?;
    rpc::write_msg(stream, delivery).map_err(gone)?;
    match rpc::read_msg::<Result<(), String>>(stream).map_err(gone)? {
        Ok(()) => Ok(()),
        Err(err) => Err(Undelivered::Failed(format!("the shim refused it: {err}"))),
    }
}

/// Whether a shim's connection is still up: nothing to read (it never speaks unasked) and
/// no hang-up.
fn alive(stream: &UnixStream) -> bool {
    use rustix::event::{poll, PollFd, PollFlags};
    let mut fds = [PollFd::new(stream, PollFlags::IN)];
    matches!(
        poll(&mut fds, Some(&rustix::event::Timespec::default())),
        Ok(0)
    )
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
    autostarted: std::sync::Once,
    /// The `single` apps started, by uid.
    instances: Mutex<HashMap<u32, Instance>>,
}

impl Appd {
    pub fn new(config: Config, forker: Arc<dyn Forker>, base_env: Vec<(String, String)>) -> Self {
        Self {
            config,
            forker,
            base_env,
            autostarted: std::sync::Once::new(),
            instances: Mutex::new(HashMap::new()),
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

    /// Starts `app`, unless it is `single` and started already: then the URI (or, without
    /// one, an activation) goes to the instance. One that is starting gets it once its shim
    /// asks; one whose shim is gone is started again.
    fn run(&self, app: &AppConfig, uri: Option<&str>, token: Option<&str>) -> Result<u32, String> {
        let token = token.filter(|t| rpc::activation_token(t));
        if app.single.is_none() {
            return self.start(app, uri, token);
        }
        if let Some(uri) = uri {
            check_opens(app, uri)?;
        }
        let token_ = token.map(str::to_owned);
        let delivery = match uri {
            Some(uri) => Delivery::Open {
                uris: vec![uri.to_owned()],
                token: token_,
            },
            None => Delivery::Activate { token: token_ },
        };
        let mut instances = self.instances.lock().unwrap();
        match instances.get_mut(&app.uid) {
            Some(Instance::Starting { since, waiting }) if since.elapsed() < STARTUP => {
                // A launch while it starts: its window is coming anyway.
                if matches!(delivery, Delivery::Open { .. }) {
                    waiting.push(delivery);
                }
                return Ok(app.uid);
            }
            Some(Instance::Starting { waiting, .. }) => {
                drv_os::say!(
                    "drv-appd: {:?} never asked for deliveries ({} lost); starting it again",
                    app.name,
                    waiting.len()
                );
            }
            Some(Instance::Running(conn)) => {
                let conn = conn.clone();
                // Not under the map's lock: one stuck shim must not hold up every app.
                drop(instances);
                let delivered = deliver(&conn.lock().unwrap(), &delivery);
                match delivered {
                    Ok(()) => return Ok(app.uid),
                    Err(Undelivered::Failed(err)) => {
                        drv_os::say!("drv-appd: delivering to {:?}: {err}", app.name);
                        let mut instances = self.instances.lock().unwrap();
                        if matches!(instances.get(&app.uid), Some(Instance::Running(c)) if Arc::ptr_eq(c, &conn))
                            && !alive(&conn.lock().unwrap())
                        {
                            instances.remove(&app.uid);
                        }
                        return Err(format!("{:?} runs but did not take it: {err}", app.name));
                    }
                    Err(Undelivered::Gone(err)) => {
                        drv_os::say!("drv-appd: {:?} has exited ({err}); starting it", app.name);
                        instances = self.instances.lock().unwrap();
                        // Someone else may have started it again meanwhile.
                        match instances.get(&app.uid) {
                            Some(Instance::Running(c)) if Arc::ptr_eq(c, &conn) => {}
                            None => {}
                            Some(_) => {
                                drop(instances);
                                return self.run(app, uri, token);
                            }
                        }
                    }
                }
            }
            None => {}
        }
        let uid = self.start(app, uri, token)?;
        instances.insert(
            uid,
            Instance::Starting {
                since: Instant::now(),
                waiting: Vec::new(),
            },
        );
        Ok(uid)
    }

    /// A shim's `Receive`: its connection becomes `peer`'s instance, and what waited for it
    /// goes down it first.
    fn take_receiver(&self, peer: u32, stream: UnixStream) -> io::Result<()> {
        let refuse = |err: String| rpc::write_msg(&stream, &Response::Error(err));
        let Some(app) = self.by_uid(peer) else {
            return refuse(format!("uid {peer} is not an app"));
        };
        let Some(name) = app.single.clone() else {
            return refuse(format!(
                "{:?} is not single; nothing is delivered to it",
                app.name
            ));
        };
        let mut instances = self.instances.lock().unwrap();
        let waiting = match instances.remove(&peer) {
            Some(Instance::Running(old)) if alive(&old.lock().unwrap()) => {
                instances.insert(peer, Instance::Running(old));
                drop(instances);
                return refuse(format!("{:?} already has a receiver", app.name));
            }
            Some(Instance::Starting { waiting, .. }) => waiting,
            Some(Instance::Running(_)) | None => Vec::new(),
        };
        rpc::write_msg(&stream, &Response::Receiving { name })?;
        let conn = Arc::new(Mutex::new(stream));
        // Ours until the waiting ones are down it: they go first.
        let line = conn.lock().unwrap();
        instances.insert(peer, Instance::Running(conn.clone()));
        drop(instances);
        drv_os::say!(
            "drv-appd: {:?} (uid {peer}) receives; {} waited",
            app.name,
            waiting.len()
        );
        for delivery in waiting {
            match deliver(&line, &delivery) {
                Ok(()) => {}
                Err(Undelivered::Failed(err)) => {
                    drv_os::say!("drv-appd: delivering to {:?}: {err}", app.name)
                }
                Err(Undelivered::Gone(err)) => {
                    drv_os::say!("drv-appd: delivering to {:?}: {err}", app.name);
                    break;
                }
            }
        }
        Ok(())
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
        };
        self.forker.launch(&launch)?;
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

    /// A `single` app's shim, asking for its deliveries.
    fn receive(&self, peer: u32, stream: UnixStream) -> io::Result<()> {
        self.take_receiver(peer, stream)
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

    struct Recorder(Mutex<Vec<Launch>>);

    impl Forker for Recorder {
        fn launch(&self, launch: &Launch) -> Result<u32, String> {
            self.0.lock().unwrap().push(launch.clone());
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

    /// A shim on `uid`'s `Receive`: what it is delivered comes out of the channel, each
    /// answered `Ok`. The stream returned is its end, to shut down as the app exiting.
    fn shim(id: &Arc<Appd>, uid: u32) -> (UnixStream, std::sync::mpsc::Receiver<Delivery>) {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let end = ours.try_clone().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            match rpc::read_msg::<Response>(&ours) {
                Ok(Response::Receiving { name }) => assert_eq!(name, "dev.rho.Gui"),
                other => panic!("receive answered {other:?}"),
            }
            while let Ok(delivery) = rpc::read_msg::<Delivery>(&ours) {
                tx.send(delivery).unwrap();
                if rpc::write_msg(&ours, &Ok::<(), String>(())).is_err() {
                    break;
                }
            }
        });
        id.receive(uid, theirs).unwrap();
        (end, rx)
    }

    fn got(rx: &std::sync::mpsc::Receiver<Delivery>) -> Delivery {
        rx.recv_timeout(Duration::from_secs(2)).expect("a delivery")
    }

    fn open(uri: &str, token: Option<&str>) -> Delivery {
        Delivery::Open {
            uris: vec![uri.to_owned()],
            token: token.map(str::to_owned),
        }
    }

    #[test]
    fn a_single_app_that_runs_is_handed_the_uri_not_started_again() {
        let (id, recorder) = identity();
        let launches = || recorder.0.lock().unwrap().len();
        // Not running: started with the URI, as any handler.
        assert_eq!(id.open(100042, "rho://a", None), Ok(100102));
        assert_eq!(
            recorder.0.lock().unwrap()[0].argv,
            vec!["rho-gui", "rho://a"]
        );
        // Starting: the next one waits for its shim, and is no second process.
        assert_eq!(id.open(100042, "rho://b", Some("t1")), Ok(100102));
        // A launch meanwhile is nothing: the window is coming.
        assert_eq!(launcher(&id).launch(5, "rho", Some("t0")), Ok(100102));
        assert_eq!(launches(), 1);
        let (end, rx) = shim(&id, 100102);
        assert_eq!(got(&rx), open("rho://b", Some("t1")));
        // Running: straight to the shim.
        assert_eq!(id.open(100042, "rho://c", None), Ok(100102));
        assert_eq!(got(&rx), open("rho://c", None));
        assert_eq!(launcher(&id).launch(5, "rho", Some("t2")), Ok(100102));
        assert_eq!(
            got(&rx),
            Delivery::Activate {
                token: Some("t2".to_owned())
            }
        );
        // Still the scheme check: rho opens rho: URIs only.
        assert!(id.open(100042, "https://x", None).is_ok());
        assert_eq!(recorder.0.lock().unwrap()[1].argv[0], "firefox");
        assert!(rx.try_recv().is_err());
        assert_eq!(launches(), 2);
        // The app exits (its shim with it): the next open starts it again, with that URI.
        end.shutdown(std::net::Shutdown::Both).unwrap();
        drop(rx);
        assert_eq!(id.open(100042, "rho://d", None), Ok(100102));
        assert_eq!(launches(), 3);
        assert_eq!(
            recorder.0.lock().unwrap()[2].argv,
            vec!["rho-gui", "rho://d"]
        );
    }

    #[test]
    fn an_app_that_is_not_single_starts_every_time() {
        let (id, recorder) = identity();
        assert_eq!(id.open(100102, "https://a", None), Ok(100042));
        assert_eq!(id.open(100102, "https://b", None), Ok(100042));
        assert_eq!(launcher(&id).launch(5, "firefox", None), Ok(100042));
        assert_eq!(recorder.0.lock().unwrap().len(), 3);
        // And its shim is told so: nothing is delivered to it.
        let (ours, theirs) = UnixStream::pair().unwrap();
        id.receive(100042, theirs).unwrap();
        match rpc::read_msg::<Response>(&ours).unwrap() {
            Response::Error(err) => assert!(err.contains("not single"), "{err}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_single_app_that_never_receives_is_started_again_after_a_while() {
        let (id, recorder) = identity();
        assert!(id.open(100042, "rho://a", None).is_ok());
        assert!(id.open(100042, "rho://b", None).is_ok());
        assert_eq!(recorder.0.lock().unwrap().len(), 1);
        if let Some(Instance::Starting { since, .. }) =
            id.instances.lock().unwrap().get_mut(&100102)
        {
            *since -= STARTUP + Duration::from_secs(1);
        }
        assert!(id.open(100042, "rho://c", None).is_ok());
        assert_eq!(recorder.0.lock().unwrap().len(), 2);
        assert_eq!(
            recorder.0.lock().unwrap()[1].argv,
            vec!["rho-gui", "rho://c"]
        );
    }

    #[test]
    fn a_running_instance_keeps_its_receiver() {
        let (id, _) = identity();
        assert!(launcher(&id).launch(5, "rho", None).is_ok());
        let (_end, rx) = shim(&id, 100102);
        let (ours, theirs) = UnixStream::pair().unwrap();
        id.receive(100102, theirs).unwrap();
        match rpc::read_msg::<Response>(&ours).unwrap() {
            Response::Error(err) => assert!(err.contains("already"), "{err}"),
            other => panic!("{other:?}"),
        }
        assert!(id.open(100042, "rho://a", None).is_ok());
        assert_eq!(got(&rx), open("rho://a", None));
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
