#!/usr/bin/env bash
# Smoke run against a freshly booted dev VM (nix/dev-vm-run.sh): the checks that used to be
# done by hand after every build. Drives the desktop with QEMU's monitor (sendkey) and reads
# the evidence over ssh: journal lines, the probe apps' result files, PipeWire state. Every
# step prints PASS or FAIL with what it saw; the exit code is the number of failures.
# Wants: the monitor at /tmp/niri-vm/monitor, /tmp/niri-vm/ssh, socat; or VM_BACKEND=m2 with
# the M2 guest up (nix/m2-vm-run.sh). Run once per boot: the unlock step types the dev PIN,
# and the chooser walks the dialog from its start.
set -uo pipefail
. "$(dirname "$0")/vm-lib.sh"

echo "== waiting for ssh"
wait_ssh

echo "== the set"
for m in compositor-gpu compositor drv-seatd drv-authd drv-shell drv-files drv-cast drv-agent drv-keys drv-forker drv-appd; do
  expect "member $m" "drv-supervisor: $m running as uid [0-9]+"
done
count "set started once" "drv-supervisor: compositor running as uid" 1
expect "the host workspace's terminal" "Started the host workspace's terminal"
expect "named host by drv-appd" 'new client: policy "host"'

echo "== kernel state"
for who in flower forker compositor; do
  if out=$(bash "$(dirname "$0")/kernel-state.sh" $who 2>&1); then echo "PASS $out"; else echo "FAIL kernel state of $who:"; echo "$out" | head -30; fails=$((fails + 1)); fi
done
echo "== an app's view (the hello probe)"
H=/var/lib/drv-apps/100001/out
for _ in $(seq 1 30); do $SSH test -e $H/done && break; sleep 1; done
file_has "own uid" $H/id.txt '^uid=100001\(app-hello\) gid=100001\(app-hello\) groups=100001\(app-hello\)$'
file_has "only loopback" $H/net.txt '^ *lo:'
count_ifaces=$($SSH cat $H/net.txt | grep -c ':'); [ "$count_ifaces" = 1 ] && echo "PASS no other interface" || fail "other interfaces: $count_ifaces"
others=$($SSH cat $H/ps.txt | tail -n +2 | grep -v '^app-hel'); [ -z "$others" ] && echo "PASS sees only its own processes" || fail "sees other processes: $others"
file_has "no user namespace" $H/userns.txt 'Operation not permitted'
$SSH grep -q cpuinfo $H/proc.txt && fail "proc beyond the pid entries" || echo "PASS proc is the pid entries only"
file_has "no PipeWire without audio" $H/pipewire.txt 'failed to connect|Host is down'
hidden='zwlr_layer_shell|ext_session_lock|screencopy|image_copy_capture|image_capture_source|output_management|output_power|foreign_toplevel|virtual_pointer|virtual_keyboard|security_context|gamma_control|input_method|ext_transient_seat|zwlr_data_control|ext_data_control'
leaked=$($SSH cat $H/globals.txt | grep -oE "interface: '[a-z_0-9]+'" | grep -E "$hidden")
[ -z "$leaked" ] && echo "PASS no privileged globals" || fail "privileged globals: $leaked"
file_has "but the ordinary ones" $H/globals.txt "interface: 'xdg_wm_base'"
file_has "state linked into HOME" $H/home.txt 'out -> /state/out'
file_has "defaults linked from the store" $H/home.txt '^/nix/store/.*-drv-files-hello/.config/hello/greeting$'
file_has "and readable through the closure" $H/home.txt '^hello from the store$'
file_has "store not listable beyond the closure" $H/store.txt 'Permission denied'
file_has "the ssh agent answers the grant" $H/agent.txt 'The agent has no identities'
file_has "the person's folder is linked from HOME" $H/folder.txt '^/files/Shared$'
file_has "and is the app's own inside" $H/folder.txt '^100001 100001 700$'
file_has "including what it writes there" $H/folder.txt '^100001 100001 644$'
owner=$($SSH "stat -c '%U %G' /var/lib/drv-files/Shared/hello.txt" 2>&1); [ "$owner" = "drv-files drv-files" ] && echo "PASS but drv-files' on disk" || fail "on disk the file is $owner"
expect "a daemon that exited is started again" 'drv-init: the app exited \(3\); starting it again in 2s'
G=/var/lib/drv-apps/100002/out
for _ in $(seq 1 30); do $SSH test -e $G/done && break; sleep 1; done
$SSH grep -q 'no identities' $G/agent.txt && fail "the agent answered an app without the grant" || echo "PASS the agent's door is shut without the grant"
expect "and says so" 'drv-agent: refused uid 100002'

echo "== unlock"
mark; key 1 2 3 4 ret; sleep 2
expect "PIN accepted" "PIN accepted; unlocking"

