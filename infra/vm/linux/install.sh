#!/usr/bin/env bash
# Runs as root in the live installer of a dedicated candidate VM, which is the
# Linux builder: the system is evaluated and built here, never on the host.
# Usage: install.sh <flake dir> <configuration> <authorized_keys> <host key>
set -euo pipefail
flake=$1 config=$2 authorized=$3 hostkey=$4
disk=/dev/sda

[ -b "$disk" ] || { echo "install.sh: $disk is not a block device" >&2; exit 1; }
if lsblk -no PARTTYPE "$disk" | grep -q .; then
  echo "install.sh: $disk already has partitions; refusing to overwrite" >&2
  exit 1
fi

parted -s "$disk" -- mklabel gpt mkpart ESP fat32 1MiB 513MiB set 1 esp on mkpart root ext4 513MiB 100%
udevadm settle
mkfs.fat -F 32 -n BOOT "${disk}1"
mkfs.ext4 -q -F -L nixos "${disk}2"
udevadm settle
mount /dev/disk/by-label/nixos /mnt
mkdir -p /mnt/boot
mount -o umask=0077 /dev/disk/by-label/BOOT /mnt/boot

install -D -m 0644 "$authorized" /mnt/etc/busybee-lab/authorized_keys
install -D -m 0600 "$hostkey" /mnt/etc/ssh/ssh_host_ed25519_key

nixos-install --flake "$flake#$config" --no-root-passwd --no-channel-copy
