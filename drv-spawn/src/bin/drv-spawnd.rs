//! Root supervisor. Binds the public identity socket, makes a socketpair, forks the identity
//! daemon (this same binary, `identityd` subcommand) as its own system user with the listener
//! on fd 3 and the channel on fd 4, then answers spawn requests on its end of the channel
//! until the daemon dies, and forks a new one. Nothing else can reach the channel: it never
//! touches the filesystem.
//!
//! Also forks and restarts `drv-authd` and the compositor, each with a wire on fd 3, and
//! links them: whenever either comes up, both get the ends of a fresh socketpair.

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::time::Duration;
use std::{fs, io};

use clap::{Parser, Subcommand};
use drv_identity::{load_config, Identity};
use drv_policy::spawn::{Channel, CHANNEL_FD};
use drv_spawn::{dup_high, group_id, start_service, user_groups, user_ids, Peer, Server, Service, Wiring};

const LISTENER_FD: i32 = 3;

#[derive(Parser)]
#[command(
    name = "drv-spawnd",
    about = "Fork the identity daemon and start apps for it"
)]
struct Args {
    /// System user the identity daemon runs as.
    #[arg(long, default_value = "drv-identity")]
    identity_user: String,
    /// The identity daemon's manifest.
    #[arg(long, default_value = "/etc/drv/identity.toml")]
    identity_config: PathBuf,
    /// Public identity socket, world-connectable: launch is not a privilege, lookups of other
    /// UIDs are gated inside the daemon.
    #[arg(long, default_value = "/run/drv/identity.sock")]
    socket: PathBuf,
    /// `start:count`: the UIDs apps may run as. Required for the supervisor.
    #[arg(long)]
    range: Option<String>,
    /// A supplementary group an app may be given. Repeatable.
    #[arg(long = "group")]
    groups: Vec<String>,
    /// Per-UID `XDG_RUNTIME_DIR` parent.
    #[arg(long, default_value = "/run/drv-apps")]
    runtime_base: PathBuf,
    /// Per-UID `HOME` parent.
    #[arg(long, default_value = "/var/lib/drv-apps")]
    home_base: PathBuf,
    /// An entry of `/run` apps may see (a directory to bind mount or a symlink to recreate).
    /// Repeatable. Apps get a fresh `/run` with only these plus their own runtime directory.
    #[arg(long = "expose")]
    expose: Vec<PathBuf>,
    /// An entry of `/run` an app may ask for in its manifest. Repeatable.
    #[arg(long = "expose-optional")]
    optional_expose: Vec<PathBuf>,
    /// System user the auth daemon runs as.
    #[arg(long)]
    authd_user: Option<String>,
    /// The auth daemon's command line, whitespace-separated. Without it no app gets auth.
    #[arg(long)]
    authd_exec: Option<String>,
    /// `PATH:MODE` (octal): a directory the auth daemon owns, created before it starts.
    #[arg(long = "authd-dir")]
    authd_dirs: Vec<String>,
    /// System user the compositor runs as.
    #[arg(long)]
    compositor_user: Option<String>,
    /// The compositor's command line, whitespace-separated.
    #[arg(long)]
    compositor_exec: Option<String>,
    /// `NAME=VALUE` in the compositor's environment. Repeatable; it gets nothing else.
    #[arg(long = "compositor-env")]
    compositor_env: Vec<String>,
    /// `PATH:MODE` (octal): a directory the compositor owns, created before it starts.
    #[arg(long = "compositor-dir")]
    compositor_dirs: Vec<String>,
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Internal: the identity daemon, run by the supervisor with fds 3 and 4 set up.
    Identityd {
        #[arg(long)]
        config: PathBuf,
    },
}

fn main() -> ExitCode {
    let args = Args::parse();
    let result = match args.cmd {
        Some(Cmd::Identityd { config }) => identityd(config),
        None => supervise(args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("drv-spawnd: {err}");
            ExitCode::FAILURE
        }
    }
}

