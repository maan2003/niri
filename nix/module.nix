# NixOS module for the multi-UID desktop. One app list becomes the passwd entries, drv-appd's
# manifest, the session bus policy and the units. Nothing shares a UID: the supervisor and the
# forker are root, and drv-appd, the compositor, the bridge, the session bus and every app
# each have their own. What a process may reach is its UID plus the groups, grants and /run
# entries listed here; nothing else.
{ niri }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.drv;
  toml = pkgs.formats.toml { };
  bridgeSocket = "/run/drv-bridge/bridge.sock";
  sessionBus = "unix:path=/run/drv-session/bus";
  appdSocket = "/run/drv/appd.sock";
  # Sound. Apps reach PipeWire through this socket alone (the daemon marks its clients
  # "drv-app", and WirePlumber's drv-access.lua decides what they may do), and PulseAudio
  # through a pipewire-pulse of their own, run as their UID, so what it does is theirs.
  appsSocket = "/run/drv-audio/apps";
  pulseDir = name: "/run/drv-pulse/${name}";
  pulseConfig = name: pkgs.writeTextDir "pipewire/pipewire-pulse.conf.d/10-drv.conf" ''
    pulse.properties = {
      server.address = [ "unix:${pulseDir name}/native" ]
      pulse.allow-module-loading = false
    }
  '';
  forkerExec = lib.concatStringsSep " " ([
    "${cfg.package}/bin/drv-forker"
    "--range ${toString cfg.uidRange.start}:${toString cfg.uidRange.count}"
    "--runtime-base /run/drv-apps"
    "--home-base /var/lib/drv-apps"
  ] ++ map (g: "--group ${g}") cfg.groups ++ map (p: "--expose ${p}") cfg.expose
    ++ map (p: "--expose-optional ${p}") optionalExpose);
  rangeEnd = cfg.uidRange.start + cfg.uidRange.count;
  inRange = uid: uid >= cfg.uidRange.start && uid < rangeEnd;
  appEntries = lib.mapAttrsToList (name: app: {
    inherit name;
    inherit (app) uid groups gpu network globals grants autostart menu opens;
    env = app.env // lib.optionalAttrs app.audio {
      PIPEWIRE_REMOTE = appsSocket;
      PULSE_SERVER = "unix:${pulseDir name}/native";
    };
    expose = app.expose ++ lib.optionals app.audio [ "/run/drv-audio" "/run/drv-pulse" ];
    # A private bus is a compat shim: the bridge on it forwards to the services' bus, which
    # keys everything on the app's UID.
    exec = lib.optionals app.bus [
      "${pkgs.dbus}/bin/dbus-run-session" "--dbus-daemon=${pkgs.dbus}/bin/dbus-daemon" "--"
      "${cfg.package}/bin/drv-bridge" "app" "--"
    ] ++ app.exec;
  } // lib.optionalAttrs (app.icon != null) { icon = app.icon; }) cfg.apps;
  appdFile = toml.generate "appd.toml" {
    wayland-socket = "/run/drv-wayland/wayland";
    env = cfg.env;
    app = [
      # Services: identified, never launched. They may ask who other UIDs are.
      { name = "compositor"; uid = cfg.ids.compositor; grants = [ "lookup" ]; }
      { name = "bridge"; uid = cfg.ids.bridge; grants = [ "lookup" ]; }
    ] ++ appEntries;
  };
  # Everything any app may ask to see; the spawner refuses anything else.
  optionalExpose = lib.unique (lib.concatMap (a: a.expose ++ lib.optionals a.audio [ "/run/drv-audio" "/run/drv-pulse" ]) (lib.attrValues cfg.apps));
  # The services' bus: distinct UIDs, so the bus itself says who may own what: the
  # notification daemon its name, nobody else anything.
  sessionBusConfig = pkgs.writeText "drv-session-bus.conf" ''
    <!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
     "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
    <busconfig>
      <type>session</type>
      <listen>${sessionBus}</listen>
      <auth>EXTERNAL</auth>
      <policy context="default">
        <allow user="*"/>
        <allow send_destination="*"/>
        <allow receive_sender="*"/>
        <deny own="*"/>
      </policy>
      <policy user="drv-notifier">
        <allow own="org.freedesktop.Notifications"/>
      </policy>
    </busconfig>
  '';
