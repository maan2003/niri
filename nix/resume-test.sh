#!/usr/bin/env bash
# Does the kernel hand the old picture back on resume? Runs against a booted resume-vm
# (`nix build .#resume-vm && ./result/bin/run-nixos-vm`: monitor at /tmp/niri-vm/resume-monitor,
# ssh on port 2223, QEMU's QXL trace in /tmp/niri-vm/qxl-trace.log). Puts modetest's colour bars on
# screen, does ACPI S3, wakes the guest from the monitor and asks it which framebuffer the
# primary plane shows, once with blank_on_resume off and once on. QEMU keeps showing the
# last surface after the guest destroys it, so the screendumps are only a sanity check: the
# plane's fb and the trace are the evidence.
# Expect: off -> the plane keeps its fb and QXL got a new primary surface during S3 (the
# kernel replayed the picture); on -> crtc 0 fb 0, no new primary surface, no WARNING in
# dmesg, then a fresh modetest lights it again.
set -euo pipefail
TRACE=/tmp/niri-vm/qxl-trace.log
SSH="ssh -p 2223 -i /tmp/niri-vm/id_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR root@127.0.0.1"
mon() { printf '%s\n' "$1" | socat - UNIX-CONNECT:/tmp/niri-vm/resume-monitor | tr -d '\r' | grep -v '^QEMU\|^(qemu)' || true; }
conn() { $SSH "modetest -M qxl -c | awk '\$3==\"connected\"{c=\$1} c&&/^  #0 /{print c\":\"\$2; exit}'"; }
plane() { $SSH "modetest -M qxl -p | awk '/^Planes/{p=1;next} p&&/^[0-9]/{print \"plane\", \$1, \"crtc\", \$2, \"fb\", \$3; exit}'"; }
bars() { $SSH "pkill modetest || true; sleep 0.5; (sleep 1000 | setsid modetest -M qxl -s $(conn)) </dev/null >/dev/null 2>&1 & sleep 1"; }

run_once() {
  local mode=$1
  $SSH "echo $mode > /sys/module/drm_kms_helper/parameters/blank_on_resume; echo deep > /sys/power/mem_sleep; dmesg -C"
  bars
  echo "mode $mode before: $(plane)"
  mon "screendump /tmp/niri-vm/resume-before-$mode.ppm" >/dev/null
  local n0; n0=$(wc -l < "$TRACE")
  $SSH "echo mem > /sys/power/state" &
  for _ in $(seq 1 50); do mon "info status" | grep -q suspended && break; sleep 0.2; done
  sleep 3
  mon "system_wakeup" >/dev/null
  wait
  sleep 2
  echo "mode $mode after:  $(plane), dmesg: $($SSH "dmesg | grep -E 'WARNING|blank-on-resume' | wc -l") warnings"
  echo "  qxl during S3: $(tail -n +$((n0 + 1)) "$TRACE" | grep -c qxl_destroy_primary) destroy, $(tail -n +$((n0 + 1)) "$TRACE" | grep -c qxl_create_guest_primary) create primary"
  mon "screendump /tmp/niri-vm/resume-after-$mode.ppm" >/dev/null
}

run_once 0
run_once 1
bars
echo "relit:         $(plane)"
mon "screendump /tmp/niri-vm/resume-relit.ppm" >/dev/null
echo "relit qxl:     $(tail -n 1 "$TRACE")"
echo "dumps in /tmp/niri-vm/resume-*.ppm"
