"""Command access to a guest: SSH with a run-scoped key and a pinned host key.

The guest's address comes from Parallels' DHCP lease for the VM's MAC, so the
host runs no listener the guest calls back to.
"""
from pathlib import Path
import shlex
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
    """`user` is the account the key logs into. `posix` runs every command
    under /bin/sh instead of that account's login shell: macOS's is zsh, which
    aborts a command on a glob that matches nothing."""

    def __init__(self, ip, key, known_hosts, accept_new=False, user="root", posix=False):
        self.ip, self.key, self.known_hosts = ip, Path(key), Path(known_hosts)
        self.accept_new, self.user, self.posix = accept_new, user, posix

    def _command(self, command):
        return f"exec /bin/sh -c {shlex.quote(command)}" if self.posix else command

    def _ssh(self, extra=()):
        return ["ssh", "-i", str(self.key), "-o", "IdentitiesOnly=yes", "-o", "BatchMode=yes",
                "-o", f"UserKnownHostsFile={self.known_hosts}",
                "-o", f"StrictHostKeyChecking={'accept-new' if self.accept_new else 'yes'}",
                "-o", f"ConnectTimeout={SSH_CONNECT_S}", "-o", "ServerAliveInterval=15", *extra,
                f"{self.user}@{self.ip}"]

    def run(self, command, timeout, stdin=None, tty=False, check=True, raw=False):
        """Run a shell command in the guest; returns (exit status, stdout, stderr).
        `raw` keeps stdout as bytes, for content that must round-trip exactly."""
        argv = self._ssh(["-tt"] if tty else []) + [self._command(command)]
        try:
            done = subprocess.run(argv, input=stdin, capture_output=True, timeout=timeout)
        except subprocess.TimeoutExpired as err:
            raise GuestError(f"`{command}` exceeded its {timeout}s deadline") from err
        out = done.stdout if raw else done.stdout.decode(errors="replace")
        err = done.stderr.decode(errors="replace")
        if check and done.returncode != 0:
            raise GuestError(f"`{command}` exited {done.returncode}: {err.strip()[-500:]}")
        return done.returncode, out, err

    def spawn(self, command, stdout, stderr):
        """Start a command with its output written straight to open files and
        return the process to poll. It outlives a controller that dies, so the
        files keep everything the guest sent."""
        return subprocess.Popen(self._ssh() + [self._command(command)], stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr)

    def session(self, command, forward, stdin=None, stdout=None, stderr=None):
        """An issue agent's turn: `command` with this process's streams, and the
        session broker's host socket (`forward` = (guest path, host path))
        forwarded into the guest. A forward that cannot be set up ends it."""
        remote, local = forward
        extra = ["-T", "-o", "ExitOnForwardFailure=yes", "-R", f"{remote}:{local}"]
        return subprocess.Popen(self._ssh(extra) + [self._command(command)], stdin=stdin, stdout=stdout,
                                stderr=stderr)

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
