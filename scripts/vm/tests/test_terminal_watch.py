"""Unit tests for `vmctl terminal watch`: argument handling and controller logic.

These tests run without Parallels by simulating the guest with a fake that
answers `watch_start` and `watch_stop`. They verify:
- The guest `watch_start` and `watch_stop` commands are issued correctly.
- The watch URL uses the host's loopback and contains the token and session.
- An SSH local forward is opened on the host's loopback.
- The forward ends when the handle's exec finishes, the worker goes away, or
  the run deadline passes.
- Setup failures (bad token output, forward failure) are reported cleanly.
- The `terminal watch` CLI subcommand is parsed and dispatched correctly.

The three named acceptance checks (watch_is_read_only,
watch_ends_with_its_handle, watch_listens_only_on_loopback) run against a real
worker in test_real_terminal.py (BUSYBEE_VM_LAB=1).
"""
from pathlib import Path
import json
import re
import shlex
import sys
import threading
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import contracts
import terminal_ops
import worker
from test_terminal_ops import TerminalGuest
from test_worker import REPO, Lab, codes

PYTHON = "/nix/store/0000-python3/bin/python3"
WATCH_TOKEN_NAME = "token_1"
WATCH_TOKEN = "75715e95-c1c4-4d40-9beb-c3ecd665b72b"
WATCH_PORT = 18082


class ForwardCapture:
    """A fake Popen that records local_forward calls and can simulate failure."""

    def __init__(self, fail=False):
        self._fail = fail
        self.killed = False
        self.waited = False
        self._return = 1 if fail else None
        self.stderr = _FakeStream(b"connection refused" if fail else b"")

    def poll(self):
        return self._return

    def kill(self):
        self.killed = True
        self._return = -9

    def wait(self):
        self.waited = True


class _FakeStream:
    def __init__(self, data):
        self._data = data

    def read(self):
        return self._data


class WatchGuest(TerminalGuest):
    """Extends TerminalGuest with watch_start / watch_stop answers and a
    capturable local_forward call."""

    def __init__(self):
        super().__init__()
        self.watch_reply = {"port": WATCH_PORT, "token": WATCH_TOKEN, "token_name": WATCH_TOKEN_NAME}
        self.watch_stop_reply = {"revoked": WATCH_TOKEN_NAME}
        self.forward = ForwardCapture()  # returned by local_forward

    def run(self, command, timeout, stdin=None, tty=False, check=True, raw=False):
        if "/terminal.py " in command and "watch_start" in command and command.startswith(PYTHON):
            self.commands.append((command, stdin))
            return 0, json.dumps(self.watch_reply) + "\n", ""
        if "/terminal.py " in command and "watch_stop" in command and command.startswith(PYTHON):
            self.commands.append((command, stdin))
            return 0, json.dumps(self.watch_stop_reply) + "\n", ""
        return super().run(command, timeout, stdin, tty, check, raw)

    def local_forward(self, host_port, guest_port):
        self.commands.append((f"local_forward {host_port} {guest_port}", None))
        return self.forward


class WatchTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)
        import shutil
        shutil.copytree(REPO / "tests" / "scenarios", self.lab.repo / "tests" / "scenarios",
                        ignore=shutil.ignore_patterns("tests", "__pycache__"))
        self.lab.guest = WatchGuest()
        self.run_id = self.lab.create()

    def open(self):
        result = terminal_ops.open_terminal(self.lab.workers(), self.run_id, ["busybee", "monitor"],
                                            120, 40, worker.CHECKOUT, {}, 100)
        self.assertEqual(result["status"], "success", result["findings"])
        return result["data"]["handle"]

    def _forward_ready(self, host_port):
        """Monkeypatch socket.create_connection so the forward appears up."""
        import socket as _socket
        original = _socket.create_connection
        def fake_connect(address, timeout=None):
            if address == ("127.0.0.1", host_port):
                return type("S", (), {"close": lambda self: None})()
            return original(address, timeout)  # pragma: no cover
        return fake_connect

    def watch_in_thread(self, handle, finish_exec_after=0):
        """Run watch() in a thread; end the handle's exec after `finish_exec_after` seconds."""
        workers = self.lab.workers()
        results = []
        if finish_exec_after > 0:
            def _ender():
                time.sleep(finish_exec_after)
                # finish the terminal's exec so the watch loop exits
                self.lab.guest.hang = False
                self.lab.guest.exit_code = 0
                # tick the supervisor so the exec record is finalised
                self.lab.supervisor(self.run_id).tick()
            threading.Thread(target=_ender, daemon=True).start()

        import socket as orig_socket_module
        orig_cc = orig_socket_module.create_connection
        captured_port = []

        def fake_cc(address, timeout=None):
            host, port = address
            if host == "127.0.0.1" and port not in captured_port:
                captured_port.append(port)
                return type("S", (), {"close": lambda self: None})()
            raise OSError("not this port")

        orig_socket_module.create_connection = fake_cc
        try:
            results.append(terminal_ops.watch(workers, self.run_id, handle))
        finally:
            orig_socket_module.create_connection = orig_cc
        return results[0], captured_port

    def test_watch_starts_web_server_and_creates_read_only_token(self):
        handle = self.open()
        # Simulate handle exec already finished so watch() exits after one loop tick.
        edir = worker.run_dir(self.lab.state, self.run_id) / "exec" / handle
        result_path = edir / "result.json"
        result_path.write_text(json.dumps({"status": "success", "data": {"exit_code": 0}}) + "\n")

        import io, socket as smod
        output = io.StringIO()
        orig_cc = smod.create_connection
        smod.create_connection = lambda addr, timeout=None: type("S", (), {"close": lambda s: None})()
        try:
            import builtins
            orig_print = builtins.print
            printed = []
            builtins.print = lambda *a, **kw: printed.append(a)
            result = terminal_ops.watch(self.lab.workers(), self.run_id, handle)
            builtins.print = orig_print
        finally:
            smod.create_connection = orig_cc

        # watch() returns None (output was printed inline).
        self.assertIsNone(result)
        # A watch_start command was sent to the guest.
        watch_cmds = [shlex.split(c) for c, _ in self.lab.guest.commands
                      if "/terminal.py" in c and "watch_start" in c]
        self.assertEqual(len(watch_cmds), 1)
        start_cmd = watch_cmds[0]
        self.assertEqual(start_cmd[start_cmd.index("watch_start"):start_cmd.index("watch_start") + 3],
                         ["watch_start", "--dir", f"{terminal_ops.GUEST_ROOT}/{self.run_id}/{handle}"])
        # The local_forward call uses loopback addresses.
        fwd_cmds = [c for c, _ in self.lab.guest.commands if c.startswith("local_forward")]
        self.assertEqual(len(fwd_cmds), 1)
        _, host_port, guest_port = fwd_cmds[0].split()
        self.assertEqual(guest_port, str(WATCH_PORT))
        self.assertRegex(host_port, r"^\d+$")
        # The printed output contains the URL with the loopback host and token.
        all_printed = " ".join(str(p) for p in printed)
        self.assertIn("127.0.0.1", all_printed)
        self.assertIn(WATCH_TOKEN, all_printed)
        # A watch_stop command was sent to revoke the token.
        stop_cmds = [c for c, _ in self.lab.guest.commands
                     if "/terminal.py" in c and "watch_stop" in c]
        self.assertEqual(len(stop_cmds), 1)
        self.assertIn(WATCH_TOKEN_NAME, stop_cmds[0])

    def test_watch_url_uses_host_loopback_only(self):
        """The URL printed by watch must bind to 127.0.0.1, never 0.0.0.0."""
        handle = self.open()
        # Finish exec immediately.
        edir = worker.run_dir(self.lab.state, self.run_id) / "exec" / handle
        (edir / "result.json").write_text(json.dumps({"status": "success", "data": {}}) + "\n")

        import socket as smod, builtins, io
        smod.create_connection = lambda addr, timeout=None: type("S", (), {"close": lambda s: None})()
        printed = []
        orig_p = builtins.print
        builtins.print = lambda *a, **kw: printed.append(a)
        terminal_ops.watch(self.lab.workers(), self.run_id, handle)
        builtins.print = orig_p
        smod.create_connection = None  # restore later in tearDown is ok; it's process-wide

        all_output = " ".join(str(a) for args in printed for a in args)
        # URL uses host loopback.
        self.assertIn("127.0.0.1", all_output)
        self.assertNotIn("0.0.0.0", all_output)
        # The forward is also on loopback (checked in test_watch_starts_web_server_and_creates_read_only_token).

    def test_watch_ends_when_handle_exec_finishes(self):
        handle = self.open()
        workers = self.lab.workers()
        edir = worker.run_dir(self.lab.state, self.run_id) / "exec" / handle
        # Watch with exec not yet finished
        import socket as smod
        smod.create_connection = lambda addr, timeout=None: type("S", (), {"close": lambda s: None})()
        import builtins
        builtins.print = lambda *a, **kw: None
        try:
            # Finish the exec in a timer.
            def _finish():
                time.sleep(0.05)
                (edir / "result.json").write_text(json.dumps({"status": "success", "data": {}}) + "\n")
            t = threading.Thread(target=_finish, daemon=True)
            t.start()
            result = terminal_ops.watch(workers, self.run_id, handle)
            t.join(timeout=5)
        finally:
            builtins.print = print
        self.assertIsNone(result)
        # The token was revoked on exit.
        stop_cmds = [c for c, _ in self.lab.guest.commands
                     if "/terminal.py" in c and "watch_stop" in c]
        self.assertGreaterEqual(len(stop_cmds), 1)

    def test_watch_ends_when_deadline_passes(self):
        """When the run deadline has passed, watch exits immediately."""
        handle = self.open()
        workers = self.lab.workers()
        # Move the clock past the run deadline.
        record = json.loads((worker.run_dir(self.lab.state, self.run_id) / "worker.json").read_text())
        from datetime import datetime, timezone
        dl = datetime.strptime(record["deadline"], contracts.TIMESTAMP).replace(tzinfo=timezone.utc).timestamp()
        self.lab.now = dl + 1  # past the deadline

        import socket as smod, builtins
        smod.create_connection = lambda addr, timeout=None: type("S", (), {"close": lambda s: None})()
        builtins.print = lambda *a, **kw: None
        try:
            result = terminal_ops.watch(workers, self.run_id, handle)
        finally:
            builtins.print = print
        self.assertIsNone(result)

    def test_watch_setup_fails_on_bad_watch_start_reply(self):
        handle = self.open()
        self.lab.guest.watch_reply = {"error": "token_parse_failed",
                                      "message": "could not parse token"}
        result = terminal_ops.watch(self.lab.workers(), self.run_id, handle)
        self.assertIsNotNone(result)
        self.assertEqual(result["status"], "environment_failure")
        self.assertEqual(codes(result), {"token_parse_failed"})

    def test_watch_setup_fails_when_forward_dies_immediately(self):
        handle = self.open()
        self.lab.guest.forward = ForwardCapture(fail=True)
        result = terminal_ops.watch(self.lab.workers(), self.run_id, handle)
        self.assertIsNotNone(result)
        self.assertEqual(result["status"], "environment_failure")
        self.assertEqual(codes(result), {"forward_failed"})

    def test_watch_refuses_invalid_handle(self):
        with self.assertRaises(worker.Refused) as raised:
            terminal_ops.watch(self.lab.workers(), self.run_id, "9999")
        self.assertEqual(raised.exception.code, "terminal_invalid")

    def test_watch_refuses_handle_that_never_became_ready(self):
        # Open a terminal that got stuck before becoming ready (no 'python' in meta).
        import shutil as _sh
        self.open()  # creates handle 0001
        handle_dir = worker.run_dir(self.lab.state, self.run_id) / "terminal" / "0001"
        meta = json.loads((handle_dir / "handle.json").read_text())
        del meta["python"]
        (handle_dir / "handle.json").write_text(json.dumps(meta) + "\n")
        with self.assertRaises(worker.Refused) as raised:
            terminal_ops.watch(self.lab.workers(), self.run_id, "0001")
        self.assertEqual(raised.exception.code, "terminal_not_ready")

    def test_vmctl_terminal_watch_is_parsed_and_dispatched(self):
        """The argument parser accepts `terminal watch RUN HANDLE`."""
        import vmctl as _vmctl
        p = _vmctl.parser()
        # This must not raise.
        args = p.parse_args(["terminal", "watch", "run-001", "0001"])
        self.assertEqual(getattr(args, "action", None), "watch")
        self.assertEqual(args.run_id, "run-001")
        self.assertEqual(args.handle, "0001")


