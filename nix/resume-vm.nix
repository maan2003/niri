# A tiny VM for the kernel blank-on-resume patch: a QXL display (its DRM driver suspends and
# resumes through the generic helpers; this kernel's virtio-gpu cannot suspend at all), no
# desktop, `modetest` to put a picture on screen. See nix/resume-test.sh.
{ lib, pkgs, modulesPath, ... }:
{
  imports = [ "${modulesPath}/virtualisation/qemu-vm.nix" ];

  # The emulated IDE CD-ROM hangs S3 ("Check power mode failed"); nothing here needs it.
  boot.kernelParams = [ "libata.force=disable" "console=ttyS0" "no_console_suspend" ];
  boot.kernelPatches = [ { name = "drm-blank-on-resume"; patch = ./linux-drm-blank-on-resume.patch; } ];

  virtualisation = {
    memorySize = 2048;
    cores = 2;
    graphics = true;
    # virtio-9p has no suspend support: after S3 every exec from a 9p store hangs. Keep the
    # store on a block image (virtio-blk resumes fine) and mount nothing over 9p.
    useNixStoreImage = true;
    writableStore = false;
    sharedDirectories = lib.mkForce { };
    qemu.options = [
      "-vga none" "-device qxl-vga"
      "-monitor unix:/tmp/niri-vm/resume-monitor,server,nowait"
      "-trace enable=qxl_destroy_primary" "-trace enable=qxl_create_guest_primary"
      "-D /tmp/niri-vm/qxl-trace.log"
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
