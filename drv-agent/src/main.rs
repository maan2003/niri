//! The ssh agent, a supervisor service. OpenSSH's ssh-agent runs here, as this uid, with the
//! authenticators' hidraw nodes (a udev rule makes them ours) and its socket in our private
//! `/tmp`. This process fronts it on the public socket (`/run/drv-agent/agent`, in every
//! app's root) and lets through the UIDs the manifest grants `agent`, checked on each
//! connection (SO_PEERCRED) and refused at accept otherwise. ssh-agent itself would refuse
//! them all: it serves its own uid only, which is why the door is a separate process.
//!
//! The authenticator's PIN never passes through an app. When a granted app lists or signs
//! while the agent holds no keys and an authenticator is plugged in, the door asks the
//! person at the shell (fd `shell`) and loads the resident keys with `ssh-add -K` itself.
//! ssh-agent's own prompts while signing (the PIN of a verify-required key, a touch) come
//! the same way: we are its askpass, reaching the door over a socket in our `/tmp`.

use std::collections::HashMap;
use std::io::{self, BufRead as _, Read as _, Write as _};
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{self, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use drv_os::fds::Kind;
use drv_policy::seq;
use drv_policy::door::{self, Door as Appd};
use drv_shell::ask::{Request, Response, VERSION};

const SSH_AGENTC_REQUEST_IDENTITIES: u8 = 11;
const SSH_AGENT_IDENTITIES_ANSWER: u8 = 12;
const SSH_AGENTC_SIGN_REQUEST: u8 = 13;
/// Longer than any agent message has a right to be.
const MAX_MESSAGE: usize = 256 * 1024;
/// Where ssh-agent's askpass (us again) finds the door; our /tmp is private.
const ASKPASS: &str = "/tmp/askpass";

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
        #[arg(long)]
        ssh_agent: PathBuf,
        #[arg(long)]
        ssh_add: PathBuf,
    },
}

fn main() {
    // ssh-add's askpass while loading: it runs us with the prompt as the one argument, and
    // the PIN the door already has is in the environment.
    if let Some(pin) = std::env::var_os("DRV_PIN") {
        println!("{}", pin.to_string_lossy());
        return;
    }
    // ssh-agent's askpass while signing: the prompt as the one argument, the door's socket
    // in the environment (ours to it).
    if let Some(sock) = std::env::var_os("DRV_ASKPASS") {
        let prompt = std::env::args().nth(1).unwrap_or_default();
        let kind = std::env::var("SSH_ASKPASS_PROMPT").unwrap_or_default();
        if let Err(err) = askpass(Path::new(&sock), &kind, &prompt) {
            drv_os::say!("drv-agent: askpass: {err}");
            process::exit(1);
        }
        return;
    }
    let result = match Args::parse().command {
        Cmd::Serve {
            listen,
            private,
            ssh_agent,
            ssh_add,
        } => serve(&listen, &private, &ssh_agent, &ssh_add),
    };
    if let Err(err) = result {
        drv_os::say!("drv-agent: {err}");
        process::exit(1);
    }
}

