"""The guest terminal host without zellij: the generated session, input,
captures, the version gate and closing, against a substituted zellij and clock
(pty_records_input_resize_and_cells, pty_quit_drains_output_and_reaps_child)."""
from pathlib import Path
import json
import os
import shlex
import subprocess
import sys
import tempfile
import time
import unittest
import unittest.mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import procedures
import terminal


def done(argv, out="", code=0, err=""):
    return subprocess.CompletedProcess(argv, code, out, err)


class FakeZellij:
    """Answers the zellij commands a Terminal runs, from attributes the test sets."""

    def __init__(self, path):
        self.path = Path(path)
        self.version = "zellij 0.45.1"
        self.screens = ["SIZE 80x24"]
        self.panes = None
        self.calls = []
        self.on_dump = None  # what the program writes while zellij is asked for its screen

    def pane(self, **over):
        return {"id": 0, "is_plugin": False, "terminal_command": str(self.path / "run.sh"),
                "pane_content_columns": 80, "pane_content_rows": 24, "exited": False, "exit_status": None, **over}

    def __call__(self, argv, env=None, capture_output=True, text=True, timeout=None):
        self.calls.append(argv)
        args = argv[argv.index("zellij") + 1:]
        if args == ["--version"]:
            return done(argv, self.version + "\n")
        if "list-panes" in args:
            panes = self.panes if self.panes is not None else [{"id": 0, "is_plugin": True}, self.pane()]
            return done(argv, json.dumps(panes))
        if "dump-screen" in args:
            if self.on_dump:
                self.on_dump()
            shown = self.screens.pop(0) if len(self.screens) > 1 else self.screens[0]
            return done(argv, shown + "\n")
        return done(argv)


class Clock:
    def __init__(self):
        self.now = 0.0
        self.step = 0.0  # time each reading of the clock takes

    def __call__(self):
        self.now += self.step
        return self.now

    def sleep(self, seconds):
        self.now += seconds


class TerminalTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.root = Path(tmp.name)
        self.zellij = FakeZellij(self.root / "t")
        self.clock = Clock()
        self.alive = []

    def term(self, argv=("busybee", "monitor"), cols=80, rows=24, **kw):
        t = terminal.Terminal(self.root / "t", "bzt-0001-abcdef", list(argv), cols, rows, "/work",
                              {"PATH": "/bin", "HOME": "/home/x"}, self.root / "rt", run=self.zellij,
                              clock=self.clock, sleep=self.clock.sleep, procs=lambda path: list(self.alive), **kw)
        return t

    def started(self):
        t = self.term()
        t._write()
        t.pane = {"id": 0}
        return t

    def test_the_pane_runs_argv_under_script_with_every_word_kept(self):
        argv = ["sh", "-c", "echo 'a b' \"$HOME\"; exit 3"]
        t = self.term(argv)
        t._write()
        run = (t.path / "run.sh").read_text().splitlines()
        self.assertEqual(run[1], "cd /work || exit 127")
        self.assertEqual(run[2], terminal.AWAIT_SIZE)
        words = shlex.split(run[3])
        self.assertEqual(words[0], "exec")
        script = words[words.index("script"):]
        # Flushed on every write, the child's exit status, advanced timing, separate logs.
        for flag in ("-q", "-f", "-e"):
            self.assertIn(flag, script)
        self.assertEqual(script[script.index("-m") + 1], "advanced")
        for flag, name in (("-O", "output"), ("-I", "input"), ("-T", "timing")):
            self.assertEqual(script[script.index(flag) + 1], str(t.rec / name))
        self.assertEqual(shlex.split(script[script.index("-c") + 1]), ["exec", *argv])
        self.assertIn("TERM=xterm-256color", words)
        layout = (t.path / "layout.kdl").read_text()
        self.assertIn(f'command="{t.path / "run.sh"}"', layout)
        self.assertIn("borderless=true", layout)
        config = (t.path / "config.kdl").read_text()
        for line in ("pane_frames false", "session_serialization false", "show_startup_tips false"):
            self.assertIn(line, config)

    def test_the_pane_waits_for_its_size_before_recording(self):
        # zellij can start the pane before sizing its PTY; started then, script
        # records -1x-1 and the program may draw before any size is known.
        import fcntl, pty, struct, subprocess, termios
        primary, secondary = pty.openpty()
        self.addCleanup(os.close, primary)
        size = lambda rows, cols: fcntl.ioctl(secondary, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        size(0, 0)
        proc = subprocess.Popen(["sh", "-c", terminal.AWAIT_SIZE + "\nstty size"], stdin=secondary,
                                stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        time.sleep(0.5)
        self.assertIsNone(proc.poll(), "the pane went ahead without a size")
        size(30, 100)
        out, _ = proc.communicate(timeout=10)
        os.close(secondary)
        self.assertEqual(out.decode().strip(), "30 100")

    def test_a_path_the_layout_cannot_hold_is_refused(self):
        with self.assertRaises(terminal.TerminalError) as raised:
            terminal.layout('/tmp/a"b/run.sh')
        self.assertEqual(raised.exception.code, "path_invalid")

    def test_reusing_a_session_name_is_an_explicit_failure(self):
        command = self.root / "t" / "run.sh"
        ours = self.zellij.pane()
        self.assertEqual(terminal.pick_pane([{"id": 1, "is_plugin": True}, ours], command), ours)
        for panes in ([self.zellij.pane(terminal_command=None)],  # an old session's shell
                      [self.zellij.pane(terminal_command="/var/tmp/other/run.sh")],
                      [ours, self.zellij.pane(id=1)], []):
            with self.subTest(panes=panes):
                with self.assertRaises(terminal.TerminalError) as raised:
                    terminal.pick_pane(panes, command)
                self.assertEqual(raised.exception.code, "session_mismatch")

    def test_a_missing_or_old_zellij_is_refused(self):
        self.assertEqual(terminal.version_of("zellij 0.45.1"), "0.45.1")
        self.assertIsNone(terminal.version_of("zellij"))
        for version, code in (("zellij 0.44.2", "tool_version_too_old"), ("", "tool_missing")):
            with self.subTest(version=version):
                self.zellij.version = version
                with self.assertRaises(terminal.TerminalError) as raised:
                    self.term().preflight()
                self.assertEqual(raised.exception.code, code)

    def test_sizes_and_socket_paths_are_bounded(self):
        with self.assertRaises(terminal.TerminalError) as raised:
            self.term(cols=5)
        self.assertEqual(raised.exception.code, "size_invalid")
        t = terminal.Terminal(self.root / "t", "bzt-0001-abcdef", ["true"], 80, 24, "/", {}, self.root / ("x" * 90))
        with self.assertRaises(terminal.TerminalError) as raised:
            t._write()
        self.assertEqual(raised.exception.code, "socket_path_too_long")

    def test_no_zellij_action_runs_before_the_pane_command_starts(self):
        t = self.started()

        class Client:
            returncode = None

            def poll(self):
                return None
        t.client = Client()
        self.assertFalse(t._recording_started())
        self.assertEqual(self.zellij.calls, [])  # nothing has connected to the server yet
        (t.rec / "timing").write_text("H 0.000000 TERM xterm-256color\n")
        self.assertTrue(t._recording_started())
        Client.returncode = 1
        Client.poll = lambda self: 1
        with self.assertRaises(terminal.TerminalError) as raised:
            t._recording_started()
        self.assertEqual(raised.exception.code, "zellij_failed")

    def test_input_reaches_the_pane_as_bytes_text_and_keys(self):
        t = self.started()
        t.send_bytes(b"\x1b[A")
        t.send_text("hi")
        t.send_keys("Ctrl c")
        actions = [c[c.index("action") + 1:] for c in self.zellij.calls if "action" in c]
        self.assertEqual(actions, [["write", "-p", "terminal_0", "27", "91", "65"],
                                   ["write-chars", "-p", "terminal_0", "hi"],
                                   ["send-keys", "-p", "terminal_0", "Ctrl c"]])
        self.assertTrue(all(c[c.index("zellij") + 1:c.index("zellij") + 3] == ["--session", "bzt-0001-abcdef"]
                            for c in self.zellij.calls))
        self.zellij.calls.clear()
        t.send_bytes(bytes(range(256)))
        self.assertEqual(self.zellij.calls[0][-256:], [str(b) for b in range(256)])
        self.assertEqual(terminal.key_bytes("1b 5b41"), b"\x1b[A")
        with self.assertRaises(terminal.TerminalError):
            terminal.key_bytes("zz")

    def test_a_capture_waits_for_output_to_pause_and_pins_the_screen_to_it(self):
        t = self.started()
        output = t.rec / "output"
        output.write_bytes(b"")
        # The program writes on the first three polls, then pauses.
        writes = [b"SIZE 80x24", b"\nKEYS 71", b"\nBYE"]
        self.zellij.on_dump = lambda: output.write_bytes(output.read_bytes() + writes.pop(0)) if writes else None
        self.zellij.screens = ["SIZE 80x24\nKEYS 71\nBYE"]
        found = t.capture(self.clock() + 10, label="after-q")
        self.assertTrue(found["settled"])
        self.assertEqual(found["output_bytes"], len(b"SIZE 80x24\nKEYS 71\nBYE"))
        self.assertEqual((found["cols"], found["rows"], found["label"]), (80, 24, "after-q"))
        self.assertEqual(json.loads((t.path / "captures" / "0001.json").read_text())["screen"], found["screen"])
        self.assertEqual(t.capture(self.clock() + 10)["capture"], "0002")

    def test_a_program_that_redraws_on_a_timer_is_captured_between_redraws(self):
        # As the monitor does: a redraw every 250ms, never quiet for long.
        t = self.started()
        output = t.rec / "output"
        output.write_bytes(b"")
        clock, drawn = self.clock, []

        def tick():
            clock.now += 0.01
            frame = int(clock.now / 0.25)
            if frame not in drawn:
                drawn.append(frame)
                output.write_bytes(output.read_bytes() + b"frame %d;" % frame)
            return clock.now
        t.clock = tick
        self.zellij.screens = ["any"]
        found = t.capture(t.clock() + 5)
        self.assertTrue(found["settled"])

    def test_a_program_that_never_pauses_is_captured_unsettled(self):
        t = self.started()
        output = t.rec / "output"
        output.write_bytes(b"")
        self.zellij.on_dump = lambda: output.write_bytes(output.read_bytes() + b"tick")
        self.clock.step = 0.06  # a redraw lands within every quiet window
        found = t.capture(self.clock() + 1)
        self.assertFalse(found["settled"])
        self.assertLessEqual(self.clock(), 1.5)

    def test_wait_text_is_bounded(self):
        t = self.started()
        self.assertTrue(t.wait_text("SIZE", self.clock() + 1))
        self.assertFalse(t.wait_text("never", self.clock() + 1))

    def test_closing_reaps_everything_the_terminal_started(self):
        t = self.started()
        self.zellij.panes = [self.zellij.pane(exited=True, exit_status=130)]
        closed = t.close(seconds=2)
        self.assertEqual(closed, {"forced": [], "remaining": []})
        state = json.loads((t.path / "state.json").read_text())
        self.assertEqual(state["status"], "closed")
        self.assertEqual(state["app"], {"exited": True, "exit_status": 130})
        self.assertIn(["zellij", "kill-session", "bzt-0001-abcdef"], self.zellij.calls)

    def test_a_survivor_is_signalled_and_reported(self):
        t = self.started()
        self.alive = [4242]
        killed = []
        with unittest.mock.patch.object(terminal.os, "kill", lambda pid, sig: killed.append((pid, sig.name))):
            closed = t.close(seconds=2)
        self.assertEqual(killed, [(4242, "SIGTERM"), (4242, "SIGKILL")])
        self.assertEqual(closed["remaining"], [4242])
        self.assertLessEqual(self.clock(), 2.1)

    def test_operations_on_a_missing_or_closed_terminal_say_so(self):
        out = self._main(["send", "--dir", str(self.root / "none"), "--text", "q"])
        self.assertEqual(out["error"], "terminal_not_ready")
        t = self.started()
        t.state(schema=terminal.SCHEMA, status="closed", session=t.session, argv=t.argv, cols=80, rows=24,
                cwd="/work", env={}, runtime=str(t.runtime), pane=0, pts="/dev/pts/9", zellij="0.45.1",
                client_pid=1)
        self.assertEqual(self._main(["capture", "--dir", str(t.path)])["error"], "terminal_closed")

    def _main(self, argv):
        with unittest.mock.patch("builtins.print") as printed:
            code = terminal.main(argv)
        self.assertEqual(code, 2)
        return json.loads(printed.call_args[0][0])


class MonitorProcedureTests(unittest.TestCase):
    def test_lease_rows_are_found_by_id_and_state(self):
        screen = "│#1    running 0m30s   sleep  exclusive   r│\n│#12   queued  0m30s   sleep  1 ahead     q│"
        leases = [{"id": 1, "state": "running"}, {"id": 12, "state": "queued"}, {"id": 2, "state": "queued"}]
        rows = procedures._lease_rows(screen, leases)
        self.assertIn("running", rows[1])
        self.assertIn("#12", rows[12])
        self.assertIsNone(rows[2])

    def test_accounting_needs_every_token_owned(self):
        status = {"pool_size": 2, "held": 2, "free": 0, "approx_in_use": 0,
                  "leases": [{"id": 1, "state": "running", "cores": 2}, {"id": 2, "state": "queued", "cores": 0}]}
        self.assertTrue(procedures.accounted(status))
        for change in ({"held": 1}, {"free": 1}, {"leases": status["leases"][:1]}):
            with self.subTest(change=change):
                self.assertFalse(procedures.accounted({**status, **change}))


if __name__ == "__main__":
    unittest.main()
