# The m2sh host's desktop (an Apple M2 Air on Asahi): the set from nix/module.nix and the
# person's apps as manifests. The nixos repo's hosts/m2sh.nix imports this next to greetd
# and the Home Manager desktop, with `services.drv.package = pkgs.niri-bin` (the binaries
# come prebuilt: nothing compiles here). Enrol the lock PIN once, as root:
# `drv-authd set-pin --state-dir /var/lib/drv-auth`. Load the authenticator's ssh keys from
# the terminal app: `drv-agent load`.
{ config, lib, pkgs, ... }:
let
  # The browser's managed policies (what programs.brave set): read from its /etc.
  policies = {
  AlternateErrorPagesEnabled = false; AutofillAddressEnabled = false; AutofillCreditCardEnabled = false;
  BackgroundModeEnabled = false; BlockThirdPartyCookies = true; BrowserSignin = 0; BraveP3AEnabled = false;
  BraveStatsPingEnabled = false; BraveWebDiscoveryEnabled = false; EnableMediaRouter = false;
  HyperlinkAuditingEnabled = false; MetricsReportingEnabled = false; NetworkPredictionOptions = 2;
  PasswordManagerEnabled = false; PaymentMethodQueryEnabled = false; PrivacySandboxAdMeasurementEnabled = false;
  PrivacySandboxAdTopicsEnabled = false; PrivacySandboxPromptEnabled = false;
  SafeBrowsingExtendedReportingEnabled = false; SafeBrowsingProtectionLevel = 0; SearchSuggestEnabled = false;
  TranslateEnabled = false; UrlKeyedAnonymizedDataCollectionEnabled = false;
  WebRtcEventLogCollectionAllowed = false; WebRtcIPHandling = "default_public_interface_only";
  BraveAIChatEnabled = false; BraveNewsDisabled = true; BravePlaylistEnabled = false; BraveRewardsDisabled = true;
  BraveSpeedreaderEnabled = false; BraveTalkDisabled = true; BraveVPNDisabled = true; BraveWalletDisabled = true;
  BraveWaybackMachineEnabled = false; CommandLineFlagSecurityWarningsEnabled = false;
  DefaultBrowserSettingEnabled = false; HighEfficiencyModeEnabled = true; MemorySaverModeSavings = 2;
  SyncDisabled = true; TorDisabled = true;
};
in
{
  environment.etc."brave/policies/managed/drv.json".text = builtins.toJSON policies;
  fonts.packages = [ pkgs.ia-fonts ];
  # Next to the host's own session (greetd on VT 1), which keeps the screen at boot;
  # Ctrl-Alt-F7 and Ctrl-Alt-F1 switch. PipeWire is the module's system-wide one; the host's
  # session keeps a PulseAudio server (the module has one per audio app), at /run/pulse for
  # members of group pipewire.
  services.drv.homeVt = 1;
  services.pipewire.pulse.enable = lib.mkForce true;
  environment.sessionVariables.PULSE_SERVER = "unix:/run/pulse/native";
  systemd.user.tmpfiles.rules = [ "L %t/pulse - - - - /run/pulse" ];

  services.drv = {
    enable = true;
    # The Asahi kernel comes from the cache; the resume blanking is not worth a kernel build.
    resumePatch = false;
    screenshots = "/var/lib/drv-files/Screenshots";
    apps = {
      # rho's own browser integration is off: it opens links through the portal (its private
      # bus), and this app is what `https` resolves to.
      browser = {
        uid = 100101;
        exec = [ (lib.getExe pkgs.brave-origin) "--ozone-platform=wayland" "--password-store=basic" "--js-flags=--jitless" ];
        gpu = true; network = true; audio = true; bus = true;
        # --jitless: no code at runtime, so W^X memory holds. Its own sandbox wants user namespaces.
        userns = true;
        etc = [ "brave/policies/managed/drv.json" ];
        state = [ ".config/BraveSoftware" ".cache/BraveSoftware" ];
        opens = [ "http" "https" ];
      };
      # The shell. It does ssh itself (the agent) and opens links (the bus). The notch is the
      # compositor's to keep clear; rho only needs its size, in physical pixels, to lay out around.
      rho = {
        uid = 100102;
        exec = [ "${pkgs.rho-gui-bin}/bin/rho-gui" ];
        gpu = true; network = true; bus = true; agent = true;
        autostart = true;
        env.RHO_NOTCH = "290x56";
        state = [ ".local/state/rho" ];
      };
      # A terminal for ssh, nothing local: the agent's keys, known_hosts kept, fish and tmux
      # on its PATH. `drv-agent load` here loads the authenticator's resident keys.
      terminal = {
        uid = 100103;
        exec = [ "${pkgs.alacritty}/bin/alacritty" "-e" "${pkgs.fish}/bin/fish" ];
        gpu = true; network = true; agent = true;
        packages = [ pkgs.openssh pkgs.fish pkgs.tmux pkgs.coreutils config.services.drv.package ];
        state = [ ".ssh" ".config/fish" ".local/share/fish" ];
      };
      mail = {
        uid = 100104;
        exec = [ "${pkgs.thunderbird}/bin/thunderbird" ];
        gpu = true; network = true; audio = true; bus = true;
        state = [ ".thunderbird" ];
        opens = [ "mailto" ];
      };
    };
    config = ''
      input {
          keyboard {
              xkb {
                  options "fkeys:basic_13-24"
              }
          }
          touchpad {
              tap
              natural-scroll
              tap-button-map "left-middle-right"
          }
          focus-follows-mouse
      }

      output "eDP-1" {
          scale 2
          wide-gamut-p3
      }

      layout {
          gaps 0
          focus-ring {
              off
          }
          default-column-width { proportion 1.0; }
          preset-column-widths {
              proportion 0.33333
              proportion 0.5
              proportion 0.66667
          }
      }

      prefer-no-csd
      screenshot-path "/var/lib/drv-files/Screenshots/%Y-%m-%d %H-%M-%S.png"

      overview {
          zoom 0.25
      }

      animations {
          off
      }

      window-rule {
          match is-window-cast-target=true
          border {
              on
          }
      }

      binds {
          // spawn names apps of the manifest above; nothing else runs.
          Mod+Return cooldown-ms=500 { spawn "terminal"; }
          // Space+F (via keyd F18)
          F18 { spawn "browser"; }
          Mod+D { show-launcher; }

          // The media keys are drv-keys' (a member of the set): the compositor spawns nothing.
          XF86AudioRaiseVolume allow-when-locked=true { volume-up; }
          XF86AudioLowerVolume allow-when-locked=true { volume-down; }
          XF86AudioMute allow-when-locked=true { volume-mute; }
          XF86AudioMicMute allow-when-locked=true { mic-mute; }
          XF86MonBrightnessUp allow-when-locked=true { brightness-up; }
          XF86MonBrightnessDown allow-when-locked=true { brightness-down; }

          Mod+Period { expel-window-from-column; }
          Mod+Comma { consume-window-into-column; }

          Mod+H { focus-column-left; }
          Mod+L { focus-column-right; }
          Mod+J { focus-workspace-down; }
          Mod+K { focus-workspace-up; }
          Mod+Left { focus-column-left; }
          Mod+Right { focus-column-right; }
          Mod+Down { focus-window-down; }
          Mod+Up { focus-window-up; }

          Mod+P { screenshot; }

          Mod+Shift+H { move-column-left; }
          Mod+Shift+J { move-workspace-down; }
          Mod+Shift+K { move-workspace-up; }
          Mod+Shift+L { move-column-right; }

          Mod+U { move-column-to-workspace-down; }
          Mod+I { move-column-to-workspace-up; }

          Mod+Minus { set-column-width "-10%"; }
          Mod+Equal { set-column-width "+10%"; }
          Mod+Shift+Minus { set-window-height "-10%"; }
          Mod+Shift+Equal { set-window-height "+10%"; }

          Mod+R { switch-preset-column-width; }
          Mod+F { maximize-column; }
          Mod+Shift+F { toggle-windowed-fullscreen; }
          Mod+Shift+Space { set-dynamic-cast-window; }
          Mod+C { center-column; }
          Mod+Space { toggle-window-floating; }
          Mod+Tab { focus-monitor-next; }
          Mod+Shift+Tab { move-workspace-to-monitor-next; }
          Mod+Alt+L { lock-session; }

          Mod+Shift+Q { close-window; }

          Mod+1 { focus-workspace 1; }
          Mod+2 { focus-workspace 2; }
          Mod+3 { focus-workspace 3; }
          Mod+4 { focus-workspace 4; }
          Mod+5 { focus-workspace 5; }
          Mod+6 { focus-workspace 6; }
          Mod+7 { focus-workspace 7; }
          Mod+8 { focus-workspace 8; }
          Mod+9 { focus-workspace 9; }
          Mod+Shift+1 { move-column-to-workspace 1; }
          Mod+Shift+2 { move-column-to-workspace 2; }
          Mod+Shift+3 { move-column-to-workspace 3; }
          Mod+Shift+4 { move-column-to-workspace 4; }
          Mod+Shift+5 { move-column-to-workspace 5; }
          Mod+Shift+6 { move-column-to-workspace 6; }
          Mod+Shift+7 { move-column-to-workspace 7; }
          Mod+Shift+8 { move-column-to-workspace 8; }
          Mod+Shift+9 { move-column-to-workspace 9; }
      }
    '';
  };
}
