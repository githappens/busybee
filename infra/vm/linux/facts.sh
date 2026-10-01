#!/usr/bin/env bash
# Prints the guest state a baseline must not carry, one `kind: value` per line.
# Not -e: an empty search is the expected answer, and find reports unreadable
# pseudo-files under /run as errors.
set -u
pgrep -lx 'pueued|bzbd|busybee' | sed 's/^[0-9]* /process: /'
find /run /tmp /var/tmp /root -type s \( -name '*bzbd*' -o -name '*pueue*' \) 2>/dev/null | sed 's/^/socket: /'
for path in /root/busybee /root/.local/state/busybee /root/.local/share/pueue /root/.config/pueue; do
  [ -e "$path" ] && echo "path: $path"
done
find / -xdev -name leases.json -path '*busybee*' 2>/dev/null | sed 's/^/path: /'
cat /etc/busybee-lab/authorized_keys /root/.ssh/authorized_keys 2>/dev/null | sed '/^$/d; s/^/key: /'
