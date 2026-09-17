# Development VM for the multi-UID stack. Not a test: run it (nix/dev-vm-run.sh), ssh in,
# iterate. The desktop itself comes from nix/module.nix; this file is the machine and the
# probe apps.
{ niri }:
{ pkgs, lib, modulesPath, ... }:
let
  # A page that plays a sound forever, so a browser's audio path can be seen in PipeWire.
  audioPage = pkgs.writeText "audio.html" ''
    <title>audio test</title>
    <audio autoplay loop src="file://${pkgs.sound-theme-freedesktop}/share/sounds/freedesktop/stereo/bell.oga"></audio>
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

  users.users.alice = {
    isNormalUser = true;
    uid = 1000;
  };

  services.niri-desktop = {
    enable = true;
    user = "alice";
    # niri's stock binds (its spawn lines name apps that do not exist here and are refused).
    config = builtins.replaceStrings [ "// skip-at-startup" ] [ "skip-at-startup" ] (builtins.readFile ../resources/default-config.kdl) + ''
      spawn-at-startup "hello"
      spawn-at-startup "gpu-probe"
      spawn-at-startup "flower"
      spawn-at-startup "sneaky"
      spawn-at-startup "hello" "extra-argument"
      spawn-at-startup "mako"
      spawn-at-startup "notify-test"
    '';
    apps = {
      # The launcher: the human's own tool, trusted, runs as the human (Mod+D in the stock
      # binds). It starts apps through `niri msg`, so the compositor does the launching.
      fuzzel = { uid = 1000; trusted = true; exec = [ "${pkgs.fuzzel}/bin/fuzzel" ]; };
      # Notification daemon: the human's tool on the human's bus; apps reach it only through
      # the bridge, which names them.
      mako = { uid = 1000; trusted = true; exec = [ "${pkgs.mako}/bin/mako" ]; };
      # Sends one notification from inside the sandbox over its private bus.
      notify-test = {
        uid = 100007;
        bus = true;
        exec = [ "${pkgs.libnotify}/bin/notify-send" "-a" "Evil Corp" "<b>Hello</b>" "from uid 100007 via the bridge" ];
      };
      hello = { uid = 100001; exec = [ "${probe}" ]; };
      gpu-probe = { uid = 100002; exec = [ "${probe}" ]; gpu = true; groups = [ "render" ]; };
      flower = { uid = 100003; exec = [ "${pkgs.weston}/bin/weston-flower" ]; };
      # Asks for a group the forker was not told to hand out: must be refused.
      sneaky = { uid = 100004; exec = [ "${probe}" ]; groups = [ "wheel" ]; };
      # A real browser: GPU, audio, its own home, a private session bus (compatibility, not
      # a boundary; the sandbox already hides the system bus). Flags come from here only.
      chromium = {
        uid = 100005;
        bus = true;
        exec = [
          "${pkgs.chromium}/bin/chromium" "--ozone-platform=wayland"
          "--autoplay-policy=no-user-gesture-required" "file://${audioPage}"
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
