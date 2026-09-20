# The M2 development VM: the guest of nix/dev-guest.nix as a microvm.nix crosvm machine on an
# Apple M2 (aarch64, Asahi). crosvm hands the guest the host GPU as a virtio-gpu DRM native
# context, so the compositor's GPU process runs the real Asahi Mesa driver on the real GPU.
# The guest's screen is a window on a headless host compositor (nix/m2-vm-run.sh starts
# sway, wayvnc and noVNC as an ordinary user), so a browser shows it and drives it, and no
# seat is involved on the host. The host user runs unprivileged: no vsock, no tap, so the
# control channel is a virtiofs share (/host in the guest): the host drops a script into
# cmd/, the guest agent runs it as root and writes out/ and done/ (nix/m2-vm-exec).
{ niri }:
{ config, options, pkgs, lib, ... }:
let
  # The guest boots a kernel the host already has: compiling one on the M2 is out. These
  # are the Asahi kernel and modules of an earlier crosvm guest on m2sh; builtins.storePath
  # needs --impure and the paths must exist on the building host.
  hostKernel = "/nix/store/gn5r4sx3hkv8zg67c9kpr32cd94zb7p6-linux-asahi-6.18.10";
  hostModules = "/nix/store/81vcy99yp26x8rljcw1kf7a24p2hdxi9-linux-asahi-6.18.10-modules";
  kernel = pkgs.runCommand "linux-asahi-6.18.10-host" {
    version = "6.18.10";
    modDirVersion = "6.18.10";
    # `features` present: NixOS then skips its kernel-config assertions; `override` is
    # what boot.kernelPackages' apply calls to add patches, which we cannot take.
    passthru = {
      features = { };
      override = _: kernel;
      buildDTBs = false;
      target = "Image";
      isLTS = false;
      isZen = false;
      kernelOlder = lib.versionOlder "6.18.10";
      kernelAtLeast = lib.versionAtLeast "6.18.10";
      commonMakeFlags = [ ];
      # Only read for the ASLR sysctl (arm64, 16K pages); the real config is not in the store.
      configfile = pkgs.writeText "linux-asahi-6.18.10-config" ''
        CONFIG_ARCH_MMAP_RND_BITS_MAX=31
        CONFIG_ARCH_MMAP_RND_COMPAT_BITS_MAX=14
      '';
      config = { isYes = _: true; isEnabled = _: true; isNo = _: false; isSet = _: true; isModule = _: false; };
    };
  } ''
    mkdir -p $out/lib
    ln -s ${builtins.storePath hostKernel}/Image $out/Image
    ln -s ${builtins.storePath hostModules}/lib/modules $out/lib/modules
  '';
  agent = pkgs.writeShellScript "host-agent" ''
    export PATH=/run/current-system/sw/bin:$PATH
    mkdir -p /host/cmd /host/out /host/done
    while :; do
      for f in /host/cmd/*.sh; do
        [ -e "$f" ] || continue
        n=$(basename "$f" .sh)
        ${pkgs.bash}/bin/bash "$f" > "/host/out/$n" 2>&1
        echo $? > "/host/done/$n.tmp" && mv "/host/done/$n.tmp" "/host/done/$n"
        rm -f "$f"
      done
      sleep 0.2
    done
  '';
in
{
  # crosvm's virtio keyboard declares no Super key, so the guest swaps Alt and Super: Alt in
  # the noVNC page is Mod. nix/vm-lib.sh's M2 key map follows.
  imports = [ (import ./dev-guest.nix { inherit niri; camera = false; xkbOptions = "altwin:swap_alt_win"; }) ];

  boot.kernelPackages = pkgs.linuxPackagesFor kernel;

  # Mesa's loader has no native-context probe for asahi: under virtio-gpu it would pick virgl,
  # which the host does not offer. Told to use asahi, agx notices the virtio device itself.
  services.drv.gpuEnv.MESA_LOADER_DRIVER_OVERRIDE = "asahi";
  services.drv.env = options.services.drv.env.default // { MESA_LOADER_DRIVER_OVERRIDE = "asahi"; };

  microvm = {
    hypervisor = "crosvm";
    # The host maps blob resources into the guest with 16K pages; unpatched crosvm asks KVM
    # for the resource's exact size and gets EINVAL. This is the host's own crosvm with the
    # map size rounded up (nixos repo, crosvm-cross-domain-map-page-size.patch).
    crosvm.package = builtins.storePath "/nix/store/09zb64p83cfxjv3aqg68150mmm5f8myj-crosvm-0-unstable-2026-02-13";
    vcpu = 4;
    mem = 4096;
    shares = [
      { proto = "virtiofs"; tag = "ro-store"; source = "/nix/store"; mountPoint = "/nix/.ro-store"; }
      { proto = "virtiofs"; tag = "host"; source = "share"; mountPoint = "/host"; cache = "never"; }
    ];
    # The host user is not root: virtiofsd cannot make its namespace sandbox.
    virtiofsd.extraArgs = [ "--sandbox" "none" ];
    crosvm.extraArgs = [
      "--disable-sandbox"
      # The window's keyboard and mouse reach the guest as virtio-input devices.
      "--display-window-keyboard" "--display-window-mouse"
      # A sound card whose input is silence, so audio capture and its grant can be exercised.
      "--virtio-snd" "capture=true,backend=null,num_input_devices=1"
      "--gpu" "backend=virglrenderer,context-types=drm:cross-domain,egl=true,vulkan=true,surfaceless=true,fixed-blob-mapping=true,displays=[[mode=windowed[1280,832]]]"
    ];
    # The host compositor's socket is only known at run time: the runner writes it here.
    extraArgsScript = toString (pkgs.writeShellScript "m2-vm-extra-args" ''
      echo "--wayland-sock $(cat wayland-sock)"
    '');
  };

  # microvm.nix blacklists drm without its own graphics option; that option runs crosvm's
  # GPU as a separate device process, which is not the native-context path.
  boot.blacklistedKernelModules = lib.mkForce [ "rfkill" ];
  boot.kernelModules = [ "virtio_gpu" "virtio_snd" "uinput" ];
  boot.kernelParams = [ "8250.nr_uarts=4" ];

  services.getty.autologinUser = "root";
  programs.ydotool.enable = true;

  systemd.services.host-agent = {
    wantedBy = [ "multi-user.target" ];
    after = [ "host.mount" ];
    requires = [ "host.mount" ];
    serviceConfig = { ExecStart = agent; Restart = "always"; RestartSec = 1; };
  };

  networking.useDHCP = false;
  networking.useNetworkd = false;
}
