# NixOS module for the multi-UID desktop. One app list becomes the passwd entries, the identity
# daemon's manifest, the session bus policy and the units. Nothing shares a UID: the spawner is
# root, and the identity daemon, the compositor, the bridge, the session bus and every app
# each have their own. What a process may reach is its UID plus the groups, grants and /run
# entries listed here; nothing else.
{ niri }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.drv;
  toml = pkgs.formats.toml { };
  bridgeSocket = "/run/drv-bridge/bridge.sock";
  sessionBus = "unix:path=/run/drv-session/bus";
  identitySocket = "/run/drv/identity.sock";
  rangeEnd = cfg.uidRange.start + cfg.uidRange.count;
  inRange = uid: uid >= cfg.uidRange.start && uid < rangeEnd;
  appEntries = lib.mapAttrsToList (name: app: {
    inherit name;
    inherit (app) uid groups gpu network globals grants autostart;
    env = lib.optionalAttrs app.servicesBus { DBUS_SESSION_BUS_ADDRESS = sessionBus; } // app.env;
    expose = lib.optional app.servicesBus "/run/drv-session" ++ app.expose;
    # A private bus is a compat shim: the bridge on it forwards to the services' bus, which
    # keys everything on the app's UID.
    exec = lib.optionals app.bus [
      "${pkgs.dbus}/bin/dbus-run-session" "--dbus-daemon=${pkgs.dbus}/bin/dbus-daemon" "--"
      "${cfg.package}/bin/drv-bridge" "app" "--"
    ] ++ app.exec;
  } // lib.optionalAttrs (app.icon != null) { icon = app.icon; }) cfg.apps;
  identityFile = toml.generate "identity.toml" {
    wayland-socket = "/run/drv-wayland/wayland";
    env = cfg.env;
    app = [
      # Services: identified, never launched. They may ask who other UIDs are.
      { name = "compositor"; uid = cfg.ids.compositor; grants = [ "lookup" ]; }
      { name = "bridge"; uid = cfg.ids.bridge; grants = [ "lookup" ]; }
    ] ++ appEntries;
  };
  # One launcher entry per app, run by whoever: launch is not a privilege. The file is named
  # `drv.app.<name>.desktop`: the app id the bridge registers with the portals.
  desktopEntries = pkgs.runCommand "drv-desktop-entries" { } (
    lib.concatStrings (lib.mapAttrsToList (name: app: ''
      mkdir -p $out/share/applications
      cat > $out/share/applications/drv.app.${name}.desktop <<EOF
      [Desktop Entry]
      Type=Application
      Name=${name}
      Exec=${cfg.package}/bin/drv launch ${name}
      ${lib.optionalString (app.icon != null) "Icon=${app.icon}"}
      EOF
    '') cfg.apps)
  );
  # Names only the compositor owns, and only screencast-granted users may call.
  compositorNames = [
    "org.gnome.Mutter.ScreenCast" "org.gnome.Mutter.ServiceChannel" "org.gnome.Mutter.DisplayConfig"
    "org.gnome.Shell.Screenshot" "org.gnome.Shell.Introspect" "org.freedesktop.ScreenSaver"
    "org.freedesktop.a11y.Manager"
  ];
  # Everything any app may ask to see; the spawner refuses anything else.
  optionalExpose = lib.unique (lib.concatMap (a: lib.optional a.servicesBus "/run/drv-session" ++ a.expose) (lib.attrValues cfg.apps));
  xmlRules = f: names: lib.concatMapStrings (n: "    ${f n}\n") names;
  # The services' bus: distinct UIDs, so the bus itself says who may own what. Defense in
  # depth: the compositor checks its callers' grants too.
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
    ${xmlRules (n: "<deny send_destination=\"${n}\"/>") compositorNames}
      </policy>
      <policy user="drv-compositor">
    ${xmlRules (n: "<allow own=\"${n}\"/>") compositorNames}
      </policy>
    ${lib.concatStrings (lib.mapAttrsToList (name: app: ''
      <policy user="app-${name}">
    ${xmlRules (n: "<allow own=\"${n}\"/>") app.sessionBusNames}
    ${lib.optionalString (lib.elem "screencast" app.grants)
        (xmlRules (n: "<allow send_destination=\"${n}\"/>") compositorNames)}
      </policy>
    '') cfg.apps)}
    </busconfig>
  '';
in
{
  options.services.drv = {
    enable = lib.mkEnableOption "the multi-UID desktop";
    package = lib.mkOption {
      type = lib.types.package;
      default = niri;
      description = "niri build with drv-spawnd, drv and drv-bridge.";
    };
    portalPackage = lib.mkOption {
      type = lib.types.package;
      # The portal cannot look into its callers' /proc across UIDs; the patch makes it treat
      # them as host apps instead of refusing them.
      default = pkgs.xdg-desktop-portal.overrideAttrs (old: {
        patches = (old.patches or [ ]) ++ [ ./xdg-desktop-portal-cross-uid.patch ];
      });
      description = "xdg-desktop-portal frontend, patched for callers on other UIDs.";
    };
    ids = {
      identity = lib.mkOption { type = lib.types.int; default = 901; };
      compositor = lib.mkOption { type = lib.types.int; default = 902; };
      bridge = lib.mkOption { type = lib.types.int; default = 903; };
      bus = lib.mkOption { type = lib.types.int; default = 904; };
    };
    uidRange = {
      start = lib.mkOption { type = lib.types.int; default = 100000; };
      count = lib.mkOption { type = lib.types.int; default = 1000; };
    };
    groups = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "render" "pipewire" ];
      description = "Supplementary groups the spawner may hand out to apps.";
    };
    expose = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "/run/drv" "/run/drv-wayland" "/run/drv-bridge" "/run/opengl-driver" "/run/current-system" "/run/pipewire" "/run/pulse" ];
      description = "Entries of /run apps may see; the rest of /run is hidden.";
    };
    env = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = {
        PIPEWIRE_RUNTIME_DIR = "/run/pipewire";
        PULSE_SERVER = "unix:/run/pulse/native";
        DRV_BRIDGE_SOCKET = bridgeSocket;
        DRV_IDENTITY_SOCKET = identitySocket;
        XDG_SESSION_TYPE = "wayland";
        XDG_DATA_DIRS = "/run/current-system/sw/share";
      };
      description = "Environment every app gets.";
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
          servicesBus = lib.mkOption {
            type = lib.types.bool;
            default = false;
            description = "A desktop service: sees the services' bus (notification daemon, portals).";
          };
          expose = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            description = "Extra entries of /run this app sees.";
          };
          sessionBusNames = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [ ];
            description = "Names the app may own on the services' bus (a notification daemon, a portal).";
          };
          globals = lib.mkOption { type = lib.types.listOf lib.types.str; default = [ ]; };
          grants = lib.mkOption {
            type = lib.types.listOf (lib.types.enum [ "lookup" "screencast" ]);
            default = [ ];
            description = "Non-Wayland capabilities; `screencast` is for the portal backend only.";
          };
          env = lib.mkOption { type = lib.types.attrsOf lib.types.str; default = { }; };
          icon = lib.mkOption { type = lib.types.nullOr lib.types.str; default = null; };
          autostart = lib.mkOption { type = lib.types.bool; default = false; };
        };
      }));
    };
  };

  config = lib.mkIf cfg.enable {
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
      drv-identity = { uid = cfg.ids.identity; group = "drv-identity"; isSystemUser = true; };
      drv-compositor = {
        uid = cfg.ids.compositor;
        group = "drv-compositor";
        isSystemUser = true;
        # Until a seat daemon hands out the devices.
        extraGroups = [ "seat" "video" "input" "pipewire" ];
      };
      drv-bridge = { uid = cfg.ids.bridge; group = "drv-bridge"; isSystemUser = true; };
      drv-bus = { uid = cfg.ids.bus; group = "drv-bus"; isSystemUser = true; };
    };
    users.groups = lib.mapAttrs' (name: app: lib.nameValuePair "app-${name}" { gid = app.uid; }) cfg.apps // {
      drv-identity.gid = cfg.ids.identity;
      drv-compositor.gid = cfg.ids.compositor;
      drv-bridge.gid = cfg.ids.bridge;
      drv-bus.gid = cfg.ids.bus;
      render = { };
    };

    services.seatd.enable = true;
    hardware.graphics.enable = true;
    services.pipewire = {
      enable = true;
      systemWide = true;
      pulse.enable = true;
      alsa.enable = true;
    };

    environment.etc."drv/identity.toml".source = identityFile;
    environment.etc."drv/config.kdl".text = cfg.config;
    environment.etc."xdg/xdg-desktop-portal/portals.conf".text = "[preferred]\ndefault=gnome\n";
    environment.systemPackages = [ cfg.package desktopEntries ];

    # The services' bus: the compositor, the bridge, the notification daemon and the portals,
    # each its own UID. Sandboxed apps never see it.
    systemd.services.drv-session-bus = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        User = "drv-bus";
        ExecStart = "${pkgs.dbus}/bin/dbus-daemon --nofork --nopidfile --config-file=${sessionBusConfig}";
        RuntimeDirectory = "drv-session";
        RuntimeDirectoryMode = "0755";
      };
    };

    # Root. Forks the identity daemon over a socketpair and starts apps for it.
    systemd.services.drv-spawnd = {
      wantedBy = [ "multi-user.target" ];
      after = [ "drv-session-bus.service" ];
      requires = [ "drv-session-bus.service" ];
      serviceConfig = {
        ExecStart = lib.concatStringsSep " " ([
          "${cfg.package}/bin/drv-spawnd"
          "--identity-user drv-identity"
          "--identity-config /etc/drv/identity.toml"
          "--socket ${identitySocket}"
          "--range ${toString cfg.uidRange.start}:${toString cfg.uidRange.count}"
          "--runtime-base /run/drv-apps"
          "--home-base /var/lib/drv-apps"
        ] ++ map (g: "--group ${g}") cfg.groups ++ map (p: "--expose ${p}") cfg.expose
          ++ map (p: "--expose-optional ${p}") optionalExpose);
        RuntimeDirectory = "drv";
        RuntimeDirectoryMode = "0755";
        # So the spawner may create a cgroup per app under its own.
        Delegate = true;
      };
    };

    systemd.services.drv-bridge = {
      wantedBy = [ "multi-user.target" ];
      after = [ "drv-spawnd.service" "drv-session-bus.service" ];
      requires = [ "drv-spawnd.service" "drv-session-bus.service" ];
      environment = {
        DBUS_SESSION_BUS_ADDRESS = sessionBus;
        DRV_IDENTITY_SOCKET = identitySocket;
      };
      serviceConfig = {
        User = "drv-bridge";
        ExecStart = "${cfg.package}/bin/drv-bridge serve --socket ${bridgeSocket}";
        RuntimeDirectory = "drv-bridge";
        RuntimeDirectoryMode = "0755";
        Restart = "on-failure";
        RestartSec = 1;
      };
    };

    systemd.services.drv-compositor = {
      wantedBy = [ "multi-user.target" ];
      after = [ "drv-spawnd.service" "drv-session-bus.service" "drv-bridge.service" "seatd.service" ];
      # Apps autostart once the compositor's socket exists; the desktop services among them
      # (`servicesBus`) need the bus, which is up before us.
      requires = [ "drv-spawnd.service" "drv-session-bus.service" "seatd.service" ];
      environment = {
        DBUS_SESSION_BUS_ADDRESS = sessionBus;
        # Screencasts go to the system PipeWire, like everyone's audio.
        PIPEWIRE_RUNTIME_DIR = "/run/pipewire";
        LIBSEAT_BACKEND = "seatd";
        DRV_IDENTITY_SOCKET = identitySocket;
        DRV_APPS_SOCKET = "/run/drv-wayland/wayland";
        XDG_RUNTIME_DIR = "/run/drv-compositor";
        RUST_BACKTRACE = "1";
        RUST_LOG = "niri=debug";
      };
      serviceConfig = {
        User = "drv-compositor";
        ExecStart = "${cfg.package}/bin/niri -c /etc/drv/config.kdl";
        RuntimeDirectory = "drv-compositor drv-wayland";
        # Apps as other UIDs must traverse the socket directory.
        RuntimeDirectoryMode = "0711";
        Restart = "no";
      };
    };
  };
}
