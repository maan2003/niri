//! The supervisor: forks and restarts the trusted set, each as its own user with the fds it
//! needs already in place, and links them (see the crate docs). Two groups restart as a
//! whole: drv-appd with drv-forker (they share a channel nothing else can reach), and the
//! compositor with its GPU process (the sealed GPU process cannot be given a new core).

use std::os::fd::AsFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Child, ExitCode};
use std::sync::{mpsc, Arc};
use std::time::Duration;
use std::{fs, thread};

use clap::Parser;
use drv_os::{user_groups, user_ids};
use drv_policy::{seq, wire};
use drv_supervisor::{start_service, start_wired, Peer, Service, Supervisor};

/// Where drv-appd finds what the supervisor put in place (its wire is fd 3).
const APPD_LISTENER_FD: i32 = 4;
const APPD_NOTICE_FD: i32 = 5;
const APPD_CHANNEL_FD: i32 = 6;

#[derive(Parser)]
#[command(name = "drv-supervisor", about = "Start and wire the trusted set")]
struct Args {
    /// drv-appd's public socket, world-connectable: lookups of other UIDs are gated inside.
    #[arg(long, default_value = "/run/drv/appd.sock")]
    socket: PathBuf,
    /// System user drv-appd runs as.
    #[arg(long, default_value = "drv-appd")]
    appd_user: String,
    /// drv-appd's command line, whitespace-separated.
    #[arg(long)]
    appd_exec: String,
    /// drv-forker's command line, whitespace-separated. It runs as root.
    #[arg(long)]
    forker_exec: String,
    /// System user the auth daemon runs as.
    #[arg(long)]
    authd_user: Option<String>,
    /// The auth daemon's command line, whitespace-separated. Without it no app gets auth.
    #[arg(long)]
    authd_exec: Option<String>,
    /// `PATH:MODE` (octal): a directory the auth daemon owns, created before it starts.
    #[arg(long = "authd-dir")]
    authd_dirs: Vec<String>,
    /// The seat daemon's command line, whitespace-separated. It runs as root.
    #[arg(long)]
    seatd_exec: Option<String>,
    /// `NAME=VALUE` in the seat daemon's environment. Repeatable.
    #[arg(long = "seatd-env")]
    seatd_env: Vec<String>,
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
    /// System user the GPU process runs as (`render` group for Mesa's render nodes).
    #[arg(long)]
    gpu_user: Option<String>,
    /// The GPU process's command line, whitespace-separated (`niri gpu-process --mode drm`).
    /// It is the compositor's group: either dying restarts both.
    #[arg(long)]
    gpu_exec: Option<String>,
    /// `NAME=VALUE` in the GPU process's environment. Repeatable; it gets nothing else.
    #[arg(long = "gpu-env")]
    gpu_env: Vec<String>,
}

fn main() -> ExitCode {
    match supervise(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("drv-supervisor: {err}");
            ExitCode::FAILURE
        }
    }
}

/// `PATH` and backtraces: what our own children get on top of what is listed for them.
fn base_env() -> Vec<(String, String)> {
    let mut env = Vec::new();
    if let Ok(path) = std::env::var("PATH") {
        env.push(("PATH".to_owned(), path));
    }
    env.push(("RUST_BACKTRACE".to_owned(), "1".to_owned()));
    env
}

