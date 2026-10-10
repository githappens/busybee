#!/usr/bin/env python3
"""A real PTY in a lab guest: one zellij session per terminal, its command recorded by `script`.

See docs/design/agent-lab.md §Make UI behavior observable. Runs in the guest:
scenario procedures use Terminal directly, and the controller's `vmctl
terminal` operations use this file's CLI. Nothing here renders; cells and
images are made on the host from the recording.

- A terminal is one zellij session with a unique name and one borderless pane,
  no bars and no session serialization. A headless zellij session is fixed at
  50x50, so a zellij client is attached through a PTY this process owns, and a
  resize is a window-size change on that PTY.
- The pane runs the command under util-linux `script`, whose advanced timing
  log records output and input chunks, every window-size change and the exit
  code: the raw recording. zellij's own screen (`dump-screen`) is the second,
  independent view a capture records.
- Input goes through zellij actions aimed at the pane: bytes (`write`), text
  (`write-chars`) or named keys (`send-keys`), which zellij encodes for the
  pane's current terminal modes as a terminal would.
- Every process the terminal starts carries MARKER=<its directory>, and close
  stops exactly those.

A terminal's directory holds `state.json` (SCHEMA), the generated zellij
config and layout, `run.sh`, `recording/{output,input,timing}` and
`captures/NNNN.json`; zellij's sockets, home and temp files go under a separate
runtime directory, inside the scenario's fixture root when a scenario owns it.

CLI (the controller's side; prints one JSON document):

    terminal.py hold --dir D --session S --cols C --rows R --cwd W [--env N=V]... -- ARGV
    terminal.py send --dir D (--text T | --key K... | --bytes HEX)
    terminal.py resize --dir D --cols C --rows R
    terminal.py capture --dir D [--expect TEXT] [--timeout S]

`hold` starts the terminal and keeps it until its command exits or it is
signalled (TERM, INT or HUP); its exit status is 0 when the terminal was held
and closed, whatever the command's own status, which is in state.json.
"""
import argparse
from datetime import datetime, timezone
import fcntl
import json
import os
from pathlib import Path
import re
import shlex
import signal
import struct
import subprocess
import sys
import termios
import threading
import time

SCHEMA = "busybee.terminal/v1"
CAPTURE_SCHEMA = "busybee.terminal.capture/v1"
# Pane-id actions (write, send-keys, dump-screen and list-panes for one pane).
MIN_ZELLIJ = "0.45"
MARKER = "BUSYBEE_TERMINAL"
TERM = "xterm-256color"
LANG = "C.UTF-8"
# sun_path on Linux, including its terminating NUL; zellij adds a version
# directory and the session name under its socket directory.
SOCKET_LIMIT = 108
SOCKET_SUFFIX = len("/contract_version_1/")
POLL_S = 0.05
# A capture waits for output to pause this long; shorter than a 4 Hz redraw.
SETTLE_S = 0.1
ACTION_S = 10
START_S = 30
# Inside timeout(1)'s 10s kill grace, so a holder ended by its deadline still closes.
CLOSE_S = 8
HOLD_POLL_S = 0.2
HOLD_FAILURES = 5
SIZE_BOUNDS = {"cols": (10, 500), "rows": (5, 200)}
SESSION = re.compile(r"^[a-z0-9][a-z0-9-]{0,40}$")
# What a failed start reports of the zellij client's own output.
CLIENT_TAIL = 2048

CONFIG = """\
show_release_notes false
show_startup_tips false
pane_frames false
session_serialization false
mouse_mode false
web_server false
"""

# Token format: token_N: <UUID>[ (read-only)]
_TOKEN_RE = re.compile(r"^(token_\d+):\s+([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})",
                       re.IGNORECASE)


def _parse_web_token(output):
    """Parse (name, value) from `zellij web --create-*-token` output.
    Format: 'token_N: <UUID>[ (read-only)]'  Returns (None, None) on failure."""
    for line in output.splitlines():
        m = _TOKEN_RE.match(line.strip())
        if m:
            return m.group(1), m.group(2)
    return None, None


