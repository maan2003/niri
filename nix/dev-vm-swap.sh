#!/usr/bin/env bash
# Swap one binary of the running dev VM's drv package for a local debug build and restart
# the set: a sub-minute loop for the forker and friends, no VM rebuild. The local build
# links against the same store paths as the guest (same flake), so it runs unchanged.
#   nix/dev-vm-swap.sh drv-forker    (after: nix develop -c cargo build -p drv-forker)
# Any binary of the package goes: the path comes from the forker's in the supervisor's unit.
set -eu
bin=$1
SSH=${SSH:-/tmp/niri-vm/ssh}
target=$($SSH "systemctl cat drv-supervisor | grep -o '/nix/store[^ ]*/bin/drv-forker' | head -1 | sed 's|/bin/drv-forker\$|/bin/$bin|'")
$SSH "cat > /tmp/$bin.new && chmod 755 /tmp/$bin.new && mv /tmp/$bin.new /tmp/$bin" < "target/debug/$bin"
# Lazy: the members' clones of the store pin the previous bind; they keep it, new starts get ours.
$SSH "mountpoint -q '$target' && umount -l '$target'; mount --bind /tmp/$bin '$target'; umount -l /run/drv-doc 2>/dev/null; systemctl reset-failed drv-supervisor; systemctl restart drv-supervisor"
echo "swapped $bin over $target; set restarted"
