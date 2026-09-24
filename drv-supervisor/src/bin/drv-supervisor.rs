//! The supervisor: forks the trusted set, each as its own user with its peers already in
//! hand, and restarts the whole set when any of it dies (see the crate docs). Every
//! connection between two members is a socketpair made here, before either exists.

use std::ffi::CString;
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ExitCode};
use std::time::{Duration, Instant};
use std::{fs, io, thread};

use clap::Parser;
use drv_os::fds::{seqpacket_pair, stream_pair};
use drv_os::{user_groups, user_ids};
use drv_supervisor::{capability, start_service, Cgroups, Service};
use rustix::thread::CapabilitySet;

#[derive(Parser)]
#[command(name = "drv-supervisor", about = "Start and wire the trusted set")]
struct Args {
    /// drv-appd's public socket, world-connectable: lookups only, gated inside.
    #[arg(long, default_value = "/run/drv/appd.sock")]
    socket: PathBuf,
    /// System user drv-appd runs as.
    #[arg(long, default_value = "drv-appd")]
    appd_user: String,
    /// drv-appd's command line, whitespace-separated.
    #[arg(long)]
    appd_exec: String,
    /// An entry under `/run` drv-appd sees (its `/run` holds nothing else). Repeatable.
    #[arg(long = "appd-expose")]
    appd_expose: Vec<PathBuf>,
    /// System user drv-forker runs as.
    #[arg(long)]
    forker_user: String,
    /// drv-forker's command line, whitespace-separated.
    #[arg(long)]
    forker_exec: String,
    /// An entry under `/run` drv-forker sees (its `/run` holds nothing else). Repeatable.
    #[arg(long = "forker-expose")]
    forker_expose: Vec<PathBuf>,
    /// A capability drv-forker keeps, by name (`setuid`, `setgid`, `setpcap`, `sys_admin`,
    /// `chown` for the UID switch, the sandbox and the apps' directories). Repeatable.
    #[arg(long = "forker-cap")]
    forker_caps: Vec<String>,
    /// `PATH:MODE` (octal): a directory drv-forker owns (the apps' runtime and home parents),
    /// created before it starts.
    #[arg(long = "forker-dir")]
    forker_dirs: Vec<String>,
    /// System user the auth daemon runs as.
    #[arg(long)]
    authd_user: String,
    /// The auth daemon's command line, whitespace-separated.
    #[arg(long)]
    authd_exec: String,
    /// An entry under `/run` the auth daemon sees (its `/run` holds nothing else). Repeatable.
    #[arg(long = "authd-expose")]
    authd_expose: Vec<PathBuf>,
    /// `PATH:MODE` (octal): a directory the auth daemon owns, created before it starts.
    #[arg(long = "authd-dir")]
    authd_dirs: Vec<String>,
    /// System user the seat daemon runs as (groups video, input, tty for the devices it opens).
    #[arg(long)]
    seatd_user: String,
    /// The seat daemon's command line, whitespace-separated.
    #[arg(long)]
    seatd_exec: String,
    /// An entry under `/run` the seat daemon sees (its `/run` holds nothing else). Repeatable.
    #[arg(long = "seatd-expose")]
    seatd_expose: Vec<PathBuf>,
    /// A capability the seat daemon keeps, by name (`sys_tty_config` for the VT ioctls).
    /// Repeatable.
    #[arg(long = "seatd-cap")]
    seatd_caps: Vec<String>,
    /// `NAME=VALUE` in the seat daemon's environment. Repeatable.
    #[arg(long = "seatd-env")]
    seatd_env: Vec<String>,
    /// System user the compositor runs as.
    #[arg(long)]
    compositor_user: String,
    /// The compositor's command line, whitespace-separated.
    #[arg(long)]
    compositor_exec: String,
    /// An entry under `/run` the compositor sees (its `/run` holds nothing else). Repeatable.
    #[arg(long = "compositor-expose")]
    compositor_expose: Vec<PathBuf>,
    /// `NAME=VALUE` in the compositor's environment. Repeatable; it gets nothing else.
    #[arg(long = "compositor-env")]
    compositor_env: Vec<String>,
    /// `PATH:MODE` (octal): a directory the compositor owns, created before it starts.
    #[arg(long = "compositor-dir")]
    compositor_dirs: Vec<String>,
    /// System user the GPU process runs as (`render` group for Mesa's render nodes).
    #[arg(long)]
    gpu_user: String,
    /// The GPU process's command line, whitespace-separated (`niri gpu-process --mode drm`).
    #[arg(long)]
    gpu_exec: String,
    /// An entry under `/run` the GPU process sees (its `/run` holds nothing else). Repeatable.
    #[arg(long = "gpu-expose")]
    gpu_expose: Vec<PathBuf>,
    /// `NAME=VALUE` in the GPU process's environment. Repeatable; it gets nothing else.
    #[arg(long = "gpu-env")]
    gpu_env: Vec<String>,
    /// System user the shell (lock screen, menu, prompts, notifications) runs as.
    #[arg(long)]
    shell_user: String,
    /// The shell's command line, whitespace-separated (`drv-shell`).
    #[arg(long)]
    shell_exec: String,
    /// An entry under `/run` the shell sees (its `/run` holds nothing else). Repeatable.
    #[arg(long = "shell-expose")]
    shell_expose: Vec<PathBuf>,
    /// `NAME=VALUE` in the shell's environment. Repeatable; it gets nothing else.
    #[arg(long = "shell-env")]
    shell_env: Vec<String>,
    /// The shell's notification socket, world-connectable: every connection is keyed on the
    /// peer UID.
    #[arg(long, default_value = "/run/drv/notify.sock")]
    notify_socket: PathBuf,
    /// System user the ssh agent runs as.
    #[arg(long)]
    agent_user: String,
    /// The agent's command line, whitespace-separated (`drv-agent serve ...`).
    #[arg(long)]
    agent_exec: String,
    /// The agent's door, world-connectable: the agent asks drv-appd about every peer UID.
    #[arg(long, default_value = "/run/drv/agent")]
    agent_socket: PathBuf,
    /// An entry under `/run` the agent sees (udev's database, for the authenticators). Repeatable.
    #[arg(long = "agent-expose")]
    agent_expose: Vec<PathBuf>,
    /// `NAME=VALUE` in the agent's environment. Repeatable; it gets nothing else.
    #[arg(long = "agent-env")]
    agent_env: Vec<String>,
    /// System user the media keys service runs as (groups pipewire and video).
    #[arg(long)]
    keys_user: String,
    /// The media keys service's command line, whitespace-separated (`drv-keys --wpctl ...`).
    #[arg(long)]
    keys_exec: String,
    /// An entry under `/run` the media keys service sees (PipeWire's socket). Repeatable.
    #[arg(long = "keys-expose")]
    keys_expose: Vec<PathBuf>,
    /// `NAME=VALUE` in the media keys service's environment. Repeatable; it gets nothing else.
    #[arg(long = "keys-env")]
    keys_env: Vec<String>,
    /// System user drv-files (file chooser and documents mount) runs as.
    #[arg(long)]
    files_user: String,
    /// drv-files' command line, whitespace-separated (`drv-files --files DIR`).
    #[arg(long)]
    files_exec: String,
    /// An entry under `/run` drv-files sees (its `/run` holds nothing else). Repeatable.
    #[arg(long = "files-expose")]
    files_expose: Vec<PathBuf>,
    /// `NAME=VALUE` in drv-files' environment. Repeatable; it gets nothing else.
    #[arg(long = "files-env")]
    files_env: Vec<String>,
    /// `PATH:MODE` (octal): a directory drv-files owns (the person's files), created before
    /// it starts and in its root.
    #[arg(long = "files-dir")]
    files_dirs: Vec<String>,
    /// Where the documents mount goes: drv-files serves it, apps see their files under it.
    #[arg(long, default_value = "/run/drv/doc")]
    docs: PathBuf,
    /// drv-files' socket, world-connectable: every connection is keyed on the peer UID.
    #[arg(long, default_value = "/run/drv/files.sock")]
    files_socket: PathBuf,
    /// System user drv-cast (screencasts, cameras, microphones) runs as.
    #[arg(long)]
    cast_user: String,
    /// drv-cast's command line, whitespace-separated (`drv-cast`).
    #[arg(long)]
    cast_exec: String,
    /// An entry under `/run` drv-cast sees (PipeWire's sockets). Repeatable.
    #[arg(long = "cast-expose")]
    cast_expose: Vec<PathBuf>,
    /// `NAME=VALUE` in drv-cast's environment. Repeatable; it gets nothing else.
    #[arg(long = "cast-env")]
    cast_env: Vec<String>,
    /// drv-cast's socket, world-connectable: every connection is keyed on the peer UID.
    #[arg(long, default_value = "/run/drv/cast.sock")]
    cast_socket: PathBuf,
    /// The compositor's Wayland socket for apps, world-connectable: the compositor asks
    /// drv-appd who each client is.
    #[arg(long, default_value = "/run/drv/wayland")]
    apps_socket: PathBuf,
}

