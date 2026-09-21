#!/usr/bin/env bash
# Second stage of the cross build for the drv desktop: after niri-build-inner has built
# `niri` for aarch64 in its state repo, build the drv crates there with the same toolchain
# and environment (the exports come from the inner script itself), then collect every binary.
set -euo pipefail
INNER=${INNER:?path to niri-build-inner}
state=${STATE_DIRECTORY:-$HOME/.local/state/niri-build}
eval "$(grep -E '^\s*export ' "$INNER" | sed 's/^ *//')"
cd "$state/repo"
git log --oneline -1
cargo build --release --locked --target aarch64-unknown-linux-gnu --no-default-features \
  --features drv-portal/service \
  -p drv-supervisor -p drv-appd -p drv-forker -p drv-trampoline -p drv-bridge -p drv-seat -p drv-auth \
  -p drv-lock -p drv-menu -p drv-portal 2>&1 | grep -vE '^\s+(Compiling|Downloaded|Downloading|Checking)'
out=${OUT:-$state/drv-out}
rm -rf "$out"; mkdir -p "$out/bin"
# Debug info stays here (target/); the copies that travel to the M2 keep their symbol table
# (names in backtraces) but not the line tables: niri alone is 130 MB with them.
for b in target/aarch64-unknown-linux-gnu/release/*; do
  [ -f "$b" ] && [ -x "$b" ] && case "$b" in *.d) ;; *) llvm-objcopy --strip-debug "$b" "$out/bin/$(basename "$b")" && chmod 0755 "$out/bin/$(basename "$b")";; esac
done
cp -r resources "$out/resources"
ls -la "$out/bin"
