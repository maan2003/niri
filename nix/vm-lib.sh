# Shared by nix/smoke.sh and nix/drill.sh: drive the booted dev VM through QEMU's monitor
# (sendkey) and read the evidence over ssh. Every check prints PASS or FAIL and counts.
SSH=${SSH:-/tmp/niri-vm/ssh}
MON=${MON:-/tmp/niri-vm/monitor}
fails=0
mon() { printf '%s\n' "$1" | socat - UNIX-CONNECT:"$MON" >/dev/null; }
key() { for k in "$@"; do mon "sendkey $k"; sleep 0.08; done; }
# Types a word letter by letter (sendkey has no hyphen; menu names are matched by prefix).
type_word() { local w=$1; for ((i = 0; i < ${#w}; i++)); do key "${w:i:1}"; done; }
menu() { key meta_l-d; sleep 0.8; type_word "$1"; sleep 0.5; key ret; }
since="1970-01-01"
mark() { since=$($SSH "date '+%F %T'"); }
journal() { $SSH "journalctl -b -o cat --no-pager --since '$since'"; }
fail() { echo "FAIL $*"; fails=$((fails + 1)); }
# expect NAME PATTERN: the journal since the last mark must match PATTERN.
expect() {
  local name=$1 pat=$2 out
  out=$(journal | grep -E -- "$pat" | head -3)
  if [ -n "$out" ]; then echo "PASS $name: $out"; else fail "$name: nothing matched '$pat'"; fi
}
# expect_soon NAME PATTERN [SECONDS]: like expect, waiting up to SECONDS (default 30).
expect_soon() {
  local name=$1 pat=$2 wait=${3:-30} out
  for _ in $(seq 1 "$wait"); do
    out=$(journal | grep -E -- "$pat" | head -3)
    [ -n "$out" ] && break
    sleep 1
  done
  if [ -n "$out" ]; then echo "PASS $name: $out"; else fail "$name: nothing matched '$pat' in ${wait}s"; fi
}
# file_has NAME PATH PATTERN: the guest file must match PATTERN (waits up to 30 s for it).
file_has() {
  local name=$1 path=$2 pat=$3 out content
  for _ in $(seq 1 30); do
    content=$($SSH "cat '$path' 2>/dev/null")
    out=$(grep -E -- "$pat" <<<"$content" | head -2)
    [ -n "$out" ] && break
    sleep 1
  done
  if [ -n "$out" ]; then echo "PASS $name: $out"; else fail "$name: '$pat' not in $path ($(head -5 <<<"$content"))"; fi
}
# absent NAME PATTERN: nothing in the whole boot's journal matches.
absent() {
  local name=$1 pat=$2 out
  out=$($SSH "journalctl -b -o cat --no-pager" | grep -E -- "$pat" | head -3)
  if [ -z "$out" ]; then echo "PASS $name"; else fail "$name: $out"; fi
}
# count NAME PATTERN N: exactly N whole-boot matches.
count() {
  local name=$1 pat=$2 want=$3 n
  n=$($SSH "journalctl -b -o cat --no-pager" | grep -cE -- "$pat")
  if [ "$n" = "$want" ]; then echo "PASS $name: $n"; else fail "$name: $n matches of '$pat', wanted $want"; fi
}
wait_ssh() {
  for _ in $(seq 1 120); do $SSH true 2>/dev/null && break; sleep 2; done
  $SSH true || { echo "FAIL ssh: the VM is not up"; exit 1; }
}
