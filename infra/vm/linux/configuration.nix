# The Linux worker template. Access material is written by install.sh at
# provisioning time and never committed: the root authorized key and the SSH
# host key live outside the Nix store.
{ modulesPath, pkgs, ... }:
{
  imports = [ (modulesPath + "/profiles/qemu-guest.nix") ];

  boot.loader.systemd-boot.enable = true;
  boot.loader.efi.canTouchEfiVariables = false;
  boot.initrd.availableKernelModules = [ "ahci" "sd_mod" "sr_mod" "virtio_pci" "virtio_net" ];

  fileSystems."/" = { device = "/dev/disk/by-label/nixos"; fsType = "ext4"; };
  fileSystems."/boot" = { device = "/dev/disk/by-label/BOOT"; fsType = "vfat"; options = [ "umask=0077" ]; };

  networking.hostName = "busybee-lab";
  networking.useDHCP = true;

  services.openssh = {
    enable = true;
    settings = { PasswordAuthentication = false; KbdInteractiveAuthentication = false; PermitRootLogin = "prohibit-password"; };
    hostKeys = [ { path = "/etc/ssh/ssh_host_ed25519_key"; type = "ed25519"; } ];
    # Written by install.sh; the designated bootstrap access, nothing else.
    authorizedKeysFiles = [ "/etc/busybee-lab/authorized_keys" ];
  };

  nix.settings = {
    experimental-features = [ "nix-command" "flakes" ];
    trusted-users = [ "root" ];
  };

  environment.systemPackages = with pkgs; [ git ];

  users.mutableUsers = false;
  # Root logs in only with the key file above, which NixOS cannot see at
  # evaluation time; without this the lockout assertion fails the build.
  users.allowNoPasswordLogin = true;
  time.timeZone = "UTC";
  system.stateVersion = "26.05";
}
