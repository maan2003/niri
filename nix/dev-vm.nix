# The QEMU development VM: the guest of nix/dev-guest.nix on x86_64 with virgl, an absolute
# pointer, a discarded sound card and a monitor socket (nix/dev-vm-run.sh runs it).
{ niri }:
{ modulesPath, pkgs, ... }:
{
  imports = [
    "${modulesPath}/virtualisation/qemu-vm.nix"
    (import ./dev-guest.nix { inherit niri; })
  ];
  virtualisation = {
    memorySize = 4096;
    cores = 4;
    diskSize = 4096;
    graphics = true;
    # QEMU with CanoKey: an emulated FIDO2 security key (canokey-core; user presence is not
    # asked, as over NFC), for drv-agent's FIDO door. Its state lives beside the disk image.
    qemu.package = pkgs.qemu_kvm.override { canokeySupport = true; };
    qemu.options = [
      # Only a virgl GPU (the default VGA has no render node), an absolute pointer so host
      # clicks land, a sound card whose output is discarded, a security key and a monitor
      # socket.
      "-vga none" "-device virtio-gpu-gl-pci"
      "-device qemu-xhci" "-device usb-tablet" "-device canokey,file=/tmp/niri-vm/canokey-file"
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
}