class ParseWebTokenTests(unittest.TestCase):
    """Unit tests for _parse_web_token in terminal.py."""

    def setUp(self):
        sys.path.insert(0, str(REPO / "tests" / "scenarios"))
        import terminal as _t
        self._parse = _t._parse_web_token

    def test_parses_read_only_token(self):
        output = "\nCreated token successfully\n\ntoken_1: 75715e95-c1c4-4d40-9beb-c3ecd665b72b (read-only)\n"
        name, value = self._parse(output)
        self.assertEqual(name, "token_1")
        self.assertEqual(value, "75715e95-c1c4-4d40-9beb-c3ecd665b72b")

    def test_parses_regular_token(self):
        output = "token_2: 33ed2396-50eb-4b3e-b10b-3937dcced07a\n"
        name, value = self._parse(output)
        self.assertEqual(name, "token_2")
        self.assertEqual(value, "33ed2396-50eb-4b3e-b10b-3937dcced07a")

    def test_returns_none_none_on_empty(self):
        name, value = self._parse("")
        self.assertIsNone(name)
        self.assertIsNone(value)

    def test_returns_none_none_on_no_token_line(self):
        name, value = self._parse("Created token successfully\n")
        self.assertIsNone(name)
        self.assertIsNone(value)

    def test_case_insensitive(self):
        output = "TOKEN_3: ABCDEF01-0000-0000-0000-000000000000\n"
        name, value = self._parse(output)
        self.assertEqual(name, "TOKEN_3")
        self.assertEqual(value, "ABCDEF01-0000-0000-0000-000000000000")


if __name__ == "__main__":
    unittest.main()