class TerminalError(Exception):
    """The terminal could not be provided as asked: never a product result."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


def now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ")


def version_of(text):
    """`zellij 0.45.1` to `0.45.1`, or None."""
    words = (text or "").split()
    return words[1] if len(words) == 2 and words[0] == "zellij" and re.match(r"^\d+\.\d+", words[1]) else None


def at_least(version, minimum):
    def parts(v):
        return [int(p) for p in re.findall(r"\d+", v)[:3]]
    return parts(version) >= parts(minimum)


def next_capture(captures):
    """The next free NNNN capture name in a captures directory."""
    taken = [int(p.stem) for p in Path(captures).glob("[0-9][0-9][0-9][0-9].json")]
    return f"{max(taken, default=0) + 1:04d}"


def check_size(cols, rows):
    for key, value in (("cols", cols), ("rows", rows)):
        low, high = SIZE_BOUNDS[key]
        if not low <= value <= high:
            raise TerminalError("size_invalid", f"{key} {value} is outside {low}..{high}")


def kdl_string(text):
    """A KDL string for a generated path; anything needing escapes is refused."""
    if re.search(r'["\\\x00-\x1f]', text):
        raise TerminalError("path_invalid", f"{text!r} cannot be written into the zellij layout")
    return f'"{text}"'


# zellij can start a pane's command before it sizes the pane's PTY. Started
# then, `script` records the size as -1x-1 and the program may draw before any
# size is known, which no replay can lay out. Wait (bounded: 20 x 0.1s) for a
# size; a pane that never gets one still starts, and its replay fails loudly.
AWAIT_SIZE = ('i=0; while [ "$(stty size 2>/dev/null)" = "0 0" ] && [ $i -lt 20 ]; do '
              'sleep 0.1; i=$((i + 1)); done')


def run_script(rec, argv, cwd, env):
    """The pane's command: `script` recording argv in advanced timing format,
    flushing every write (-f) so a capture can replay what the screen shows,
    and exiting with argv's status (-e) so zellij reports it too."""
    words = ["env", *(f"{k}={v}" for k, v in env.items()), "script", "-q", "-f", "-e", "-E", "never", "-m", "advanced",
             "-O", str(rec / "output"), "-I", str(rec / "input"), "-T", str(rec / "timing"),
             "-c", "exec " + shlex.join(argv)]
    return f"#!/bin/sh\ncd {shlex.quote(str(cwd))} || exit 127\n{AWAIT_SIZE}\nexec {shlex.join(words)}\n"


def layout(command):
    return f"layout {{\n    pane borderless=true command={kdl_string(str(command))}\n}}\n"


def key_bytes(spec):
    """`--bytes` input: hex digits, optionally separated by spaces."""
    try:
        data = bytes.fromhex(spec)
    except ValueError as err:
        raise TerminalError("input_invalid", f"--bytes takes hex, not {spec!r}") from err
    if not data:
        raise TerminalError("input_invalid", "--bytes is empty")
    return data


def pick_pane(panes, command):
    """The session's one terminal pane, checked to be running our command. A
    session that already existed under this name would run something else."""
    terminals = [p for p in panes if not p.get("is_plugin")]
    if len(terminals) != 1:
        raise TerminalError("session_mismatch", f"the session has {len(terminals)} terminal panes, not 1")
    pane = terminals[0]
    if str(command) not in str(pane.get("terminal_command") or ""):
        raise TerminalError("session_mismatch", f"the pane runs {pane.get('terminal_command')!r}, not {command}; "
                            "a session of that name already existed")
    return pane


def pane_size(pane):
    return pane.get("pane_content_columns"), pane.get("pane_content_rows")


def marked(path, proc=Path("/proc")):
    """Pids of processes carrying MARKER=<path> (Linux /proc)."""
    want = f"{MARKER}={path}".encode()
    found = []
    for entry in proc.iterdir():
        if not entry.name.isdigit() or int(entry.name) == os.getpid():
            continue
        try:
            environ = (entry / "environ").read_bytes()
        except OSError:
            continue
        if want in environ.split(b"\0"):
            found.append(int(entry.name))
    return sorted(found)


def _set_size(fd, cols, rows):
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))


