//! The ssh agent, a supervisor service. OpenSSH's ssh-agent runs here, as this uid, with the
//! authenticators' hidraw nodes (a udev rule makes them ours) and its socket in our private
//! `/tmp`. This process fronts it on the public socket (`/run/drv-agent/agent`, in every
//! app's root) and lets through the UIDs the manifest grants `agent`, checked on each
//! connection (SO_PEERCRED) and refused at accept otherwise. ssh-agent itself would refuse
//! them all: it serves its own uid only, which is why the door is a separate process.
//!
//! Resident keys live on the authenticator and are loaded where the authenticator is, so one
//! agent-protocol extension, `load-resident@drv`, carries the person's PIN from `drv-agent
//! load` (run in a terminal app with the grant) and runs `ssh-add -K` here. Every other
//! message goes to ssh-agent unread and its reply comes back the same way.

use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};

const SSH_AGENT_SUCCESS: u8 = 6;
const SSH_AGENTC_EXTENSION: u8 = 27;
const SSH_AGENT_EXTENSION_FAILURE: u8 = 28;
const EXTENSION: &[u8] = b"load-resident@drv";
/// Longer than any agent message has a right to be.
const MAX_MESSAGE: usize = 256 * 1024;

#[derive(Parser)]
struct Args {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// The service: ssh-agent behind the door.
    Serve {
        /// The public socket, in every app's root.
        #[arg(long, default_value = "/run/drv-agent/agent")]
        listen: PathBuf,
        /// ssh-agent's own socket, in our private /tmp.
        #[arg(long, default_value = "/tmp/ssh-agent")]
        private: PathBuf,
        /// A UID that may use the agent (the manifest's `agent` grant). Repeatable.
        #[arg(long = "allow")]
        allow: Vec<u32>,
        #[arg(long)]
        ssh_agent: PathBuf,
        #[arg(long)]
        ssh_add: PathBuf,
    },
    /// Load the resident keys of the plugged-in authenticator into the agent: asks the PIN
    /// on the terminal and sends it to the service, which has the authenticator.
    Load,
}

fn main() {
    // ssh-add's askpass while loading: it runs us with the prompt as the one argument, so
    // this is told apart by the PIN the door put in the environment, not by a subcommand.
    if let Some(pin) = std::env::var_os("DRV_PIN") {
        println!("{}", pin.to_string_lossy());
        return;
    }
    let result = match Args::parse().command {
        Cmd::Serve {
            listen,
            private,
            allow,
            ssh_agent,
            ssh_add,
        } => serve(&listen, &private, &allow, &ssh_agent, &ssh_add),
        Cmd::Load => load(),
    };
    if let Err(err) = result {
        drv_os::say!("drv-agent: {err}");
        process::exit(1);
    }
}

fn serve(
    listen: &Path,
    private: &Path,
    allow: &[u32],
    ssh_agent: &Path,
    ssh_add: &Path,
) -> Result<(), String> {
    let mut agent = Command::new(ssh_agent)
        .arg("-D")
        .arg("-a")
        .arg(private)
        .stdout(Stdio::null())
        .spawn()
        .map_err(|e| format!("{}: {e}", ssh_agent.display()))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while !private.exists() {
        if let Ok(Some(status)) = agent.try_wait() {
            return Err(format!("ssh-agent: {status}"));
        }
        if Instant::now() > deadline {
            return Err("ssh-agent: no socket after 5 s".to_owned());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_file(listen);
    let listener =
        UnixListener::bind(listen).map_err(|e| format!("{}: {e}", listen.display()))?;
    // The door is the uid check below, not the mode: apps of any uid may connect.
    std::fs::set_permissions(listen, std::os::unix::fs::PermissionsExt::from_mode(0o666))
        .map_err(|e| format!("{}: {e}", listen.display()))?;
    drv_os::say!("drv-agent: serving {} for uids {allow:?}", listen.display());
    let ctx = Ctx {
        private: private.to_owned(),
        ssh_add: ssh_add.to_owned(),
    };
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                drv_os::say!("drv-agent: accept: {err}");
                continue;
            }
        };
        let uid = match rustix::net::sockopt::socket_peercred(&stream) {
            Ok(cred) => cred.uid.as_raw(),
            Err(err) => {
                drv_os::say!("drv-agent: peer credentials: {err}");
                continue;
            }
        };
        if !allow.contains(&uid) {
            drv_os::say!("drv-agent: refused uid {uid}");
            continue;
        }
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Err(err) = ctx.client(stream) {
                drv_os::say!("drv-agent: uid {uid}: {err}");
            }
        });
    }
    Ok(())
}

#[derive(Clone)]
struct Ctx {
    private: PathBuf,
    ssh_add: PathBuf,
}

