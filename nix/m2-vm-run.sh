#!/usr/bin/env bash
# Runs the M2 development VM (nix/m2-vm.nix) on the Asahi host as an ordinary user. Builds
# the microvm.nix runner for aarch64 (niri from nix/m2-prebuilt, see nix/cross-drv.sh), then:
# sway on wlroots' headless backend (the guest's window lives there), wayvnc on it, noVNC on
# http://<host>:6080/vnc.html?autoconnect=1, virtiofsd for the store and the control share,
# and crosvm under a pty with its console in vm.log. State in $M2VM (default ~/m2vm).
# `nix/m2-vm-run.sh stop` ends it all. Talk to the guest with nix/m2-vm-exec.
set -euo pipefail
M2VM=${M2VM:-$HOME/m2vm}
# Every mapped GPU blob costs crosvm a descriptor; a login shell's 1024 runs out within minutes.
ulimit -n "$(ulimit -Hn)" 2>/dev/null || ulimit -n 65536
SRC=$(cd "$(dirname "$0")/.." && pwd)
W=1280; H=832
mkdir -p "$M2VM/share/cmd" "$M2VM/share/out" "$M2VM/share/done"
cd "$M2VM"
stop() {
  pkill -f 'crosvm run' 2>/dev/null || true
  pkill -x virtiofsd 2>/dev/null || true; pkill -f virtiofsd-run 2>/dev/null || true
  pkill -x wayvnc 2>/dev/null || true; pkill -f websockify 2>/dev/null || true
  pkill -f 'sway -c' 2>/dev/null || true
}
if [ "${1:-}" = stop ]; then stop; exit 0; fi
# The guest reuses the host's kernel (nix/m2-vm.nix); nothing heavy may be compiled here.
plan=$(nix build --impure --dry-run "path:$SRC#packages.aarch64-linux.m2-vm" 2>&1 >/dev/null || true)
if echo "$plan" | grep -qE -- '-linux-[0-9]|-linux-asahi-[0-9.]+\.drv|-linux-config-|-virglrenderer-|-crosvm-[0-9]|-mesa-[0-9]|-llvm-'; then
  echo "refusing to build: the plan compiles heavy things:"; echo "$plan"; exit 1
fi
nix build --impure -o "$M2VM/result" "path:$SRC#packages.aarch64-linux.m2-vm"
nix build -o "$M2VM/tools" --print-out-paths nixpkgs#sway nixpkgs#wayvnc nixpkgs#novnc nixpkgs#python3Packages.websockify nixpkgs#grim > tools.txt
SWAY=$(grep -- -sway- tools.txt); WAYVNC=$(grep -- -wayvnc- tools.txt); NOVNC=$(grep -- -novnc- tools.txt); WS=$(grep -- -websockify- tools.txt)
stop; sleep 0.5
rm -f "$M2VM"/*.sock
export XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}
cat > sway.conf <<EOF
output HEADLESS-1 resolution ${W}x${H} position 0 0
default_border none
focus_follows_mouse yes
for_window [app_id=".*"] fullscreen enable
EOF
: > sway.log
socks() { for f in "$XDG_RUNTIME_DIR"/wayland-[0-9]*; do case $f in *.lock) ;; *) [ -S "$f" ] && basename "$f";; esac; done; return 0; }
before=$(socks)
WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1 WLR_RENDERER=gles2 \
  nohup "$SWAY/bin/sway" -c sway.conf > sway.log 2>&1 &
for _ in $(seq 1 50); do s=$(comm -13 <(echo "$before") <(socks) | head -1); [ -n "$s" ] && break; sleep 0.2; done
[ -n "${s:-}" ] || { echo "sway did not come up:"; tail sway.log; exit 1; }
printf '%s/%s' "$XDG_RUNTIME_DIR" "$s" > wayland-sock
export WAYLAND_DISPLAY=$s
nohup "$WAYVNC/bin/wayvnc" 127.0.0.1 5900 > wayvnc.log 2>&1 &
nohup "$WS/bin/websockify" --web "$NOVNC/share/webapps/novnc" 0.0.0.0:6080 127.0.0.1:5900 > novnc.log 2>&1 &
# microvm's virtiofsd-run is a root-only supervisord; run its two virtiofsd programs directly,
# minus the socket-group chown (we are not in kvm).
: > virtiofsd.log
conf=$(grep -o '/nix/store/[^ ]*supervisord.conf' result/bin/virtiofsd-run)
for prog in $(grep '^command=' "$conf" | grep -- -virtiofsd- | cut -d= -f2-); do
  sed '/--socket-group/d' "$prog" | nohup bash -s >> virtiofsd.log 2>&1 &
done
for _ in $(seq 1 50); do [ "$(ls -- *-virtiofs-*.sock 2>/dev/null | wc -l)" -ge 2 ] && break; sleep 0.2; done
ls -- *.sock
: > vm.log
nohup script -qfc ./result/bin/microvm-run vm.log > /dev/null 2>&1 < /dev/null &
echo "started: display $WAYLAND_DISPLAY, noVNC on port 6080, console in $M2VM/vm.log"
