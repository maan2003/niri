# NixOS module for the multi-UID desktop: one app list becomes the passwd entries, the
# identity daemon's manifests, the forker's allow list and the three units. What an app may
# reach is its UID plus the groups and /run entries listed here; nothing else.
{ niri }:
{ config, lib, pkgs, ... }:
let
  cfg = config.services.niri-desktop;
  toml = pkgs.formats.toml { };
  appEntries = lib.mapAttrsToList (name: app: {
    inherit name;
    inherit (app) uid groups trusted gpu globals;
    exec = app.exec;
  } // lib.optionalAttrs (app.icon != null) { icon = app.icon; }) cfg.apps;
  identityFile = toml.generate "identity.toml" {
    forker = "/run/niri/forker.sock";
    env = cfg.env;
    app = [
      {
        name = "session";
        uid = config.users.users.${cfg.user}.uid;
        trusted = true;
      }
    ] ++ appEntries;
  };
  rangeEnd = cfg.uidRange.start + cfg.uidRange.count;
  humanUid = config.users.users.${cfg.user}.uid;
  inRange = uid: uid >= cfg.uidRange.start && uid < rangeEnd;
  # Apps in the range get a passwd entry; the human's own tools share the human's. Decided by
  # the range, not by looking up the human's uid, which would recurse through `users.users`.
  appUsers = lib.filterAttrs (_: app: inRange app.uid) cfg.apps;
  # One launcher entry per app. A launcher runs these as the human; the compositor then asks
  # the identity daemon, so the entry carries a name and nothing else.
  desktopEntries = pkgs.runCommand "niri-desktop-entries" { } (
    lib.concatStrings (lib.mapAttrsToList (name: app: ''
      mkdir -p $out/share/applications
      cat > $out/share/applications/${name}.desktop <<EOF
      [Desktop Entry]
      Type=Application
      Name=${name}
      Exec=${cfg.package}/bin/niri msg action spawn -- ${name}
      ${lib.optionalString (app.icon != null) "Icon=${app.icon}"}
      EOF
    '') cfg.apps)
  );
in
{
  options.services.niri-desktop = {
    enable = lib.mkEnableOption "the multi-UID niri desktop";
    package = lib.mkOption {
      type = lib.types.package;
      default = niri;
      description = "niri build with niri-forker and niri-identityd.";
    };
    user = lib.mkOption {
      type = lib.types.str;
      description = "The human: runs the compositor and the identity daemon; trusted.";
    };
    uidRange = {
      start = lib.mkOption { type = lib.types.int; default = 100000; };
      count = lib.mkOption { type = lib.types.int; default = 1000; };
    };
    groups = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "render" "pipewire" ];
      description = "Supplementary groups the forker may hand out to apps.";
    };
    expose = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ "/run/niri-wayland" "/run/opengl-driver" "/run/current-system" "/run/pipewire" "/run/pulse" ];
      description = "Entries of /run apps may see; the rest of /run is hidden.";
    };
    env = lib.mkOption {
      type = lib.types.attrsOf lib.types.str;
      default = {
        PIPEWIRE_RUNTIME_DIR = "/run/pipewire";
        PULSE_SERVER = "unix:/run/pulse/native";
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
          trusted = lib.mkOption { type = lib.types.bool; default = false; };
          gpu = lib.mkOption { type = lib.types.bool; default = false; };
          globals = lib.mkOption { type = lib.types.listOf lib.types.str; default = [ ]; };
          icon = lib.mkOption { type = lib.types.nullOr lib.types.str; default = null; };
        };
      }));
    };
  };

  config = lib.mkIf cfg.enable {
    assertions = lib.mapAttrsToList (name: app: {
      assertion = app.uid == humanUid || inRange app.uid;
      message = "services.niri-desktop.apps.${name}.uid ${toString app.uid} is outside ${toString cfg.uidRange.start}..${toString rangeEnd} and not the human's";
    }) cfg.apps ++ [
      {
        assertion = lib.length (lib.unique (map (a: a.uid) (lib.attrValues appUsers))) == lib.length (lib.attrValues appUsers);
        message = "services.niri-desktop.apps: two apps share a uid";
      }
      {
        assertion = lib.all (a: a.trusted) (lib.attrValues (lib.filterAttrs (_: a: a.uid == humanUid) cfg.apps));
        message = "services.niri-desktop.apps: an app on the human's uid must be trusted";
      }
    ];

    users.users = lib.mapAttrs' (name: app: lib.nameValuePair "app-${name}" {
      uid = app.uid;
      group = "app-${name}";
      isSystemUser = true;
    }) appUsers // {
      ${cfg.user}.extraGroups = [ "seat" "video" "input" ];
    };
    users.groups = lib.mapAttrs' (name: app: lib.nameValuePair "app-${name}" { gid = app.uid; }) appUsers
      // { render = { }; };

    services.seatd.enable = true;
    hardware.graphics.enable = true;
    services.pipewire = {
      enable = true;
      systemWide = true;
      pulse.enable = true;
      alsa.enable = true;
    };

    environment.etc."niri/identity.toml".source = identityFile;
    environment.etc."niri/config.kdl".text = cfg.config;
    environment.systemPackages = [ cfg.package desktopEntries ];

    systemd.services.niri-forker = {
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        ExecStart = lib.concatStringsSep " " ([
          "${cfg.package}/bin/niri-forker"
          "--allow ${toString config.users.users.${cfg.user}.uid}:${toString cfg.uidRange.start}:${toString cfg.uidRange.count}:${lib.concatStringsSep "," cfg.groups}"
          "--socket /run/niri/forker.sock"
          "--runtime-base /run/niri-app-runtime"
          "--home-base /var/lib/niri-apps"
        ] ++ map (p: "--expose ${p}") cfg.expose);
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
        User = cfg.user;
        ExecStart = "${cfg.package}/bin/niri-identityd --config /etc/niri/identity.toml --socket /run/niri-identity/identity.sock";
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
        User = cfg.user;
        SupplementaryGroups = [ "seat" "video" "input" ];
        ExecStart = "${cfg.package}/bin/niri -c /etc/niri/config.kdl";
        RuntimeDirectory = "niri-compositor niri-wayland";
        # Apps as other UIDs must traverse the socket directory.
        RuntimeDirectoryMode = "0711";
        Restart = "no";
      };
    };
  };
}
