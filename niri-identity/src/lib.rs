//! The brain of app launching, unprivileged. Knows the apps (static manifests generated from the
//! system configuration: every app has a fixed UID), answers the compositor's policy lookups,
//! and turns `Launch { app }` into a forker request. Android's PackageManager plus
//! ActivityManager, in one small process per human user.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use niri_policy::daemon::Handler;
use niri_policy::{AppPolicy, Global};
use serde::{Deserialize, Serialize};

/// `identity.toml`.
///
/// ```toml
/// [env]
/// PIPEWIRE_RUNTIME_DIR = "/run/pipewire"
///
/// [[app]]
/// name = "session"      # the human; not launchable, just identified
/// uid = 1000
/// trusted = true
///
/// [[app]]
/// name = "firefox"
/// uid = 100042
/// exec = ["firefox"]    # defaults to [name]; the only arguments the app ever gets
/// gpu = true
/// network = true
/// groups = ["render"]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    /// Forker socket; the command line's `--forker` wins if both are given.
    #[serde(default)]
    pub forker: Option<PathBuf>,
    /// Environment every app gets, from the system configuration: where the system put the
    /// PipeWire socket, for example. On top of `PATH` and friends, under the compositor's
    /// `WAYLAND_DISPLAY`.
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
    /// Program and its arguments. Defaults to `[name]`. Nothing is appended at launch.
    #[serde(default)]
    pub exec: Option<Vec<String>>,
    /// Supplementary groups, e.g. `render` for the GPU. The forker checks them against its
    /// allow list.
    #[serde(default)]
    pub groups: Vec<String>,
    #[serde(default)]
    pub trusted: bool,
    #[serde(default)]
    pub gpu: bool,
    /// Keep the host network; off means an empty network namespace.
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub globals: Vec<Global>,
    #[serde(default)]
    pub icon: Option<String>,
}

