# Development VM for the multi-UID stack. Not a test: run it, ssh in, iterate.
{ niri }:
{ pkgs, lib, modulesPath, ... }:
let
  probe = pkgs.writeShellScript "probe" ''
    ${pkgs.coreutils}/bin/id > "$HOME/id.txt"
    ${pkgs.coreutils}/bin/cat /proc/self/cgroup > "$HOME/cgroup.txt"
    ${pkgs.coreutils}/bin/env > "$HOME/env.txt"
    ${pkgs.wayland-utils}/bin/wayland-info > "$HOME/globals.txt" 2> "$HOME/wayland-info.err"
    ${pkgs.coreutils}/bin/touch "$HOME/done"
  '';
in
{
  imports = [ "${modulesPath}/virtualisation/qemu-vm.nix" ];

  virtualisation = {
    memorySize = 4096;
    cores = 4;
    diskSize = 4096;
    # Serial console on stdio; the GPU is a virtio-gpu we add ourselves, with a qemu
    # monitor socket for screendumps.
    graphics = true;
    qemu.options = [
      "-vga none" "-device virtio-gpu-gl-pci"
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

  # The seat daemon: root, opens /dev/dri and /dev/input for members of `seat`, handles VTs.
  services.seatd.enable = true;
  hardware.graphics.enable = true;
  fonts.enableDefaultPackages = true;

  users.users.alice = {
    isNormalUser = true;
    uid = 1000;
    extraGroups = [ "seat" "video" "input" ];
  };
  users.groups.render = { };
  users.users.app-hello = {
    uid = 100001;
    group = "app-hello";
    isSystemUser = true;
  };
  users.groups.app-hello.gid = 100001;
  users.users.app-gpu = {
    uid = 100002;
    group = "app-gpu";
    isSystemUser = true;
  };
  users.groups.app-gpu.gid = 100002;
  users.users.app-shm = {
    uid = 100003;
    group = "app-shm";
    isSystemUser = true;
  };
  users.groups.app-shm.gid = 100003;
  users.users.app-sneaky = {
    uid = 100004;
    group = "app-sneaky";
    isSystemUser = true;
  };
  users.groups.app-sneaky.gid = 100004;
  users.users.app-chromium = {
    uid = 100005;
    group = "app-chromium";
    isSystemUser = true;
  };
  users.groups.app-chromium.gid = 100005;

  environment.etc."niri/identity.toml".text = ''
    forker = "/run/niri/forker.sock"

    [[app]]
    name = "session"
    uid = 1000
    trusted = true

    [[app]]
    name = "hello"
    uid = 100001
    exec = ["${probe}"]

    [[app]]
    name = "gpu-probe"
    uid = 100002
    exec = ["${probe}"]
    gpu = true
    groups = ["render"]

    [[app]]
    name = "flower"
    uid = 100003
    exec = ["${pkgs.weston}/bin/weston-flower"]

    # A real browser: GPU, its own home, flags come from here and never from the caller.
    [[app]]
    name = "chromium"
    uid = 100005
    exec = ["${pkgs.chromium}/bin/chromium", "--ozone-platform=wayland", "https://example.com"]
    gpu = true
    groups = ["render"]

    # Asks for a group the forker was not told to hand out: must be refused.
    [[app]]
    name = "sneaky"
    uid = 100004
    exec = ["${probe}"]
    groups = ["wheel"]
  '';

  environment.etc."niri/config.kdl".text = ''
    spawn-at-startup "hello"
    spawn-at-startup "gpu-probe"
    spawn-at-startup "flower"
    spawn-at-startup "sneaky"
    spawn-at-startup "hello" "extra-argument"
  '';

  environment.systemPackages = [
    niri
    pkgs.wayland-utils
    pkgs.weston
    pkgs.foot
  ];

  systemd.services.niri-forker = {
    wantedBy = [ "multi-user.target" ];
    serviceConfig = {
      ExecStart = "${niri}/bin/niri-forker --allow 1000:100000:1000:render --socket /run/niri/forker.sock --runtime-base /run/niri-app-runtime --home-base /var/lib/niri-apps";
      RuntimeDirectory = "niri";
      RuntimeDirectoryMode = "0755";
      # So the forker may create a cgroup per app under its own.
      Delegate = true;
    };
  };

  systemd.services.niri-identity = {
    wantedBy = [ "multi-user.target" ];
    after = [ "niri-forker.service" ];
    requires = [ "niri-forker.service" ];
    serviceConfig = {
      User = "alice";
      ExecStart = "${niri}/bin/niri-identityd --config /etc/niri/identity.toml --socket /run/niri-identity/identity.sock";
      RuntimeDirectory = "niri-identity";
    };
  };

  systemd.services.niri = {
    wantedBy = [ "multi-user.target" ];
    after = [ "niri-identity.service" "seatd.service" ];
    requires = [ "niri-identity.service" "seatd.service" ];
    environment = {
      LIBSEAT_BACKEND = "seatd";
      NIRI_IDENTITY_SOCKET = "/run/niri-identity/identity.sock";
      NIRI_APPS_SOCKET = "/run/niri-wayland/wayland";
      XDG_RUNTIME_DIR = "/run/niri-compositor";
      RUST_BACKTRACE = "1";
      RUST_LOG = "niri=debug";
    };
    serviceConfig = {
      User = "alice";
      SupplementaryGroups = [ "seat" "video" "input" ];
      ExecStart = "${niri}/bin/niri -c /etc/niri/config.kdl";
      RuntimeDirectory = "niri-compositor niri-wayland";
      # Apps as other UIDs must traverse the socket directory.
      RuntimeDirectoryMode = "0711";
      Restart = "no";
    };
  };

  system.stateVersion = "25.11";
}
