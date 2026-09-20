# The QEMU development VM: the guest of nix/dev-guest.nix on x86_64 with virgl, an absolute
# pointer, a discarded sound card and a monitor socket (nix/dev-vm-run.sh runs it).
{ niri }:
{ modulesPath, ... }:
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
    qemu.options = [
      # Only a virgl GPU (the default VGA has no render node), an absolute pointer so host
      # clicks land, a sound card whose output is discarded, and a monitor socket.
      "-vga none" "-device virtio-gpu-gl-pci"
      "-device qemu-xhci" "-device usb-tablet"
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
