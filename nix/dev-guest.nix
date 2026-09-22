# The development guest for the multi-UID stack: the desktop from nix/module.nix, the probe
# apps, an ssh admin, the dev PIN and a test camera. Not a test: run it, drive it, read the
# evidence. The machine around it is nix/dev-vm.nix (QEMU, x86_64) or nix/m2-vm.nix (crosvm
# on an Apple M2 with the GPU passed through as a virtio-gpu native context).
{ niri, camera ? true, xkbOptions ? null }:
{ config, pkgs, lib, ... }:
let
  # A page that plays a sound forever, so a browser's audio path can be seen in PipeWire,
  # and shares the screen on a click.
  sharePage = pkgs.writeText "share.html" ''
    <!doctype html><title>share</title>
    <body style="margin:0;background:#224">
    <audio autoplay loop src="file://${pkgs.sound-theme-freedesktop}/share/sounds/freedesktop/stereo/bell.oga"></audio>
    <button id=b style="font-size:60px;width:100%;height:200px">share screen</button>
    <button id=m style="font-size:60px;width:49%;height:200px">mic</button>
    <button id=c style="font-size:60px;width:49%;height:200px">camera</button>
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
      m.onclick = async () => {
        try {
          const s = await navigator.mediaDevices.getUserMedia({ audio: true });
          const t = s.getAudioTracks()[0];
          log("mic: " + t.label);
          t.onended = () => log("mic ended");
        } catch (e) { log("mic failed: " + e); }
      };
      c.onclick = async () => {
        try {
          const s = await navigator.mediaDevices.getUserMedia({ video: true });
          v.srcObject = s;
          const t = s.getVideoTracks()[0];
          log("camera: " + t.label);
          t.onended = () => log("camera ended");
        } catch (e) { log("camera failed: " + e); }
      };
    </script>
  '';
  # Records the microphone into its home: the stream waits until the person allows it.
  # Asks its private bus to open URIs, as a sandboxed app would. Two must be refused.
  openTest = pkgs.writeShellScript "open-test" ''
    open() {
      ${pkgs.systemd}/bin/busctl --user -- call org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop \
        org.freedesktop.portal.OpenURI OpenURI ssa{sv} "" "$1" 0 2>&1
    }
    {
      echo "good: $(open 'https://example.com/?from=uid-100011')"
      echo "bad scheme: $(open 'mailto:x@example.com')"
      echo "not a uri: $(open '-https://example.com')"
    } > "$HOME/out/open.txt"
  '';
  micTest = pkgs.writeShellScript "mic-test" ''
    exec ${pkgs.pipewire}/bin/pw-record "$HOME/out/rec.wav"
  '';
  # Results go to $HOME/out, the one directory the probe apps declare as state.
  probe = pkgs.writeShellScript "probe" ''
    cd "$HOME/out"
    ${pkgs.coreutils}/bin/id > id.txt
    ${pkgs.coreutils}/bin/cat /proc/self/cgroup > cgroup.txt
    ${pkgs.coreutils}/bin/env > env.txt
    ${pkgs.coreutils}/bin/ls -la / /run /tmp /dev /dev/shm /etc > run.txt 2>&1
    { ${pkgs.coreutils}/bin/ls -la "$HOME"; ${pkgs.coreutils}/bin/readlink "$HOME/.config/hello/greeting"; ${pkgs.coreutils}/bin/cat "$HOME/.config/hello/greeting"; } > home.txt 2>&1
    # The store beyond the closure: not even listable.
    ${pkgs.coreutils}/bin/ls /nix/store > store.txt 2>&1 || echo "denied: $?" >> store.txt
    ${pkgs.procps}/bin/ps -eo user,pid,cmd > ps.txt 2>&1
    # The sandbox's doors: no user namespace, no /proc beyond the pid entries.
    ${pkgs.util-linux}/bin/unshare -U ${pkgs.coreutils}/bin/true > userns.txt 2>&1 || echo "denied: $?" >> userns.txt
    ${pkgs.coreutils}/bin/ls /proc > proc.txt 2>&1
    ${pkgs.coreutils}/bin/cat /proc/self/net/dev > net.txt 2>&1
    # The ssh agent's door: open for the grant (hello), shut otherwise (gpu-probe).
    SSH_AUTH_SOCK=''${SSH_AUTH_SOCK:-/run/drv-agent/agent} ${pkgs.openssh}/bin/ssh-add -l > agent.txt 2>&1 || echo "rc: $?" >> agent.txt
    ${pkgs.pipewire}/bin/pw-cli info 0 > pipewire.txt 2>&1 || echo "pw-cli failed: $?" >> pipewire.txt
    ${pkgs.wayland-utils}/bin/wayland-info > globals.txt 2> wayland-info.err
    ${pkgs.coreutils}/bin/touch done
  '';
  # The kernel's account of a task, for nix/kernel-state.sh: an out-of-tree module built
  # against this VM's kernel. Dev only; a real install has no such thing.
  kernel = config.boot.kernelPackages.kernel;
  kdump = pkgs.stdenv.mkDerivation {
    name = "drv-kdump-${kernel.version}";
    src = ./kdump;
    nativeBuildInputs = kernel.moduleBuildDependencies;
    makeFlags = [ "KDIR=${kernel.dev}/lib/modules/${kernel.modDirVersion}/build" ];
    installPhase = "install -D drv_kdump.ko $out/lib/modules/${kernel.modDirVersion}/extra/drv_kdump.ko";
  };
