#!/usr/bin/env bash
# Runs as the guest's lab account (passwordless sudo) in a candidate cloned
# from the operator's prepared macOS VM. Unattended: installs the pinned
# Command Line Tools through softwareupdate, names the host neutrally, and
# replaces the source's SSH host key and authorized keys with the run's. The
# key that ran this script stops working once it finishes.
# Usage: provision.sh <clt label> <hostname> <authorized_keys> <host key>
set -euo pipefail
label=$1 name=$2 authorized=$3 hostkey=$4

if ! xcode-select -p >/dev/null 2>&1; then
  # softwareupdate offers the Command Line Tools only while this marker exists.
  marker=/private/tmp/.com.apple.dt.CommandLineTools.installondemand.in-progress
  sudo -n touch "$marker"
  listed=$(softwareupdate --list 2>&1 || true)
  if ! grep -qF "Label: $label" <<<"$listed"; then
    echo "provision.sh: softwareupdate does not offer '$label'" >&2
    exit 1
  fi
  sudo -n softwareupdate --install "$label"
  sudo -n rm -f "$marker"
fi
xcode-select -p >/dev/null

for kind in HostName LocalHostName ComputerName; do
  sudo -n scutil --set "$kind" "$name"
done

sudo -n install -m 0600 -o root -g wheel "$hostkey" /etc/ssh/ssh_host_ed25519_key
sudo -n ssh-keygen -y -f /etc/ssh/ssh_host_ed25519_key | sudo -n tee /etc/ssh/ssh_host_ed25519_key.pub >/dev/null
install -d -m 0700 "$HOME/.ssh"
install -m 0600 "$authorized" "$HOME/.ssh/authorized_keys"
rm -f "$authorized" "$hostkey"