fn parse_range(s: &str) -> Result<(u32, u32), String> {
    let (start, count) = s
        .split_once(':')
        .ok_or_else(|| format!("--range {s:?}: expected start:count"))?;
    let num = |x: &str| x.parse::<u32>().map_err(|e| format!("--range {x:?}: {e}"));
    let (start, count) = (num(start)?, num(count)?);
    if start == 0 {
        return Err("--range must not include root".to_owned());
    }
    Ok((start, count))
}

fn supervise(args: Args) -> Result<(), String> {
    let range = args.range.as_deref().ok_or("--range is required")?;
    let (start, count) = parse_range(range)?;
    let (identity_uid, identity_gid) = user_ids(&args.identity_user)?;
    if identity_uid == 0 {
        return Err("the identity daemon must not be root".to_owned());
    }
    if start <= identity_uid && (identity_uid as u64) < start as u64 + count as u64 {
        return Err("the identity user is inside the app range".to_owned());
    }
    let groups = args
        .groups
        .iter()
        .map(|name| Ok((name.clone(), group_id(name)?)))
        .collect::<Result<Vec<_>, String>>()?;
    let server = Arc::new(Server {
        start,
        count,
        groups,
        runtime_base: args.runtime_base,
        home_base: args.home_base,
        expose: args.expose,
        optional_expose: args.optional_expose,
        wiring: Wiring::default(),
    });
    let authd = service(
        "drv-authd",
        args.authd_user.as_deref(),
        args.authd_exec.as_deref(),
        &[],
        &args.authd_dirs,
        start,
        count,
    )?;
    let compositor = service(
        "compositor",
        args.compositor_user.as_deref(),
        args.compositor_exec.as_deref(),
        &args.compositor_env,
        &args.compositor_dirs,
        start,
        count,
    )?;

    if let Some(parent) = args.socket.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let _ = fs::remove_file(&args.socket);
    let listener = UnixListener::bind(&args.socket)
        .map_err(|e| format!("listen on {}: {e}", args.socket.display()))?;
    fs::set_permissions(&args.socket, fs::Permissions::from_mode(0o666))
        .map_err(|e| format!("chmod {}: {e}", args.socket.display()))?;
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;

    // The services first: the compositor waits for the identity socket, which is bound.
    for (peer, service) in [(Peer::Authd, authd), (Peer::Compositor, compositor)] {
        let Some(service) = service else { continue };
        let server = server.clone();
        std::thread::spawn(move || supervise_service(peer, service, &server.wiring));
    }

    loop {
        let (ours, theirs) = UnixStream::pair().map_err(|e| format!("socketpair: {e}"))?;
        let mut child = {
            // Copies above the target numbers, so the dup2s below cannot clobber each other
            // and are never a same-fd no-op (which would keep close-on-exec set).
            let listener_fd = dup_high(listener.as_raw_fd())?;
            let channel_fd = dup_high(theirs.as_raw_fd())?;
            let mut command = Command::new(&exe);
            command
                .arg("identityd")
                .arg("--config")
                .arg(&args.identity_config);
            // SAFETY: only dup2/setgroups/setresgid/setresuid/prctl between fork and exec.
            unsafe {
                command.pre_exec(move || {
                    // dup2 clears close-on-exec on the target fds; everything else we hold
                    // stays close-on-exec and never reaches the daemon.
                    if libc::dup2(listener_fd, LISTENER_FD) < 0
                        || libc::dup2(channel_fd, CHANNEL_FD) < 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::setgroups(0, std::ptr::null()) != 0
                        || libc::setresgid(identity_gid, identity_gid, identity_gid) != 0
                        || libc::setresuid(identity_uid, identity_uid, identity_uid) != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let child = command
                .spawn()
                .map_err(|e| format!("spawn identity daemon: {e}"))?;
            // SAFETY: our own duplicates, used only by the child.
            unsafe {
                libc::close(listener_fd);
                libc::close(channel_fd);
            }
            child
        };
        drop(theirs);
        eprintln!(
            "drv-spawnd: identity daemon running as uid {identity_uid}, pid {}",
            child.id()
        );
        if let Err(err) = server.serve(ours) {
            eprintln!("drv-spawnd: channel: {err}");
        }
        match child.wait() {
            Ok(status) => eprintln!("drv-spawnd: identity daemon exited: {status}; restarting"),
            Err(err) => eprintln!("drv-spawnd: waiting for the identity daemon: {err}"),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// A service from the command line: its user must exist, be non-root and be outside the app
/// range. `None` when it is not configured at all.
fn service(
    name: &str,
    user: Option<&str>,
    exec: Option<&str>,
    env: &[String],
    dirs: &[String],
    start: u32,
    count: u32,
) -> Result<Option<Service>, String> {
    let (Some(user), Some(exec)) = (user, exec) else {
        if user.is_some() || exec.is_some() {
            return Err(format!("{name}: both --*-user and --*-exec are needed"));
        }
        return Ok(None);
    };
    let (uid, gid) = user_ids(user)?;
    if uid == 0 {
        return Err(format!("{name} must not be root"));
    }
    if start <= uid && (uid as u64) < start as u64 + count as u64 {
        return Err(format!("{name}'s user {user} is inside the app range"));
    }
    let env = env
        .iter()
        .map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .ok_or_else(|| format!("{name}: env {kv:?} is not NAME=VALUE"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let dirs = dirs
        .iter()
        .map(|d| {
            let (path, mode) = d
                .split_once(':')
                .ok_or_else(|| format!("{name}: dir {d:?} is not PATH:MODE"))?;
            let mode = u32::from_str_radix(mode, 8).map_err(|e| format!("{name}: mode {mode:?}: {e}"))?;
            Ok((PathBuf::from(path), mode))
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Some(Service {
        name: name.to_owned(),
        uid,
        gid,
        groups: user_groups(user, gid)?,
        argv: exec.split_whitespace().map(String::from).collect(),
        env,
        dirs,
    }))
}

/// Keeps one service running; its wire goes to the wiring on every start.
fn supervise_service(peer: Peer, service: Service, wiring: &Wiring) {
    loop {
        match start_service(&service) {
            Ok((mut child, wire)) => {
                eprintln!(
                    "drv-spawnd: {} running as uid {}, pid {}",
                    service.name,
                    service.uid,
                    child.id()
                );
                wiring.attach_service(peer, wire);
                match child.wait() {
                    Ok(status) => eprintln!("drv-spawnd: {} exited: {status}", service.name),
                    Err(err) => eprintln!("drv-spawnd: waiting for {}: {err}", service.name),
                }
                wiring.detach_service(peer);
            }
            Err(err) => eprintln!("drv-spawnd: {err}"),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The unprivileged half. Apps start from our `PATH` and the manifest, nothing else.
fn identityd(config: PathBuf) -> Result<(), String> {
    let config = load_config(&config).map_err(|e| e.to_string())?;
    // SAFETY: the supervisor put the listener on fd 3 and the channel on fd 4 and nothing
    // else owns them.
    let listener = UnixListener::from(unsafe { OwnedFd::from_raw_fd(LISTENER_FD) });
    let channel = UnixStream::from(unsafe { OwnedFd::from_raw_fd(CHANNEL_FD) });
    let base_env: Vec<(String, String)> = std::env::var("PATH")
        .map(|path| vec![("PATH".to_owned(), path)])
        .unwrap_or_default();
    let identity = Arc::new(Identity::new(
        config,
        Arc::new(Channel::new(channel)),
        base_env,
    ));
    let autostart = identity.clone();
    std::thread::spawn(move || autostart.autostart(Duration::from_secs(60)));
    drv_policy::daemon::serve(listener, identity).map_err(|e| format!("serving: {e}"))
}
