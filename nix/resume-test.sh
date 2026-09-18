#!/usr/bin/env bash
# Does the kernel hand the old picture back on resume? Runs against a booted resume-vm
# (`nix build .#resume-vm && ./result/bin/run-nixos-vm`, monitor at /tmp/niri-vm/resume-monitor,
# ssh on port 2223). Puts modetest's colour bars on screen, suspends, wakes, and dumps the
# guest display right after wake, once with blank_on_resume off and once with it on.
# Expect: off -> bars come back on their own; on -> black until modetest is started again.
set -euo pipefail
SSH="ssh -p 2223 -i /tmp/niri-vm/id_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null root@127.0.0.1"
mon() { printf '%s\n' "$1" | socat - UNIX-CONNECT:/tmp/niri-vm/resume-monitor | tr -d '\r' | grep -v '^QEMU\|^(qemu)' || true; }
conn() { $SSH "modetest -M qxl -c | awk '/connected/ && \$3==\"connected\" {print \$1\"@\"\$4\":\"\$6; exit}' | sed 's/,//g'"; }

run_once() {
  local mode=$1
  $SSH "echo $mode > /sys/module/drm_kms_helper/parameters/blank_on_resume; pkill modetest || true; sleep 0.5; (sleep 1000 | setsid modetest -M qxl -s $(conn) >/dev/null 2>&1 &) ; sleep 1"
  mon "screendump /tmp/niri-vm/resume-before-$mode.ppm" >/dev/null
  $SSH "echo mem > /sys/power/state" &
  for _ in $(seq 1 50); do mon "info status" | grep -q suspended && break; sleep 0.2; done
  mon "system_wakeup" >/dev/null
  sleep 0.3; mon "screendump /tmp/niri-vm/resume-after-$mode-0.ppm" >/dev/null
  sleep 2;   mon "screendump /tmp/niri-vm/resume-after-$mode-2.ppm" >/dev/null
  wait
}

run_once 0
run_once 1
echo "dumps in /tmp/niri-vm/resume-*.ppm"
