# Shared by nix/smoke.sh and nix/drill.sh: drive the booted dev VM and read the evidence.
# Every check prints PASS or FAIL and counts. Two machines: the QEMU dev VM (keys through
# the monitor's sendkey, commands over ssh) and, with VM_BACKEND=m2, the crosvm guest on the
# M2 (keys through ydotool inside the guest, commands through nix/m2-vm-exec).
fails=0
if [ "${VM_BACKEND:-qemu}" = m2 ]; then
  SSH=${SSH:-$(dirname "${BASH_SOURCE[0]}")/m2-vm-exec}
  # Linux input codes for the sendkey names the scripts use.
  declare -A KC=([esc]=1 [1]=2 [2]=3 [3]=4 [4]=5 [5]=6 [6]=7 [7]=8 [8]=9 [9]=10 [0]=11
    [minus]=12 [backspace]=14 [tab]=15 [q]=16 [w]=17 [e]=18 [r]=19 [t]=20 [y]=21 [u]=22
    [i]=23 [o]=24 [p]=25 [ret]=28 [ctrl]=29 [a]=30 [s]=31 [d]=32 [f]=33 [g]=34 [h]=35
    [j]=36 [k]=37 [l]=38 [shift]=42 [z]=44 [x]=45 [c]=46 [v]=47 [b]=48 [n]=49 [m]=50
    [spc]=57 [up]=103 [left]=105 [right]=106 [down]=108
    # The M2 guest swaps Alt and Super (nix/m2-vm.nix: crosvm's keyboard has no Super key).
    [alt]=125 [meta_l]=56)
  # key NAME...: each name is a chord like meta_l-d; all of them go in one round trip.
  key() {
    local seq="" k parts i
    for k in "$@"; do
      IFS=- read -ra parts <<<"$k"
      for i in "${parts[@]}"; do seq+=" ${KC[$i]}:1"; done
      for ((i = ${#parts[@]} - 1; i >= 0; i--)); do seq+=" ${KC[${parts[i]}]}:0"; done
    done
    $SSH "YDOTOOL_SOCKET=/run/ydotoold/socket ydotool key --key-delay 40 $seq" >/dev/null
  }
else
  SSH=${SSH:-/tmp/niri-vm/ssh}
  MON=${MON:-/tmp/niri-vm/monitor}
  mon() { printf '%s\n' "$1" | socat - UNIX-CONNECT:"$MON" >/dev/null; }
  key() { for k in "$@"; do mon "sendkey $k"; sleep 0.08; done; }
fi
# Types a word letter by letter (sendkey has no hyphen; menu names are matched by prefix).
type_word() { local w=$1 ks=(); for ((i = 0; i < ${#w}; i++)); do ks+=("${w:i:1}"); done; key "${ks[@]}"; }
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
