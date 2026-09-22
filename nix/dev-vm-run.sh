#!/usr/bin/env bash
# Run the dev VM on a host without a GPU: qemu's GTK window with virgl rendering on
# llvmpipe, inside a headless Wayland session (rho wayland). Screenshots and input
# then work through that session. ssh: /tmp/niri-vm/ssh (written here: -p 2222
# root@127.0.0.1 with the repo's nix/dev-vm-key, which the guest trusts).
set -euo pipefail
cd "$(dirname "$0")/.."
mkdir -p /tmp/niri-vm
install -m 600 nix/dev-vm-key /tmp/niri-vm/id_ed25519
install -m 644 nix/dev-vm-key.pub /tmp/niri-vm/id_ed25519.pub
cat > /tmp/niri-vm/ssh <<'EOF'
#!/usr/bin/env bash
exec ssh -q -p 2222 -i /tmp/niri-vm/id_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=5 -o LogLevel=ERROR root@127.0.0.1 "$@"
EOF
chmod 755 /tmp/niri-vm/ssh
nix build -o /tmp/niri-vm/result .#packages.x86_64-linux.dev-vm
GLVND=$(nix build --no-link --print-out-paths nixpkgs#libglvnd)
MESA=$(nix build --no-link --print-out-paths nixpkgs#mesa)
cd /tmp/niri-vm
exec env NIX_DISK_IMAGE=/tmp/niri-vm/nixos.qcow2 \
  LD_LIBRARY_PATH="$GLVND/lib:$MESA/lib" \
  __EGL_VENDOR_LIBRARY_FILENAMES="$MESA/share/glvnd/egl_vendor.d/50_mesa.json" \
  LIBGL_DRIVERS_PATH="$MESA/lib/dri" LIBGL_ALWAYS_SOFTWARE=1 \
  QEMU_OPTS="${QEMU_OPTS:--display gtk,gl=on}" \
  ./result/bin/run-nixos-vm "$@"