class Terminal:
    """One terminal. `user` prefixes every zellij command (the scenario's
    unprivileged user); `owner` is the (uid, gid) that must be able to write
    its files; `runtime` holds zellij's sockets, home and temp files."""

    def __init__(self, path, session, argv, cols, rows, cwd, env, runtime, user=(), owner=None, run=subprocess.run,
                 clock=time.monotonic, sleep=time.sleep, procs=marked):
        if not SESSION.match(session):
            raise TerminalError("session_invalid", f"{session!r} is not a session name")
        check_size(cols, rows)
        self.path, self.session, self.argv = Path(path), session, list(argv)
        self.cols, self.rows, self.cwd = cols, rows, str(cwd)
        self.env, self.runtime = dict(env), Path(runtime)
        self.user, self.owner = list(user), owner
        self.run, self.clock, self.sleep, self.procs = run, clock, sleep, procs
        self.client = None
        self.master = None
        self.pts = None
        self.pane = None
        self.client_pid = None
        self.drained = 0
        self.client_tail = b""
        self.zellij = None
        self.started = None

    # Files

    @property
    def rec(self):
        return self.path / "recording"

    def _zellij_env(self):
        return {**self.env, "TERM": self.env.get("TERM", TERM), "LANG": self.env.get("LANG", LANG),
                "HOME": self.env.get("HOME", str(self.runtime / "home")), "TMPDIR": str(self.runtime / "tmp"),
                "ZELLIJ_SOCKET_DIR": str(self.runtime / "sock"), MARKER: str(self.path)}

    def _zellij(self, *args, timeout=ACTION_S, check=True):
        argv = [*self.user, "zellij", "--session", self.session, *args]
        try:
            done = self.run(argv, env=self._zellij_env(), capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired as err:
            raise TerminalError("zellij_unresponsive", f"zellij {' '.join(args[:2])} took over {timeout}s") from err
        if check and done.returncode != 0:
            raise TerminalError("zellij_failed", f"zellij {' '.join(args[:2])} exited {done.returncode}: "
                                f"{done.stderr.strip()[-500:]}")
        return done

    def state(self, **update):
        path = self.path / "state.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        current = json.loads(path.read_text()) if path.is_file() else {}
        current.update(update)
        tmp = path.with_suffix(".tmp")
        tmp.write_text(json.dumps(current, indent=2) + "\n")
        os.replace(tmp, path)
        return current

    def _own(self, *paths):
        if self.owner:
            for p in paths:
                os.chown(p, *self.owner)

    def _write(self):
        socket = len(str(self.runtime / "sock")) + SOCKET_SUFFIX + len(self.session) + 1
        if socket > SOCKET_LIMIT:
            raise TerminalError("socket_path_too_long", f"zellij's socket path would be {socket} bytes; "
                                f"the limit is {SOCKET_LIMIT}")
        for d in (self.path, self.rec, self.path / "captures", self.runtime, self.runtime / "home",
                  self.runtime / "tmp", self.runtime / "sock"):
            d.mkdir(mode=0o755, parents=True, exist_ok=True)
            self._own(d)
        app_env = {"TERM": self._zellij_env()["TERM"], "LANG": self._zellij_env()["LANG"], "SHELL": "/bin/sh"}
        files = {"config.kdl": CONFIG, "layout.kdl": layout(self.path / "run.sh"),
                 "run.sh": run_script(self.rec, self.argv, self.cwd, app_env)}
        for name, text in files.items():
            (self.path / name).write_text(text)
            (self.path / name).chmod(0o755 if name == "run.sh" else 0o644)

    # Lifecycle

    def preflight(self):
        done = self.run([*self.user, "zellij", "--version"], env=self._zellij_env(), capture_output=True, text=True,
                        timeout=ACTION_S)
        self.zellij = version_of(done.stdout) if done.returncode == 0 else None
        if self.zellij is None:
            raise TerminalError("tool_missing", f"zellij --version exited {done.returncode}, printed "
                                f"{done.stdout.strip()!r}: {done.stderr.strip()[-300:]}")
        if not at_least(self.zellij, MIN_ZELLIJ):
            raise TerminalError("tool_version_too_old", f"zellij is {self.zellij}; terminals need >={MIN_ZELLIJ}")

    def start(self, deadline):
        """Start the session at cols x rows and wait for its pane. Raises
        TerminalError when what started is not the terminal that was asked for."""
        self._write()
        self.preflight()
        self.state(schema=SCHEMA, status="starting", session=self.session, argv=self.argv, cwd=self.cwd, env=self.env,
                   cols=self.cols, rows=self.rows, term=self._zellij_env()["TERM"], lang=self._zellij_env()["LANG"],
                   zellij=self.zellij, runtime=str(self.runtime), holder_pid=os.getpid(), python=sys.executable,
                   started_at=now())
        self.master, slave = os.openpty()
        _set_size(slave, self.cols, self.rows)
        self.pts = os.ttyname(slave)
        if self.owner:
            os.chown(self.pts, *self.owner)
        self.started = self.clock()
        # --config-dir, not --config: zellij 0.45.1 given --config ignores the
        # layout file and starts its default layout without a word.
        self.client = subprocess.Popen(
            [*self.user, "zellij", "--config-dir", str(self.path), "--session", self.session,
             "--new-session-with-layout", str(self.path / "layout.kdl")],
            stdin=slave, stdout=slave, stderr=slave, env=self._zellij_env(), cwd=self.runtime,
            start_new_session=True)
        os.close(slave)
        threading.Thread(target=self._drain, daemon=True).start()
        self.state(pts=self.pts, client_pid=self.client.pid)
        # No zellij action before the pane's command runs: an action is a
        # client connection too, and one that ends before the first client has
        # set the session up crashes zellij 0.45.1's server. `script` creates
        # the recording only once the session has spawned the pane.
        self._until(self._recording_started, deadline, "the pane's command")
        self.pane = self._until(self._find_pane, deadline, "the zellij pane")
        if pane_size(self.pane) != (self.cols, self.rows):
            raise TerminalError("size_mismatch", f"the pane is {'x'.join(map(str, pane_size(self.pane)))}, "
                                f"not {self.cols}x{self.rows}")
        self.state(status="ready", pane=self.pane["id"], ready_at=now())

    def _drain(self):
        """Read the client's side of the PTY so zellij never blocks writing to it."""
        while True:
            try:
                data = os.read(self.master, 65536)
            except OSError:
                return
            if not data:
                return
            self.drained += len(data)
            self.client_tail = (self.client_tail + data)[-CLIENT_TAIL:]

    def _recording_started(self):
        if self.client.poll() is not None:
            raise TerminalError("zellij_failed", f"the zellij client exited {self.client.returncode} before its "
                                f"pane started: {self.client_tail.decode(errors='replace')!r}")
        return (self.rec / "timing").exists()

    def _find_pane(self):
        done = self._zellij("action", "list-panes", "-a", "-j", check=False)
        if done.returncode != 0 or not done.stdout.strip():
            return None
        return pick_pane(json.loads(done.stdout), self.path / "run.sh")

    def _until(self, check, deadline, what):
        while self.clock() < deadline:
            found = check()
            if found:
                return found
            self.sleep(POLL_S)
        raise TerminalError("terminal_not_ready", f"{what} did not appear before the deadline")

    @classmethod
    def attach(cls, path, **kw):
        """The running terminal recorded in `path`/state.json, for one operation."""
        try:
            state = json.loads((Path(path) / "state.json").read_text())
        except (OSError, ValueError) as err:
            raise TerminalError("terminal_not_ready", f"{path} holds no terminal state") from err
        if state.get("schema") != SCHEMA or "pane" not in state:
            raise TerminalError("terminal_not_ready", f"{path} holds no ready terminal")
        t = cls(path, state["session"], state["argv"], state["cols"], state["rows"], state["cwd"], state["env"],
                state["runtime"], **kw)
        if state["status"] != "ready":
            raise TerminalError("terminal_closed", f"the terminal is {state['status']}")
        t.pts, t.pane, t.zellij, t.client_pid = state["pts"], {"id": state["pane"]}, state["zellij"], state["client_pid"]
        return t

    def _pane_id(self):
        return f"terminal_{self.pane['id']}"

    def panes(self, timeout=ACTION_S):
        out = self._zellij("action", "list-panes", "-a", "-j", timeout=timeout).stdout
        try:
            return json.loads(out)
        except ValueError as err:
            raise TerminalError("zellij_failed", f"list-panes printed {out[:200]!r}") from err

    def app(self, timeout=ACTION_S):
        """The pane as zellij reports it: geometry, whether its command exited
        and how; None when the session no longer has it."""
        return next((p for p in self.panes(timeout) if p.get("id") == self.pane["id"] and not p.get("is_plugin")), None)

    # Input and size

    def _zellij_web(self, *args, timeout=ACTION_S, check=True):
        """Run a `zellij web` subcommand in this terminal's environment."""
        argv = [*self.user, "zellij", "web", *args]
        try:
            done = self.run(argv, env=self._zellij_env(), capture_output=True, text=True, timeout=timeout)
        except subprocess.TimeoutExpired as err:
            raise TerminalError("zellij_unresponsive",
                                f"zellij web {args[0] if args else ''!r} took over {timeout}s") from err
        if check and done.returncode != 0:
            raise TerminalError("zellij_failed", f"zellij web {' '.join(args[:2])} exited {done.returncode}: "
                                f"{done.stderr.strip()[-500:]}")
        return done

    def watch_start(self, preferred_port):
        """Start the zellij web server on loopback and create a read-only token.
        Returns (port, token_value, token_name). The server listens on 127.0.0.1
        only; if one is already running its port is reused."""
        # Check whether the web server is already running (short timeout: best-effort).
        status_done = self._zellij_web("--status", "--timeout", "2", check=False)
        existing_port = None
        if "running" in status_done.stdout.lower():
            m = re.search(r":(\d+)", status_done.stdout)
            if m:
                existing_port = int(m.group(1))
        if existing_port is None:
            # Start on preferred_port; loopback binding is the default but made explicit.
            self._zellij_web("--start", "--ip", "127.0.0.1", "--port", str(preferred_port), "-d")
            port = preferred_port
        else:
            port = existing_port
        # Create a read-only token.  --token-name is mutually exclusive with
        # --create-read-only-token in zellij 0.45, so auto-naming is used.
        token_done = self._zellij_web("--create-read-only-token")
        token_name, token_value = _parse_web_token(token_done.stdout)
        if not token_value:
            raise TerminalError("token_parse_failed",
                                f"could not parse token from: {token_done.stdout[:200]!r}")
        return port, token_value, token_name

    def watch_stop(self, token_name):
        """Revoke a watch token; best-effort so a gone guest does not raise."""
        self._zellij_web("--revoke-token", token_name, check=False)

    def send_bytes(self, data):
        self._zellij("action", "write", "-p", self._pane_id(), *(str(b) for b in data))

    def send_text(self, text):
        self._zellij("action", "write-chars", "-p", self._pane_id(), text)

    def send_keys(self, *keys):
        self._zellij("action", "send-keys", "-p", self._pane_id(), *keys)

    def resize(self, cols, rows, deadline):
        """Change the PTY's window size, signal the client as a terminal
        emulator would, and wait until the pane has that size."""
        check_size(cols, rows)
        fd = os.open(self.pts, os.O_RDWR | os.O_NOCTTY)
        try:
            _set_size(fd, cols, rows)
        finally:
            os.close(fd)
        os.killpg(self.client.pid if self.client else self.client_pid, signal.SIGWINCH)
        self._until(lambda: pane_size(self.app() or {}) == (cols, rows), deadline, f"a {cols}x{rows} pane")
        self.cols, self.rows = cols, rows
        if (self.path / "state.json").is_file():
            self.state(cols=cols, rows=rows)

    # Observation

    def screen(self, ansi=False):
        return self._zellij("action", "dump-screen", "-p", self._pane_id(), *(["--ansi"] if ansi else [])).stdout

    def output_bytes(self):
        path = self.rec / "output"
        return path.stat().st_size if path.is_file() else 0

    def wait_text(self, text, deadline):
        """Until zellij's screen shows `text`; returns whether it did before the deadline."""
        while True:
            if text in self.screen():
                return True
            if self.clock() >= deadline:
                return False
            self.sleep(POLL_S)

    def capture(self, deadline, label=None):
        """zellij's screen pinned to a point of the recording, written to
        captures/NNNN.json. It waits until output has paused for SETTLE_S,
        takes the screen, and counts as settled only if no output arrived
        meanwhile, so the screen is that of the recording at `output_bytes`.
        A program that never pauses is captured at the deadline, unsettled."""
        size, quiet_since = self.output_bytes(), self.clock()
        while True:
            moment = self.clock()
            if moment - quiet_since >= SETTLE_S or moment >= deadline:
                shown, ansi, pane = self.screen(), self.screen(ansi=True), self.app()
                after = self.output_bytes()
                settled = after == size and moment - quiet_since >= SETTLE_S
                if settled or self.clock() >= deadline:
                    break
                size, quiet_since = after, self.clock()
                continue
            self.sleep(POLL_S)
            if self.output_bytes() != size:
                size, quiet_since = self.output_bytes(), self.clock()
        if pane is None:
            raise TerminalError("terminal_closed", "the session no longer has the pane")
        name = next_capture(self.path / "captures")
        capture = {"schema": CAPTURE_SCHEMA, "capture": name, "label": label, "at": now(),
                   "output_bytes": size if settled else after, "settled": settled,
                   "cols": pane_size(pane)[0], "rows": pane_size(pane)[1], "screen": shown, "ansi": ansi,
                   "pane": pane}
        (self.path / "captures" / f"{name}.json").write_text(json.dumps(capture, indent=2) + "\n")
        return capture

    def close(self, seconds=CLOSE_S):
        """End the session and every process carrying this terminal's marker:
        kill-session, then TERM and KILL for whatever is left, all within
        `seconds`. Records and returns what had to be signalled and what
        survived, after noting how zellij last saw the command."""
        start = self.clock()
        step = max(1, seconds // 4)
        try:
            pane = self.app(timeout=step) if self.pane else None
            self.state(app={"exited": bool(pane and pane.get("exited")),
                            "exit_status": pane.get("exit_status") if pane else None})
        except TerminalError:
            pass  # the session is already gone; the recording keeps the exit code
        try:
            self.run([*self.user, "zellij", "kill-session", self.session], env=self._zellij_env(),
                     capture_output=True, text=True, timeout=step)
        except subprocess.TimeoutExpired:
            pass  # the marker check below decides
        forced = []
        for sig, share in ((None, 0.5), (signal.SIGTERM, 0.75), (signal.SIGKILL, 1.0)):
            for pid in self.procs(self.path) if sig else ():
                try:
                    os.kill(pid, sig)
                    forced.append({"pid": pid, "signal": sig.name})
                except ProcessLookupError:
                    pass
            while self.procs(self.path) and self.clock() < start + seconds * share:
                self.sleep(POLL_S)
            if not self.procs(self.path):
                break
        if self.master is not None:
            os.close(self.master)
            self.master = None
        closed = {"forced": forced, "remaining": self.procs(self.path)}
        self.state(status="closed", closed_at=now(), closed=closed)
        return closed


# CLI: the controller's side

def _hold(args):
    env = {"PATH": os.environ.get("PATH", "/run/current-system/sw/bin:/usr/bin:/bin"),
           **dict(item.split("=", 1) for item in args.env)}
    stop = []
    for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(sig, lambda signum, _: stop.append(signal.Signals(signum).name))
    path = Path(args.dir)
    try:
        t = Terminal(path, args.session, args.argv, args.cols, args.rows, args.cwd, env, path / "runtime")
    except TerminalError as err:
        path.mkdir(parents=True, exist_ok=True)
        (path / "state.json").write_text(json.dumps({"schema": SCHEMA, "status": "failed", "code": err.code,
                                                     "message": str(err)}) + "\n")
        return 2
    try:
        t.start(time.monotonic() + START_S)
    except (TerminalError, OSError, ValueError) as err:
        closed = t.close()
        t.state(status="failed", code=getattr(err, "code", "terminal_failed"), message=str(err), closed=closed,
                client_output=t.client_tail.decode(errors="replace"))
        return 2
    lost, failures = None, 0
    try:
        while not stop:
            try:
                pane = t.app()
                failures = 0
            except TerminalError as err:
                # One unanswered query is not a lost session; several in a row are.
                failures += 1
                if failures >= HOLD_FAILURES:
                    lost = f"{err.code}: {err}"
                    break
                time.sleep(HOLD_POLL_S)
                continue
            if pane is None or pane.get("exited"):
                lost = None if pane else "the session no longer has the pane"
                break
            time.sleep(HOLD_POLL_S)
    finally:
        closed = t.close()
        t.state(stopped_by=stop[0] if stop else None, lost=lost, drained_bytes=t.drained)
    return 2 if closed["remaining"] or lost else 0


def _watch_start(args):
    """Start the zellij web server and mint a read-only token for this handle."""
    t = Terminal.attach(args.dir)
    # Ask the OS for a free loopback port in the guest.
    import socket as _socket
    s = _socket.socket(_socket.AF_INET, _socket.SOCK_STREAM)
    s.setsockopt(_socket.SOL_SOCKET, _socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", 0))
    preferred_port = s.getsockname()[1]
    s.close()
    port, token, token_name = t.watch_start(preferred_port)
    return {"port": port, "token": token, "token_name": token_name}


def _watch_stop(args):
    """Revoke the watch token; run on cleanup regardless of terminal state."""
    t = Terminal.attach(args.dir)
    t.watch_stop(args.token_name)
    return {"revoked": args.token_name}


def _op(args):
    t = Terminal.attach(args.dir)
    if args.command == "send":
        if args.text is not None:
            t.send_text(args.text)
        elif args.key:
            t.send_keys(*args.key)
        else:
            t.send_bytes(key_bytes(args.bytes))
        return {"sent": True}
    if args.command == "resize":
        t.resize(args.cols, args.rows, time.monotonic() + args.timeout)
        return {"cols": args.cols, "rows": args.rows}
    deadline = time.monotonic() + args.timeout
    found = t.wait_text(args.expect, deadline) if args.expect is not None else None
    capture = t.capture(deadline)
    return {**capture, "expected": args.expect, "expect_seen": found}


def main(argv=None):
    parser = argparse.ArgumentParser(prog="terminal", description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    hold = sub.add_parser("hold")
    hold.add_argument("--session", required=True)
    hold.add_argument("--cols", type=int, required=True)
    hold.add_argument("--rows", type=int, required=True)
    hold.add_argument("--cwd", required=True)
    hold.add_argument("--env", action="append", default=[], metavar="NAME=VALUE")
    send = sub.add_parser("send")
    what = send.add_mutually_exclusive_group(required=True)
    what.add_argument("--text")
    what.add_argument("--key", action="append")
    what.add_argument("--bytes")
    resize = sub.add_parser("resize")
    resize.add_argument("--cols", type=int, required=True)
    resize.add_argument("--rows", type=int, required=True)
    resize.add_argument("--timeout", type=float, default=ACTION_S)
    capture = sub.add_parser("capture")
    capture.add_argument("--expect")
    capture.add_argument("--timeout", type=float, default=ACTION_S)
    watch_start = sub.add_parser("watch_start")
    watch_stop = sub.add_parser("watch_stop")
    watch_stop.add_argument("--token-name", required=True)
    for p in (hold, send, resize, capture, watch_start, watch_stop):
        p.add_argument("--dir", required=True)
    argv = sys.argv[1:] if argv is None else argv
    split = argv.index("--") if "--" in argv else len(argv)
    args = parser.parse_args(argv[:split])
    args.argv = argv[split + 1:]
    if args.command == "hold":
        if not args.argv:
            parser.error("hold needs the command after --")
        return _hold(args)
    try:
        if args.command == "watch_start":
            print(json.dumps(_watch_start(args)))
            return 0
        if args.command == "watch_stop":
            print(json.dumps(_watch_stop(args)))
            return 0
        print(json.dumps(_op(args)))
        return 0
    except TerminalError as err:
        print(json.dumps({"error": err.code, "message": str(err)}))
        return 2


if __name__ == "__main__":
    sys.exit(main())
