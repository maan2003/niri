//! Agent-facing CLI. Input and screenshots are handled by the compositor process.
use std::ffi::OsString;
use std::io::{BufRead, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use clap::Subcommand;
use rho_desktop_proto::{Input, Request, Response, MAX_HEADER, VERSION};
const DEFAULT_OUTPUT_WIDTH: u32 = 2560;
const DEFAULT_OUTPUT_HEIGHT: u32 = 1664;
const DEFAULT_OUTPUT_SCALE: u32 = 2;
#[derive(Clone, clap::Args)]
pub struct WaylandArgs {
    /// Desktop name, scoped to the invoking agent.
    #[arg(long, id = "desktop_session", global = true, default_value = "default")]
    session: String,

    /// Directory containing driver sessions.
    #[arg(long, global = true)]
    state_dir: Option<PathBuf>,

    #[command(subcommand)]
    command: DriverCommand,
}

#[derive(Clone, Subcommand)]
enum DriverCommand {
    /// Start a headless compositor, optionally followed by an application.
    Start {
        #[arg(long, default_value_t = DEFAULT_OUTPUT_WIDTH)]
        width: u32,
        #[arg(long, default_value_t = DEFAULT_OUTPUT_HEIGHT)]
        height: u32,
        #[arg(long, default_value_t = DEFAULT_OUTPUT_SCALE)]
        scale: u32,
        /// Application and arguments to launch in the Wayland session.
        #[arg(last = true)]
        command: Vec<OsString>,
    },
    /// Report whether the compositor and application are still running.
    Status,
    /// Print the compositor’s JSON window list.
    Tree,
    /// Capture the virtual output as a PNG.
    Screenshot {
        #[arg(short, long)]
        output: PathBuf,
    },
    /// Move the pointer to absolute output coordinates.
    Move { x: i32, y: i32 },
    /// Move the pointer and click a mouse button.
    Click {
        x: i32,
        y: i32,
        #[arg(long, value_enum, default_value_t = MouseButton::Left)]
        button: MouseButton,
    },
    /// Type literal text through the virtual keyboard protocol.
    Type { text: String },
    /// Run key, text, and wait steps through one persistent virtual keyboard.
    /// Steps use `key:CHORD`, `text:TEXT`, `down:MODIFIER`, `up:MODIFIER`,
    /// or `wait:MILLISECONDS`. `down:`/`up:` hold a modifier across the
    /// steps between them, which is how a held `shift` is driven.
    Input { steps: Vec<String> },
    /// Send keys or chords in one keyboard session, for example `enter`,
    /// `ctrl+shift+p`, or `escape g g`.
    Key { chord: String },
    /// Name the drive that follows. Every step after this line is counted
    /// against this name, so a report can say which recipe produced it.
    Drive { name: String },
    /// Stop the application and compositor and remove the session directory.
    Stop,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    /// The `linux/input-event-codes.h` code the virtual pointer protocol
    /// wants. Sway's own names (`button1`) belong to its ipc, which is not
    /// how a click reaches a client here.
    fn code(self) -> u32 {
        match self {
            Self::Left => 0x110,
            Self::Right => 0x111,
            Self::Middle => 0x112,
        }
    }
}

pub fn run(args: WaylandArgs) -> Result<()> {
    ensure!(
        !args.session.is_empty()
            && args
                .session
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "invalid session name"
    );
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is required")?;
    let base = args.state_dir.unwrap_or(super::desktop_directory(
        PathBuf::from(runtime),
        std::env::var("RHO_AGENT_ID").ok().as_deref(),
    )?);
    std::fs::create_dir_all(&base)?;
    std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700))?;
    let manifest = base.join(format!("{}.json", args.session));
    if let DriverCommand::Start {
        width,
        height,
        scale,
        command,
    } = &args.command
    {
        ensure!(
            (1..=4096).contains(width) && (1..=4096).contains(height) && (1..=4).contains(scale),
            "invalid desktop geometry"
        );
        let root = base.join(&args.session);
        std::fs::create_dir_all(&root)?;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
        let config = root.join("config.kdl");
        std::fs::write(&config, include_str!("../../resources/agent-desktop.kdl"))?;
        let log = std::fs::File::create(base.join(format!("{}-desktop.log", args.session)))?;
        let mut child = Command::new(std::env::current_exe()?)
            .env("RHO_DESKTOP_STATE_DIR", &base)
            .args([
                "--headless",
                "--name",
                &args.session,
                "--width",
                &width.to_string(),
                "--height",
                &height.to_string(),
                "--scale",
                &scale.to_string(),
                "--config",
            ])
            .arg(config)
            .arg("--")
            .args(command)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(log)
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait()? {
                anyhow::bail!("desktop exited: {status}; see {}-desktop.log", args.session);
            }
            if let Ok(bytes) = std::fs::read(&manifest) {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    if value["pid"].as_u64() == Some(child.id() as u64) {
                        break;
                    }
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("desktop startup timed out");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        println!("{}", std::fs::read_to_string(manifest)?);
        return Ok(());
    }
    let descriptor: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&manifest).context("desktop session is not running")?,
    )?;
    if let DriverCommand::Drive { name } = &args.command {
        log(&base, &args.session, "drive", name);
        return Ok(());
    }
    let address = descriptor["socket"]
        .as_str()
        .context("missing desktop socket")?;
    let stream = super::connect_local(address)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut stream = std::io::BufReader::new(stream);
    let Response::Hello {
        version: VERSION,
        outputs,
        ..
    } = request(&mut stream, Request::Hello { version: VERSION })?
    else {
        anyhow::bail!("desktop version mismatch")
    };
    let output = outputs.first().context("desktop has no output")?;
    let name = output.name.clone();
    let move_to = |x: i32, y: i32| -> Result<Input> {
        ensure!(
            x >= 0 && y >= 0 && (x as u32) < output.width && (y as u32) < output.height,
            "coordinates outside desktop output"
        );
        Ok(Input::Move {
            x: x as u32,
            y: y as u32,
        })
    };
    log(
        &base,
        &args.session,
        "step",
        &format!("{:?}", std::env::args().skip(1).collect::<Vec<_>>()),
    );
    match args.command {
        DriverCommand::Start { .. } | DriverCommand::Drive { .. } => unreachable!(),
        DriverCommand::Status => println!(
            "{}",
            serde_json::json!({"session":args.session,"compositor_running":true,"output":output,"video":request(&mut stream,Request::Status)?})
        ),
        DriverCommand::Tree | DriverCommand::Stop => {
            let ipc = descriptor["ipc_socket"]
                .as_str()
                .context("missing compositor control socket")?;
            let mut socket = niri_ipc::socket::Socket::connect_to(ipc)?;
            let request = if matches!(args.command, DriverCommand::Stop) {
                niri_ipc::Request::Action(niri_ipc::Action::Quit {
                    skip_confirmation: true,
                })
            } else {
                niri_ipc::Request::Windows
            };
            let reply = match socket.send(request) {
                Ok(reply) => reply.map_err(anyhow::Error::msg)?,
                Err(error) if matches!(args.command, DriverCommand::Stop) => {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while super::connect_local(address).is_ok() {
                        if Instant::now() >= deadline {
                            return Err(error.into());
                        }
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    println!("{{\"stopped\":true}}");
                    return Ok(());
                }
                Err(error) => return Err(error.into()),
            };
            println!("{}", serde_json::to_string(&reply)?);
            if matches!(args.command, DriverCommand::Stop) {
                return Ok(());
            }
        }
        DriverCommand::Screenshot { output } => {
            super::save_capture(Some(address.into()), name, output.clone())?;
            println!(
                "{}",
                serde_json::json!({"session":args.session,"output":output})
            );
        }
        DriverCommand::Move { x, y } => send(&mut stream, move_to(x, y)?)?,
        DriverCommand::Click { x, y, button } => {
            send(&mut stream, move_to(x, y)?)?;
            std::thread::sleep(Duration::from_millis(40));
            send(
                &mut stream,
                Input::Button {
                    button: button.code(),
                    pressed: true,
                },
            )?;
            std::thread::sleep(Duration::from_millis(40));
            send(
                &mut stream,
                Input::Button {
                    button: button.code(),
                    pressed: false,
                },
            )?;
        }
        DriverCommand::Type { text } => send(&mut stream, Input::Text(text))?,
        DriverCommand::Key { chord } => {
            for chord in chord.split_ascii_whitespace() {
                send(&mut stream, Input::Key(chord.to_owned()))?;
                std::thread::sleep(Duration::from_millis(30));
            }
        }
        DriverCommand::Input { steps } => {
            for step in steps {
                let (kind, value) = step
                    .split_once(':')
                    .context("expected key:, text:, down:, up:, or wait:")?;
                match kind {
                    "wait" => std::thread::sleep(Duration::from_millis(value.parse()?)),
                    "text" => send(&mut stream, Input::Text(value.into()))?,
                    "key" => send(&mut stream, Input::Key(value.into()))?,
                    "down" | "up" => {
                        let code = match value {
                            "ctrl" | "control" => 29,
                            "shift" => 42,
                            "alt" => 56,
                            "super" | "logo" => 125,
                            _ => anyhow::bail!("unknown modifier"),
                        };
                        send(
                            &mut stream,
                            Input::Physical {
                                code,
                                pressed: kind == "down",
                            },
                        )?;
                    }
                    _ => anyhow::bail!("unknown input step"),
                }
            }
        }
    }
    send(&mut stream, Input::ReleaseAll)?;
    Ok(())
}
fn request(stream: &mut std::io::BufReader<UnixStream>, request: Request) -> Result<Response> {
    let mut bytes = serde_json::to_vec(&request)?;
    bytes.push(b'\n');
    stream.get_mut().write_all(&bytes)?;
    let mut bytes = Vec::new();
    stream.take(MAX_HEADER).read_until(b'\n', &mut bytes)?;
    ensure!(bytes.last() == Some(&b'\n'), "invalid desktop response");
    let response = serde_json::from_slice(&bytes)?;
    if let Response::Error { message } = response {
        anyhow::bail!("{message}")
    }
    Ok(response)
}
fn send(stream: &mut std::io::BufReader<UnixStream>, input: Input) -> Result<()> {
    ensure!(
        matches!(request(stream, Request::Input { input })?, Response::Done),
        "unexpected input reply"
    );
    Ok(())
}
fn log(base: &std::path::Path, session: &str, kind: &str, text: &str) {
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(base.join(format!("{session}-drive.log")))
    {
        let _ = writeln!(
            file,
            "{}",
            serde_json::json!({"kind":kind,"text":text,"at_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis()})
        );
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    #[test]
    fn driver_session_does_not_collide_with_compositor_session_flag() {
        let cli = crate::cli::Cli::try_parse_from([
            "rho-agent-desktop",
            "wayland",
            "--session",
            "browser",
            "key",
            "ctrl+l",
        ])
        .unwrap();
        let crate::cli::Sub::Wayland(args) = cli.subcommand.unwrap() else {
            panic!("wrong command")
        };
        assert_eq!(args.session, "browser");
        assert!(matches!(args.command,DriverCommand::Key {chord} if chord=="ctrl+l"));
    }
}
