# The desktop's binaries cross-built on devbox (nix/cross-drv.sh) and dropped into
# nix/m2-prebuilt/{bin,resources} (not in git), patched for the guest's libraries the way
# the nixos repo's niri-bin does it. The guest then never compiles niri.
{ lib, stdenv, autoPatchelfHook, dbus, libdisplay-info_0_3, libglvnd, libinput, libxkbcommon
, libgbm, cairo, glib, pango, pipewire, pixman, seatd, systemd, wayland, fuse3 }:
stdenv.mkDerivation {
  pname = "drv-prebuilt";
  version = "m2";
  src = ./m2-prebuilt;
  dontStrip = true;
  nativeBuildInputs = [ autoPatchelfHook ];
  buildInputs = [ (lib.getLib stdenv.cc.cc) dbus libdisplay-info_0_3 libglvnd libinput libxkbcommon
    libgbm cairo glib pango pipewire pixman seatd systemd wayland fuse3 ];
  installPhase = ''
    runHook preInstall
    mkdir -p $out/bin
    install -m0755 bin/* $out/bin/
    cp -r resources $out/share-resources
    install -Dm644 resources/default-config.kdl -t $out/share/doc/niri
    runHook postInstall
  '';
  meta.platforms = [ "aarch64-linux" ];
}