fn serve(listen: &Path, private: &Path, ssh_agent: &Path, ssh_add: &Path) -> Result<(), String> {
    let mut fds = drv_os::fds::take().map_err(|e| format!("fds from the supervisor: {e}"))?;
    let shell = fds
        .socket("shell", Kind::SeqPacket)
        .map_err(|e| e.to_string())?;
    let appd = Appd::open().map_err(|e| format!("drv-appd: {e}"))?;
    let _ = std::fs::remove_file(ASKPASS);
    let askpass = UnixListener::bind(ASKPASS).map_err(|e| format!("{ASKPASS}: {e}"))?;
    let me = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let mut agent = Command::new(ssh_agent)
        .arg("-D")
        .arg("-a")
        .arg(private)
        // Its prompts (a PIN, a touch) run us; `force`: it has no display and no terminal.
        .env("SSH_ASKPASS", &me)
        .env("SSH_ASKPASS_REQUIRE", "force")
        .env("DRV_ASKPASS", ASKPASS)
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
    let listener = UnixListener::bind(listen).map_err(|e| format!("{}: {e}", listen.display()))?;
    // The door is the uid check below, not the mode: apps of any uid may connect.
    std::fs::set_permissions(listen, std::os::unix::fs::PermissionsExt::from_mode(0o666))
        .map_err(|e| format!("{}: {e}", listen.display()))?;
    // The door is bound before the shell answers (its fonts take a moment): an app started
    // meanwhile waits in the backlog instead of finding no socket.
    let shell = Shell::start(shell)?;
    drv_os::say!("drv-agent: serving {}", listen.display());
    let door = Arc::new(Door {
        private: private.to_owned(),
        ssh_add: ssh_add.to_owned(),
        shell,
        loading: Mutex::new(()),
        signing: Mutex::new(Vec::new()),
    });
    let asked = door.clone();
    std::thread::spawn(move || asked.serve_askpass(askpass));
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                drv_os::say!("drv-agent: accept: {err}");
                continue;
            }
        };
        let uid = match door::peer_uid(&stream) {
            Ok(uid) => uid,
            Err(err) => {
                drv_os::say!("drv-agent: peer credentials: {err}");
                continue;
            }
        };
        // drv-appd's record for the uid says whether it may knock, and names it for the
        // person's prompts.
        let app = match appd.who(uid) {
            Ok(policy) if policy.agent => policy.name.clone(),
            Ok(_) => {
                drv_os::say!("drv-agent: refused uid {uid}");
                continue;
            }
            Err(err) => {
                drv_os::say!("drv-agent: refused uid {uid}: {err}");
                continue;
            }
        };
        let door = door.clone();
        std::thread::spawn(move || {
            if let Err(err) = door.client(&app, uid, stream) {
                drv_os::say!("drv-agent: {app} (uid {uid}): {err}");
            }
        });
    }
    Ok(())
}

/// Our line to the shell: requests under ids of our own, answers back on a thread to
/// whoever waits for that id.
struct Shell {
    out: Mutex<OwnedFd>,
    waiting: Mutex<HashMap<u64, mpsc::Sender<Response>>>,
    next: AtomicU64,
}

impl Shell {
    fn start(sock: OwnedFd) -> Result<Arc<Self>, String> {
        seq::send(&sock, &Request::Hello { version: VERSION }, &[])
            .map_err(|e| format!("hello to the shell: {e}"))?;
        let (hello, _) =
            seq::recv::<Response>(&sock).map_err(|e| format!("hello from the shell: {e}"))?;
        match hello {
            Response::Hello { version } if version == VERSION => {}
            Response::Hello { version } => {
                drv_os::say!("drv-agent: the shell speaks version {version}, we speak {VERSION}");
            }
            _ => return Err("no hello from the shell".to_owned()),
        }
        let reader = sock.try_clone().map_err(|e| format!("dup: {e}"))?;
        let shell = Arc::new(Self {
            out: Mutex::new(sock),
            waiting: Mutex::default(),
            next: AtomicU64::new(1),
        });
        let dispatcher = shell.clone();
        std::thread::spawn(move || {
            loop {
                match seq::recv::<Response>(&reader) {
                    Ok((resp, _)) => dispatcher.dispatch(resp),
                    Err(err) => {
                        drv_os::say!("drv-agent: the shell: {err}");
                        process::exit(1);
                    }
                }
            }
        });
        Ok(shell)
    }

    fn dispatch(&self, resp: Response) {
        let id = match &resp {
            Response::Secret { id, .. }
            | Response::Cancelled { id }
            | Response::Yes { id }
            | Response::Picked { id, .. } => *id,
            Response::Hello { .. } => return,
        };
        let waiter = self.waiting.lock().unwrap().remove(&id);
        if let Some(tx) = waiter {
            let _ = tx.send(resp);
        }
    }

    fn send(&self, req: &Request) -> Result<(), String> {
        seq::send(&*self.out.lock().unwrap(), req, &[]).map_err(|e| format!("the shell: {e}"))
    }

