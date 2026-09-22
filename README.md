# rho-agent-desktop

Rho's separately built agent desktop, hard-forked from niri. This is a Rho
component, not a compatibility layer around an upstream compositor. Internal
niri names and its useful CLI remain; upstream merge compatibility is not a
design constraint. The fork retains GPL-3.0-or-later licensing.

## Run a headless desktop

Build with the native dependencies listed in the upstream development docs,
or enter `nix develop`:

```sh
cargo build --locked --no-default-features --bin rho-agent-desktop
runtime=$(mktemp -d)
chmod 700 "$runtime"
XDG_RUNTIME_DIR="$runtime" target/debug/rho-agent-desktop \
  --headless --width 2560 --height 1664 --scale 2 \
  --config resources/agent-desktop.kdl -- YOUR_APPLICATION
```

Surfaceless EGL is required; Mesa software rendering works without a GPU.
`--headless` never selects a host Wayland/X11 display or DRM backend.
The private runtime directory is required. The compositor exports
`WAYLAND_DISPLAY`, `NIRI_SOCKET`, and `RHO_DESKTOP_SOCKET` to launched children;
it also logs the socket paths for external clients.

The inherited compositor CLI is available in this executable:

```sh
NIRI_SOCKET=/path/from/log rho-agent-desktop msg --json windows
NIRI_SOCKET=/path/from/log rho-agent-desktop msg action maximize-column
RHO_DESKTOP_SOCKET=@abstract-name-from-log rho-agent-desktop capture screenshot.png
```

