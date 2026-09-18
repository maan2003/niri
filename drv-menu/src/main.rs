//! The app menu, a supervisor service. It holds a launch channel to drv-appd (fd `appd`) and
//! a poke line from the compositor (fd `compositor`): each `show-launcher` bind sends a byte,
//! we run the dmenu-style program with the launchable names on its stdin, and launch what it
//! prints. The program runs as our uid with our environment; it never sees the channel
//! (close-on-exec) and cannot ptrace us. Its output is only a name: drv-appd decides what
//! that name runs.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, ExitCode, Stdio};

use clap::Parser;
use drv_os::fds::Kind;
use drv_policy::PolicyClient;

#[derive(Parser)]
#[command(name = "drv-menu", about = "The app menu; runs under drv-supervisor")]
struct Args {
    /// The dmenu-style program and its arguments: names on stdin, the choice on stdout.
    #[arg(required = true, trailing_var_arg = true)]
    menu: Vec<String>,
}

fn run(args: Args) -> Result<(), String> {
    // The menu program shares our uid: not dumpable means it cannot ptrace us or read our
    // fds through /proc.
    // SAFETY: prctl with constant arguments.
    if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
        return Err(format!("PR_SET_DUMPABLE: {}", io::Error::last_os_error()));
    }
    let mut fds = drv_os::fds::take().map_err(|e| format!("fds from the supervisor: {e}"))?;
    let appd = fds.socket("appd", Kind::Stream).map_err(|e| e.to_string())?;
    let compositor = fds.socket("compositor", Kind::Stream).map_err(|e| e.to_string())?;
    let mut appd = PolicyClient::from_stream(UnixStream::from(appd))
        .map_err(|e| format!("the launch channel: {e}"))?;
    let mut compositor = UnixStream::from(compositor);

    let mut buf = [0u8; 64];
    loop {
        match compositor.read(&mut buf) {
            Ok(0) => return Err("the compositor hung up".to_owned()),
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(format!("the compositor's socket: {err}")),
        }
        if let Err(err) = show(&args.menu, &mut appd) {
            eprintln!("drv-menu: {err}");
        }
        drain(&compositor);
    }
}

/// One showing: the names in, the choice out, launched if there was one.
fn show(menu: &[String], appd: &mut PolicyClient) -> Result<(), String> {
    let apps = appd
        .apps()
        .map_err(|e| format!("asking drv-appd for the apps: {e}"))?;
    let mut child = Command::new(&menu[0])
        .args(&menu[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("running {}: {e}", menu[0]))?;
    if let Some(mut stdin) = child.stdin.take() {
        // A program that closes stdin early is its own business.
        let _ = stdin.write_all(apps.join("\n").as_bytes());
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("waiting for {}: {e}", menu[0]))?;
    let choice = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if choice.is_empty() {
        return Ok(());
    }
    let uid = appd
        .launch(choice.clone())
        .map_err(|e| format!("launching {choice:?}: {e}"))?;
    eprintln!("drv-menu: launched {choice:?} as uid {uid}");
    Ok(())
}

/// Pokes that arrived while the menu was up do not reopen it.
fn drain(sock: &UnixStream) {
    let mut sock = sock;
    let mut buf = [0u8; 64];
    let _ = sock.set_nonblocking(true);
    while matches!(sock.read(&mut buf), Ok(n) if n > 0) {}
    let _ = sock.set_nonblocking(false);
}

fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("drv-menu: {err}");
            ExitCode::FAILURE
        }
    }
}
