# NixOS module for the multi-UID desktop. One app list becomes the passwd entries, drv-appd's
# manifest, the session bus policy and the units. Nothing shares a UID: the supervisor and the
# forker are root, and drv-appd, the compositor, the bridge, the session bus and every app
# each have their own. What a process may reach is its UID plus the groups, grants and /run
# entries listed here; nothing else.
{ niri }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.drv;
  json = pkgs.formats.json { };
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
    "--state-base /var/lib/drv-apps"
    "--host-views ${hostViews}"
  ]);
  hostViews = "/run/drv-host";
  # The ssh agent's door: in every app's root, answering only the UIDs with the `agent` grant.
  agentSocket = "/run/drv-agent/agent";
  # What XDG_DATA_DIRS points at: the MIME database and the icon theme's index, from the
  # store, in place of the whole system profile.
  appShare = pkgs.buildEnv {
    name = "drv-app-share";
    paths = [ pkgs.shared-mime-info pkgs.hicolor-icon-theme ];
    pathsToLink = [ "/share" ];
  };
  # The store path a string under the store belongs to, context kept: `${pkg}/bin/x` -> pkg.
  storeRoot = p: builtins.appendContext (builtins.head (builtins.match "(/nix/store/[^/]+).*" p)) (builtins.getContext p);
  # What an app may open in the store (DESIGN-app-namespace, "Store"): the closure of its
  # command, its /etc, the shared data profile, the graphics drivers, and the bus shim if it
  # has one. The forker turns the list into Landlock rules.
  appClosure = name: app: pkgs.closureInfo {
    rootPaths = [ (appEtc name app) appShare config.hardware.graphics.package ]
      ++ config.hardware.graphics.extraPackages
      ++ map storeRoot (lib.filter (lib.hasPrefix "/nix/store/") (appExec name app))
      ++ app.packages
      ++ lib.optional (app.files != { }) (appFiles name app);
  };
  # The command, as launched. In front of the app's own: the linker (its /etc from the store,
  # its state directories under $HOME/.state linked from HOME, the HOME defaults) and, for a
  # private bus, the compat shim (the bridge on it forwards to the
  # services' bus, which keys everything on the app's UID).
  appExec = name: app: [ "${cfg.package}/bin/drv-init" "--etc" "${appEtc name app}" ]
      ++ lib.concatMap (s: [ "--state" s ]) app.state
      ++ lib.optionals (app.files != { }) [ "--files" "${appFiles name app}" ]
      ++ [ "--" ]
    ++ lib.optionals app.bus [
      "${pkgs.dbus}/bin/dbus-run-session" "--dbus-daemon=${pkgs.dbus}/bin/dbus-daemon"
      # Its configuration from the store: the app's /etc has no dbus-1.
      "--config-file=${pkgs.dbus}/share/dbus-1/session.conf" "--"
      "${cfg.package}/bin/drv-bridge" "app" "--"
    ] ++ app.exec;
  # HOME defaults: a tree the state linker links into HOME entry by entry.
  appFiles = name: app: pkgs.runCommand "drv-files-${name}" { } (''
    mkdir "$out"
  '' + lib.concatStrings (lib.mapAttrsToList (path: value: ''
    mkdir -p "$out/$(dirname ${lib.escapeShellArg path})"
    ln -s ${if builtins.isString value then pkgs.writeText (baseNameOf path) value else value} "$out/"${lib.escapeShellArg path}
  '') app.files));
  # An app's /etc (DESIGN-app-namespace): what glibc, TLS and the toolkits look up, every
  # entry a store path or a fact about this app. The host's /etc is not there.
  appEtc = name: app: let
    fromHost = n: lib.optionalString (config.environment.etc ? ${n} && config.environment.etc.${n}.enable) ''
      mkdir -p "$out/$(dirname ${n})"
      ln -s ${config.environment.etc.${n}.source} "$out/${n}"
    '';
  in pkgs.runCommand "drv-etc-${name}" { } ''
    mkdir "$out"
    cd "$out"
    echo "app-${name}:x:${toString app.uid}:${toString app.uid}:${name}:/home/app:${pkgs.shadow}/bin/nologin" > passwd
    echo "app-${name}:x:${toString app.uid}:" > group
    printf 'passwd: files
group: files
hosts: files${lib.optionalString app.network " dns"}
' > nsswitch.conf
    printf '127.0.0.1 localhost
