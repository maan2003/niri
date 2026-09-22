#!/usr/bin/env bash
# Failure drills against the booted dev VM, after nix/smoke.sh: kills each member of the set
# with SIGKILL and checks that the supervisor restarts the whole set, that the running apps
# die with it, and that the desktop comes back (unlock, a notification at the shell).
# Then kills an app and checks that the set does not restart. Exit code: failures.
set -uo pipefail
. "$(dirname "$0")/vm-lib.sh"
MEMBERS="compositor-gpu compositor drv-seatd drv-authd drv-shell drv-files drv-cast drv-forker drv-appd"

wait_ssh
restarts=$($SSH "journalctl -b -o cat --no-pager" | grep -c 'restarting the set')

# pid_of NAME: the pid the supervisor last reported for member NAME.
pid_of() { $SSH "journalctl -b -o cat --no-pager" | grep -E "drv-supervisor: $1 running as uid [0-9]+, pid [0-9]+" | tail -1 | sed 's/.*pid //'; }
app_pid() { $SSH "pgrep -u app-$1 -o" 2>/dev/null; }

for m in $MEMBERS; do
  echo "== kill $m"
  pid=$(pid_of "$m"); flower=$(app_pid flower)
  [ -n "$pid" ] || { fail "$m: no pid known"; continue; }
  [ -n "$flower" ] || fail "flower is not running before killing $m"
  mark; $SSH "kill -9 $pid"
  expect_soon "supervisor named it" "drv-supervisor: .*$m \(signal 9\).* exited; restarting the set" 20
  for n in $MEMBERS; do expect_soon "$n back" "drv-supervisor: $n running as uid [0-9]+, pid [0-9]+" 30; done
  restarts=$((restarts + 1))
  count "restarted once more" 'restarting the set' "$restarts"
  if [ -n "$flower" ]; then
    $SSH "kill -0 $flower 2>/dev/null" && fail "flower (pid $flower) survived the restart" || echo "PASS the apps died with the set"
  fi
  expect_soon "flower autostarted again" 'autostarted "flower" as uid 100003' 20
  sleep 3
  key 1 2 3 4 ret; sleep 2
  expect_soon "unlocked again" "PIN accepted; unlocking" 10
  menu noti; sleep 3
  expect_soon "a notification went through" 'drv-shell: notify-test \(uid 100007\) notifies' 10
done

echo "== kill an app"
flower=$(app_pid flower)
mark; $SSH "kill -9 $flower"
sleep 3
expect "forker saw it" 'uid 100003\) exited: signal: 9'
count "the set did not restart" 'restarting the set' "$restarts"

echo "== health"
absent "no panics" 'panicked at|RUST_BACKTRACE'
cores=$($SSH "coredumpctl list --no-pager 2>&1 | tail -1"); case "$cores" in *"No coredumps"*) echo "PASS no coredumps";; *) fail "coredumps: $cores";; esac
echo "== $fails failures"
exit $fails
