# SPDX-License-Identifier: AGPL-3.0-or-later
{
  config,
  lib,
  pkgs,
  ...
}:

{
  imports = [ ];

  system.stateVersion = "26.11";

  networking.hostName = "looklab";
  networking.domain = "fujimod.com";
  time.timeZone = "UTC";

  disko.devices.disk.main = {
    type = "disk";
    device = "/dev/sda";
    content = {
      type = "gpt";
      partitions = {
        bios = {
          size = "1M";
          type = "EF02";
        };
        esp = {
          size = "512M";
          type = "EF00";
          content = {
            type = "filesystem";
            format = "vfat";
            mountpoint = "/boot";
            mountOptions = [ "umask=0077" ];
          };
        };
        root = {
          size = "100%";
          content = {
            type = "filesystem";
            format = "ext4";
            mountpoint = "/";
          };
        };
      };
    };
  };

  boot.loader.grub = {
    enable = true;
    device = "/dev/sda";
    efiSupport = true;
    efiInstallAsRemovable = true;
  };
  boot.loader.timeout = 5;
  boot.initrd.availableKernelModules = [
    "virtio_pci"
    "virtio_scsi"
    "virtio_blk"
    "virtio_net"
    "sd_mod"
    "ext4"
  ];

  networking.useNetworkd = true;
  networking.useDHCP = false;
  networking.nameservers = [ "213.186.33.99" ];
  systemd.network.networks."10-ens3" = {
    matchConfig.MACAddress = "fa:16:3e:56:c6:13";
    networkConfig = {
      DHCP = "ipv4";
      IPv6AcceptRA = false;
    };
    address = [ "2604:2dc0:101:200::1955/128" ];
    routes = [
      {
        Destination = "2604:2dc0:101:200::1/64";
        Scope = "link";
      }
      {
        Destination = "::/0";
        Gateway = "2604:2dc0:101:200::1";
      }
    ];
  };

  services.openssh = {
    enable = true;
    settings = {
      PasswordAuthentication = false;
      KbdInteractiveAuthentication = false;
      PermitRootLogin = "prohibit-password";
    };
  };
  users.users.root.openssh.authorizedKeys.keyFiles = [ ../keys/root.pub ];

  networking.firewall.enable = true;
  networking.firewall.allowedTCPPorts = [ 22 ];

  nix.settings = {
    experimental-features = [
      "nix-command"
      "flakes"
    ];
    auto-optimise-store = true;
    trusted-users = [ "root" ];
  };
  nix.gc = {
    automatic = true;
    dates = "weekly";
    options = "--delete-older-than 30d";
  };
  nix.optimise.automatic = true;

  services.looklab = {
    enable = true;
    hostname = "looklab.fujimod.com";
    caddy.acmeEmail = null;
    litestream.enable = false;
    worker.memoryMax = "2.5G";
    worker.memoryHigh = "2G";
  };

  swapDevices = [ ];
}