::1 localhost
' > hosts
    echo ${builtins.hashString "md5" "drv-app-${name}"} > machine-id
    ln -s ${pkgs.tzdata}/share/zoneinfo zoneinfo
    ln -s zoneinfo/${if config.time.timeZone != null then config.time.timeZone else "UTC"} localtime
    ${lib.optionalString app.network ''
      # The forker puts the host's live one there for a networked app.
      ln -s /run/host/resolv.conf resolv.conf
      mkdir -p ssl/certs
      ln -s ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt ssl/certs/ca-bundle.crt
      ln -s ${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt ssl/certs/ca-certificates.crt
    ''}
    ${lib.concatMapStrings fromHost (cfg.etc ++ app.etc)}
  '';
  rangeEnd = cfg.uidRange.start + cfg.uidRange.count;
  inRange = uid: uid >= cfg.uidRange.start && uid < rangeEnd;
  appEntries = lib.mapAttrsToList (name: app: {
    inherit name;
    inherit (app) uid gpu network audio jit userns globals grants autostart menu opens;
    closure = "${appClosure name app}/store-paths";
    env = lib.optionalAttrs (app.packages != [ ]) { PATH = lib.makeBinPath app.packages; }
      // lib.optionalAttrs app.agent { SSH_AUTH_SOCK = agentSocket; }
      // lib.optionalAttrs app.audio {
        PIPEWIRE_REMOTE = appsSocket;
        PULSE_SERVER = "unix:${pulseDir name}/native";
      } // app.env;
    exec = appExec name app;
  } // lib.optionalAttrs (app.icon != null) { icon = app.icon; }) cfg.apps;
  appdFile = json.generate "appd.json" {
    wayland-socket = "/run/drv-wayland/wayland";
    env = cfg.env;
    app = [
      # Services: identified, never launched. They may ask who other UIDs are.
      { name = "compositor"; uid = cfg.ids.compositor; grants = [ "lookup" ]; }
      { name = "bridge"; uid = cfg.ids.bridge; grants = [ "lookup" ]; }
    ] ++ appEntries;
  };
  # What the forker binds into apps' roots (its own view has to hold them): the layout its
  # feature table names, plus the audio sockets when any app has audio.
  appRun = [ "/run/drv-apps" hostViews "/run/drv" "/run/drv-wayland" "/run/drv-bridge" "/run/drv-doc" "/run/drv-agent" "/run/opengl-driver" ]
    ++ lib.optionals (lib.any (a: a.audio) (lib.attrValues cfg.apps)) [ "/run/drv-audio" "/run/drv-pulse" ];
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
      agent = lib.mkOption { type = lib.types.int; default = 914; };
      keys = lib.mkOption { type = lib.types.int; default = 915; };
    };
    resumePatch = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Patch the kernel to blank every plane on resume, so the old desktop is not shown before the compositor paints. Off where the kernel is not to be rebuilt.";
    };
    screenshots = lib.mkOption {
      type = lib.types.nullOr lib.types.str;
      default = null;
      example = "/var/lib/drv-screenshots";
      description = "A directory of the compositor's own to write screenshots to (point the config's screenshot-path into it). Not under `files`: that is the portal's, 0700. Screenshots also land on the clipboard.";
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
    homeVt = lib.mkOption {
      type = lib.types.nullOr lib.types.int;
      default = null;
      example = 1;
      description = "Once its outputs are up, the compositor switches to this VT: the desktop starts in the background and the host's own session (a getty, greetd) keeps the screen until Ctrl-Alt-F<vt>.";
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
    etc = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "fonts" "os-release" ];
      description = "Entries of the host's /etc (environment.etc names) copied into every app's /etc, which is otherwise generated per app.";
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
        XDG_DATA_DIRS = "${appShare}/share";
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
          etc = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            description = "Further entries of the host's /etc for this app.";
          };
          state = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            example = [ ".config/chromium" ".cache/chromium" ];
            description = "Paths under HOME that persist between runs (under /var/lib/drv-apps/<uid>). Everything else in HOME is gone with the run.";
          };
          files = lib.mkOption {
            type = lib.types.attrsOf (lib.types.either lib.types.str lib.types.path);
            default = { };
            description = "HOME defaults: path under HOME to its content (text or a path), linked from the store, read-only.";
          };
          jit = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "The app makes code at runtime (a browser's JIT): it is not held to W^X memory.";
          };
          userns = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "The app may make user namespaces (a browser's own sandbox). Every other app is refused them: they are the usual way to a kernel bug.";
          };
          globals = lib.mkOption { type = lib.types.listOf lib.types.str; default = [ ]; };
          grants = lib.mkOption {
            type = lib.types.listOf (lib.types.enum [ "lookup" ]);
            default = [ ];
            description = "Non-Wayland capabilities.";
          };
          env = lib.mkOption { type = lib.types.attrsOf lib.types.str; default = { }; };
          packages = lib.mkOption {
            type = lib.types.listOf lib.types.package;
            default = [ ];
            description = "Programs the app may run besides its command: in its closure and its PATH.";
          };
          agent = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "May use the ssh agent (drv-agent, a member of the set, which holds the keys): SSH_AUTH_SOCK points at its door and the door knows this UID.";
          };
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
      # The forker: not root. The supervisor leaves it setuid, setgid, setpcap and sys_admin,
      # and hands it the supervisor's cgroup subtree.
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
      # The ssh agent: holds the keys, opens the authenticators (udev makes their hidraw nodes
      # its group's), answers the UIDs with the grant.
      drv-agent = { uid = cfg.ids.agent; group = "drv-agent"; isSystemUser = true; };
      # The media keys: the default sink through PipeWire, the backlight through sysfs.
      drv-keys = { uid = cfg.ids.keys; group = "drv-keys"; isSystemUser = true; extraGroups = [ "pipewire" "video" ]; };
    };
    # The desktop's VT and tty0 (for switching to it), read-write for group tty, whose only
    # member is drv-seat. A getty's VT is no good: agetty resets it to 0620 on every start.
    services.udev.extraRules = ''
      SUBSYSTEM=="tty", KERNEL=="tty0", GROUP="tty", MODE="0660"
      SUBSYSTEM=="tty", KERNEL=="tty${toString cfg.vt}", GROUP="tty", MODE="0660"
      # FIDO authenticators (Yubico) are the ssh agent's; the backlight is the media keys'
      # (group video) to write.
      SUBSYSTEM=="hidraw", ATTRS{idVendor}=="1050", GROUP="drv-agent", MODE="0660"
      ACTION=="add", SUBSYSTEM=="backlight", RUN+="${pkgs.coreutils}/bin/chgrp video /sys/class/backlight/%k/brightness", RUN+="${pkgs.coreutils}/bin/chmod g+w /sys/class/backlight/%k/brightness"
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
      drv-agent.gid = cfg.ids.agent;
      drv-keys.gid = cfg.ids.keys;
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
    environment.etc."drv/appd.json".source = appdFile;
    # Suspend must not hand the old desktop back before the compositor paints: the kernel
    # resumes with every plane off until the first commit (see the patch).
    boot.kernelPatches = lib.mkIf cfg.resumePatch [ { name = "drm-blank-on-resume"; patch = ./linux-drm-blank-on-resume.patch; } ];
    # memfds are not executable unless asked for (MFD_EXEC), and the forker's seccomp filter
    # refuses apps the asking: with the noexec mounts, the store is the only place code runs from.
    boot.kernel.sysctl."vm.memfd_noexec" = 1;
    boot.kernelParams = lib.mkIf cfg.resumePatch [ "drm_kms_helper.blank_on_resume=1" ];

    environment.etc."drv/config.kdl".text = cfg.config;
    # Low priority: a stock niri may be installed next to it for a session of the host's own.
    environment.systemPackages = [ (lib.lowPrio cfg.package) ];

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
    # The host's views of itself for the apps' roots (DESIGN-app-namespace): a /dev of the
    # basic nodes, a /dev/dri of the render nodes, and a /sys of what Mesa and libdrm read
    # (the render node's device and its bus, the CPU topology), copied out of the real ones
    # once. The forker binds them; the real /dev and /sys never enter an app.
    systemd.services.drv-host-views = {
      wantedBy = [ "multi-user.target" ];
      after = [ "systemd-udevd.service" "local-fs.target" ];
      before = [ "drv-supervisor.service" ];
      serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
      path = [ pkgs.coreutils ];
      script = ''
        set -eu
        T=$(mktemp -d ${hostViews}.XXXXXX)
        chmod 755 "$T"
        mkdir -p "$T"/dev/shm "$T"/dev/dri "$T"/dev-gpu/dri "$T"/sys "$T"/sys-gpu
        for n in null:1:3 zero:1:5 full:1:7 random:1:8 urandom:1:9; do
          IFS=: read -r name maj min <<< "$n"
          mknod -m 666 "$T/dev/$name" c "$maj" "$min"
        done
        ln -s /proc/self/fd "$T"/dev/fd
        for i in 0:stdin 1:stdout 2:stderr; do ln -s "/proc/self/fd/''${i%%:*}" "$T/dev/''${i##*:}"; done
        # One entry of the real /sys, at the same place: links as links, files by content.
        take() {
          local dst="$1''${2#/sys}"
          if [ -L "$2" ]; then mkdir -p "$(dirname "$dst")"; cp -P "$2" "$dst"
          elif [ -d "$2" ]; then mkdir -p "$dst"
          elif [ -f "$2" ]; then mkdir -p "$(dirname "$dst")"; cp "$2" "$dst" 2>/dev/null || true
          fi
        }
        # The CPU topology: counts, capacities, caches (Mesa and the toolkits size their
        # thread pools from them). Once, then the same tree for both views.
        cpu=/sys/devices/system/cpu
        for f in possible online present kernel_max; do take "$T"/sys $cpu/$f; done
        for c in $cpu/cpu[0-9]*; do
          take "$T"/sys "$c"/cpu_capacity
          for d in topology cache; do
            [ -d "$c/$d" ] || continue
            mkdir -p "$T/sys''${c#/sys}/$d"
            cp -rP --no-preserve=all "$c/$d"/. "$T/sys''${c#/sys}/$d/" 2>/dev/null || true
          done
        done
        cp -rP --no-preserve=all "$T"/sys/. "$T"/sys-gpu/
        for r in /dev/dri/renderD*; do
          [ -e "$r" ] || continue
          # A render node is safe for anyone (that is what render nodes are for): no group.
          cp -a "$r" "$T"/dev-gpu/dri/ && chmod 666 "$T"/dev-gpu/dri/"$(basename "$r")"
          link=/sys/dev/char/$(printf '%d:%d' "0x$(stat -c %t "$r")" "0x$(stat -c %T "$r")")
          take "$T"/sys-gpu "$link"
          node=$(readlink -f "$link")
          for f in "$node"/dev "$node"/uevent "$node"/device "$node"/subsystem; do take "$T"/sys-gpu "$f"; done
          dev=$(readlink -f "$node"/device)
          while [ "$dev" != /sys/devices ] && [ "$dev" != /sys ] && [ "$dev" != / ]; do
            for f in uevent subsystem driver vendor device subsystem_vendor subsystem_device revision class modalias; do
              [ -e "$dev/$f" ] && take "$T"/sys-gpu "$dev/$f"
            done
            if [ -e "$dev"/of_node/compatible ]; then
              mkdir -p "$T/sys-gpu''${dev#/sys}/of_node"
              cp "$dev"/of_node/compatible "$T/sys-gpu''${dev#/sys}/of_node/"
            fi
            dev=$(dirname "$dev")
          done
        done
        rm -rf ${hostViews}.old
        [ -e ${hostViews} ] && mv ${hostViews} ${hostViews}.old
        mv "$T" ${hostViews}
        rm -rf ${hostViews}.old
      '';
    };

    systemd.services.drv-supervisor = {
      wantedBy = [ "multi-user.target" ];
      after = [ "drv-session-bus.service" "drv-host-views.service" ];
      requires = [ "drv-session-bus.service" "drv-host-views.service" ];
      serviceConfig = {
        User = "drv-supervisor";
        # setuid/setgid/setpcap: become each child's user with its own bounding set; chown:
        # the forker's cgroup subtree, once at startup, then dropped for good; sys_admin for
        # the members' roots and to hand on to the forker, sys_tty_config to hand on to seatd.
        # No kill: the members die by their cgroup's kill switch.
        AmbientCapabilities = [ "CAP_SETUID" "CAP_SETGID" "CAP_SETPCAP" "CAP_CHOWN" "CAP_SYS_ADMIN" "CAP_SYS_TTY_CONFIG" ];
        CapabilityBoundingSet = [ "CAP_SETUID" "CAP_SETGID" "CAP_SETPCAP" "CAP_CHOWN" "CAP_SYS_ADMIN" "CAP_SYS_TTY_CONFIG" ];
        NoNewPrivileges = true;
        # The tty udev rules (above) run on device events only: on a system switched to this
        # configuration while running, the nodes keep their old mode until an event. Raise
        # one. As root (the `+`).
        ExecStartPre = [ "+${config.systemd.package}/bin/udevadm trigger --action=change --settle /dev/tty0 /dev/tty${toString cfg.vt}" ];
        ExecStart = lib.concatStringsSep " " ([
          "${cfg.package}/bin/drv-supervisor"
          "--socket ${appdSocket}"
          "--appd-user drv-appd"
          "--appd-exec '${cfg.package}/bin/drv-appd --config /etc/drv/appd.json'"
          # drv-appd's privileged helper: forks once per request over the channel the
          # supervisor made for the two of them, before reading it; the child builds the
          # app's root and becomes the UID. Every directory it binds already exists
          # (tmpfiles, below): it makes and chowns nothing.
          "--forker-user drv-forker"
          "--forker-exec '${forkerExec}'"
          "--forker-dir /run/drv-apps:0711"
          "--forker-dir /var/lib/drv-apps:0711"
        ] ++ map (c: "--forker-cap ${c}") [ "setuid" "setgid" "setpcap" "sys_admin" ]
          # Every member of the set gets the apps' sandbox (drv_os::sandbox): its /run holds
          # only what is listed for it. The forker's must hold what it binds for the apps.
          ++ map (p: "--forker-expose ${p}") appRun
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
        ] ++ lib.optionals (cfg.homeVt != null) [
          # With its outputs up, the compositor switches away: the desktop starts in the background.
          "--compositor-env DRV_HOME_VT=${toString cfg.homeVt}"
        ] ++ [
          # Apps as other UIDs must traverse the socket directory.
          "--compositor-dir /run/drv-compositor:0711"
          "--compositor-dir /run/drv-wayland:0711"
        ] ++ lib.optionals (cfg.screenshots != null) [
          "--compositor-dir ${cfg.screenshots}:0755"
        ] ++ [
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
          "--portal-dir ${cfg.files}:0700"
          "--docs /run/drv-doc"
          # The notification daemon: on the services' bus, where it alone owns
          # org.freedesktop.Notifications; the bridge forwards apps' notifications to it under
          # their manifest names. Its Wayland connection is the supervisor's fd 3.
          "--notifier-user drv-notifier"
          "--notifier-exec '${lib.concatStringsSep " " cfg.notifier}'"
          "--notifier-env WAYLAND_SOCKET=3"
          "--notifier-env DBUS_SESSION_BUS_ADDRESS=${sessionBus}"
          "--notifier-expose /run/drv-session"
          # The ssh agent: OpenSSH's, behind a door that admits the UIDs with the `agent`
          # grant. Its socket directory is a tmpfiles rule; udev's database is for libfido2
          # to find the authenticators.
          "--agent-user drv-agent"
          "--agent-exec '${cfg.package}/bin/drv-agent serve --listen ${agentSocket} --ssh-agent ${pkgs.openssh}/bin/ssh-agent --ssh-add ${pkgs.openssh}/bin/ssh-add${
            lib.concatMapStrings (a: " --allow ${toString a.uid}") (lib.filter (a: a.agent) (lib.attrValues cfg.apps))}'"
          "--agent-dir /run/drv-agent:0711"
          "--agent-expose /run/udev"
          # The media keys, on the compositor's word: the volume through PipeWire (group
          # pipewire), the backlight through sysfs (group video, its /sys writable).
          "--keys-user drv-keys"
          "--keys-exec '${cfg.package}/bin/drv-keys --wpctl ${pkgs.wireplumber}/bin/wpctl'"
          "--keys-env PIPEWIRE_RUNTIME_DIR=/run/pipewire"
          "--keys-expose /run/pipewire"
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
        # Ours: the public sockets and the documents mount. Every directory a member owns is
        # a tmpfiles rule below (the supervisor checks owner and mode and makes nothing):
        # RuntimeDirectory= and StateDirectory= would chown them to us on every start.
        RuntimeDirectory = [ "drv" "drv-bridge" "drv-doc" ];
        RuntimeDirectoryMode = "0755";
        # Our cgroup subtree becomes ours (then the forker's): one cgroup per app under it.
        Delegate = true;
      };
    };

    # The apps' directories, one set per UID, made here so the forker never makes or chowns
    # anything: the runtime directory, /tmp and what persists (HOME/.state).
    systemd.tmpfiles.rules = [
      "d /run/drv-apps 0711 drv-forker drv-forker -"
      "d /run/drv-compositor 0711 drv-compositor drv-compositor -"
      "d /run/drv-wayland 0711 drv-compositor drv-compositor -"
      "d /run/drv-apps/tmp 0711 drv-forker drv-forker -"
      "d /run/drv-audio 0755 pipewire pipewire -"
      "d /var/lib/drv-apps 0711 drv-forker drv-forker -"
      "d /var/lib/drv-auth 0700 drv-auth drv-auth -"
      "d ${cfg.files} 0700 drv-portal drv-portal -"
      "d /run/drv-agent 0711 drv-agent drv-agent -"
    ] ++ lib.optional (cfg.screenshots != null) "d ${cfg.screenshots} 0755 drv-compositor drv-compositor -"
    ++ lib.concatMap (app: let u = toString app.uid; in [
      "d /run/drv-apps/${u} 0700 ${u} ${u} -"
      "d /run/drv-apps/tmp/${u} 0700 ${u} ${u} -"
      "d /var/lib/drv-apps/${u} 0700 ${u} ${u} -"
    ]) (lib.attrValues cfg.apps);

  } ]);
}
