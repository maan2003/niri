#!/usr/bin/env bash
# The kernel's account of one running app against ours: mounts and their flags, credentials,
# namespaces, cgroup, parent, normalized and diffed with nix/expect/kernel-<app>.txt. The
# forker is written from a model of what each syscall does to kernel state; this is where the
# model meets what the kernel says it did, so any divergence is a diff, not a surprise later.
# One ssh round trip. Not visible from outside a process, so not here: securebits, MDWE, the
# Landlock domain (the probe apps and the smoke's behaviour checks cover those).
#   nix/kernel-state.sh flower            check
#   nix/kernel-state.sh flower --update   accept what the kernel says as the expectation
set -eu
app=$1
here=$(dirname "${BASH_SOURCE[0]}")
. "$here/vm-lib.sh"
uid=$($SSH "id -u app-$app")
expect="$here/expect/kernel-$app.txt"
dump=$($SSH "uid=$uid; $(cat <<'EOF'
set -eu
# The app itself, not its PulseAudio service (same UID): the one in the forker's cgroup.
# The app itself, not its PulseAudio service (same UID) and not its init (drv-init): the one
# in the forker's cgroup whose parent is the init.
p=; for c in $(pgrep -u "$uid"); do grep -q "apps/app-$uid\$" /proc/$c/cgroup 2>/dev/null && [ "$(cat /proc/$c/comm)" != drv-init ] && { p=$c; break; }; done
[ -n "$p" ] || { echo "no launched process of uid $uid"; exit 1; }
echo "parent $(cat /proc/$(awk '/^PPid/ {print $2}' /proc/$p/status)/comm)"
awk -v uid="$uid" '
  { sep = index($0, " - "); left = substr($0, 1, sep - 1); right = substr($0, sep + 3)
    n = split(left, l, " "); split(right, r, " ")
    root = l[4]; mp = l[5]; opts = l[6]; fstype = r[1]; sup = r[3]
    # nix/dev-vm-swap.sh binds a debug binary over the store; not the forker s work.
    if (fstype == "ext4" && root ~ /^\/tmp\// && mp ~ /^\/nix\/store\//) next
    gsub("/" uid "$", "/UID", root); gsub("/" uid "/", "/UID/", root); gsub("/" uid "$", "/UID", mp)
    gsub(",?relatime", "", opts)
    if (fstype == "overlay") sup = "host"
    gsub(/,?(size|nr_inodes)=[^,]*/, "", sup); sub(/^,/, "", sup)
    gsub("uid=" uid, "uid=UID", sup); gsub("gid=" uid, "gid=UID", sup)
    gsub(/\/nix\/store\/[a-z0-9]{32}-/, "/nix/store/HASH-", mp)
    printf "mount %s %s %s root=%s %s\n", mp, opts, fstype, root, sup }
' /proc/$p/mountinfo | sort -k2,2
grep -E "^(Uid|Gid|Groups|Cap[A-Za-z]*|NoNewPrivs|Seccomp):" /proc/$p/status | sed "s/$uid/UID/g; s/[[:space:]]\+/ /g; s/ $//"
for n in mnt net pid user uts ipc cgroup time; do
  if [ "$(readlink /proc/$p/ns/$n)" = "$(readlink /proc/1/ns/$n)" ]; then echo "ns $n host"; else echo "ns $n own"; fi
done
sed "s/$uid/UID/" /proc/$p/cgroup
# The kernel's own account, where the dev VM has the module (nix/kdump): what /proc does not
# show. Store rules are the closure, one line per distinct access set instead of per path.
if [ -e /sys/kernel/debug/drv/task ]; then
  echo "$p" > /sys/kernel/debug/drv/task
  # Store rules (paths inside the store's own filesystem, so no /nix/store prefix) collapse
  # to one line per distinct access set; the tree is in pointer order, so rules are sorted.
  sed -E "s/^task [0-9]+ /task /; s/\b$uid\b/UID/g; s/ ino [0-9]+//" /sys/kernel/debug/drv/task \
    | awk '/^rule overlay:[^ ]* \/[a-z0-9]{32}-/ { sub(/ \/[a-z0-9]{32}-[^ ]*/, " store/*"); if (seen[$0]++) next }
           /^rule / { rules[++n] = $0; next } { print }
           END { cmd = "sort"; for (i = 1; i <= n; i++) print rules[i] | cmd; close(cmd) }'
else
  echo "kdump: no module"
fi
EOF
)")
if [ "${2:-}" = --update ]; then
  printf '%s\n' "$dump" > "$expect"
  echo "wrote $expect"
  exit 0
fi
diff -u "$expect" <(printf '%s\n' "$dump") && echo "kernel state of $app: as expected"
