# Development VM for the multi-UID stack. Not a test: run it (nix/dev-vm-run.sh), ssh in,
# iterate. The desktop itself comes from nix/module.nix; this file is the machine and the
# probe apps.
{ niri }:
{ config, pkgs, lib, modulesPath, ... }:
let
  # A page that plays a sound forever, so a browser's audio path can be seen in PipeWire.
  portalWrapper = pkgs.writeShellScript "portal" ''
    # The backend's .portal file comes from its share dir; which backend to use comes from
    # /etc/xdg/xdg-desktop-portal/portals.conf (the module writes it).
    export XDG_DATA_DIRS=${pkgs.xdg-desktop-portal-gnome}/share:$XDG_DATA_DIRS
    export XDG_CURRENT_DESKTOP=niri
    exec ${config.services.drv.portalPackage}/libexec/xdg-desktop-portal --verbose
  '';
  sharePage = pkgs.writeText "share.html" ''
    <!doctype html><title>share</title>
    <body style="margin:0;background:#224">
    <audio autoplay loop src="file://${pkgs.sound-theme-freedesktop}/share/sounds/freedesktop/stereo/bell.oga"></audio>
    <button id=b style="font-size:60px;width:100%;height:200px">share screen</button>
    <video id=v autoplay style="width:100%"></video>
    <pre id=log style="color:#fff;font-size:30px"></pre>
    <script>
      const log = m => document.getElementById("log").textContent += m + "\\n";
      b.onclick = async () => {
        try {
          const s = await navigator.mediaDevices.getDisplayMedia({ video: true });
          v.srcObject = s;
          log("got stream " + s.getVideoTracks()[0].label);
        } catch (e) { log("failed: " + e); }
      };
    </script>
  '';
  probe = pkgs.writeShellScript "probe" ''
    ${pkgs.coreutils}/bin/id > "$HOME/id.txt"
    ${pkgs.coreutils}/bin/cat /proc/self/cgroup > "$HOME/cgroup.txt"
    ${pkgs.coreutils}/bin/env > "$HOME/env.txt"
    ${pkgs.coreutils}/bin/ls -la /run /tmp /dev/shm > "$HOME/run.txt" 2>&1
    ${pkgs.procps}/bin/ps -eo user,pid,cmd > "$HOME/ps.txt" 2>&1
    ${pkgs.coreutils}/bin/cat /proc/net/dev > "$HOME/net.txt" 2>&1
    ${pkgs.pipewire}/bin/pw-cli info 0 > "$HOME/pipewire.txt" 2>&1 || echo "pw-cli failed: $?" >> "$HOME/pipewire.txt"
    ${pkgs.wayland-utils}/bin/wayland-info > "$HOME/globals.txt" 2> "$HOME/wayland-info.err"
    ${pkgs.coreutils}/bin/touch "$HOME/done"
  '';