fn supervise(args: Args) -> Result<(), String> {
    let authd = service(
        "drv-authd",
        args.authd_user.as_deref(),
        args.authd_exec.as_deref(),
        &[],
        &args.authd_dirs,
    )?;
    let seatd = service(
        "drv-seatd",
        args.seatd_exec.as_deref().map(|_| "root"),
        args.seatd_exec.as_deref(),
        &args.seatd_env,
        &[],
    )?;
    let compositor = service(
        "compositor",
        args.compositor_user.as_deref(),
        args.compositor_exec.as_deref(),
        &args.compositor_env,
        &args.compositor_dirs,
    )?;
    let gpu = service(
        "compositor-gpu",
        args.gpu_user.as_deref(),
        args.gpu_exec.as_deref(),
        &args.gpu_env,
        &[],
    )?;
    let forker = service("drv-forker", Some("root"), Some(&args.forker_exec), &[], &[])?
        .ok_or("--forker-exec is required")?;
    let appd = service(
        "drv-appd",
        Some(&args.appd_user),
        Some(&args.appd_exec),
        &[],
        &[],
    )?
    .ok_or("--appd-exec is required")?;

    if let Some(parent) = args.socket.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let _ = fs::remove_file(&args.socket);
    let listener = UnixListener::bind(&args.socket)
        .map_err(|e| format!("listen on {}: {e}", args.socket.display()))?;
    fs::set_permissions(&args.socket, fs::Permissions::from_mode(0o666))
        .map_err(|e| format!("chmod {}: {e}", args.socket.display()))?;

    let supervisor = Arc::new(Supervisor::default());
    // The services first: the compositor waits for the public socket, which is bound.
    for (peer, service) in [(Peer::Seatd, seatd), (Peer::Authd, authd)] {
        let Some(service) = service else { continue };
        let supervisor = supervisor.clone();
        thread::spawn(move || supervise_service(peer, service, &supervisor));
    }
    match (compositor, gpu) {
        (Some(compositor), Some(gpu)) => {
            let supervisor = supervisor.clone();
            thread::spawn(move || supervise_compositor(&supervisor, &compositor, &gpu));
        }
        (Some(compositor), None) => {
            let supervisor = supervisor.clone();
            thread::spawn(move || supervise_service(Peer::Compositor, compositor, &supervisor));
        }
        (None, Some(_)) => return Err("--gpu-exec without a compositor".to_owned()),
        (None, None) => {}
    }
    supervise_appd(&supervisor, &appd, &forker, &listener);
}

