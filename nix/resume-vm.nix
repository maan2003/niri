# A tiny VM for the kernel blank-on-resume patch: a QXL display (its DRM driver suspends and
# resumes through the generic helpers; this kernel's virtio-gpu cannot suspend at all), no
# desktop, `modetest` to put a picture on screen. See nix/resume-test.sh.
{ pkgs, modulesPath, ... }:
{
  imports = [ "${modulesPath}/virtualisation/qemu-vm.nix" ];

  boot.kernelPatches = [ { name = "drm-blank-on-resume"; patch = ./linux-drm-blank-on-resume.patch; } ];

  virtualisation = {
    memorySize = 2048;
    cores = 2;
    graphics = true;
    qemu.options = [
      "-vga none" "-device qxl-vga"
      "-monitor unix:/tmp/niri-vm/resume-monitor,server,nowait"
    ];
    forwardPorts = [ { from = "host"; host.port = 2223; guest.port = 22; } ];
  };

  services.openssh.enable = true;
  services.openssh.settings.PermitRootLogin = "yes";
  users.users.root.openssh.authorizedKeys.keys = [ "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIP4pE2ZiZIJvxrTMzKzwfVBtUPp2Ek7MGselzb0w6wDE maan2003@devbox-01" ];
  networking.firewall.enable = false;
  environment.systemPackages = [ pkgs.libdrm ];
  system.stateVersion = "25.11";
}
