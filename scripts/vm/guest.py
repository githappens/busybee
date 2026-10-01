"""Command access to a guest: SSH with a run-scoped key and a pinned host key.

The guest's address comes from Parallels' DHCP lease for the VM's MAC, so the
host runs no listener the guest calls back to.
"""
from pathlib import Path
import socket
import subprocess
import time

LEASES = Path("/Library/Preferences/Parallels/parallels_dhcp_leases")
SSH_CONNECT_S = 5


class GuestError(RuntimeError):
    pass


def lease_ip(text, mac):
    """The address of the newest lease for `mac` in Parallels' lease file."""
    want = mac.replace(":", "").lower()
    best = None
    for line in text.splitlines():
        ip, _, value = line.partition("=")
        fields = value.strip().strip('"').split(",")
        if len(fields) >= 3 and fields[2].lower() == want:
            started = int(fields[0])
            if best is None or started > best[0]:
                best = (started, ip.strip())
    return best[1] if best else None


def wait_for_lease(mac, deadline):
    while time.monotonic() < deadline:
        ip = lease_ip(LEASES.read_text(), mac) if LEASES.exists() else None
        if ip:
            return ip
        time.sleep(2)
    raise GuestError(f"no DHCP lease for {mac} before the deadline")


def wait_for_port(ip, port, deadline):
    while time.monotonic() < deadline:
        try:
            socket.create_connection((ip, port), timeout=2).close()
            return
        except OSError:
            time.sleep(2)
    raise GuestError(f"{ip}:{port} did not open before the deadline")


class Guest:
    def __init__(self, ip, key, known_hosts, accept_new=False):
        self.ip, self.key, self.known_hosts = ip, Path(key), Path(known_hosts)
        self.accept_new = accept_new

    def _ssh(self, extra=()):
        return ["ssh", "-i", str(self.key), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
                "-o", f"UserKnownHostsFile={self.known_hosts}",
                "-o", f"StrictHostKeyChecking={'accept-new' if self.accept_new else 'yes'}",
                "-o", f"ConnectTimeout={SSH_CONNECT_S}", "-o", "ServerAliveInterval=15", *extra,
                f"root@{self.ip}"]

    def run(self, command, timeout, stdin=None, tty=False, check=True, raw=False):
        """Run a shell command in the guest; returns (exit status, stdout, stderr).
        `raw` keeps stdout as bytes, for content that must round-trip exactly."""
        argv = self._ssh(["-tt"] if tty else []) + [command]
        try:
            done = subprocess.run(argv, input=stdin, capture_output=True, timeout=timeout)
        except subprocess.TimeoutExpired as err:
            raise GuestError(f"`{command}` exceeded its {timeout}s deadline") from err
        out = done.stdout if raw else done.stdout.decode(errors="replace")
        err = done.stderr.decode(errors="replace")
        if check and done.returncode != 0:
            raise GuestError(f"`{command}` exited {done.returncode}: {err.strip()[-500:]}")
        return done.returncode, out, err

    def wait(self, deadline):
        """Until the guest accepts a command, or the deadline passes."""
        last = "no attempt"
        while time.monotonic() < deadline:
            try:
                self.run("true", timeout=SSH_CONNECT_S + 5)
                return
            except GuestError as err:
                last = str(err)
            time.sleep(3)
        raise GuestError(f"guest at {self.ip} did not accept commands before the deadline: {last}")