in
{
  imports = [
    "${modulesPath}/virtualisation/qemu-vm.nix"
    (import ./module.nix { inherit niri; })
  ];

  virtualisation = {
    memorySize = 4096;
    cores = 4;
    diskSize = 4096;
    graphics = true;
    qemu.options = [
      # Only a virgl GPU (the default VGA has no render node), an absolute pointer so host
      # clicks land, a sound card whose output is discarded, and a monitor socket.
      "-vga none" "-device virtio-gpu-gl-pci"
      "-device qemu-xhci" "-device usb-tablet"
      "-audiodev none,id=snd0" "-device ich9-intel-hda" "-device hda-duplex,audiodev=snd0"
      "-monitor unix:/tmp/niri-vm/monitor,server,nowait"
    ];
    forwardPorts = [
      {
        from = "host";
        host.port = 2222;
        guest.port = 22;
      }
    ];
    sharedDirectories.niri = {
      source = "/src/niri";
      target = "/src/niri";
    };
  };

  services.openssh.enable = true;
  services.openssh.settings.PermitRootLogin = "yes";
  users.users.root.openssh.authorizedKeys.keys = [ "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIP4pE2ZiZIJvxrTMzKzwfVBtUPp2Ek7MGselzb0w6wDE maan2003@devbox-01" ];
  networking.firewall.enable = false;
  fonts.enableDefaultPackages = true;

  # Only an ssh admin; nothing on the desktop runs as a human.
  users.users.alice = {
    isNormalUser = true;
    uid = 1000;
  };

  # Dev PIN 1234, enrolled once. Real installs run `drv-authd set-pin` by hand. As root
  # (the `+`): the unit's own user may not write drv-auth's directory.
  systemd.services.drv-supervisor.serviceConfig.ExecStartPre = "+" + pkgs.writeShellScript "drv-enrol-dev-pin" ''
    if [ ! -e /var/lib/drv-auth/pin ]; then
      printf 1234 | ${config.services.drv.package}/bin/drv-authd set-pin --state-dir /var/lib/drv-auth
    fi
    chown -R drv-auth:drv-auth /var/lib/drv-auth
  '';

  services.drv = {
    enable = true;
    # niri's stock binds; its spawn lines name apps that do not exist here and are refused.
    # Mod+D shows the menu (drv-menu, a supervisor service) instead of spawning fuzzel.
    config = builtins.replaceStrings [ "// skip-at-startup" "{ spawn \"fuzzel\"; }" ] [ "skip-at-startup" "{ show-launcher; }" ] (builtins.readFile ../resources/default-config.kdl) + ''
      // The screencast and introspection D-Bus services the GNOME portal backend needs.
      debug { dbus-interfaces-in-non-session-instances; }
    '';
    apps = {
      # Notification daemon on the services' bus; apps reach it only through the bridge, which
      # names them.
      mako = {
        uid = 100011;
        exec = [ "${pkgs.mako}/bin/mako" ];
        globals = [ "layer-shell" ];
        servicesBus = true;
        sessionBusNames = [ "org.freedesktop.Notifications" ];
        autostart = true; menu = false;
      };
      # Portals on the services' bus: the frontend and the GNOME backend (niri speaks its
      # Mutter screencast API). The frontend hands screencast consumers a PipeWire fd
      # (OpenPipeWireRemote), so it needs the socket itself.
      portal = {
        uid = 100012;
        exec = [ "${portalWrapper}" ];
        groups = [ "pipewire" ];
        servicesBus = true;
        sessionBusNames = [ "org.freedesktop.portal.Desktop" ];
        autostart = true; menu = false;
      };
      # The only holder of the screencast grant: it shows the consent dialog and opens one
      # session per consent.
      portal-gnome = {
        uid = 100013;
        exec = [ "${pkgs.xdg-desktop-portal-gnome}/libexec/xdg-desktop-portal-gnome" ];
        servicesBus = true;
        sessionBusNames = [ "org.freedesktop.impl.portal.desktop.gnome" ];
        grants = [ "screencast" ];
        autostart = true; menu = false;
      };
      # Sends one notification from inside the sandbox over its private bus. Not autostarted:
      # it would race the notification daemon; `drv launch notify-test`.
      notify-test = {
        uid = 100007;
        bus = true;
        exec = [ "${pkgs.libnotify}/bin/notify-send" "-a" "Evil Corp" "<b>Hello</b>" "from uid 100007 via the bridge" ];
      };
      hello = { uid = 100001; exec = [ "${probe}" ]; autostart = true; menu = false; };
      gpu-probe = { uid = 100002; exec = [ "${probe}" ]; gpu = true; groups = [ "render" ]; autostart = true; menu = false; };
      flower = { uid = 100003; exec = [ "${pkgs.weston}/bin/weston-flower" ]; autostart = true; };
      # Asks for a group the spawner was not told to hand out: must be refused.
      sneaky = { uid = 100004; exec = [ "${probe}" ]; groups = [ "wheel" ]; autostart = true; menu = false; };
      # A real browser: GPU, audio, its own home, a private session bus (compatibility, not
      # a boundary; the sandbox already hides the system bus). Flags come from here only.
      chromium = {
        uid = 100005;
        bus = true;
        exec = [
          "${pkgs.chromium}/bin/chromium" "--ozone-platform=wayland"
          "--autoplay-policy=no-user-gesture-required" "file://${sharePage}"
        ];
        gpu = true;
        network = true;
        groups = [ "render" "pipewire" ];
      };
      # Plays a sound: audio is just the `pipewire` group plus the exposed socket directory.
      beep = {
        uid = 100006;
        exec = [ "${pkgs.pipewire}/bin/pw-play" "${pkgs.sound-theme-freedesktop}/share/sounds/freedesktop/stereo/bell.oga" ];
        groups = [ "pipewire" ];
      };
    };
  };

  environment.systemPackages = [
    pkgs.wayland-utils
    pkgs.weston
    pkgs.foot
  ];

  system.stateVersion = "25.11";
}