in
{
  imports = [ (import ./module.nix { inherit niri; }) ];


  services.openssh.enable = true;
  # Something for the chooser to show.
  systemd.tmpfiles.rules = [
    "d /var/lib/drv-files/notes 0700 drv-portal drv-portal -"
    "f+ /var/lib/drv-files/hello.txt 0600 drv-portal drv-portal - hello from the persons files\\n"
    "f+ /var/lib/drv-files/notes/todo.txt 0600 drv-portal drv-portal - build the portal\\n"
  ];
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
  systemd.services.drv-supervisor.serviceConfig.ExecStartPre = [ ("+" + pkgs.writeShellScript "drv-enrol-dev-pin" ''
    if [ ! -e /var/lib/drv-auth/pin ]; then
      printf 1234 | ${config.services.drv.package}/bin/drv-authd set-pin --state-dir /var/lib/drv-auth
    fi
  '') ];

  services.drv = {
    debug = true;
    screenshots = "/var/lib/drv-screenshots";
    enable = true;
    # niri's stock binds; its spawn lines name apps that do not exist here and are refused.
    # Mod+D shows the menu (drv-menu, a supervisor service) instead of spawning fuzzel; the
    # volume and brightness keys are drv-keys' actions instead of wpctl and brightnessctl.
    config = builtins.replaceStrings
      [ "// skip-at-startup" "{ spawn \"fuzzel\"; }" "// options \"grp:win_space_toggle,compose:ralt,ctrl:nocaps\""
        "{ spawn-sh \"wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.1+ -l 1.0\"; }" "{ spawn-sh \"wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.1-\"; }"
        "{ spawn-sh \"wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle\"; }" "{ spawn-sh \"wpctl set-mute @DEFAULT_AUDIO_SOURCE@ toggle\"; }"
        "{ spawn \"brightnessctl\" \"--class=backlight\" \"set\" \"+10%\"; }" "{ spawn \"brightnessctl\" \"--class=backlight\" \"set\" \"10%-\"; }" ]
      [ "skip-at-startup" "{ show-launcher; }" (if xkbOptions == null then "" else "options \"${xkbOptions}\"")
        "{ volume-up; }" "{ volume-down; }" "{ volume-mute; }" "{ mic-mute; }" "{ brightness-up; }" "{ brightness-down; }" ]
      (builtins.readFile ../resources/default-config.kdl);
    apps = {
      # Sends one notification from inside the sandbox over its private bus. Not autostarted:
      # launch it from the menu.
      notify-test = {
        uid = 100007;
        bus = true;
        exec = [ "${pkgs.libnotify}/bin/notify-send" "-a" "Evil Corp" "<b>Hello</b>" "from uid 100007 via the bridge" ];
      };
      hello = {
        uid = 100001; exec = [ "${probe}" ]; autostart = true; menu = false;
        state = [ "out" ];
        agent = true;
        packages = [ pkgs.openssh ];
        files = { ".config/hello/greeting" = "hello from the store"; };
      };
      gpu-probe = { uid = 100002; exec = [ "${probe}" ]; gpu = true; autostart = true; menu = false; state = [ "out" ]; packages = [ pkgs.openssh ]; };
      flower = { uid = 100003; exec = [ "${pkgs.weston}/bin/weston-flower" ]; autostart = true; };
      # A real browser: GPU, audio, its own home, a private session bus (compatibility, not
      # a boundary; the sandbox already hides the system bus). Flags come from here only.
      chromium = {
        uid = 100005;
        bus = true;
        exec = [
          "${pkgs.chromium}/bin/chromium" "--ozone-platform=wayland"
          "--autoplay-policy=no-user-gesture-required" "--enable-features=WebRtcPipeWireCamera"
          # Its log, for the camera: the portal dance happens in its video utility process.
          "--enable-logging=stderr" "--v=0" "--vmodule=camera_portal=2,pipewire_session=2,video_capture_device_factory_webrtc=2"
          "file://${sharePage}"
        ];
        gpu = true;
        network = true;
        audio = true;
        opens = [ "http" "https" ];
        state = [ ".config/chromium" ".cache/chromium" ];
        jit = true;
        userns = true;
      };
      # A client of the file chooser, as a GTK app would use it: asks its private bus, the
      # bridge asks drv-portal, the person picks, and the file arrives under /run/drv-doc.
      # Then it saves a copy the same way. Results in its home, result.txt.
      chooser-test = { uid = 100008; bus = true; exec = [ "${config.services.drv.package}/bin/chooser-probe" ]; state = [ "out" ]; };
      # A client of screen sharing, as a browser would use it: session, Start (the person
      # picks a screen at drv-portal), the PipeWire remote, and what that remote can see.
      # Holds the cast 20 s, then closes. Results in its home, cast.txt.
      cast-test = { uid = 100009; bus = true; exec = [ "${config.services.drv.package}/bin/cast-probe" ]; audio = true; state = [ "out" ]; };
      # Plays a sound: playback is free for an audio app.
      beep = {
        uid = 100006;
        exec = [ "${pkgs.pipewire}/bin/pw-play" "${pkgs.sound-theme-freedesktop}/share/sounds/freedesktop/stereo/bell.oga" ];
        audio = true;
      };
      # Records: the person is asked at drv-portal; Mod+Shift+Esc ends it.
      mic-test = { uid = 100010; exec = [ "${micTest}" ]; audio = true; state = [ "out" ]; };
      open-test = { uid = 100011; bus = true; exec = [ "${openTest}" ]; state = [ "out" ]; };
      # A terminal: a pty of its own, /bin/sh for what its shell runs.
      terminal = {
        uid = 100012; exec = [ "${pkgs.alacritty}/bin/alacritty" "-e" "${pkgs.fish}/bin/fish" ]; gpu = true;
        packages = [ pkgs.fish pkgs.coreutils ];
        links = { "/bin/sh" = "${pkgs.bash}/bin/sh"; "/usr/bin/env" = "${pkgs.coreutils}/bin/env"; };
      };
    };
  };

  environment.systemPackages = [
    pkgs.wayland-utils
    pkgs.weston
    pkgs.foot
  ];

  # A camera for the portal: a loopback device fed a test pattern, which WirePlumber
  # picks up like any v4l2 camera (the spa videotestsrc node lacks node-level formats,
  # which browsers ask for).
  # The stock kernel, from the cache: the module above needs its build tree, and a patched
  # kernel would be a kernel build. The resume patch has resume-vm to itself.
  boot.kernelPatches = lib.mkForce [ ];
  boot.extraModulePackages = lib.optional camera config.boot.kernelPackages.v4l2loopback ++ [ kdump ];
  boot.kernelModules = lib.optional camera "v4l2loopback" ++ [ "drv_kdump" ];
  boot.extraModprobeConfig = lib.optionalString camera ''options v4l2loopback card_label="Test camera"'';
  systemd.services.test-camera = lib.mkIf camera {
    wantedBy = [ "multi-user.target" ];
    after = [ "systemd-modules-load.service" ];
    serviceConfig = {
      ExecStart = "${pkgs.ffmpeg-headless}/bin/ffmpeg -loglevel error -re -f lavfi -i testsrc=size=640x480:rate=30 -pix_fmt yuyv422 -f v4l2 /dev/video0";
      Restart = "always";
      RestartSec = 2;
    };
  };

  system.stateVersion = "25.11";
}
