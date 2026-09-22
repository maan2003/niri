//! The media keys, a supervisor service. The compositor writes a line per key down its wire
//! (fd `compositor`: `volume up`, `volume down`, `volume mute`, `mic mute`, `brightness up`,
//! `brightness down`) and this does the part neither it nor any app may: the default sink
//! and source through PipeWire (group pipewire, `/run/pipewire`) and the backlight through
//! sysfs (group video; the supervisor leaves this member its `/sys` writable). Nothing else
//! comes down the wire and nothing goes back up.

use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use clap::Parser;
use drv_os::fds::Kind;

#[derive(Parser)]
struct Args {
    /// wpctl, for the volume. Without it the volume keys do nothing.
    #[arg(long)]
    wpctl: Option<PathBuf>,
    /// Volume step, in percent.
    #[arg(long, default_value_t = 5)]
    volume_step: u32,
    /// Brightness step, in percent of the panel's maximum.
    #[arg(long, default_value_t = 5)]
    brightness_step: u64,
    /// The backlight never goes below this raw value: dark, not off.
    #[arg(long, default_value_t = 2)]
    brightness_floor: u64,
}

const BACKLIGHT: &str = "/sys/class/backlight";

fn main() {
    let args = Args::parse();
    let mut fds = match drv_os::fds::take() {
        Ok(fds) => fds,
        Err(err) => {
            drv_os::say!("drv-keys: the supervisor's fds: {err}");
            process::exit(1);
        }
    };
    let compositor = match fds.socket("compositor", Kind::Stream) {
        Ok(fd) => UnixStream::from(fd),
        Err(err) => {
            drv_os::say!("drv-keys: fd \"compositor\" from the supervisor: {err}");
            process::exit(1);
        }
    };
    for line in BufReader::new(compositor).lines() {
        let line = match line {
            Ok(line) => line,
            Err(err) => {
                drv_os::say!("drv-keys: the compositor's line: {err}");
                process::exit(1);
            }
        };
        let result = match line.as_str() {
            "volume up" => volume(&args, &["set-volume", "-l", "1.0", "@DEFAULT_AUDIO_SINK@"], Some(format!("{}%+", args.volume_step))),
            "volume down" => volume(&args, &["set-volume", "@DEFAULT_AUDIO_SINK@"], Some(format!("{}%-", args.volume_step))),
            "volume mute" => volume(&args, &["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"], None),
            "mic mute" => volume(&args, &["set-mute", "@DEFAULT_AUDIO_SOURCE@", "toggle"], None),
            "brightness up" => brightness(&args, 1),
            "brightness down" => brightness(&args, -1),
            other => Err(format!("not a key: {other:?}")),
        };
        match result {
            Ok(()) => drv_os::say!("drv-keys: {line}"),
            Err(err) => drv_os::say!("drv-keys: {line}: {err}"),
        }
    }
    // The compositor closed its end: the set is going down.
}

fn volume(args: &Args, wpctl_args: &[&str], step: Option<String>) -> Result<(), String> {
    let wpctl = args.wpctl.as_ref().ok_or("no wpctl")?;
    let mut cmd = Command::new(wpctl);
    cmd.args(wpctl_args);
    if let Some(step) = step {
        cmd.arg(step);
    }
    let out = cmd.output().map_err(|e| format!("{}: {e}", wpctl.display()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "wpctl: {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Every panel under `/sys/class/backlight`, one step up or down, clamped to the floor.
fn brightness(args: &Args, direction: i64) -> Result<(), String> {
    let panels = std::fs::read_dir(BACKLIGHT).map_err(|e| format!("{BACKLIGHT}: {e}"))?;
    let mut seen = false;
    for panel in panels {
        let panel = panel.map_err(|e| format!("{BACKLIGHT}: {e}"))?.path();
        seen = true;
        let max = read_number(&panel.join("max_brightness"))?;
        let now = read_number(&panel.join("brightness"))?;
        let step = (max * args.brightness_step / 100).max(1);
        let want = if direction > 0 {
            now.saturating_add(step).min(max)
        } else {
            now.saturating_sub(step).max(args.brightness_floor.min(max))
        };
        std::fs::write(panel.join("brightness"), want.to_string())
            .map_err(|e| format!("{}: {e}", panel.display()))?;
    }
    if seen { Ok(()) } else { Err("no backlight".to_owned()) }
}

fn read_number(path: &Path) -> Result<u64, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.trim()
        .parse()
        .map_err(|e| format!("{}: {text:?}: {e}", path.display()))
}