The control endpoint supports version negotiation, output discovery and
on-demand lossless BGRA screenshots. `capture` converts those pixels to PNG on
the client. Protocol types and framing documentation live in
[`rho-desktop-proto`](https://github.com/maan2003/rho/tree/rho/agent-desktop/crates/rho-desktop-proto),
pinned by Git revision in Cargo.toml. The protocol crate has no compositor, codec, Rho daemon, or GUI dependencies.

Applications are **not sandboxed**. Possession of the local desktop socket
grants application control and screenshot access. Abstract sockets check peer UIDs; session descriptors live in a private runtime
directory. Headers, dimensions, and simultaneous connections are bounded.

## Live MoQ desktop

VP9 Profile 1 (8-bit 4:4:4, screen-content tuning, no lookahead) runs **inside
this compositor process**, on a subscription-owned encoder thread. Composition
is damage-driven and capped at 30 fps. Static output gets one quality refinement
and goes idle. With no subscribers, no video images are composed or encoded.
There is no external screenshot/encoding process.

The daemon discovers the named session through its workset worker, then connects
directly to this process. Control and MoQ use same-user abstract Unix sockets
across the filesystem namespace; the worker never relays media. The daemon
relays MoQ tracks onto the GUI's single existing authenticated Iroh connection.
Codec and compositor implementation are not daemon dependencies.

```sh
rho-agent-desktop wayland --session browser start -- YOUR_APPLICATION
rho-agent-desktop wayland --session browser status
rho-agent-desktop wayland --session browser click 500 300
rho-agent-desktop wayland --session browser type 'hello'
rho-agent-desktop wayland --session browser key ctrl+l
rho-agent-desktop wayland --session browser screenshot --output browser.png
rho-agent-desktop wayland --session browser stop
```

`rho wayland` forwards these commands. `tree`, `move`, `input`, and `drive` also
remain available; `tree` now reports niri's window list rather than Sway's tree.
Session descriptors are in `$XDG_RUNTIME_DIR/rho-desktop/NAME.json`.
The direct `--headless` entry point accepts `--name NAME` (default: `default`).
Peer credentials restrict both abstract sockets to the same host user. The
compositor's inherited IPC and CLI are still available for window management.

Open the desktop from the running Rho GUI’s agent menu (`w`), then enter the
session name. It opens as a full-window overlay; closing it returns to the agent.
No separate viewer process is needed.

The native viewer handles pointer, keyboard, and scrolling. Input goes directly
through the compositor's input handling; held keys/buttons release when the
control connection closes or the viewer loses focus. Annotation freezes the
presented frame on the client, supports drawing/undo/copy, and can add a PNG to
the selected agent's prompt. No annotation protocol is required.

Validation:

```sh
cargo test --locked --no-default-features --lib
python3 tests/desktop_smoke.py target/debug/rho-agent-desktop
```

The Rho repo's `wayland_stream` integration test starts a real daemon and desktop,
decodes VP9, tests static idleness, late joins, independent viewer teardown, and
RPC after detach. Set `RHO_AGENT_DESKTOP_BIN` to this executable. GUI decoding and
software encoding require libvpx >= 1.15. The Nix development shell supplies it.

---

## Upstream background

<h1 align="center"><img alt="niri" src="https://github.com/user-attachments/assets/07d05cd0-d5dc-4a28-9a35-51bae8f119a0"></h1>
<p align="center">A scrollable-tiling Wayland compositor.</p>
<p align="center">
    <a href="https://matrix.to/#/#niri:matrix.org"><img alt="Matrix" src="https://img.shields.io/badge/matrix-%23niri-blue?logo=matrix"></a>
    <a href="https://github.com/YaLTeR/niri/blob/main/LICENSE"><img alt="GitHub License" src="https://img.shields.io/github/license/YaLTeR/niri"></a>
    <a href="https://github.com/YaLTeR/niri/releases"><img alt="GitHub Release" src="https://img.shields.io/github/v/release/YaLTeR/niri?logo=github"></a>
</p>

<p align="center">
    <a href="https://yalter.github.io/niri/Getting-Started.html">Getting Started</a> | <a href="https://yalter.github.io/niri/Configuration%3A-Introduction.html">Configuration</a> | <a href="https://github.com/YaLTeR/niri/discussions/325">Setup&nbsp;Showcase</a>
</p>

![niri with a few windows open](https://github.com/user-attachments/assets/535e6530-2f44-4b84-a883-1240a3eee6e9)

## About

Windows are arranged in columns on an infinite strip going to the right.
Opening a new window never causes existing windows to resize.

Every monitor has its own separate window strip.
Windows can never "overflow" onto an adjacent monitor.

Workspaces are dynamic and arranged vertically.
Every monitor has an independent set of workspaces, and there's always one empty workspace present all the way down.

The workspace arrangement is preserved across disconnecting and connecting monitors where it makes sense.
When a monitor disconnects, its workspaces will move to another monitor, but upon reconnection they will move back to the original monitor.

## Features

- Built from the ground up for scrollable tiling
- [Dynamic workspaces](https://yalter.github.io/niri/Workspaces.html) like in GNOME
- An [Overview](https://github.com/user-attachments/assets/379a5d1f-acdb-4c11-b36c-e85fd91f0995) that zooms out workspaces and windows
- Built-in screenshot UI
- Monitor and window screencasting through xdg-desktop-portal-gnome
    - You can [block out](https://yalter.github.io/niri/Configuration%3A-Window-Rules.html#block-out-from) sensitive windows from screencasts
    - [Dynamic cast target](https://yalter.github.io/niri/Screencasting.html#dynamic-screencast-target) that can change what it shows on the go
- [Touchpad](https://github.com/YaLTeR/niri/assets/1794388/946a910e-9bec-4cd1-a923-4a9421707515) and [mouse](https://github.com/YaLTeR/niri/assets/1794388/8464e65d-4bf2-44fa-8c8e-5883355bd000) gestures
- Group windows into [tabs](https://yalter.github.io/niri/Tabs.html)
- Configurable layout: gaps, borders, struts, window sizes
- [Gradient borders](https://yalter.github.io/niri/Configuration%3A-Layout.html#gradients) with Oklab and Oklch support
- [Animations](https://github.com/YaLTeR/niri/assets/1794388/ce178da2-af9e-4c51-876f-8709c241d95e) with support for [custom shaders](https://github.com/YaLTeR/niri/assets/1794388/27a238d6-0a22-4692-b794-30dc7a626fad)
- Live-reloading config
- Works with [screen readers](https://yalter.github.io/niri/Accessibility.html)

## Video Demo

https://github.com/YaLTeR/niri/assets/1794388/bce834b0-f205-434e-a027-b373495f9729

Also check out this video from Brodie Robertson that showcases a lot of the niri functionality: [Niri Is My New Favorite Wayland Compositor](https://youtu.be/DeYx2exm04M)

## Status

Niri is stable for day-to-day use and does most things expected of a Wayland compositor.
Many people are daily-driving niri, and are happy to help in our [Matrix channel].

Give it a try!
Follow the instructions on the [Getting Started](https://yalter.github.io/niri/Getting-Started.html) page.
Have your [waybar]s and [fuzzel]s ready: niri is not a complete desktop environment.
Also check out [awesome-niri], a list of niri-related links and projects.

Here are some points you may have questions about:

- **Multi-monitor**: yes, a core part of the design from the very start. Mixed DPI works.
- **Fractional scaling**: yes, plus all niri UI stays pixel-perfect.
- **NVIDIA**: seems to work fine.
- **Floating windows**: yes, starting from niri 25.01.
- **Input devices**: niri supports tablets, touchpads, and touchscreens.
You can map the tablet to a specific monitor, or use [OpenTabletDriver].
We have touchpad gestures, but no touchscreen gestures yet.
- **Wlr protocols**: yes, we have most of the important ones like layer-shell, gamma-control, screencopy.
You can check on [wayland.app](https://wayland.app) at the bottom of each protocol's page.
- **Performance**: while I run niri on beefy machines, I try to stay conscious of performance.
I've seen someone use it fine on an Eee PC 900 from 2008, of all things.
- **Xwayland**: [integrated](https://yalter.github.io/niri/Xwayland.html#using-xwayland-satellite) via xwayland-satellite starting from niri 25.08.

## Media

[niri: Making a Wayland compositor in Rust](https://youtu.be/Kmz8ODolnDg?list=PLRdS-n5seLRqrmWDQY4KDqtRMfIwU0U3T) · *December 2024*

My talk from the 2024 Moscow RustCon about niri, and how I do randomized property testing and profiling, and measure input latency.
The talk is in Russian, but I prepared full English subtitles that you can find in YouTube's subtitle language selector.

[An interview with Ivan, the developer behind Niri](https://www.trommelspeicher.de/podcast/special_the_developer_behind_niri) · *June 2025*

An interview by a German tech podcast Das Triumvirat (in English).
We talk about niri development and history, and my experience building and maintaining niri.

[A tour of the niri scrolling-tiling Wayland compositor](https://lwn.net/Articles/1025866/) · *July 2025*

An LWN article with a nice overview and introduction to niri.

## Contributing

If you'd like to help with niri, there are plenty of both coding- and non-coding-related ways to do so.
See [CONTRIBUTING.md](https://github.com/YaLTeR/niri/blob/main/CONTRIBUTING.md) for an overview.

## Inspiration

Niri is heavily inspired by [PaperWM] which implements scrollable tiling on top of GNOME Shell.

One of the reasons that prompted me to try writing my own compositor is being able to properly separate the monitors.
Being a GNOME Shell extension, PaperWM has to work against Shell's global window coordinate space to prevent windows from overflowing.

## Tile Scrollably Elsewhere

Here are some other projects which implement a similar workflow:

- [PaperWM]: scrollable tiling on top of GNOME Shell.
- [karousel]: scrollable tiling on top of KDE.
- [scroll](https://github.com/dawsers/scroll) and [papersway]: scrollable tiling on top of sway/i3.
- [hyprscrolling] and [hyprslidr]: scrollable tiling on top of Hyprland.
- [PaperWM.spoon]: scrollable tiling on top of macOS.

## Contact

Our main communication channel is a Matrix chat, feel free to join and ask a question: https://matrix.to/#/#niri:matrix.org

We also have a community Discord server: https://discord.gg/vT8Sfjy7sx

[PaperWM]: https://github.com/paperwm/PaperWM
[waybar]: https://github.com/Alexays/Waybar
[fuzzel]: https://codeberg.org/dnkl/fuzzel
[awesome-niri]: https://github.com/Vortriz/awesome-niri
[karousel]: https://github.com/peterfajdiga/karousel
[papersway]: https://spwhitton.name/tech/code/papersway/
[hyprscrolling]: https://github.com/hyprwm/hyprland-plugins/tree/main/hyprscrolling
[hyprslidr]: https://gitlab.com/magus/hyprslidr
[PaperWM.spoon]: https://github.com/mogenson/PaperWM.spoon
[Matrix channel]: https://matrix.to/#/#niri:matrix.org
[OpenTabletDriver]: https://opentabletdriver.net/