/// A service from the command line: its user must exist, and be root only for the pieces that
/// need it. `None` when it is not configured at all.
fn service(
    name: &str,
    user: Option<&str>,
    exec: Option<&str>,
    env: &[String],
    dirs: &[String],
) -> Result<Option<Service>, String> {
    let (Some(user), Some(exec)) = (user, exec) else {
        if user.is_some() || exec.is_some() {
            return Err(format!("{name}: both --*-user and --*-exec are needed"));
        }
        return Ok(None);
    };
    let (uid, gid) = user_ids(user)?;
    if uid == 0 && !matches!(name, "drv-seatd" | "drv-forker") {
        return Err(format!("{name} must not be root"));
    }
    let mut env = env
        .iter()
        .map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .ok_or_else(|| format!("{name}: env {kv:?} is not NAME=VALUE"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if matches!(name, "drv-appd" | "drv-forker") {
        env.extend(base_env());
    }
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

/// Keeps one service running; its wire goes to the wiring on every start, and a compositor
/// start is announced to drv-appd (autostart).
fn supervise_service(peer: Peer, service: Service, supervisor: &Supervisor) {
    loop {
        match start_wired(&service) {
            Ok((mut child, wire)) => {
                eprintln!(
                    "drv-supervisor: {} running as uid {}, pid {}",
                    service.name,
                    service.uid,
                    child.id()
                );
                supervisor.wiring.attach_service(peer, wire);
                if peer == Peer::Compositor {
                    supervisor.compositor_started();
                }
                match child.wait() {
                    Ok(status) => eprintln!("drv-supervisor: {} exited: {status}", service.name),
                    Err(err) => eprintln!("drv-supervisor: waiting for {}: {err}", service.name),
                }
                if peer == Peer::Compositor {
                    supervisor.compositor_stopped();
                }
                supervisor.wiring.detach_service(peer);
            }
            Err(err) => eprintln!("drv-supervisor: {err}"),
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// drv-appd and drv-forker as one group: a fresh channel between them on every start, the
/// public listener, a notice socket and a wire for drv-appd. When either dies the other is
/// stopped and both come back.
fn supervise_appd(supervisor: &Supervisor, appd: &Service, forker: &Service, listener: &UnixListener) -> ! {
    loop {
        match start_appd_group(appd, forker, listener) {
            Ok((children, wire, notices)) => {
                supervisor.wiring.attach_service(Peer::Appd, wire);
                supervisor.appd_started(notices);
                wait_group(children);
                supervisor.appd_stopped();
                supervisor.wiring.detach_service(Peer::Appd);
            }
            Err(err) => eprintln!("drv-supervisor: {err}"),
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// The compositor and its GPU process as one group. The GPU process is started first and
/// gets the core's connection down its wire; it waits there for the core's `Start`. When
/// either dies the other is stopped and both come back: a sealed GPU process cannot bring up
/// a renderer for a new core, and a core cannot outlive its renderer.
fn supervise_compositor(supervisor: &Supervisor, compositor: &Service, gpu: &Service) {
    loop {
        match start_compositor_group(compositor, gpu) {
            Ok((children, wire_compositor, wire_gpu)) => {
                supervisor.wiring.attach_service(Peer::Gpu, wire_gpu);
                supervisor.wiring.attach_service(Peer::Compositor, wire_compositor);
                supervisor.compositor_started();
                wait_group(children);
                supervisor.compositor_stopped();
                supervisor.wiring.detach_service(Peer::Compositor);
                supervisor.wiring.detach_service(Peer::Gpu);
            }
            Err(err) => eprintln!("drv-supervisor: {err}"),
        }
        thread::sleep(Duration::from_secs(1));
    }
}

fn start_compositor_group(
    compositor: &Service,
    gpu: &Service,
) -> Result<(Group, std::os::fd::OwnedFd, std::os::fd::OwnedFd), String> {
    let (gpu_child, wire_gpu) = start_wired(gpu)?;
    eprintln!(
        "drv-supervisor: {} running as uid {}, pid {}",
        gpu.name,
        gpu.uid,
        gpu_child.id()
    );
    let (compositor_child, wire_compositor) = match start_wired(compositor) {
        Ok(started) => started,
        Err(err) => {
            stop(gpu_child);
            return Err(err);
        }
    };
    eprintln!(
        "drv-supervisor: {} running as uid {}, pid {}",
        compositor.name,
        compositor.uid,
        compositor_child.id()
    );
    Ok((
        vec![("compositor-gpu", gpu_child), ("compositor", compositor_child)],
        wire_compositor,
        wire_gpu,
    ))
}

/// Terminates and reaps a child whose group could not be completed.
fn stop(mut child: Child) {
    // SAFETY: our child's pid, not yet reaped.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let _ = child.wait();
}

type Group = Vec<(&'static str, Child)>;

fn start_appd_group(
    appd: &Service,
    forker: &Service,
    listener: &UnixListener,
) -> Result<(Group, std::os::fd::OwnedFd, std::os::fd::OwnedFd), String> {
    let (channel_appd, channel_forker) = seq::pair().map_err(|e| format!("socketpair: {e}"))?;
    let (notices_ours, notices_appd) = seq::pair().map_err(|e| format!("socketpair: {e}"))?;
    let (wire_appd, wire_ours) = wire::pair().map_err(|e| format!("socketpair: {e}"))?;
    let forker_child = start_service(forker, &[(wire::WIRE_FD, channel_forker.as_fd())])?;
    eprintln!("drv-supervisor: drv-forker running as root, pid {}", forker_child.id());
    let appd_child = match start_service(
        appd,
        &[
            (wire::WIRE_FD, wire_appd.as_fd()),
            (APPD_LISTENER_FD, listener.as_fd()),
            (APPD_NOTICE_FD, notices_appd.as_fd()),
            (APPD_CHANNEL_FD, channel_appd.as_fd()),
        ],
    ) {
        Ok(child) => child,
        Err(err) => {
            stop(forker_child);
            return Err(err);
        }
    };
    eprintln!(
        "drv-supervisor: drv-appd running as uid {}, pid {}",
        appd.uid,
        appd_child.id()
    );
    Ok((
        vec![("drv-forker", forker_child), ("drv-appd", appd_child)],
        wire_ours,
        notices_ours,
    ))
}

/// Waits for the first of the group to exit, stops the rest, and reaps them all.
fn wait_group(children: Group) {
    let (tx, rx) = mpsc::channel();
    let mut pids = Vec::new();
    for (name, mut child) in children {
        pids.push((name, child.id()));
        let tx = tx.clone();
        thread::spawn(move || {
            let result = child.wait();
            let _ = tx.send((name, result));
        });
    }
    drop(tx);
    let Ok((first, result)) = rx.recv() else { return };
    match result {
        Ok(status) => eprintln!("drv-supervisor: {first} exited: {status}; restarting the group"),
        Err(err) => eprintln!("drv-supervisor: waiting for {first}: {err}; restarting the group"),
    }
    for (name, pid) in pids {
        if name != first {
            // SAFETY: our child's pid; its waiter thread has not returned, so not reaped.
            unsafe { libc::kill(pid as i32, libc::SIGTERM) };
        }
    }
    for (name, result) in rx {
        match result {
            Ok(status) => eprintln!("drv-supervisor: {name} exited: {status}"),
            Err(err) => eprintln!("drv-supervisor: waiting for {name}: {err}"),
        }
    }
}