impl AppConfig {
    fn policy(&self) -> AppPolicy {
        AppPolicy {
            name: self.name.clone(),
            trusted: self.trusted,
            gpu: self.gpu,
            globals: self.globals.clone(),
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
    let mut names = HashSet::new();
    let mut uids = HashSet::new();
    for app in &config.apps {
        if !names.insert(&app.name) {
            return Err(Error::Config(format!("app {:?} listed twice", app.name)));
        }
        // A UID is one identity. The exception is the human's own tools (launcher, bar):
        // they may share the human's UID, but then every entry on it must be trusted, so a
        // lookup can never confuse an untrusted app with a trusted one.
        if !uids.insert(app.uid) {
            let all_trusted = config
                .apps
                .iter()
                .filter(|a| a.uid == app.uid)
                .all(|a| a.trusted);
            if !all_trusted {
                return Err(Error::Config(format!(
                    "uid {} used by more than one app; only trusted apps may share a UID",
                    app.uid
                )));
            }
        }
        if app.exec.as_ref().is_some_and(|e| e.is_empty()) {
            return Err(Error::Config(format!(
                "app {:?} has an empty exec",
                app.name
            )));
        }
    }
    Ok(config)
}

pub struct Identity {
    config: Config,
    forker: PathBuf,
    /// Environment every launched app gets before the compositor's additions: `PATH` and
    /// friends from our own environment.
    base_env: Vec<(String, String)>,
}

impl Identity {
    pub fn new(config: Config, forker: PathBuf, base_env: Vec<(String, String)>) -> Self {
        Self {
            config,
            forker,
            base_env,
        }
    }

    fn app(&self, name: &str) -> Option<&AppConfig> {
        self.config.apps.iter().find(|a| a.name == name)
    }

    /// The child's environment: ours (`PATH`..), the config's `[env]`, then the compositor's.
    /// The compositor sends its own session (`XDG_RUNTIME_DIR`, a relative `WAYLAND_DISPLAY`)
    /// plus the apps socket path. The human's tools keep the session and get our `HOME`;
    /// every other app gets the apps socket as `WAYLAND_DISPLAY` (the forker sets its
    /// `HOME` and `XDG_RUNTIME_DIR`).
    fn env_for(&self, app: &AppConfig, env: &[(String, String)], me: u32) -> Vec<(String, String)> {
        let mut full_env = self.base_env.clone();
        full_env.extend(self.config.env.iter().map(|(k, v)| (k.clone(), v.clone())));
        full_env.extend(env.iter().cloned());
        let apps_display = full_env
            .iter()
            .find(|(k, _)| k == niri_policy::env::APPS_WAYLAND_DISPLAY)
            .map(|(_, v)| v.clone());
        full_env.retain(|(k, _)| k != niri_policy::env::APPS_WAYLAND_DISPLAY);
        if app.uid == me {
            if let Ok(home) = std::env::var("HOME") {
                full_env.push(("HOME".to_owned(), home));
            }
        } else {
            full_env.retain(|(k, _)| k != "XDG_RUNTIME_DIR");
            if let Some(display) = apps_display {
                full_env.retain(|(k, _)| k != "WAYLAND_DISPLAY");
                full_env.push(("WAYLAND_DISPLAY".to_owned(), display));
            }
        }
        full_env
    }
}

impl Handler for Identity {
    fn lookup(&self, uid: u32) -> AppPolicy {
        self.config
            .apps
            .iter()
            .find(|a| a.uid == uid)
            .map(AppConfig::policy)
            .unwrap_or_else(AppPolicy::unknown)
    }

    fn launch(&self, name: &str, env: &[(String, String)]) -> Result<u32, String> {
        let app = self
            .app(name)
            .ok_or_else(|| format!("unknown app {name:?}; add it to identity.toml"))?;
        let argv = app.exec.clone().unwrap_or_else(|| vec![app.name.clone()]);
        let full_env = self.env_for(app, env, rustix::process::getuid().as_raw());
        let request = niri_forker::Request {
            uid: app.uid,
            groups: app.groups.clone(),
            argv,
            env: full_env,
            network: app.network,
        };
        niri_forker::fork(&self.forker, &request).map_err(|e| format!("forker: {e}"))?;
        Ok(app.uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> Identity {
        let config: Config = toml::from_str(
            r#"
            [[app]]
            name = "session"
            uid = 1000
            trusted = true

            [[app]]
            name = "firefox"
            uid = 100042
            gpu = true
            groups = ["render"]
            "#,
        )
        .unwrap();
        Identity::new(
            config,
            PathBuf::from("/nonexistent/forker.sock"),
            Vec::new(),
        )
    }

    #[test]
    fn trusted_apps_may_share_a_uid_untrusted_may_not() {
        let dir = std::env::temp_dir().join(format!("niri-identity-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("identity.toml");
        std::fs::write(
            &path,
            "[[app]]\nname = \"session\"\nuid = 1000\ntrusted = true\n[[app]]\nname = \"launcher\"\nuid = 1000\ntrusted = true\n",
        )
        .unwrap();
        assert!(load_config(&path).is_ok());
        std::fs::write(
            &path,
            "[[app]]\nname = \"session\"\nuid = 1000\ntrusted = true\n[[app]]\nname = \"sneaky\"\nuid = 1000\n",
        )
        .unwrap();
        let err = load_config(&path).unwrap_err().to_string();
        assert!(err.contains("only trusted"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_for_the_human_apps_socket_for_the_rest() {
        let id = identity();
        let sent = vec![
            ("XDG_RUNTIME_DIR".to_owned(), "/run/user/1000".to_owned()),
            ("WAYLAND_DISPLAY".to_owned(), "wayland-1".to_owned()),
            (
                niri_policy::env::APPS_WAYLAND_DISPLAY.to_owned(),
                "/run/niri-wayland/wayland".to_owned(),
            ),
        ];
        let get = |env: &[(String, String)], k: &str| {
            env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
        };
        let human = id.env_for(id.app("session").unwrap(), &sent, 1000);
        assert_eq!(get(&human, "WAYLAND_DISPLAY").as_deref(), Some("wayland-1"));
        assert_eq!(get(&human, "XDG_RUNTIME_DIR").as_deref(), Some("/run/user/1000"));
        assert!(get(&human, niri_policy::env::APPS_WAYLAND_DISPLAY).is_none());
        let app = id.env_for(id.app("firefox").unwrap(), &sent, 1000);
        assert_eq!(
            get(&app, "WAYLAND_DISPLAY").as_deref(),
            Some("/run/niri-wayland/wayland")
        );
        assert!(get(&app, "XDG_RUNTIME_DIR").is_none());
        assert!(get(&app, niri_policy::env::APPS_WAYLAND_DISPLAY).is_none());
    }

    #[test]
    fn looks_up_static_uids() {
        let id = identity();
        assert_eq!(id.lookup(100042).name, "firefox");
        assert!(id.lookup(100042).allows(Global::Dmabuf));
        assert!(id.lookup(1000).trusted);
        assert_eq!(id.lookup(5).name, "unknown");
    }

    #[test]
    fn rejects_duplicate_uids_and_names() {
        let dir = std::env::temp_dir().join(format!("niri-identity-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("identity.toml");
        std::fs::write(
            &path,
            "[[app]]\nname = \"a\"\nuid = 12\n[[app]]\nname = \"b\"\nuid = 12\n",
        )
        .unwrap();
        assert!(matches!(load_config(&path), Err(Error::Config(_))));
        std::fs::write(
            &path,
            "[[app]]\nname = \"a\"\nuid = 12\n[[app]]\nname = \"a\"\nuid = 13\n",
        )
        .unwrap();
        assert!(matches!(load_config(&path), Err(Error::Config(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_app_is_refused() {
        let err = identity().launch("nope", &[]).unwrap_err();
        assert!(err.contains("unknown app"));
    }
}