impl Ctx {
    /// One client: each of its messages to ssh-agent and the reply back, except ours.
    fn client(&self, mut client: UnixStream) -> io::Result<()> {
        let mut agent = UnixStream::connect(&self.private)?;
        loop {
            let Some(msg) = read_message(&mut client)? else {
                return Ok(());
            };
            let reply = match parse_extension(&msg) {
                Some(pin) => match self.load_resident(pin) {
                    Ok(()) => vec![SSH_AGENT_SUCCESS],
                    Err(err) => {
                        let mut reply = vec![SSH_AGENT_EXTENSION_FAILURE];
                        put_string(&mut reply, err.as_bytes());
                        reply
                    }
                },
                None => {
                    write_message(&mut agent, &msg)?;
                    match read_message(&mut agent)? {
                        Some(reply) => reply,
                        None => return Err(io::Error::other("ssh-agent hung up")),
                    }
                }
            };
            write_message(&mut client, &reply)?;
        }
    }

    /// `ssh-add -K` against ssh-agent, the PIN through our own askpass.
    fn load_resident(&self, pin: &[u8]) -> Result<(), String> {
        let pin = std::str::from_utf8(pin).map_err(|_| "the PIN is not text".to_owned())?;
        let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
        let out = Command::new(&self.ssh_add)
            .arg("-K")
            .env("SSH_AUTH_SOCK", &self.private)
            .env("SSH_ASKPASS", me)
            .env("SSH_ASKPASS_REQUIRE", "force")
            .env("DRV_PIN", pin)
            .stdin(Stdio::null())
            .output()
            .map_err(|e| format!("{}: {e}", self.ssh_add.display()))?;
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let text = text.trim();
        if out.status.success() {
            drv_os::say!("drv-agent: loaded resident keys: {text}");
            Ok(())
        } else {
            Err(if text.is_empty() {
                format!("ssh-add -K: {}", out.status)
            } else {
                text.to_owned()
            })
        }
    }
}

/// The PIN in a `load-resident@drv` extension request, if that is what `msg` is.
fn parse_extension(msg: &[u8]) -> Option<&[u8]> {
    let (&SSH_AGENTC_EXTENSION, rest) = msg.split_first()? else {
        return None;
    };
    let (name, rest) = get_string(rest)?;
    if name != EXTENSION {
        return None;
    }
    let (pin, rest) = get_string(rest)?;
    rest.is_empty().then_some(pin)
}

fn get_string(buf: &[u8]) -> Option<(&[u8], &[u8])> {
    let len = u32::from_be_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let rest = buf.get(4..)?;
    (rest.len() >= len).then(|| rest.split_at(len))
}

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(&(s.len() as u32).to_be_bytes());
    out.extend_from_slice(s);
}

/// One agent message (its body, after the length); `None` at a clean end of stream.
fn read_message(from: &mut UnixStream) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match from.read_exact(&mut len) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_MESSAGE {
        return Err(io::Error::other(format!("message of {len} bytes")));
    }
    let mut body = vec![0u8; len];
    from.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_message(to: &mut UnixStream, body: &[u8]) -> io::Result<()> {
    to.write_all(&(body.len() as u32).to_be_bytes())?;
    to.write_all(body)
}

/// The client half: the PIN from the terminal, the extension to the agent, its answer.
fn load() -> Result<(), String> {
    let sock = std::env::var_os("SSH_AUTH_SOCK").ok_or("SSH_AUTH_SOCK is not set")?;
    let mut agent = UnixStream::connect(&sock)
        .map_err(|e| format!("{}: {e}", Path::new(&sock).display()))?;
    let pin = ask_pin()?;
    let mut msg = vec![SSH_AGENTC_EXTENSION];
    put_string(&mut msg, EXTENSION);
    put_string(&mut msg, pin.as_bytes());
    write_message(&mut agent, &msg).map_err(|e| format!("agent: {e}"))?;
    let reply = read_message(&mut agent)
        .map_err(|e| format!("agent: {e}"))?
        .ok_or("the agent hung up: no grant for this app?")?;
    match reply.split_first() {
        Some((&SSH_AGENT_SUCCESS, _)) => {
            println!("resident keys loaded");
            Ok(())
        }
        Some((&SSH_AGENT_EXTENSION_FAILURE, rest)) => match get_string(rest) {
            Some((text, _)) => Err(String::from_utf8_lossy(text).into_owned()),
            None => Err("refused".to_owned()),
        },
        Some((kind, _)) => Err(format!("agent replied with message type {kind}")),
        None => Err("empty reply".to_owned()),
    }
}

/// A line from the terminal with echo off.
fn ask_pin() -> Result<String, String> {
    use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};
    let stdin = io::stdin();
    let saved = tcgetattr(&stdin).map_err(|e| format!("stdin is not a terminal: {e}"))?;
    let mut quiet = saved.clone();
    quiet.local_modes.remove(LocalModes::ECHO);
    tcsetattr(&stdin, OptionalActions::Now, &quiet).map_err(|e| format!("termios: {e}"))?;
    eprint!("PIN for the authenticator: ");
    let mut line = String::new();
    let read = stdin.lock().read_line(&mut line);
    let _ = tcsetattr(&stdin, OptionalActions::Now, &saved);
    eprintln!();
    read.map_err(|e| format!("stdin: {e}"))?;
    let pin = line.trim_end_matches(['\n', '\r']).to_owned();
    if pin.is_empty() {
        return Err("no PIN".to_owned());
    }
    Ok(pin)
}

use std::io::BufRead as _;
