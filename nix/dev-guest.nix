# The development guest for the multi-UID stack: the desktop from the nixos repo's module, the probe
# apps, an ssh admin, the dev PIN and a test camera. Not a test: run it, drive it, read the
# evidence. The machine around it is nix/dev-vm.nix (QEMU, x86_64) or nix/m2-vm.nix (crosvm
# on an Apple M2 with the GPU passed through as a virtio-gpu native context).
{ niri, camera ? true, xkbOptions ? null }:
{ config, pkgs, lib, ... }:
let
  inherit (config.services.drv) mkApp;
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
      // The smoke test drives it by key: s share, m mic, c camera.
      document.onkeydown = e => ({ s: b, m: m, c: c }[e.key] || {}).onclick?.();
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
  # The FIDO door through the shim, as linux-credentials' portal API, on the VM's emulated
  # CanoKey (nix/dev-vm.nix): a credential made with hmac-secret (`hmacCreateSecret`, the
  # extension libwebauthn serves without asking a PIN; PRF it upgrades to UV required) for
  # the app's own origin, then two assertions with the same salt must agree (32 bytes: the
  # identity rho derives) and one with another salt must not; the same for another origin
  # is refused by the door.
  fidoTest = pkgs.writeShellScript "fido-test" ''
    PATH=${lib.makeBinPath [ pkgs.systemd pkgs.jq pkgs.coreutils ]}
    call() {
      busctl --user --json=short -- call xyz.iinuwa.credentialsd.Credentials /org/freedesktop/portal/desktop \
        org.freedesktop.handler.portal.experimental.Credential "$@" 2>&1
    }
    # The one string in the reply's a{sv} under KEY, whatever busctl's JSON nesting.
    field() { jq -r --arg k "$1" '[.. | objects | select(has($k)) | .[$k] | (.data // .)] | first // empty' 2>/dev/null; }
    create() {
      call CreateCredential 'sssa{sv}s' "" "$1" publicKey 1 public_key s \
        '{"rp":{"id":"fidotest.drv.dev","name":"fido-test"},"user":{"id":"AQID","name":"probe","displayName":"probe"},"challenge":"Y2hhbGxlbmdl","pubKeyCredParams":[{"type":"public-key","alg":-7}],"authenticatorSelection":{"residentKey":"discouraged","userVerification":"discouraged"},"extensions":{"hmacCreateSecret":true}}' \
        dev.drv.FidoTest
    }
    get() { # origin, credential id, salt
      call GetCredential 'ssa{sv}s' "" "$1" 1 public_key s \
        "{\"challenge\":\"Y2hhbGxlbmdl\",\"rpId\":\"fidotest.drv.dev\",\"allowCredentials\":[{\"type\":\"public-key\",\"id\":\"$2\"}],\"userVerification\":\"discouraged\",\"extensions\":{\"hmacGetSecret\":{\"salt1\":\"$3\"}}}" \
        dev.drv.FidoTest
    }
    {
      made=$(create app:dev.drv.FidoTest)
      reg=$(field registration_response_json <<<"$made")
      if [ -z "$reg" ]; then echo "created: $made"; else
        id=$(jq -r .id <<<"$reg")
        echo "created: hmac-secret $(jq -r .clientExtensionResults.hmacCreateSecret <<<"$reg"), credential id of ''${#id} chars"
        hmac() { get app:dev.drv.FidoTest "$id" "$1" | field authentication_response_json | jq -r '.clientExtensionResults.hmacGetSecret.output1 // "none"'; }
        salt=cmhvIGlyb2ggaWRlbnRpdHkgdjEgKDMyIGJ5dGVzKSE
        a=$(hmac $salt); b=$(hmac $salt); c=$(hmac YW5vdGhlciBzYWx0IGZvciB0aGUgZmlkbyBwcm9iZSE)
        if [ "$a" = "$b" ] && [ ''${#a} -eq 43 ] && [ "$c" != "$a" ] && [ ''${#c} -eq 43 ]; then
          echo "hmac-secret: the same salt agrees, 32 bytes; another salt differs"
        else
          echo "hmac-secret: $a / $b / $c"
        fi
      fi
      echo "other origin: $(get app:dev.rho.Gui AQID $salt)"
      # A web origin (the https://* grant): the relying party must be a registrable suffix of
      # the host. A bogus credential id keeps the key from asking anything.
      web() { # rp id
        call GetCredential 'ssa{sv}s' "" https://login.example.com 1 public_key s \
          "{\"challenge\":\"Y2hhbGxlbmdl\",\"rpId\":\"$1\",\"allowCredentials\":[{\"type\":\"public-key\",\"id\":\"AQID\"}],\"userVerification\":\"discouraged\"}" \
          dev.drv.FidoTest
      }
      echo "web origin, rp a suffix: $(web example.com)"
      echo "web origin, rp elsewhere: $(web evil.com)"
      echo "web origin, rp a public suffix: $(web co.uk)"
    } > "$HOME/out/fido.txt"
    touch "$HOME/out/done"
    # The PIN dialog and its refusal (nix/smoke.sh sets a PIN on the key first, then drives
    # the shell): an assertion with UV required, twice, each timed.
    for n in 1 2; do
      while [ ! -e "$HOME/out/go-pin$n" ]; do sleep 0.5; done
      start=$(date +%s)
      reply=$(call GetCredential 'ssa{sv}s' "" app:dev.drv.FidoTest 1 public_key s \
        "{\"challenge\":\"Y2hhbGxlbmdl\",\"rpId\":\"fidotest.drv.dev\",\"allowCredentials\":[{\"type\":\"public-key\",\"id\":\"$id\"}],\"userVerification\":\"required\"}" \
        dev.drv.FidoTest)
      echo "uv required, answered after $(( $(date +%s) - start ))s: $reply" > "$HOME/out/pin$n.txt"
    done
    # Cancelling: the same ask, named by a handle_token, then `Request.Close` on the portal
    # request handle (the sender's unique name is in its path: busctl's, by its pid) while
    # the shell asks the PIN; the door sees the app hang up and the call is answered at once.
    while [ ! -e "$HOME/out/go-cancel" ]; do sleep 0.5; done
    start=$(date +%s)
    busctl --user --json=short -- call xyz.iinuwa.credentialsd.Credentials /org/freedesktop/portal/desktop \
      org.freedesktop.handler.portal.experimental.Credential GetCredential 'ssa{sv}s' "" app:dev.drv.FidoTest 2 public_key s \
      "{\"challenge\":\"Y2hhbGxlbmdl\",\"rpId\":\"fidotest.drv.dev\",\"allowCredentials\":[{\"type\":\"public-key\",\"id\":\"$id\"}],\"userVerification\":\"required\"}" \
      handle_token s cancelme dev.drv.FidoTest > "$HOME/out/cancel.reply" 2>&1 &
    pid=$!
    sleep 2
    sender=$(busctl --user --json=short list --unique | jq -r --argjson pid $pid '[.[] | select(.pid == $pid) | .name | ltrimstr(":") | gsub("[.]"; "_")] | first')
    busctl --user call xyz.iinuwa.credentialsd.Credentials "/org/freedesktop/portal/desktop/request/$sender/cancelme" \
      org.freedesktop.portal.Request Close > "$HOME/out/cancel.close" 2>&1
    wait $pid
    echo "cancelled, answered after $(( $(date +%s) - start ))s: $(cat "$HOME/out/cancel.reply")" > "$HOME/out/cancel.txt"
  '';
  micTest = pkgs.writeShellScript "mic-test" ''
    exec ${pkgs.pipewire}/bin/pw-record "$HOME/out/rec.wav"
  '';
  # An sshd of the app's own uid, as rho runs one (m2sh.nix): the app logs into itself over
  # the loopback and records what a session sees.
  sshTest = pkgs.writeShellScript "ssh-test" ''
    cd "$HOME/out"
    ${pkgs.openssh}/bin/ssh-keygen -q -t ed25519 -N "" -f host_ed25519
    ${pkgs.openssh}/bin/ssh-keygen -q -t ed25519 -N "" -f client_ed25519
    printf '%s\n' "AddressFamily inet" "ListenAddress 127.0.0.1" "Port 2222" "HostKey $HOME/out/host_ed25519" \
      "PidFile none" "UsePAM no" "PasswordAuthentication no" "KbdInteractiveAuthentication no" \
      "AuthenticationMethods publickey" "AuthorizedKeysFile $HOME/out/client_ed25519.pub" "StrictModes no" \
      "PermitUserEnvironment yes" > sshd_config
    ${pkgs.coreutils}/bin/mkdir -p "$HOME/.ssh"
    ${pkgs.coreutils}/bin/env | ${pkgs.gnugrep}/bin/grep -E '^[A-Za-z][A-Za-z0-9_]*=' > "$HOME/.ssh/environment"
    ${pkgs.openssh}/bin/sshd -D -e -f sshd_config 2> sshd.log &
    for _ in $(${pkgs.coreutils}/bin/seq 50); do
      ${pkgs.openssh}/bin/ssh -p 2222 -i client_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        app-ssh-test@127.0.0.1 '${pkgs.coreutils}/bin/id; echo NIX_REMOTE=$NIX_REMOTE; nix --extra-experimental-features nix-command store info 2>&1' > session.txt 2> ssh.log && break
      ${pkgs.coreutils}/bin/sleep 0.2
    done
    ${pkgs.coreutils}/bin/touch done
    exec ${pkgs.coreutils}/bin/sleep infinity
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
    SSH_AUTH_SOCK=''${SSH_AUTH_SOCK:-/run/drv/agent} ${pkgs.openssh}/bin/ssh-add -l > agent.txt 2>&1 || echo "rc: $?" >> agent.txt
    ${pkgs.pipewire}/bin/pw-cli info 0 > pipewire.txt 2>&1 || echo "pw-cli failed: $?" >> pipewire.txt
    ${pkgs.wayland-utils}/bin/wayland-info > globals.txt 2> wayland-info.err
    # The person's folder, if this app has one: writable, and ours by uid inside.
    if [ -d "$HOME/Shared" ]; then
      echo "from uid $(${pkgs.coreutils}/bin/id -u)" > "$HOME/Shared/hello.txt"
      { ${pkgs.coreutils}/bin/readlink "$HOME/Shared"; ${pkgs.coreutils}/bin/stat -L -c '%u %g %a' "$HOME/Shared" "$HOME/Shared/hello.txt"; } > folder.txt 2>&1
    fi
    ${pkgs.coreutils}/bin/touch done
  '';
  # The kernel's account of a task, for nix/kernel-state.sh: an out-of-tree module built
  # against this VM's kernel. Dev only; a real install has no such thing.
  # LibreOffice's launcher (oosplash) exits unless /proc/version exists, which the sandbox's
  # subset=pid proc hides. All it does besides is run soffice.bin again when that asks for a
  # restart (exit 79 or 81, e.g. after creating the profile), so keep the nixpkgs wrapper for
  # its environment and replace its last line with that loop.
  libreoffice = pkgs.runCommand "libreoffice-nosplash" { } ''
    mkdir -p $out/bin
    sed -E 's|^("/nix/store/[^"]*/lib/libreoffice/program/)soffice" +"\$@" *$|while \1soffice.bin" "$@"; code=$?; [ $code = 79 ] \|\| [ $code = 81 ]; do :; done; (exit $code)|' \
      ${pkgs.libreoffice-qt6-fresh}/lib/libreoffice/program/soffice > $out/bin/libreoffice
    test "$(grep -c '^while ' $out/bin/libreoffice)" = 1
    chmod +x $out/bin/libreoffice
  '';
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
  # The module itself (services.drv) comes from the nixos repo, imported by flake.nix.
  services.drv.package = niri;

  services.openssh.enable = true;
  # The emulated CanoKey (nix/dev-vm.nix) is drv-fido's like a YubiKey (the module's rule
  # names Yubico's vendor id only).
  services.udev.extraRules = ''
    SUBSYSTEM=="hidraw", ATTRS{idVendor}=="20a0", ATTRS{idProduct}=="42d4", GROUP="drv-fido", MODE="0660"
  '';
  # Something for the chooser to show.
  systemd.tmpfiles.rules = [
    "d /var/lib/drv-files/notes 0700 drv-files drv-files -"
    "f+ /var/lib/drv-files/hello.txt 0600 drv-files drv-files - hello from the persons files\\n"
    "f+ /var/lib/drv-files/notes/todo.txt 0600 drv-files drv-files - build the portal\\n"
  ];
  services.openssh.settings.PermitRootLogin = "yes";
  # The dev key in the repo (nix/dev-vm-key: the VM listens on localhost only) and the
  # person's own.
  users.users.root.openssh.authorizedKeys.keys = [
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICW0SCsYGlDnCl1mdjoS/HtlG1LTfYlhuTlDux7f/QQS dev-vm"
    "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIP4pE2ZiZIJvxrTMzKzwfVBtUPp2Ek7MGselzb0w6wDE maan2003@devbox-01"
  ];
  networking.firewall.enable = false;
  fonts.enableDefaultPackages = true;

  # The ssh admin.
  users.users.alice = {
    isNormalUser = true;
    uid = 1000;
  };
  services.drv.host = {
    enable = true;
    exec = [ "${pkgs.weston}/bin/weston-terminal" ];
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
    # Mod+D shows the menu (drv-shell, a supervisor service) instead of spawning fuzzel; the
    # volume and brightness keys are drv-keys' actions instead of wpctl and brightnessctl.
    config = builtins.replaceStrings
      [ "// skip-at-startup" "{ spawn \"fuzzel\"; }" "// options \"grp:win_space_toggle,compose:ralt,ctrl:nocaps\"" "Mod+Shift+E { quit; }"
        "{ spawn-sh \"wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.1+ -l 1.0\"; }" "{ spawn-sh \"wpctl set-volume @DEFAULT_AUDIO_SINK@ 0.1-\"; }"
        "{ spawn-sh \"wpctl set-mute @DEFAULT_AUDIO_SINK@ toggle\"; }" "{ spawn-sh \"wpctl set-mute @DEFAULT_AUDIO_SOURCE@ toggle\"; }"
        "{ spawn \"brightnessctl\" \"--class=backlight\" \"set\" \"+10%\"; }" "{ spawn \"brightnessctl\" \"--class=backlight\" \"set\" \"10%-\"; }" ]
      [ "skip-at-startup" "{ show-launcher; }" (if xkbOptions == null then "" else "options \"${xkbOptions}\"") "Mod+Shift+E { quit; }\n    Mod+Grave { toggle-host; }"
        "{ volume-up; }" "{ volume-down; }" "{ volume-mute; }" "{ mic-mute; }" "{ brightness-up; }" "{ brightness-down; }" ]
      (builtins.readFile ../resources/default-config.kdl);
    apps = {
      # Sends one notification from inside the sandbox over its private bus. Not autostarted:
      # launch it from the menu.
      notify-test = {
        uid = 100007;
        run = mkApp {
          bus = true;
          exec = [ "${pkgs.libnotify}/bin/notify-send" "-a" "Evil Corp" "<b>Hello</b>" "from uid 100007 via the shim" ];
        };
      };
      hello = {
        uid = 100001; autostart = true; menu = false;
        run = mkApp {
          name = "hello";
          exec = [ "${probe}" ];
          state = [ "out" ];
          packages = [ pkgs.openssh ];
          files = { ".config/hello/greeting" = "hello from the store"; };
        };
        agent = true;
        # A folder of the person's files, its own inside (the probe writes into it).
        folders = [ "Shared" ];
      };
      # A daemon that dies once: drv-init starts it again, then it stays.
      restart-test = {
        uid = 100013; autostart = true; menu = false; folders = [ "Shared" ];
        run = mkApp {
          restart = true; state = [ "out" ];
          exec = [ "${pkgs.writeShellScript "restart-test" ''
            if [ -e "$HOME/out/ran" ]; then exec ${pkgs.coreutils}/bin/sleep infinity; fi
            ${pkgs.coreutils}/bin/touch "$HOME/out/ran"; exit 3
          ''}" ];
        };
      };
      gpu-probe = { uid = 100002; run = mkApp { exec = [ "${probe}" ]; state = [ "out" ]; packages = [ pkgs.openssh ]; }; gpu = true; autostart = true; menu = false; };
      flower = { uid = 100003; run = mkApp { exec = [ "${pkgs.weston}/bin/weston-flower" ]; }; autostart = true; };
      # A real browser: GPU, audio, its own home, a private session bus (compatibility, not
      # a boundary; the sandbox already hides the system bus). Flags come from here only.
      chromium = {
        uid = 100005;
        run = mkApp {
          name = "chromium";
          bus = true;
          exec = [
            "${pkgs.chromium}/bin/chromium" "--ozone-platform=wayland"
            "--autoplay-policy=no-user-gesture-required" "--enable-features=WebRtcPipeWireCamera"
            # Its log, for the camera: the portal dance happens in its video utility process.
            "--enable-logging=stderr" "--v=0" "--vmodule=camera_portal=2,pipewire_session=2,video_capture_device_factory_webrtc=2"
            "file:///share.html"
          ];
          # The page: a URL is not a store path the closure would list, a link is.
          links."/share.html" = "${sharePage}";
          state = [ ".config/chromium" ".cache/chromium" ];
          # Its single-instance socket lives under TMPDIR; /tmp is of the run, so a second
          # launch (OpenURI while it runs) has to find the first one's socket in its state.
          env.TMPDIR = "/home/app/.cache/chromium";
        };
        gpu = true;
        network = true;
        audio = true;
        jit = true;
        userns = true;
        opens = [ "http" "https" ];
      };
      # LibreOffice as m2sh has it: the kf6 plugin under Qt's xdgdesktopportal theme, so
      # opening and saving go through the shell's chooser and the documents mount (a save
      # writes a scratch file next to the document and renames it onto it; the lock file
      # `.~lock.<name>#` is a scratch file too). It only takes the native (portal) dialog
      # when it believes it is on Plasma, hence OOO_FORCE_DESKTOP. jit: its UNO bridge
      # writes vtable trampolines at runtime. edits: what it opens it may save in place.
      libreoffice = {
        uid = 100016;
        run = mkApp {
          name = "libreoffice";
          exec = [ "${libreoffice}/bin/libreoffice" ];
          bus = true; edits = true;
          state = [ ".config/libreoffice" ];
          env.SAL_USE_VCLPLUGIN = "kf6";
          env.QT_QPA_PLATFORM = "wayland";
          env.QT_QPA_PLATFORMTHEME = "xdgdesktopportal";
          env.OOO_FORCE_DESKTOP = "plasma6";
          env.SAL_ENABLE_FILE_LOCKING = "1";
        };
        gpu = true; jit = true;
      };
      # A client of the file chooser, as a GTK app would use it: asks its private bus, the
      # shim asks drv-files, the person picks, and the file arrives under /run/drv/doc.
      # Then it saves a copy the same way. Results in its home, result.txt.
      chooser-test = { uid = 100008; run = mkApp { bus = true; exec = [ "${config.services.drv.package}/bin/chooser-probe" ]; state = [ "out" ]; }; };
      # A client of screen sharing, as a browser would use it: session, Start (the person
      # picks a screen at the shell), the PipeWire remote, and what that remote can see.
      # Holds the cast 20 s, then closes. Results in its home, cast.txt.
      cast-test = { uid = 100009; run = mkApp { bus = true; exec = [ "${config.services.drv.package}/bin/cast-probe" ]; state = [ "out" ]; }; audio = true; };
      # Plays a sound: playback is free for an audio app.
      beep = {
        uid = 100006;
        run = mkApp { exec = [ "${pkgs.pipewire}/bin/pw-play" "${pkgs.sound-theme-freedesktop}/share/sounds/freedesktop/stereo/bell.oga" ]; };
        audio = true;
      };
      # Records: the person is asked at the shell; Mod+Shift+Esc ends it.
      mic-test = { uid = 100010; run = mkApp { exec = [ "${micTest}" ]; state = [ "out" ]; }; audio = true; };
      open-test = { uid = 100011; run = mkApp { bus = true; exec = [ "${openTest}" ]; state = [ "out" ]; }; };
      fido-test = { uid = 100015; run = mkApp { bus = true; exec = [ "${fidoTest}" ]; state = [ "out" ]; }; autostart = true; menu = false; fido = [ "app:dev.drv.FidoTest" "https://*" ]; };
      ssh-test = {
        uid = 100014; autostart = true; menu = false; network = true; nix = true;
        run = mkApp { nix = true; state = [ "out" ]; shell = "${pkgs.bashInteractive}/bin/bash"; exec = [ "${sshTest}" ]; };
      };
      # A terminal: a pty of its own.
      terminal = { uid = 100012; run = mkApp { exec = [ "${pkgs.alacritty}/bin/alacritty" "-e" "${pkgs.fish}/bin/fish" ]; packages = [ pkgs.fish pkgs.coreutils ]; }; gpu = true; };
    };
  };

  # SELinux (the drv module's), enforcing: the dev VM's stock kernel has it built in.
  services.drv.selinux.enable = true;

  environment.systemPackages = [
    pkgs.wayland-utils
    pkgs.weston
    pkgs.foot
    # nix/smoke.sh sets a PIN on the emulated key (fido2-token asks at a tty: expect).
    pkgs.libfido2
    pkgs.expect
  ];

  # A camera for drv-cast: a loopback device fed a test pattern, which WirePlumber
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
