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
    /// Brightness step, in percent of the perceived level (see `brightness_step`).
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
            "volume up" => step_volume(&args, &["set-volume", "-l", "1.0", "@DEFAULT_AUDIO_SINK@"], Some(format!("{}%+", args.volume_step))),
            "volume down" => step_volume(&args, &["set-volume", "@DEFAULT_AUDIO_SINK@"], Some(format!("{}%-", args.volume_step))),
            "volume mute" => volume(&args, &["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"], None).map(|()| None),
            "mic mute" => volume(&args, &["set-mute", "@DEFAULT_AUDIO_SOURCE@", "toggle"], None).map(|()| None),
            "brightness up" => brightness(&args, 1).map(|()| None),
            "brightness down" => brightness(&args, -1).map(|()| None),
            other => Err(format!("not a key: {other:?}")),
        };
        match result {
            Ok(Some(change)) => drv_os::say!("drv-keys: {line}: {change}"),
            Ok(None) => drv_os::say!("drv-keys: {line}"),
            Err(err) => drv_os::say!("drv-keys: {line}: {err}"),
        }
    }
    // The compositor closed its end: the set is going down.
}

/// A volume step, and what the sink says before and after (`0.50 -> 0.45`): a sink whose
/// device sets the volume itself (Bluetooth's absolute volume) may answer with another.
fn step_volume(args: &Args, wpctl_args: &[&str], step: Option<String>) -> Result<Option<String>, String> {
    let before = get_volume(args);
    volume(args, wpctl_args, step)?;
    let after = get_volume(args);
    Ok(Some(format!("{before} -> {after}")))
}

/// `wpctl get-volume`'s answer for the default sink (`Volume: 0.45`, maybe `[MUTED]`).
fn get_volume(args: &Args) -> String {
    let Some(wpctl) = args.wpctl.as_ref() else { return "?".to_owned() };
    match Command::new(wpctl).args(["get-volume", "@DEFAULT_AUDIO_SINK@"]).output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .trim()
            .trim_start_matches("Volume: ")
            .to_owned(),
        Ok(out) => format!("? ({})", out.status),
        Err(e) => format!("? ({e})"),
    }
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
        let want = brightness_step(now, max, args.brightness_step, args.brightness_floor, direction);
        std::fs::write(panel.join("brightness"), want.to_string())
            .map_err(|e| format!("{}: {e}", panel.display()))?;
    }
    if seen { Ok(()) } else { Err("no backlight".to_owned()) }
}

/// Brightness is perceived roughly as a power of the panel's value (brightnessctl's `-e`):
/// the step is in percent of `(now / max) ^ (1 / 4)`, so the dark end gets small steps and
/// the bright end large ones. Each press moves at least one raw unit, never below the floor.
const EXPONENT: f64 = 4.0;

fn brightness_step(now: u64, max: u64, step: u64, floor: u64, direction: i64) -> u64 {
    if max == 0 {
        return 0;
    }
    let floor = floor.min(max);
    let level = (now.min(max) as f64 / max as f64).powf(1.0 / EXPONENT);
    let level = (level + direction.signum() as f64 * step as f64 / 100.0).clamp(0.0, 1.0);
    let mut want = (level.powf(EXPONENT) * max as f64).round() as u64;
    if direction > 0 && want <= now {
        want = now + 1;
    } else if direction < 0 && want >= now {
        want = now.saturating_sub(1);
    }
    want.clamp(floor, max)
}

#[cfg(test)]
mod tests {
    use super::brightness_step;

    #[test]
    fn steps_are_exponential() {
        // max 500 (the M2 Air's panel): 5% of the perceived level from full is 500*0.95^4.
        assert_eq!(brightness_step(500, 500, 5, 2, -1), 407);
        // From the dark end the step is small, but at least one unit.
        assert_eq!(brightness_step(2, 500, 5, 2, 1), 4);
        assert_eq!(brightness_step(3, 500, 5, 2, -1), 2);
        // Never below the floor, never above max.
        assert_eq!(brightness_step(2, 500, 5, 2, -1), 2);
        assert_eq!(brightness_step(499, 500, 5, 2, 1), 500);
        // Up then down comes back to where it started (away from the floor).
        let up = brightness_step(100, 500, 5, 2, 1);
        assert_eq!(brightness_step(up, 500, 5, 2, -1), 100);
    }
}

fn read_number(path: &Path) -> Result<u64, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    text.trim()
        .parse()
        .map_err(|e| format!("{}: {text:?}: {e}", path.display()))
}