    /// A request under a fresh id, and where its answer arrives.
    fn ask(
        &self,
        make: impl FnOnce(u64) -> Request,
    ) -> Result<(u64, mpsc::Receiver<Response>), String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel();
        self.waiting.lock().unwrap().insert(id, tx);
        if let Err(err) = self.send(&make(id)) {
            self.waiting.lock().unwrap().remove(&id);
            return Err(err);
        }
        Ok((id, rx))
    }

    /// The PIN the person typed, or None if they refused.
    fn pin(&self, app: &str, uid: u32, prompt: &str) -> Result<Option<String>, String> {
        let (app, prompt) = (app.to_owned(), prompt.to_owned());
        let (_, rx) = self.ask(|id| Request::Secret {
            id,
            app,
            uid,
            what: "use your security key".to_owned(),
            prompt,
        })?;
        match rx.recv() {
            Ok(Response::Secret { secret, .. }) => Ok(Some(secret)),
            Ok(Response::Cancelled { .. }) => Ok(None),
            Ok(_) => Err("the shell answered something else".to_owned()),
            Err(_) => Err("the shell is gone".to_owned()),
        }
    }

    /// A touch prompt, up until what this returns is dropped.
    fn touch(self: &Arc<Self>, app: &str, uid: u32, prompt: &str) -> Result<Touching, String> {
        let (app, prompt) = (app.to_owned(), prompt.to_owned());
        let (id, rx) = self.ask(|id| Request::Touch {
            id,
            app,
            uid,
            what: "use your security key".to_owned(),
            prompt,
        })?;
        Ok(Touching {
            shell: self.clone(),
            id,
            _rx: rx,
        })
    }
}

struct Touching {
    shell: Arc<Shell>,
    id: u64,
    _rx: mpsc::Receiver<Response>,
}

impl Drop for Touching {
    fn drop(&mut self) {
        self.shell.waiting.lock().unwrap().remove(&self.id);
        if let Err(err) = self.shell.send(&Request::Cancel { id: self.id }) {
            drv_os::say!("drv-agent: {err}");
        }
    }
}

struct Door {
    private: PathBuf,
    ssh_add: PathBuf,
    shell: Arc<Shell>,
    /// One load at a time: a second client waits, then finds the keys there.
    loading: Mutex<()>,
    /// The apps in a sign request right now: whom ssh-agent's prompts are for.
    signing: Mutex<Vec<(String, u32)>>,
}

impl Door {
    /// One client: each of its messages to ssh-agent and the reply back. A list or a sign
    /// with nothing loaded loads first.
    fn client(&self, app: &str, uid: u32, mut client: UnixStream) -> io::Result<()> {
        let mut agent = UnixStream::connect(&self.private)?;
        loop {
            let Some(msg) = read_message(&mut client)? else {
                return Ok(());
            };
            let kind = msg[0];
            if matches!(
                kind,
                SSH_AGENTC_REQUEST_IDENTITIES | SSH_AGENTC_SIGN_REQUEST
            ) {
                self.load_if_empty(app, uid);
            }
            let _signing =
                (kind == SSH_AGENTC_SIGN_REQUEST).then(|| Signing::start(self, app, uid));
            write_message(&mut agent, &msg)?;
            let Some(reply) = read_message(&mut agent)? else {
                return Err(io::Error::other("ssh-agent hung up"));
            };
            write_message(&mut client, &reply)?;
        }
    }

    /// The first list or sign with nothing loaded, while an authenticator is plugged in:
    /// the person's PIN from the shell, the resident keys from the authenticator.
    fn load_if_empty(&self, app: &str, uid: u32) {
        let _one = self.loading.lock().unwrap();
        match self.count_keys() {
            Ok(0) => {}
            Ok(_) => return,
            Err(err) => {
                drv_os::say!("drv-agent: listing: {err}");
                return;
            }
        }
        if !authenticator_present() {
            return;
        }
        let prompt = "Enter its PIN to load its ssh keys";
        match self.shell.pin(app, uid, prompt) {
            Ok(Some(pin)) => {
                if let Err(err) = self.load_resident(&pin) {
                    drv_os::say!("drv-agent: {app} (uid {uid}): {err}");
                }
            }
            Ok(None) => drv_os::say!("drv-agent: {app} (uid {uid}): the PIN was refused"),
            Err(err) => drv_os::say!("drv-agent: {err}"),
        }
    }