echo "== host workspace"
mark; key meta_l-grave_accent; sleep 1
vt=$($SSH cat /sys/class/tty/tty0/active); [ "$vt" = tty7 ] && echo "PASS Mod+Grave stays on the desktop ($vt)" || fail "Mod+Grave switched to $vt"
# run0 in it: its terminal is a logind session, so polkit lets the agent register (the
# password prompt then waits; ^C ends it).
type_word run0; key spc; type_word true; key ret; sleep 2; key ctrl-c; sleep 1
expect "run0 got as far as polkit's password prompt" "polkit-agent-helper-1.*pam_authenticate failed" 
# The overlay holds the keyboard while it is up: put it away before the menu steps.
key meta_l-grave_accent; sleep 1

echo "== media keys"
mark; key volumeup; sleep 2
expect "volume-up reached drv-keys" 'drv-keys: volume up'
absent "and wpctl took it" 'drv-keys: volume up: '

echo "== notification"
mark; menu noti; sleep 3
expect "notify-test launched" 'launched "notify-test" as uid 100007'
expect "reached the shell" 'drv-shell: notify-test \(uid 100007\) notifies: "<b>Hello</b>"'
expect "notify-test exited 0" 'uid 100007\) exited: exit status: 0'

echo "== file chooser"
$SSH rm -f /var/lib/drv-apps/100008/out/result.txt
mark; menu choo; sleep 3
key ret; sleep 1.2      # into notes/
key ret; sleep 2.5      # todo.txt
key ret; sleep 3        # save under the offered name
R=/var/lib/drv-apps/100008/out/result.txt
file_has "read the picked file" $R 'read /run/drv/doc/[0-9]+/todo.txt: Ok\("build the portal\\n"\)'
file_has "read-only pick" $R 'open it for writing: Err'
file_has "saved a copy" $R 'wrote /run/drv/doc/[0-9]+/result copy.txt, length now Ok\('
expect "drv-files granted it" 'drv-files: chooser-test \(uid 100008\) gets /var/lib/drv-files/notes/todo.txt'

echo "== screen cast"
$SSH rm -f /var/lib/drv-apps/100009/out/cast.txt
mark; menu cast; sleep 3
key ret; sleep 5        # consent
expect "drv-cast started the cast" 'drv-cast: cast-test \(uid 100009\) shares Virtual-1 on PipeWire node [0-9]+'
C=/var/lib/drv-apps/100009/out/cast.txt
file_has "Start answered with a stream" $C 'Start: response 0, streams Some'
file_has "remote sees its node only" $C 'the remote sees: \[\(0, "Core"\), \([0-9]+, "Factory"\), \([0-9]+, "Node"\)\]$'
key meta_l-shift-esc; sleep 3
expect "revoke ended the cast" 'drv-cast: the cast of Virtual-1 for cast-test ended'
file_has "probe got Session.Closed" $C '^Closed by the desktop$'
expect "cast-test exited 0" 'uid 100009\) exited: exit status: 0'

echo "== microphone"
mark; menu mic; sleep 3
key ret; sleep 3        # allow
expect "mic granted" 'drv-cast: mic-test \(uid 100010\) may use the microphone'
key meta_l-alt-l; sleep 3   # lock-session: the lock is the other switch that revokes
expect "locking revoked the mic" 'drv-cast: mic-test \(uid 100010\) loses the microphone'
expect "session locked" 'locking session'
key 1 2 3 4 ret; sleep 2
expect "unlocked again" 'PIN accepted; unlocking'

echo "== OpenURI"
$SSH rm -f /var/lib/drv-apps/100011/out/open.txt
mark; menu open; sleep 6
O=/var/lib/drv-apps/100011/out/open.txt
file_has "three calls answered" $O '^not a uri: o "/org/freedesktop/portal/desktop/request/'
expect "https went to chromium" '"chromium" \(uid 100005\) opens "https://example.com/\?from=uid-100011" for open-test \(uid 100011\)'
expect "mailto refused" 'no app opens mailto: URIs'
expect "junk refused" 'drv-dbus-shim: OpenURI: "-https://example.com" is not a URI'

echo "== health"
absent "no panics" 'panicked at|RUST_BACKTRACE'
absent "no members died" 'drv-supervisor: .* (exited|died|killed)|restarting the set'
absent "no seccomp kills" 'exited: signal: 31|SIGSYS'
count "set still started once" "drv-supervisor: compositor running as uid" 1
cores=$($SSH "coredumpctl list --no-pager 2>&1 | tail -1"); case "$cores" in *"No coredumps"*) echo "PASS no coredumps";; *) fail "coredumps: $cores";; esac

echo "== $fails failures"
exit $fails
