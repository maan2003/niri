//! The brain of app launching, unprivileged. Owns the app registry (name to UID, stable across
//! runs), answers the compositor's policy lookups, and turns `Launch` into a forker request.
//! Android's PackageManager plus ActivityManager, in one small process per human user.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use niri_policy::daemon::Handler;
use niri_policy::{AppPolicy, Global};
use serde::{Deserialize, Serialize};

/// `identity.toml`.
///
/// ```toml
/// uid-start = 100000
/// uid-count = 65536
///
/// [[app]]
/// name = "session"      # the human; not launchable, just identified
/// uid = 1000
/// trusted = true
///
/// [[app]]
/// name = "firefox"
/// exec = ["firefox"]    # defaults to [name]
/// gpu = true
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Config {
    /// First UID handed out to apps without a pinned `uid`.
    pub uid_start: u32,
    pub uid_count: u32,
    /// Forker socket; the command line's `--forker` wins if both are given.
    #[serde(default)]
    pub forker: Option<PathBuf>,
    #[serde(default, rename = "app")]
    pub apps: Vec<AppConfig>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct AppConfig {
    pub name: String,
    /// Program and fixed arguments. Defaults to `[name]`.
    #[serde(default)]
    pub exec: Option<Vec<String>>,
    /// Fixed UID, for things that already exist as a user (the human's own session). Unpinned
    /// apps get one from the range on first launch and keep it.
    #[serde(default)]
    pub uid: Option<u32>,
    #[serde(default)]
    pub trusted: bool,
    #[serde(default)]
    pub gpu: bool,
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

/// Allocated UIDs, persisted so an app keeps its UID (and so its files) across runs.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    #[serde(default)]
    uids: BTreeMap<String, u32>,
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
    let mut names = std::collections::HashSet::new();
    for app in &config.apps {
        if !names.insert(&app.name) {
            return Err(Error::Config(format!("app {:?} listed twice", app.name)));
        }
        if let Some(uid) = app.uid {
            if config.uid_start <= uid
                && (uid as u64) < config.uid_start as u64 + config.uid_count as u64
            {
                return Err(Error::Config(format!(
                    "app {:?} pins uid {uid} inside the allocation range",
                    app.name
                )));
            }
        }
    }
    Ok(config)
}

pub struct Identity {
    config: Config,
    registry_path: PathBuf,
    registry: Mutex<RegistryFile>,
    forker: PathBuf,
    /// Environment every launched app gets before the compositor's additions: `PATH` and
    /// friends from our own environment.
    base_env: Vec<(String, String)>,
}

impl Identity {
    pub fn new(
        config: Config,
        registry_path: PathBuf,
        forker: PathBuf,
        base_env: Vec<(String, String)>,
    ) -> Result<Self, Error> {
        let registry = match std::fs::read_to_string(&registry_path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| Error::Parse(registry_path.clone(), e))?
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => RegistryFile::default(),
            Err(err) => return Err(Error::Io(registry_path, err)),
        };
        Ok(Self {
            config,
            registry_path,
            registry: Mutex::new(registry),
            forker,
            base_env,
        })
    }

    fn app(&self, name: &str) -> Option<&AppConfig> {
        self.config.apps.iter().find(|a| a.name == name)
    }

    /// The app's UID, allocating and persisting one on first use.
    pub fn uid_of(&self, app: &AppConfig) -> Result<u32, String> {
        if let Some(uid) = app.uid {
            return Ok(uid);
        }
        let mut registry = self.registry.lock().unwrap();
        if let Some(uid) = registry.uids.get(&app.name) {
            return Ok(*uid);
        }
        let end = self.config.uid_start as u64 + self.config.uid_count as u64;
        let taken: std::collections::HashSet<u32> = registry.uids.values().copied().collect();
        let uid = (self.config.uid_start as u64..end)
            .map(|u| u as u32)
            .find(|u| !taken.contains(u))
            .ok_or_else(|| "uid range exhausted".to_owned())?;
        registry.uids.insert(app.name.clone(), uid);
        let text = toml::to_string(&*registry).map_err(|e| e.to_string())?;
        if let Some(parent) = self.registry_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        }
        let tmp = self.registry_path.with_extension("tmp");
        std::fs::write(&tmp, text).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &self.registry_path)
            .map_err(|e| format!("{}: {e}", self.registry_path.display()))?;
        Ok(uid)
    }

    fn app_for_uid(&self, uid: u32) -> Option<&AppConfig> {
        if let Some(app) = self.config.apps.iter().find(|a| a.uid == Some(uid)) {
            return Some(app);
        }
        let registry = self.registry.lock().unwrap();
        let name = registry.uids.iter().find(|(_, u)| **u == uid)?.0.clone();
        drop(registry);
        self.app(&name)
    }
}

impl Handler for Identity {
    fn lookup(&self, uid: u32) -> AppPolicy {
        self.app_for_uid(uid)
            .map(AppConfig::policy)
            .unwrap_or_else(AppPolicy::unknown)
    }

    fn launch(&self, command: &[String], env: &[(String, String)]) -> Result<u32, String> {
        let name = command.first().ok_or_else(|| "empty command".to_owned())?;
        let app = self
            .app(name)
            .ok_or_else(|| format!("unknown app {name:?}; add it to identity.toml"))?;
        let uid = self.uid_of(app)?;
        let mut argv = app.exec.clone().unwrap_or_else(|| vec![app.name.clone()]);
        argv.extend(command[1..].iter().cloned());
        let mut full_env = self.base_env.clone();
        full_env.extend(env.iter().cloned());
        let request = niri_forker::Request {
            uid,
            argv,
            env: full_env,
        };
        niri_forker::fork(&self.forker, &request).map_err(|e| format!("forker: {e}"))?;
        Ok(uid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(dir: &Path) -> Identity {
        let config: Config = toml::from_str(
            r#"
            uid-start = 100000
            uid-count = 3

            [[app]]
            name = "session"
            uid = 1000
            trusted = true

            [[app]]
            name = "firefox"
            gpu = true
            "#,
        )
        .unwrap();
        Identity::new(
            config,
            dir.join("uids.toml"),
            dir.join("none.sock"),
            Vec::new(),
        )
        .unwrap()
    }

    #[test]
    fn allocates_stable_uids() {
        let dir = std::env::temp_dir().join(format!("niri-identity-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let id = identity(&dir);
        let firefox = id.app("firefox").unwrap();
        let uid = id.uid_of(firefox).unwrap();
        assert_eq!(uid, 100000);
        assert_eq!(id.uid_of(firefox).unwrap(), uid);
        assert_eq!(id.lookup(uid).name, "firefox");
        assert!(id.lookup(uid).allows(Global::Dmabuf));
        assert!(id.lookup(1000).trusted);
        assert_eq!(id.lookup(5).name, "unknown");

        // A fresh daemon reads the same UID back from the registry file.
        let again = identity(&dir);
        assert_eq!(again.lookup(uid).name, "firefox");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_pins_inside_the_range() {
        let text = "uid-start = 10\nuid-count = 5\n[[app]]\nname = \"x\"\nuid = 12\n";
        let dir = std::env::temp_dir().join(format!("niri-identity-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("identity.toml");
        std::fs::write(&path, text).unwrap();
        assert!(matches!(load_config(&path), Err(Error::Config(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_app_is_refused() {
        let dir = std::env::temp_dir().join(format!("niri-identity-launch-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = identity(&dir);
        let err = id.launch(&["nope".to_owned()], &[]).unwrap_err();
        assert!(err.contains("unknown app"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