fn main() -> ExitCode {
    match supervise(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            drv_os::say!("drv-supervisor: {err}");
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

/// The set, in start order.
struct Set {
    seatd: Service,
    authd: Service,
    gpu: Service,
    compositor: Service,
    files: Service,
    forker: Service,
    appd: Service,
    shell: Service,
    cast: Service,
    agent: Service,
    keys: Service,
}

fn supervise(args: Args) -> Result<(), String> {
    let set = Set {
        seatd: service(
            "drv-seatd",
            &args.seatd_user,
            &args.seatd_exec,
            &args.seatd_env,
            &[],
            &args.seatd_caps,
            &args.seatd_expose,
        )?,
        authd: service(
            "drv-authd",
            &args.authd_user,
            &args.authd_exec,
            &[],
            &args.authd_dirs,
            &[],
            &args.authd_expose,
        )?,
        gpu: service(
            "compositor-gpu",
            &args.gpu_user,
            &args.gpu_exec,
            &args.gpu_env,
            &[],
            &[],
            &args.gpu_expose,
        )?,
        compositor: service(
            "compositor",
            &args.compositor_user,
            &args.compositor_exec,
            &args.compositor_env,
            &args.compositor_dirs,
            &[],
            &args.compositor_expose,
        )?,
        shell: service(
            "drv-shell",
            &args.shell_user,
            &args.shell_exec,
            &args.shell_env,
            &[],
            &[],
            &args.shell_expose,
        )?,
        files: service(
            "drv-files",
            &args.files_user,
            &args.files_exec,
            &args.files_env,
            &args.files_dirs,
            &[],
            &args.files_expose,
        )?,
        cast: service(
            "drv-cast",
            &args.cast_user,
            &args.cast_exec,
            &args.cast_env,
            &[],
            &[],
            &args.cast_expose,
        )?,
        agent: service(
            "drv-agent",
            &args.agent_user,
            &args.agent_exec,
            &args.agent_env,
            &[],
            &[],
            &args.agent_expose,
        )?,
        keys: service(
            "drv-keys",
            &args.keys_user,
            &args.keys_exec,
            &args.keys_env,
            &[],
            &[],
            &args.keys_expose,
        )?,
        forker: service(
            "drv-forker",
            &args.forker_user,
            &args.forker_exec,
            &[],
            &args.forker_dirs,
            &args.forker_caps,
            &args.forker_expose,
        )?,
        appd: service(
            "drv-appd",
            &args.appd_user,
            &args.appd_exec,
            &[],
            &[],
            &[],
            &args.appd_expose,
        )?,
    };
    // The apps' cgroups live under ours; the subtree is the forker's across restarts, the
    // kill switches stay ours. That chown was the only one: CHOWN goes, for good.
    let cgroups = Cgroups::create(set.forker.uid, set.forker.gid)?;
    drv_os::creds::drop_for_good(CapabilitySet::CHOWN)?;

    let listener = listen(&args.socket)?;
    // Datagrams with fds: the apps' shims speak each service's `wire`, not a stream.
    let doors = Doors {
        files: listen_seqpacket(&args.files_socket)?,
        cast: listen_seqpacket(&args.cast_socket)?,
        notify: listen_seqpacket(&args.notify_socket)?,
        agent: listen(&args.agent_socket)?,
        apps: listen(&args.apps_socket)?,
    };

    // A set that keeps dying is not restarted for good: drv-seatd takes the VT on every start,
    // which would leave the console unusable. systemd's start limit takes it from here.
    const DEATHS: usize = 5;
    const WINDOW: Duration = Duration::from_secs(60);
    let mut deaths: Vec<Instant> = Vec::new();
    loop {
        match start_set(&set, &cgroups, &listener, &doors, &args.docs) {
            Ok(children) => {
                let gone = wait_first(&children);
                drv_os::say!("drv-supervisor: {gone} exited; restarting the set");
                stop_set(&cgroups, children);
            }
            Err(err) => drv_os::say!("drv-supervisor: {err}"),
        }
        let now = Instant::now();
        deaths.retain(|t| now.duration_since(*t) < WINDOW);
        deaths.push(now);
        if deaths.len() >= DEATHS {
            return Err(format!(
                "the set died {DEATHS} times within {}s; giving up",
                WINDOW.as_secs()
            ));
        }
        thread::sleep(Duration::from_secs(1));
    }
}

/// A world-connectable socket for a member that keys every connection on the peer UID.
fn listen(path: &Path) -> Result<UnixListener, String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let _ = fs::remove_file(path);
    let listener =
        UnixListener::bind(path).map_err(|e| format!("listen on {}: {e}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    Ok(listener)
}

/// The app-facing sockets of the services, all in the one directory the forker binds into
/// every app (`/run/drv`, ours), bound here so an app started early waits in a backlog
/// instead of finding no socket. Every one keys on the peer UID on the service's side.
struct Doors {
    files: UnixListener,
    cast: UnixListener,
    notify: UnixListener,
    agent: UnixListener,
    apps: UnixListener,
}

/// Like `listen`, a `SOCK_SEQPACKET` listener.
fn listen_seqpacket(path: &Path) -> Result<UnixListener, String> {
    use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
    }
    let _ = fs::remove_file(path);
    let sock = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::SEQPACKET,
        SocketFlags::CLOEXEC,
        None,
    )
    .map_err(|e| format!("socket: {e}"))?;
    let addr = SocketAddrUnix::new(path).map_err(|e| format!("{}: {e}", path.display()))?;
    rustix::net::bind(&sock, &addr).map_err(|e| format!("bind {}: {e}", path.display()))?;
    rustix::net::listen(&sock, 64).map_err(|e| format!("listen on {}: {e}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o666))
        .map_err(|e| format!("chmod {}: {e}", path.display()))?;
    Ok(UnixListener::from(sock))
}

/// The documents mount, fresh for this set: a FUSE connection whose serving end goes to
/// drv-files as fd `fuse`. `allow_other` because the apps are the readers; who may see what
/// is drv-files' check, per request, by the caller's UID. What the last set served is gone
/// with it, like the apps that held it.
fn mount_docs(at: &Path, uid: u32, gid: u32) -> Result<OwnedFd, String> {
    let target = CString::new(at.as_os_str().as_bytes()).map_err(|e| e.to_string())?;
    // SAFETY: a NUL-terminated path; a stale mount from the last set is the expected case.
    unsafe { libc::umount2(target.as_ptr(), libc::MNT_DETACH) };
    fs::create_dir_all(at).map_err(|e| format!("mkdir {}: {e}", at.display()))?;
    let fuse = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .map_err(|e| format!("open /dev/fuse: {e}"))?;
    let data = CString::new(format!(
        "fd={},rootmode=40000,user_id={uid},group_id={gid},allow_other",
        fuse.as_raw_fd()
    ))
    .map_err(|e| e.to_string())?;
    // SAFETY: NUL-terminated strings, flags and data as mount(2) wants them.
    let rc = unsafe {
        libc::mount(
            c"drv-doc".as_ptr(),
            target.as_ptr(),
            c"fuse".as_ptr(),
            libc::MS_NOSUID | libc::MS_NODEV,
            data.as_ptr().cast(),
        )
    };
    if rc != 0 {
        return Err(format!(
            "mount fuse on {}: {}",
            at.display(),
            io::Error::last_os_error()
        ));
    }
    Ok(OwnedFd::from(fuse))
}

/// Every link is a socketpair made here; each member gets its ends by name.
struct Links {
    compositor_seat: (OwnedFd, OwnedFd),
    compositor_auth: (OwnedFd, OwnedFd),
    compositor_gpu: (OwnedFd, OwnedFd),
    compositor_appd: (OwnedFd, OwnedFd),
    /// The compositor's poke line to the shell: a byte per `show-launcher`.
    compositor_menu: (OwnedFd, OwnedFd),
    /// The compositor's key line to drv-keys: a line per media key.
    compositor_keys: (OwnedFd, OwnedFd),
    /// The cast line: drv-cast starts and stops what the person consented to.
    compositor_cast: (OwnedFd, OwnedFd),
    /// The shell's Wayland connection: session-lock and layer-shell.
    shell_client: (OwnedFd, OwnedFd),
    /// The shell's launch channel: the menu starts apps.
    shell_appd: (OwnedFd, OwnedFd),
    /// The shell's line to drv-authd: the lock screen's PIN goes there.
    shell_auth: (OwnedFd, OwnedFd),
    /// drv-cast asks the person at the shell: screen picks, device consent.
    cast_shell: (OwnedFd, OwnedFd),
    /// The ssh agent asks the person at the shell: the authenticator's PIN and touch.
    agent_shell: (OwnedFd, OwnedFd),
    /// drv-files' Wayland connection: layer-shell for the chooser.
    files_client: (OwnedFd, OwnedFd),
    appd_forker: (OwnedFd, OwnedFd),
}

impl Links {
    fn make() -> Result<Self, String> {
        let seq = || seqpacket_pair().map_err(|e| format!("socketpair: {e}"));
        let stream = || stream_pair().map_err(|e| format!("socketpair: {e}"));
        Ok(Self {
            compositor_seat: seq()?,
            compositor_auth: seq()?,
            compositor_gpu: stream()?,
            compositor_appd: stream()?,
            compositor_menu: stream()?,
            compositor_keys: stream()?,
            compositor_cast: seq()?,
            shell_client: stream()?,
            shell_appd: stream()?,
            shell_auth: seq()?,
            cast_shell: seq()?,
            agent_shell: seq()?,
            files_client: stream()?,
            appd_forker: seq()?,
        })
    }
}

type Group = Vec<(&'static str, Child)>;

/// Starts the set in order; a member that fails to start takes the ones already up down.
fn start_set(
    set: &Set,
    cgroups: &Cgroups,
    listener: &UnixListener,
    doors: &Doors,
    docs: &Path,
) -> Result<Group, String> {
    let l = Links::make()?;
    let fuse = mount_docs(docs, set.files.uid, set.files.gid)?;
    let members: [(
        &'static str,
        &Service,
        Vec<(&str, std::os::fd::BorrowedFd<'_>)>,
    ); 11] = [
        (
            "drv-seatd",
            &set.seatd,
            vec![("compositor", l.compositor_seat.1.as_fd())],
        ),
        (
            "drv-authd",
            &set.authd,
            vec![
                ("compositor", l.compositor_auth.1.as_fd()),
                ("locker", l.shell_auth.1.as_fd()),
            ],
        ),
        (
            "compositor-gpu",
            &set.gpu,
            vec![("compositor", l.compositor_gpu.1.as_fd())],
        ),
        (
            "compositor",
            &set.compositor,
            vec![
                ("seat", l.compositor_seat.0.as_fd()),
                ("auth", l.compositor_auth.0.as_fd()),
                ("gpu", l.compositor_gpu.0.as_fd()),
                ("appd", l.compositor_appd.0.as_fd()),
                ("menu", l.compositor_menu.0.as_fd()),
                ("keys", l.compositor_keys.0.as_fd()),
                ("shell-client", l.shell_client.0.as_fd()),
                ("files-client", l.files_client.0.as_fd()),
                ("cast", l.compositor_cast.0.as_fd()),
                ("apps", doors.apps.as_fd()),
            ],
        ),
        // drv-files serves the documents mount: anything that touches it (the forker,
        // cloning the doors for an app's root) blocks until it answers, so it comes first.
        (
            "drv-files",
            &set.files,
            vec![
                ("wayland", l.files_client.1.as_fd()),
                ("fuse", fuse.as_fd()),
                ("listener", doors.files.as_fd()),
            ],
        ),
        // The forker and drv-appd before the services that ask drv-appd who their callers
        // are.
        (
            "drv-forker",
            &set.forker,
            vec![("channel", l.appd_forker.1.as_fd())],
        ),
        (
            "drv-appd",
            &set.appd,
            vec![
                ("listener", listener.as_fd()),
                ("channel", l.appd_forker.0.as_fd()),
                ("compositor", l.compositor_appd.1.as_fd()),
                ("shell", l.shell_appd.1.as_fd()),
            ],
        ),
        (
            "drv-shell",
            &set.shell,
            vec![
                ("wayland", l.shell_client.1.as_fd()),
                ("auth", l.shell_auth.0.as_fd()),
                ("appd", l.shell_appd.0.as_fd()),
                ("poke", l.compositor_menu.1.as_fd()),
                ("cast", l.cast_shell.1.as_fd()),
                ("agent", l.agent_shell.1.as_fd()),
                ("listener", doors.notify.as_fd()),
            ],
        ),
        (
            "drv-cast",
            &set.cast,
            vec![
                ("compositor", l.compositor_cast.1.as_fd()),
                ("shell", l.cast_shell.0.as_fd()),
                ("listener", doors.cast.as_fd()),
            ],
        ),
        // Its door's uid check is its own; the wire is for the person's PIN and touch,
        // asked at the shell.
        (
            "drv-agent",
            &set.agent,
            vec![
                ("shell", l.agent_shell.0.as_fd()),
                ("listener", doors.agent.as_fd()),
            ],
        ),
        (
            "drv-keys",
            &set.keys,
            vec![("compositor", l.compositor_keys.1.as_fd())],
        ),
    ];
    let mut children: Group = Vec::new();
    for (name, service, fds) in members {
        match start_service(service, &fds, &cgroups.set_procs) {
            Ok(child) => {
                drv_os::say!(
                    "drv-supervisor: {name} running as uid {}, pid {}",
                    service.uid,
                    child.id()
                );
                children.push((name, child));
            }
            Err(err) => {
                stop_set(cgroups, children);
                return Err(err);
            }
        }
    }
    Ok(children)
}

/// The apps first (one cgroup write), then every member of the set (one more), then the
/// reaping.
fn stop_set(cgroups: &Cgroups, children: Group) {
    if let Err(err) = cgroups.kill_apps() {
        drv_os::say!("drv-supervisor: killing the apps: {err}");
    }
    if let Err(err) = cgroups.kill_set() {
        drv_os::say!("drv-supervisor: killing the set: {err}");
    }
    for (_, mut child) in children {
        let _ = child.wait();
    }
}

/// Waits for the first of the group to exit, then names every member already gone, with
/// how ("compositor-gpu (signal 9), compositor (exit status 1)"): a member's death takes
/// its peers down within the same instant, and which exited first says nothing about who
/// died first. The children are not reaped here (waiting on a `&Child` is not possible);
/// `stop_set` reaps them all.
fn wait_first(children: &Group) -> String {
    use rustix::process::{waitid, WaitId, WaitIdOptions};
    loop {
        match waitid(WaitId::All, WaitIdOptions::EXITED | WaitIdOptions::NOWAIT) {
            Ok(_) => break,
            Err(rustix::io::Errno::INTR) => {}
            Err(e) => return format!("? (waitid: {e})"),
        }
    }
    // A moment for the cascade, so the report has the cause and not only its first victim.
    thread::sleep(Duration::from_millis(200));
    let gone: Vec<String> = children
        .iter()
        .filter_map(|(name, child)| {
            exited(child.id() as i32, libc::WNOHANG).map(|how| format!("{name} ({how})"))
        })
        .collect();
    if gone.is_empty() {
        "? (lost)".to_owned()
    } else {
        gone.join(", ")
    }
}

/// Whether `pid` has exited, and how, without reaping it. `flags` adds `WNOHANG` to poll.
fn exited(pid: i32, flags: libc::c_int) -> Option<String> {
    // SAFETY: waitid with WNOWAIT leaves the child for `Child::wait` to reap.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT | flags,
        )
    };
    // SAFETY: a successful waitid filled the child fields.
    if rc != 0 || unsafe { info.si_pid() } == 0 {
        return None;
    }
    let status = unsafe { info.si_status() };
    Some(match info.si_code {
        libc::CLD_EXITED => format!("exit status {status}"),
        libc::CLD_DUMPED => format!("signal {status}, core dumped"),
        _ => format!("signal {status}"),
    })
}

/// A service from the command line: its user must exist and must not be root.
fn service(
    name: &str,
    user: &str,
    exec: &str,
    env: &[String],
    dirs: &[String],
    caps: &[String],
    expose: &[PathBuf],
) -> Result<Service, String> {
    let (uid, gid) = user_ids(user)?;
    if uid == 0 {
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
            let mode =
                u32::from_str_radix(mode, 8).map_err(|e| format!("{name}: mode {mode:?}: {e}"))?;
            Ok((PathBuf::from(path), mode))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let mut capset = CapabilitySet::empty();
    for cap in caps {
        capset |= capability(cap).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(Service {
        name: name.to_owned(),
        uid,
        gid,
        groups: user_groups(user, gid)?,
        argv: exec.split_whitespace().map(String::from).collect(),
        env,
        dirs,
        caps: capset,
        expose: expose.to_vec(),
        // Only the forker keeps the network: apps with `network` get it from the forker's
        // namespace. Nobody else in the set talks to anything but its fds and sockets.
        network: name == "drv-forker",
        cgroups: name == "drv-forker",
        // The media keys write the backlight; nobody else touches sysfs.
        writable_sys: name == "drv-keys",
        nix_daemon: name == "drv-forker",
    })
}
