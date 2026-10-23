#!/usr/bin/env bash
# Run only inside the isolated M2 virtio GPU VM as root.
# Require NIRI_TEST_CLIENT (executable or ELF-loader wrapper), NIRI_TEST_CLIENT_UID,
# NIRI_TEST_DRM_NODE, WAYLAND_DISPLAY, and XDG_RUNTIME_DIR. The caller supplies
# the client driver/library environment; this harness inherits it unchanged.
# NIRI_TEST_EXPECT_RELEASE=early proves the stock defect; =deferred checks the fix.
# Set NIRI_TEST_PIXEL_GATE=none for event-only checks. For stable screenshot capture,
# set /host/UNIQUE: after repaint gating, the core is stopped and worker resumed;
# wait for UNIQUE.ready in the host share, capture, then create UNIQUE.continue.
# Strace proves queued Present BEFORE the replacing commit. Cleanup resumes both.
set -euo pipefail
client_executable=${NIRI_TEST_CLIENT:?set the built C client executable or wrapper}
client_uid=${NIRI_TEST_CLIENT_UID:?set the compositor-authorized client UID}
drm_node=${NIRI_TEST_DRM_NODE:?set the VM DRM render node}
: "${WAYLAND_DISPLAY:?set the VM compositor socket}"
: "${XDG_RUNTIME_DIR:?set the client runtime directory}"
command -v "$client_executable" >/dev/null
[[ "$client_uid" =~ ^[0-9]+$ ]] && test "$client_uid" -gt 0
test -c "$drm_node"
[[ $(basename "$(readlink -f "$drm_node")") == renderD* ]]
expected=${NIRI_TEST_EXPECT_RELEASE:?set early or deferred}
pixel_gate=${NIRI_TEST_PIXEL_GATE:?set none or /host/unique-gate-name}
case "$pixel_gate" in none|/host/*) ;; *) exit 2 ;; esac
case "$expected" in early|deferred) ;; *) exit 2 ;; esac
# This script runs ONLY inside the disposable virtio GPU VM as root.
read -r node_major node_minor < <(stat -Lc '%t %T' "$drm_node")
device="/sys/dev/char/$((16#$node_major)):$((16#$node_minor))/device"
test "$(cat "$device/vendor")" = 0x1af4
test "$(cat "$device/device")" = 0x1050
worker=$(pgrep -f '(^|/)niri gpu-process --mode drm$')
core=$(pgrep -f '(^|/)niri -c [^ ]+$')
[[ "$worker" =~ ^[0-9]+$ ]] && [[ "$core" =~ ^[0-9]+$ ]]
echo "VM worker=$worker core=$core exe=$(readlink /proc/$core/exe)"
dir=$(mktemp -d /tmp/niri-release-probe.XXXXXX)
mkfifo "$dir/commands"
exec 3<>"$dir/commands"
stopped=0
core_stopped=0
client=
tracer=
cleanup() {
    if [ "$core_stopped" = 1 ]; then kill -CONT "$core" || true; fi
    if [ "$stopped" = 1 ]; then kill -CONT "$worker" || true; fi
    if [ -n "$tracer" ]; then kill -INT "$tracer" || true; wait "$tracer" || true; fi
    if [ -n "$client" ]; then kill "$client" 2>/dev/null || true; wait "$client" || true; fi
}
trap cleanup EXIT
"$client_executable" "$drm_node" "$client_uid" <&3 >"$dir/client.log" 2>&1 &
client=$!
wait_log() {
    for i in $(seq 1 100); do
        grep -q "$1" "$dir/client.log" && return
        kill -0 "$client"
        sleep .05
    done
    echo "TIMEOUT waiting $1"; cat "$dir/client.log"; exit 1
}
wait_log READY
echo A >&3
wait_log 'FRAME buffer=A commit=1'
sleep 1
echo B >&3
wait_log 'FRAME buffer=B commit=2'
sleep 2
strace -xx -ttt -s 65535 -e trace=sendmsg -p "$core" -o "$dir/core.trace" &
tracer=$!
sleep .2
before=$(wc -l < "$dir/client.log")
kill -STOP "$worker"
stopped=1
grep '^State:' "/proc/$worker/status"
echo A >&3
wait_log 'SYNC commit=3'
for i in $(seq 1 100); do
    grep -qP 'iov_base="(?:\\x[0-9a-f]{2}){8}\\x17' "$dir/core.trace" && break
    sleep .05
done
# The transport header is 8 bytes; postcard Request::Present is variant23=0x17.
grep -qP 'iov_base="(?:\\x[0-9a-f]{2}){8}\\x17' "$dir/core.trace"
echo "QUEUED PRESENT BEFORE REPLACING COMMIT"
grep -P 'iov_base="(?:\\x[0-9a-f]{2}){8}\\x17' "$dir/core.trace"
echo "AFTER A, BEFORE B"; cat "$dir/client.log"
ss -xnp | grep -E "pid=($core|$worker)," || true
echo B >&3
wait_log 'SYNC commit=4'
sleep .3
echo "AFTER B, GPU STILL STOPPED"; grep '^State:' "/proc/$worker/status"
tail -n +"$((before + 1))" "$dir/client.log" > "$dir/stopped.log"
cat "$dir/client.log"
early_a=$(grep -c 'RELEASE buffer=A' "$dir/stopped.log" || true)
early_b=$(grep -c 'RELEASE buffer=B' "$dir/stopped.log" || true)
echo "STOPPED_RELEASE_COUNTS A=$early_a B=$early_b"
echo repaint-A >&3
wait_log 'REPAINT_A'
if [ "$expected" = early ]; then
    grep -q 'REPAINT_A performed=1.*latest_attach=3' "$dir/client.log"
else
    grep -q 'REPAINT_A performed=0 reason=unreleased latest_attach=3' "$dir/client.log"
fi
resume_line=$(wc -l < "$dir/client.log")
if [ "$pixel_gate" != none ]; then
    test ! -e "$pixel_gate.ready" && test ! -e "$pixel_gate.continue"
    kill -STOP "$core"
    core_stopped=1
    grep '^State:' "/proc/$core/status"
fi
kill -CONT "$worker"
stopped=0
if [ "$pixel_gate" != none ]; then
    # A can now render, but the stopped core cannot submit replacement B.
    sleep .5
    touch "$pixel_gate.ready"
    echo "PIXEL_GATE_READY=$pixel_gate.ready core=$core worker=$worker"
    for i in $(seq 1 300); do
        test -e "$pixel_gate.continue" && break
        sleep .1
    done
    test -e "$pixel_gate.continue"
    kill -CONT "$core"
    core_stopped=0
fi
wait_log 'FRAME buffer=B commit=4'
for i in $(seq 1 100); do
    tail -n +"$((resume_line + 1))" "$dir/client.log" > "$dir/resumed.log"
    if grep -q 'RELEASE buffer=A' "$dir/resumed.log" &&
       grep -q 'RELEASE buffer=B' "$dir/resumed.log"; then break; fi
    if [ "$expected" = early ]; then break; fi
    sleep .05
done
echo "AFTER RESUME"; cat "$dir/client.log"
echo quit >&3
wait "$client"
client=
echo "ARTIFACTS=$dir"
if [ "$expected" = early ]; then
    test "$early_a" -eq 1 && test "$early_b" -eq 1
    echo "PASS: reproduced early A+B release with actual Present queued in stopped worker"
else
    test "$early_a" -eq 0 && test "$early_b" -eq 0
    grep -q 'RELEASE buffer=A' "$dir/resumed.log"
    grep -q 'RELEASE buffer=B' "$dir/resumed.log"
    echo "PASS: no stopped-worker release; both old buffers eventually released after resume"
fi