    /// How many keys ssh-agent holds.
    fn count_keys(&self) -> io::Result<u32> {
        let mut agent = UnixStream::connect(&self.private)?;
        write_message(&mut agent, &[SSH_AGENTC_REQUEST_IDENTITIES])?;
        let reply =
            read_message(&mut agent)?.ok_or_else(|| io::Error::other("ssh-agent hung up"))?;
        match reply.split_first() {
            Some((&SSH_AGENT_IDENTITIES_ANSWER, rest)) if rest.len() >= 4 => {
                Ok(u32::from_be_bytes([rest[0], rest[1], rest[2], rest[3]]))
            }
            _ => Err(io::Error::other("no identities answer")),
        }
    }

    /// `ssh-add -K` against ssh-agent, the PIN through our own askpass.
    fn load_resident(&self, pin: &str) -> Result<(), String> {
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

    /// ssh-agent's askpass calls, one connection each: `kind\tprompt` on a line. A PIN
    /// (any kind but `none`) goes to the shell and the answer back as a line; a touch
    /// (`none`) is shown until ssh-agent ends the caller and the connection closes.
    fn serve_askpass(self: Arc<Self>, listener: UnixListener) {
        for conn in listener.incoming() {
            let Ok(conn) = conn else { continue };
            let door = self.clone();
            std::thread::spawn(move || door.askpass(conn));
        }
    }

    fn askpass(&self, conn: UnixStream) {
        let mut conn = io::BufReader::new(conn);
        let mut line = String::new();
        if conn.read_line(&mut line).is_err() {
            return;
        }
        let line = line.trim_end();
        let (kind, prompt) = line.split_once('\t').unwrap_or(("", line));
        let (app, uid) = self
            .signing
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_else(|| ("an app".to_owned(), 0));
        if kind == "none" {
            match self.shell.touch(&app, uid, prompt) {
                Ok(_up) => {
                    let _ = conn.get_mut().read(&mut [0u8; 1]);
                }
                Err(err) => drv_os::say!("drv-agent: {err}"),
            }
            return;
        }
        match self.shell.pin(&app, uid, prompt) {
            Ok(Some(pin)) => {
                let _ = writeln!(conn.get_mut(), "{pin}");
            }
            Ok(None) => drv_os::say!("drv-agent: {app} (uid {uid}): the PIN was refused"),
            Err(err) => drv_os::say!("drv-agent: {err}"),
        }
    }
}

/// An app's sign request, in flight: ssh-agent's prompts meanwhile are for it.
struct Signing<'a> {
    door: &'a Door,
    app: String,
    uid: u32,
}

impl<'a> Signing<'a> {
    fn start(door: &'a Door, app: &str, uid: u32) -> Self {
        door.signing.lock().unwrap().push((app.to_owned(), uid));
        Self {
            door,
            app: app.to_owned(),
            uid,
        }
    }
}

impl Drop for Signing<'_> {
    fn drop(&mut self) {
        let mut signing = self.door.signing.lock().unwrap();
        if let Some(i) = signing
            .iter()
            .rposition(|(a, u)| *a == self.app && *u == self.uid)
        {
            signing.remove(i);
        }
    }
}

/// An authenticator we may open: a hidraw node udev gave our group.
fn authenticator_present() -> bool {
    let Ok(dev) = std::fs::read_dir("/dev") else {
        return false;
    };
    dev.flatten().any(|e| {
        e.file_name().to_string_lossy().starts_with("hidraw")
            && rustix::fs::access(
                e.path(),
                rustix::fs::Access::READ_OK | rustix::fs::Access::WRITE_OK,
            )
            .is_ok()
    })
}

/// The askpass end: ssh-agent's prompt to the door, its answer to stdout. A touch (`none`)
/// has no answer: we stay until ssh-agent ends us, and the door sees the socket close.
fn askpass(sock: &Path, kind: &str, prompt: &str) -> Result<(), String> {
    let door = UnixStream::connect(sock).map_err(|e| format!("{}: {e}", sock.display()))?;
    let mut door = io::BufReader::new(door);
    let prompt = prompt.replace(['\t', '\n', '\r'], " ");
    writeln!(door.get_mut(), "{kind}\t{prompt}").map_err(|e| format!("door: {e}"))?;
    let mut line = String::new();
    door.read_line(&mut line)
        .map_err(|e| format!("door: {e}"))?;
    if kind == "none" {
        return Ok(());
    }
    if line.is_empty() {
        return Err("refused".to_owned());
    }
    print!("{line}");
    Ok(())
}

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