in
{
  options.services.drv = {
    enable = lib.mkEnableOption "the multi-UID desktop";
    debug = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Log every portal call apps make to the bridge.";
    };
    package = lib.mkOption {
      type = lib.types.package;
      default = niri;
      description = "niri build with drv-supervisor, drv-appd, drv-forker, drv and drv-bridge.";
    };
    ids = {
      appd = lib.mkOption { type = lib.types.int; default = 901; };
      compositor = lib.mkOption { type = lib.types.int; default = 902; };
      bridge = lib.mkOption { type = lib.types.int; default = 903; };
      portal = lib.mkOption { type = lib.types.int; default = 912; };
      bus = lib.mkOption { type = lib.types.int; default = 904; };
      gpu = lib.mkOption { type = lib.types.int; default = 905; };
      auth = lib.mkOption { type = lib.types.int; default = 906; };
      seat = lib.mkOption { type = lib.types.int; default = 907; };
      lock = lib.mkOption { type = lib.types.int; default = 908; };
      forker = lib.mkOption { type = lib.types.int; default = 909; };
      supervisor = lib.mkOption { type = lib.types.int; default = 910; };
      menu = lib.mkOption { type = lib.types.int; default = 911; };
      notifier = lib.mkOption { type = lib.types.int; default = 913; };
    };
    notifier = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "${pkgs.mako}/bin/mako" ];
      description = "The notification daemon's command line: a member of the set, the one owner of org.freedesktop.Notifications on the services' bus, its Wayland connection on fd 3.";
    };
    vt = lib.mkOption {
      type = lib.types.int;
      default = 7;
      description = "The VT the desktop runs on; keep it above logind's autovt range (6).";
    };
    idleTimeout = lib.mkOption {
      type = lib.types.int;
      default = 300;
      description = "Seconds without input before the session locks again.";
    };
    uidRange = {
      start = lib.mkOption { type = lib.types.int; default = 100000; };
      count = lib.mkOption { type = lib.types.int; default = 1000; };
    };
    groups = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "render" ];
      description = "Supplementary groups the spawner may hand out to apps.";
    };
    expose = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "/run/drv" "/run/drv-wayland" "/run/drv-bridge" "/run/drv-doc" "/run/opengl-driver" "/run/current-system" ];
      description = "Entries of /run apps may see; the rest of /run is hidden.";
    };
    files = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/drv-files";
      description = "The person's files: owned by drv-portal, shown by its file chooser, handed to apps one at a time through /run/drv-doc.";
    };
    env = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = {
        DRV_BRIDGE_SOCKET = bridgeSocket;
        DRV_APPD_SOCKET = appdSocket;
        XDG_SESSION_TYPE = "wayland";
        XDG_DATA_DIRS = "/run/current-system/sw/share";
        # GTK asks the portal for files instead of browsing a home that holds nothing.
        GTK_USE_PORTAL = "1";
      };
      description = "Environment every app gets.";
    };
    gpuEnv = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = { };
      description = "Extra environment for the compositor's GPU process (Mesa knobs, say).";
    };
    config = lib.mkOption {
      type = lib.types.lines;
      default = "";
      description = "The compositor's config.kdl.";
    };
    apps = lib.mkOption {
      default = { };
      description = "Named apps, each with a fixed UID from the range.";
      type = lib.types.attrsOf (lib.types.submodule ({ name, ... }: {
        options = {
          uid = lib.mkOption { type = lib.types.int; };
          exec = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ name ];
            description = "Program and arguments; the only arguments the app ever gets.";
          };
          groups = lib.mkOption { type = lib.types.listOf lib.types.str; default = [ ]; };
          gpu = lib.mkOption { type = lib.types.bool; default = false; };
          network = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Keep the host network; otherwise an empty network namespace.";
          };
          bus = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "Give the app a private session bus with the bridge shim on it (notifications).";
          };
          audio = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "PipeWire and a PulseAudio server of its own: playback freely, capture when the person allows it.";
          };
          expose = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            description = "Extra entries of /run this app sees.";
          };
          globals = lib.mkOption { type = lib.types.listOf lib.types.str; default = [ ]; };
          grants = lib.mkOption {
            type = lib.types.listOf (lib.types.enum [ "lookup" ]);
            default = [ ];
            description = "Non-Wayland capabilities.";
          };
          env = lib.mkOption { type = lib.types.attrsOf lib.types.str; default = { }; };
          icon = lib.mkOption { type = lib.types.nullOr lib.types.str; default = null; };
          autostart = lib.mkOption { type = lib.types.bool; default = false; };
          menu = lib.mkOption {
            type = lib.types.bool;
            default = true;
            description = "Listed by the app menu.";
          };
          opens = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            description = "URI schemes this app handles for the OpenURI portal (one handler per scheme); the URI becomes its last argument.";
          };
        };
      }));
    };
  };

  config = lib.mkIf cfg.enable (lib.mkMerge [ {
    # One PulseAudio server per audio app, as the app's uid: so PipeWire sees whose
    # streams they are.
    systemd.services = lib.mapAttrs' (name: app: lib.nameValuePair "drv-pulse-${name}" {
      description = "PulseAudio server of ${name}";
      wantedBy = [ "multi-user.target" ];
      wants = [ "pipewire.service" ];
      after = [ "pipewire.service" ];
      environment = {
        PIPEWIRE_REMOTE = appsSocket;
        PIPEWIRE_RUNTIME_DIR = pulseDir name;
        PULSE_RUNTIME_PATH = pulseDir name;
        XDG_CONFIG_HOME = pulseConfig name;
      };
      serviceConfig = {
        User = "app-${name}";
        Group = "app-${name}";
        RuntimeDirectory = "drv-pulse/${name}";
        RuntimeDirectoryMode = "0700";
        ExecStart = "${config.services.pipewire.package}/bin/pipewire -c pipewire-pulse.conf";
        # Until PipeWire listens.
        Restart = "always";
        RestartSec = 1;
        NoNewPrivileges = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        PrivateTmp = true;
      };
    }) (lib.filterAttrs (_: app: app.audio) cfg.apps);

  } {
    assertions = lib.mapAttrsToList (name: app: {
      assertion = inRange app.uid;
      message = "services.drv.apps.${name}.uid ${toString app.uid} is outside ${toString cfg.uidRange.start}..${toString rangeEnd}";
    }) cfg.apps ++ [
      {
        assertion = lib.length (lib.unique (map (a: a.uid) (lib.attrValues cfg.apps))) == lib.length (lib.attrValues cfg.apps);
        message = "services.drv.apps: two apps share a uid";
      }
    ];

    users.users = lib.mapAttrs' (name: app: lib.nameValuePair "app-${name}" {
      uid = app.uid;
      group = "app-${name}";
      isSystemUser = true;
    }) cfg.apps // {
      drv-appd = { uid = cfg.ids.appd; group = "drv-appd"; isSystemUser = true; };
      drv-compositor = {
        uid = cfg.ids.compositor;
        group = "drv-compositor";
        isSystemUser = true;
        # Devices come from drv-seatd; PipeWire is for screencasts.
        extraGroups = [ "pipewire" ];
      };
      # PipeWire is for the screencast remotes it hands to apps.
      drv-bridge = { uid = cfg.ids.bridge; group = "drv-bridge"; isSystemUser = true; extraGroups = [ "pipewire" ]; };
      drv-bus = { uid = cfg.ids.bus; group = "drv-bus"; isSystemUser = true; };
      # The GPU process. Mesa opens render nodes itself.
      drv-gpu = { uid = cfg.ids.gpu; group = "drv-gpu"; isSystemUser = true; extraGroups = [ "render" ]; };
      drv-auth = { uid = cfg.ids.auth; group = "drv-auth"; isSystemUser = true; };
      # The seat daemon: cards, evdev nodes and the VT are group-owned devices; the VT ioctls
      # come from CAP_SYS_TTY_CONFIG, which the supervisor leaves it.
      drv-seat = { uid = cfg.ids.seat; group = "drv-seat"; isSystemUser = true; extraGroups = [ "video" "input" "tty" ]; };
      # The lock screen: no devices, no sockets; everything it talks to comes down its wire.
      drv-lock = { uid = cfg.ids.lock; group = "drv-lock"; isSystemUser = true; };
      # The forker: not root. The supervisor leaves it setuid, setgid, setpcap, sys_admin and
      # chown, and hands it the supervisor's cgroup subtree and the apps' directories.
      drv-forker = { uid = cfg.ids.forker; group = "drv-forker"; isSystemUser = true; };
      # The supervisor: the capabilities its unit grants it (below), nothing else.
      drv-supervisor = { uid = cfg.ids.supervisor; group = "drv-supervisor"; isSystemUser = true; };
      # The app menu: a launcher because the supervisor handed it a channel; its Wayland
      # connection is a supervisor fd too.
      drv-menu = { uid = cfg.ids.menu; group = "drv-menu"; isSystemUser = true; };
      # The portal: owns the person's files, shows the chooser, serves the documents mount.
      drv-portal = { uid = cfg.ids.portal; group = "drv-portal"; isSystemUser = true; };
      # The notification daemon: sees every notification, so a member of the set, not an app.
      drv-notifier = { uid = cfg.ids.notifier; group = "drv-notifier"; isSystemUser = true; };
    };
    # The desktop's VT and tty0 (for switching to it), read-write for group tty, whose only
    # member is drv-seat. A getty's VT is no good: agetty resets it to 0620 on every start.
    services.udev.extraRules = ''
      SUBSYSTEM=="tty", KERNEL=="tty0", GROUP="tty", MODE="0660"
      SUBSYSTEM=="tty", KERNEL=="tty${toString cfg.vt}", GROUP="tty", MODE="0660"
    '';
    users.groups = lib.mapAttrs' (name: app: lib.nameValuePair "app-${name}" { gid = app.uid; }) cfg.apps // {
      drv-appd.gid = cfg.ids.appd;
      drv-compositor.gid = cfg.ids.compositor;
      drv-bridge.gid = cfg.ids.bridge;
      drv-bus.gid = cfg.ids.bus;
      drv-gpu.gid = cfg.ids.gpu;
      drv-auth.gid = cfg.ids.auth;
      drv-seat.gid = cfg.ids.seat;
      drv-lock.gid = cfg.ids.lock;
      drv-forker.gid = cfg.ids.forker;
      drv-supervisor.gid = cfg.ids.supervisor;
      drv-menu.gid = cfg.ids.menu;
      drv-portal.gid = cfg.ids.portal;
      drv-notifier.gid = cfg.ids.notifier;
      render = { };
    };

    hardware.graphics.enable = true;
    services.pipewire = {
      enable = true;
      systemWide = true;
      # Each audio app has a pipewire-pulse of its own (below).
      pulse.enable = false;
      alsa.enable = true;
      # The apps' socket: anyone may connect, and gets nothing until WirePlumber decides
      # (drv-access.lua). The daemon records which socket a client came through; the
      # client cannot change that.
      extraConfig.pipewire."50-drv" = {
        "module.protocol-native.args".sockets = [
          { name = "pipewire-0"; }
          # The bridge's line for grants and cameras.
          { name = "pipewire-0-manager"; mode = "0660"; }
          { name = appsSocket; mode = "0666"; }
        ];
        "module.access.args"."access.socket" = {
          "pipewire-0" = "unrestricted";
          "pipewire-0-manager" = "unrestricted";
          ${appsSocket} = "drv-app";
        };
      };
      wireplumber.extraScripts."drv-access.lua" = builtins.readFile ./drv-access.lua;
      wireplumber.extraConfig."50-drv-access" = {
        "wireplumber.components" = [
          { name = "drv-access.lua"; type = "script/lua"; provides = "script.drv-access"; }
        ];
        "wireplumber.profiles".main."script.drv-access" = "required";
      };
      # The bridge hands apps remotes cut down to a few nodes (a cast, the cameras).
      # WirePlumber grants every new client everything a moment after it connects, which
      # would undo that cut, so those clients get nothing from it. The bridge's own
      # connection uses the manager socket and is left alone.
      wireplumber.extraConfig."50-drv-bridge" = {
        "access.rules" = [
          {
            matches = [ { "pipewire.sec.uid" = toString cfg.ids.bridge; "pipewire.sec.socket" = "pipewire-0"; } ];
            actions.update-props.default_permissions = "-";
          }
        ];
      };
    };
    environment.etc."drv/appd.toml".source = appdFile;
    # Suspend must not hand the old desktop back before the compositor paints: the kernel
    # resumes with every plane off until the first commit (see the patch).
    boot.kernelPatches = [ { name = "drm-blank-on-resume"; patch = ./linux-drm-blank-on-resume.patch; } ];
    boot.kernelParams = [ "drm_kms_helper.blank_on_resume=1" ];

    environment.etc."drv/config.kdl".text = cfg.config;
    environment.systemPackages = [ cfg.package ];

    # The services' bus: the compositor, the bridge and the notification daemon, each its own
    # UID. Sandboxed apps never see it.
    systemd.services.drv-session-bus = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        User = "drv-bus";
        ExecStart = "${pkgs.dbus}/bin/dbus-daemon --nofork --nopidfile --config-file=${sessionBusConfig}";
        RuntimeDirectory = "drv-session";
        RuntimeDirectoryMode = "0755";
      };
    };

    # Starts the trusted set (drv-seatd, drv-authd, the compositor with its GPU process and
    # locker, drv-appd with its forker) as their own users, wires them with socketpairs and
    # restarts what dies; their logs land here. The compositor's environment is exactly what
    # is listed. Not root: it holds the union of what its children keep plus what switching
    # them takes, and nothing outside that bounding set.
    systemd.services.drv-supervisor = {
      wantedBy = [ "multi-user.target" ];
      after = [ "drv-session-bus.service" ];
      requires = [ "drv-session-bus.service" ];
      serviceConfig = {
        User = "drv-supervisor";
        # setuid/setgid/setpcap: become each child's user with its own bounding set; chown:
        # the children's directories and the forker's cgroup subtree; kill: stopping a group
        # member of another UID; sys_admin and sys_tty_config only to hand on to the forker
        # and seatd.
        AmbientCapabilities = [ "CAP_SETUID" "CAP_SETGID" "CAP_SETPCAP" "CAP_CHOWN" "CAP_KILL" "CAP_SYS_ADMIN" "CAP_SYS_TTY_CONFIG" ];
        CapabilityBoundingSet = [ "CAP_SETUID" "CAP_SETGID" "CAP_SETPCAP" "CAP_CHOWN" "CAP_KILL" "CAP_SYS_ADMIN" "CAP_SYS_TTY_CONFIG" ];
        NoNewPrivileges = true;
        ExecStart = lib.concatStringsSep " " ([
          "${cfg.package}/bin/drv-supervisor"
          "--socket ${appdSocket}"
          "--appd-user drv-appd"
          "--appd-exec '${cfg.package}/bin/drv-appd --config /etc/drv/appd.toml'"
          # drv-appd's privileged helper: forks one sandboxed app per request over the channel
          # the supervisor made for the two of them, checks UIDs, groups and /run entries
          # against these lists, and nothing else.
          "--forker-user drv-forker"
          "--forker-exec '${forkerExec}'"
          "--forker-dir /run/drv-apps:0711"
          "--forker-dir /var/lib/drv-apps:0711"
        ] ++ map (c: "--forker-cap ${c}") [ "setuid" "setgid" "setpcap" "sys_admin" "chown" ]
          # Every member of the set gets the apps' sandbox (drv_os::sandbox): its /run holds
          # only what is listed for it. The forker's must hold what it binds for the apps.
          ++ map (p: "--forker-expose ${p}") (lib.unique ([ "/run/drv-apps" ] ++ cfg.expose ++ optionalExpose))
          ++ map (p: "--seatd-expose ${p}") [ "/run/udev" ]
          # udev: libinput initialises the evdev devices seatd hands over from udev's database.
          ++ map (p: "--compositor-expose ${p}") [ "/run/udev" "/run/drv-compositor" "/run/drv-wayland" "/run/drv" "/run/drv-session" "/run/pipewire" ]
          # Mesa's drivers live behind this symlink.
          ++ map (p: "--gpu-expose ${p}") [ "/run/opengl-driver" ]
          ++ [
          # Verifies the lock PIN (argon2id in /var/lib/drv-auth, enrol with `drv-authd
          # set-pin`) and pushes the unlock straight to the compositor; the lock app only asks.
          # The only process on the seat: opens DRM and evdev nodes through libseat's builtin
          # backend and hands the fds to the compositor. Its own user with the device groups
          # and CAP_SYS_TTY_CONFIG for the VT.
          "--seatd-user drv-seat"
          "--seatd-cap sys_tty_config"
          "--seatd-exec '${cfg.package}/bin/drv-seatd --vt ${toString cfg.vt}'"
          "--seatd-env LIBSEAT_BACKEND=builtin"
          "--seatd-env RUST_BACKTRACE=1"
          "--seatd-env RUST_LOG=niri=debug"
          "--authd-user drv-auth"
          "--authd-exec '${cfg.package}/bin/drv-authd serve --state-dir /var/lib/drv-auth --idle-timeout ${toString cfg.idleTimeout}'"
          "--authd-dir /var/lib/drv-auth:0700"
          "--compositor-user drv-compositor"
          "--compositor-exec '${cfg.package}/bin/niri -c /etc/drv/config.kdl'"
          # Apps as other UIDs must traverse the socket directory.
          "--compositor-dir /run/drv-compositor:0711"
          "--compositor-dir /run/drv-wayland:0711"
          # DRM and Mesa, as drv-gpu (group render), sealed with seccomp once the compositor
          # has handed it the devices. One group with the compositor: either dying restarts both.
          "--gpu-user drv-gpu"
          "--gpu-exec '${cfg.package}/bin/niri gpu-process --mode drm'"
          # The lock screen, in the compositor's group: draws and takes the PIN, nothing more.
          # Its Wayland connection and its drv-authd connection come down its wire from the
          # supervisor; it has no socket to find and none finds it.
          "--locker-user drv-lock"
          "--locker-exec '${cfg.package}/bin/drv-lock'"
          "--locker-env RUST_BACKTRACE=1"
          # The app menu: draws the launchable names and launches the pick down its channel
          # to drv-appd when the compositor's `show-launcher` bind pokes it. Its Wayland
          # connection is a supervisor fd; it needs nothing under /run.
          "--menu-user drv-menu"
          "--menu-exec '${cfg.package}/bin/drv-menu'"
          "--menu-env RUST_BACKTRACE=1"
          # The file chooser and the documents mount (drv-portal): the supervisor mounts a
          # FUSE filesystem at /run/drv-doc and hands the portal its serving end; apps see
          # the files they were given under it, as their own UID only.
          "--portal-user drv-portal"
          "--portal-exec '${cfg.package}/bin/drv-portal --files ${cfg.files} --docs /run/drv-doc'"
          "--portal-env RUST_BACKTRACE=1"
          "--docs /run/drv-doc"
          # The notification daemon: on the services' bus, where it alone owns
          # org.freedesktop.Notifications; the bridge forwards apps' notifications to it under
          # their manifest names. Its Wayland connection is the supervisor's fd 3.
          "--notifier-user drv-notifier"
          "--notifier-exec '${lib.concatStringsSep " " cfg.notifier}'"
          "--notifier-env WAYLAND_SOCKET=3"
          "--notifier-env DBUS_SESSION_BUS_ADDRESS=${sessionBus}"
          "--notifier-expose /run/drv-session"
          # The bridge: the apps' desktop services, keyed on the peer UID. Notifications go
          # to the services' bus; the file chooser and the screencast go down its supervisor
          # link to drv-portal; settings it answers itself. PipeWire is for the screencast
          # remotes: a connection per share that sees the one node.
          "--bridge-user drv-bridge"
          "--bridge-exec '${cfg.package}/bin/drv-bridge serve'"
          "--bridge-socket ${bridgeSocket}"
          "--bridge-env DBUS_SESSION_BUS_ADDRESS=${sessionBus}"
          "--bridge-env DRV_APPD_SOCKET=${appdSocket}"
          "--bridge-env PIPEWIRE_RUNTIME_DIR=/run/pipewire"
          "--bridge-env RUST_BACKTRACE=1"
        ] ++ lib.optionals cfg.debug [
          "--bridge-env DRV_BRIDGE_TRACE=1"
        ] ++ map (p: "--bridge-expose ${p}") [ "/run/drv" "/run/drv-session" "/run/pipewire" ]
          ++ map (e: "--gpu-env ${e}") [
          # No home directory after the seal, so no shader cache on disk.
          "MESA_SHADER_CACHE_DISABLE=true"
          "MESA_GLSL_CACHE_DISABLE=true"
          "RUST_BACKTRACE=1"
          "RUST_LOG=niri=debug"
        ] ++ lib.mapAttrsToList (n: v: "--gpu-env ${n}=${v}") cfg.gpuEnv
          ++ map (e: "--compositor-env ${e}") [
          "DBUS_SESSION_BUS_ADDRESS=${sessionBus}"
          # Screencasts go to the system PipeWire, like everyone's audio.
          "PIPEWIRE_RUNTIME_DIR=/run/pipewire"
          "DRV_APPD_SOCKET=${appdSocket}"
          "DRV_APPS_SOCKET=/run/drv-wayland/wayland"
          "XDG_RUNTIME_DIR=/run/drv-compositor"
          "RUST_BACKTRACE=1"
          "RUST_LOG=niri=debug"
        ]);
        # Fresh per start, made as ours and handed over (chown is ours, mkdir under /run is
        # not). What must outlive a start (the apps' directories, the PIN store) is a tmpfiles
        # rule below: StateDirectory= would chown their contents to us on every start.
        RuntimeDirectory = [ "drv" "drv-compositor" "drv-wayland" "drv-bridge" "drv-doc" ];
        RuntimeDirectoryMode = "0755";
        # Our cgroup subtree becomes ours (then the forker's): one cgroup per app under it.
        Delegate = true;
      };
    };

    systemd.tmpfiles.rules = [
      "d /run/drv-apps 0711 drv-forker drv-forker -"
      "d /run/drv-audio 0755 pipewire pipewire -"
      "d /var/lib/drv-apps 0711 drv-forker drv-forker -"
      "d /var/lib/drv-auth 0700 drv-auth drv-auth -"
      "d ${cfg.files} 0700 drv-portal drv-portal -"
    ];

  } ]);
}
