#!/usr/bin/env bash
# Swap one binary of the running dev VM's drv package for a local debug build and restart
# the set: a sub-minute loop for the forker and friends, no VM rebuild. The local build
# links against the same store paths as the guest (same flake), so it runs unchanged.
#   nix/dev-vm-swap.sh drv-forker    (after: nix develop -c cargo build -p drv-forker)
set -eu
bin=$1
SSH=${SSH:-/tmp/niri-vm/ssh}
target=$($SSH "systemctl cat drv-supervisor | grep -o '/nix/store[^ ]*/bin/$bin' | head -1")
$SSH "cat > /tmp/$bin.new && chmod 755 /tmp/$bin.new && mv /tmp/$bin.new /tmp/$bin" < "target/debug/$bin"
$SSH "mountpoint -q '$target' && umount '$target'; mount --bind /tmp/$bin '$target'; umount -l /run/drv-doc 2>/dev/null; systemctl reset-failed drv-supervisor; systemctl restart drv-supervisor"
echo "swapped $bin over $target; set restarted"
