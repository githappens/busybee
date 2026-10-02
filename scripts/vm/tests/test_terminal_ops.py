"""`vmctl terminal` against a simulated worker: the holder as a detached exec,
input and resize as single guest commands, captures fetched and rendered on
the host, and terminal evidence through collect and export. The guest's files
are the real recording in fixtures/terminal."""
from pathlib import Path
import io
import json
import shlex
import shutil
import sys
import tarfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import contracts
import evidence
import scenario
import terminal_ops
import worker
from test_screen import FIXTURE
from test_worker import REPO, FakeGuest, Lab, codes

PYTHON = "/nix/store/0000-python3/bin/python3"


def tar_of(root, names=("state.json", "recording", "captures")):
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tar:
        for name in names:
            tar.add(Path(root) / name, arcname=name)
    return buf.getvalue()


class TerminalGuest(FakeGuest):
    """A guest whose terminal host answers from `state`, `replies` and `files`."""

    def __init__(self):
        super().__init__()
        self.hang = True  # the holder runs until something ends it
        self.state = {"schema": "busybee.terminal/v1", "status": "ready", "python": PYTHON, "pane": 0,
                      "pts": "/dev/pts/1", "client_pid": 101, "holder_pid": 100, "zellij": "0.45.1"}
        self.replies = {}
        self.files = FIXTURE

    def run(self, command, timeout, stdin=None, tty=False, check=True, raw=False):
        if command.startswith("cat ") and command.endswith("/state.json"):
            self.commands.append((command, stdin))
            return (0, json.dumps(self.state), "") if self.state else (1, "", "No such file")
        if "/terminal.py " in command and command.startswith(PYTHON):
            self.commands.append((command, stdin))
            op = shlex.split(command)[2]
            return 0, json.dumps(self.replies.get(op, {"sent": True})) + "\n", ""
        if "tar -cf - state.json recording captures" in command:
            self.commands.append((command, stdin))
            return 0, tar_of(self.files), ""
        return super().run(command, timeout, stdin, tty, check, raw)


class TerminalOpsTests(unittest.TestCase):
    def setUp(self):
        self.lab = Lab(self)
        shutil.copytree(REPO / "tests" / "scenarios", self.lab.repo / "tests" / "scenarios",
                        ignore=shutil.ignore_patterns("tests", "__pycache__"))
        self.lab.guest = TerminalGuest()
        self.run_id = self.lab.create()

    def open(self, argv=("busybee", "monitor"), cols=120, rows=40, env=None):
        return terminal_ops.open_terminal(self.lab.workers(), self.run_id, list(argv), cols, rows, worker.CHECKOUT,
                                          env or {}, 100)

    def guest_ops(self):
        return [shlex.split(c) for c, _ in self.lab.guest.commands if c.startswith(PYTHON)]

    def test_open_stages_the_host_and_holds_the_terminal_as_a_detached_exec(self):
        result = self.open(env={"TOKEN": "s3cr3t-value"})
        self.assertEqual(result["status"], "success", result["findings"])
        self.assertEqual((result["data"]["handle"], result["data"]["cols"], result["data"]["rows"]), ("0001", 120, 40))
        staged = [s for c, s in self.lab.guest.commands if s and "tar -xf -" in c]
        self.assertIn("terminal.py", tarfile.open(fileobj=io.BytesIO(staged[0])).getnames())
        command = json.loads((worker.run_dir(self.lab.state, self.run_id) / "exec" / "0001" / "command.json")
                             .read_text())
        stage = f"{scenario.STAGE}/{scenario.archive(self.lab.repo)[1][:16]}"
        self.assertEqual(command["argv"][:6], ["nix", "develop", "-c", "python3", f"{stage}/terminal.py", "hold"])
        argv = command["argv"]
        self.assertEqual(argv[argv.index("--") + 1:], ["busybee", "monitor"])
        self.assertEqual((argv[argv.index("--cols") + 1], argv[argv.index("--rows") + 1]), ("120", "40"))
        self.assertIn("--env=TOKEN=s3cr3t-value", argv)
        self.assertEqual(command["timeout_s"], 100)
        # Still running: the terminal lives as long as its exec.
        self.assertEqual(self.lab.workers().status(self.run_id, "0001")["data"]["state"], "running")
        handle = json.loads((worker.run_dir(self.lab.state, self.run_id) / "terminal" / "0001" / "handle.json")
                            .read_text())
        self.assertEqual((handle["exec"], handle["python"], handle["pane"]), ("0001", PYTHON, 0))
        self.assertRegex(handle["session"], r"^bzt-0001-[0-9a-f]{6}$")
        self.assertEqual(self.open()["data"]["handle"], "0002")

    def test_a_terminal_that_cannot_start_says_why(self):
        self.lab.guest.state = {"schema": "busybee.terminal/v1", "status": "failed", "code": "tool_version_too_old",
                                "message": "zellij is 0.40.0; terminals need >=0.45"}
        result = self.open()
        self.assertEqual(result["status"], "environment_failure")
        self.assertEqual(codes(result), {"tool_version_too_old"})

    def test_a_holder_that_exits_before_it_is_ready_is_reported(self):
        self.lab.guest.state, self.lab.guest.hang, self.lab.guest.exit_code = None, False, 2
        self.lab.guest.stderr = b"Traceback: zellij exited 1\n"
        result = self.open()
        self.assertEqual(result["status"], "environment_failure")
        self.assertEqual(codes(result), {"terminal_unavailable"})
        self.assertIn("zellij exited 1", result["findings"][0]["message"])

    def test_a_terminal_that_never_becomes_ready_times_out(self):
        self.lab.guest.state = {"schema": "busybee.terminal/v1", "status": "starting"}
        result = self.open()
        self.assertEqual(result["status"], "timeout")
        self.assertEqual(codes(result), {"terminal_not_ready"})

    def test_input_and_resize_are_one_guest_command_each(self):
        self.open()
        w = self.lab.workers()
        for kwargs, args in (({"text": "q"}, ["--text", "q"]), ({"keys": ["Ctrl c", "Up"]}, ["--key", "Ctrl c", "--key", "Up"]),
                             ({"data": "1b5b41"}, ["--bytes", "1b5b41"])):
            with self.subTest(kwargs=kwargs):
                result = terminal_ops.send(w, self.run_id, "0001", **kwargs)
                self.assertEqual(result["status"], "success", result["findings"])
                op = self.guest_ops()[-1]
                self.assertEqual(op[2:5], ["send", "--dir", f"{terminal_ops.GUEST_ROOT}/{self.run_id}/0001"])
                self.assertEqual(op[5:], args)
        result = terminal_ops.resize(w, self.run_id, "0001", 60, 20)
        self.assertEqual(result["status"], "success", result["findings"])
        self.assertEqual(self.guest_ops()[-1][2:], ["resize", "--dir", f"{terminal_ops.GUEST_ROOT}/{self.run_id}/0001",
                                                    "--cols", "60", "--rows", "20"])
        self.lab.guest.replies["resize"] = {"error": "terminal_not_ready", "message": "a 60x20 pane did not appear"}
        self.assertEqual(codes(terminal_ops.resize(w, self.run_id, "0001", 60, 20)), {"terminal_not_ready"})

    def test_requests_the_controller_cannot_act_on_are_refused(self):
        self.open()
        w = self.lab.workers()
        for call, code in ((lambda: terminal_ops.send(w, self.run_id, "0009", text="q"), "terminal_invalid"),
                           (lambda: terminal_ops.capture(w, self.run_id, "../x"), "terminal_invalid"),
                           (lambda: self.open(cols=5), "size_invalid"),
                           (lambda: self.open(argv=()), "argv_empty"),
                           (lambda: self.open(env={"BAD NAME": "x"}), "env_invalid"),
                           (lambda: terminal_ops.open_terminal(w, self.run_id, ["true"], 80, 24, "/", {}, 10 ** 6),
                            "timeout_invalid")):
            with self.subTest(code=code):
                with self.assertRaises(worker.Refused) as raised:
                    call()
                self.assertEqual(raised.exception.code, code)
        # A refused terminal leaves no handle behind.
        self.assertEqual(sorted(p.name for p in (worker.run_dir(self.lab.state, self.run_id) / "terminal").iterdir()),
                         ["0001"])

    def capture(self, reply, **kw):
        self.open()
        self.lab.guest.replies["capture"] = reply
        return terminal_ops.capture(self.lab.workers(), self.run_id, "0001", **kw)

    def test_a_capture_is_fetched_replayed_and_rendered_on_the_host(self):
        found = json.loads((FIXTURE / "captures" / "0003.json").read_text())
        result = self.capture({**found, "expected": "SIZE 60x20", "expect_seen": True}, expect="SIZE 60x20")
        self.assertEqual(result["status"], "success", result["findings"])
        data = result["data"]
        self.assertEqual((data["capture"], data["cols"], data["rows"], data["agrees"]), ("0003", 60, 20, True))
        self.assertTrue(data["text"].startswith("SIZE 60x20"))
        self.assertTrue(data["image"].startswith(f"runs/{self.run_id}/terminal/0001/captures/"))
        self.assertTrue((self.lab.state / data["image"]).read_bytes().startswith(b"\x89PNG"))
        self.assertEqual(data["missing_glyphs"], ["界"])
        op = self.guest_ops()[-1]
        self.assertEqual(op[op.index("--expect") + 1], "SIZE 60x20")

    def test_an_expected_screen_that_never_comes_is_a_timeout_with_the_capture_kept(self):
        found = json.loads((FIXTURE / "captures" / "0001.json").read_text())
        result = self.capture({**found, "expected": "BYE", "expect_seen": False}, expect="BYE")
        self.assertEqual(result["status"], "timeout")
        self.assertEqual(codes(result), {"expect_not_seen"})
        self.assertTrue((self.lab.state / result["data"]["image"]).is_file())

    def test_a_closed_terminal_is_captured_from_its_recording(self):
        result = self.capture({"error": "terminal_closed", "message": "the terminal is closed"})
        self.assertEqual(result["status"], "success", result["findings"])
        data = result["data"]
        self.assertEqual(data["capture"], "0005")
        self.assertIsNone(data["agrees"])  # zellij's screen went with its session
        self.assertTrue(data["text"].endswith("KEYS 03\nINTERRUPTED"))
        self.assertEqual(data["app"], {"exited": True, "exit_status": 130})

    def test_terminal_evidence_is_collected_and_published_rendered_again(self):
        found = json.loads((FIXTURE / "captures" / "0001.json").read_text())
        # The guest's terminal showed the run's identity; the export must not.
        secret = self.run_id.encode()
        shown = b"SIZE 100x30 TERM=xterm-25"
        self.assertEqual(len(secret), len(shown))
        planted = self.lab.state.parent / "planted"
        shutil.copytree(FIXTURE, planted)
        output = planted / "recording" / "output"
        output.write_bytes(output.read_bytes().replace(shown, secret))
        for path in (planted / "captures").glob("*.json"):
            path.write_text(path.read_text().replace(shown.decode(), self.run_id))
        self.lab.guest.files = planted
        result = self.capture({**found, "screen": found["screen"].replace(shown.decode(), self.run_id),
                               "expected": None, "expect_seen": None})
        self.assertEqual(result["status"], "success", result["findings"])
        self.assertTrue(result["data"]["agrees"])
        self.assertTrue(self.lab.workers().console_capture(self.run_id)["status"] == "success")

        collected = self.lab.workers().collect(self.run_id)
        self.assertEqual(collected["status"], "success", collected["findings"])
        manifest = json.loads((self.lab.state / collected["data"]["manifest"]).read_text())
        self.assertEqual(manifest["schema"], "busybee.vm.evidence/v3")
        self.assertEqual(contracts.evidence_errors(manifest), [])
        self.assertEqual(manifest["terminals"]["0001"]["captures"][0]["agrees"], True)
        terminal_files = [a for a in manifest["artifacts"] if a.startswith(f"runs/{self.run_id}/terminal/0001/")]
        for name in ("recording/output", "recording/timing", "captures/0001.png", "captures/0001.cells.json",
                     "handle.json"):
            self.assertIn(f"runs/{self.run_id}/terminal/0001/{name}", terminal_files)

        out = self.lab.workers().export(self.run_id)
        self.assertEqual(out["status"], "success", out)
        public = self.lab.state / out["data"]["path"]
        withheld = out["data"]["withheld"]
        self.assertTrue(any(w.startswith("console/") for w in withheld))
        self.assertFalse(any(w.startswith("terminal/") for w in withheld))
        private_output = worker.run_dir(self.lab.state, self.run_id) / "terminal" / "0001" / "recording" / "output"
        public_output = public / "terminal" / "0001" / "recording" / "output"
        self.assertEqual(len(public_output.read_bytes()), len(private_output.read_bytes()))
        for path in (public / "terminal").rglob("*"):
            if path.is_file() and path.suffix != ".png":
                self.assertNotIn(secret, path.read_bytes(), path)
        cells = json.loads((public / "terminal" / "0001" / "captures" / "0001.cells.json").read_text())
        self.assertTrue(cells["agrees"])  # both views redacted alike
        self.assertIn("<run-id>", cells["text"])
        self.assertTrue((public / "terminal" / "0001" / "captures" / "0001.png").read_bytes().startswith(b"\x89PNG"))
        listed = json.loads((public / "manifest.json").read_text())["files"]
        self.assertIn("terminal/0001/captures/0001.png", listed)
        self.assertNotIn(b"s3cr3t", b"".join(p.read_bytes() for p in public.rglob("*") if p.is_file()))


    def test_collect_fetches_terminals_an_agent_never_captured(self):
        self.open()
        hdir = worker.run_dir(self.lab.state, self.run_id) / "terminal" / "0001"
        self.assertFalse((hdir / "recording").exists())
        collected = self.lab.workers().collect(self.run_id)
        self.assertEqual(collected["status"], "success", collected["findings"])
        self.assertIn(f"runs/{self.run_id}/terminal/0001/recording/output", collected["data"]["artifacts"])
        self.assertTrue((hdir / "captures" / "0003.png").is_file())
        manifest = json.loads((self.lab.state / collected["data"]["manifest"]).read_text())
        self.assertEqual(manifest["terminals"]["0001"]["exit_code"], 130)

    def test_a_terminal_collect_cannot_fetch_is_missing_and_keeps_the_worker(self):
        self.open()
        self.lab.guest.files = self.lab.state / "absent"
        collected = self.lab.workers().collect(self.run_id)
        self.assertEqual(collected["status"], "incomplete_collection")
        self.assertTrue(any(m.startswith("terminal 0001:") for m in collected["data"]["missing"]))
        self.lab.guest.files = FIXTURE
        self.assertEqual(self.lab.workers().destroy(self.run_id)["status"], "success")

    def test_export_lists_a_terminal_without_a_recording(self):
        self.open()
        self.lab.guest.files = self.lab.state / "absent"
        self.lab.workers().collect(self.run_id)
        out = self.lab.workers().export(self.run_id)
        self.assertEqual(out["status"], "success", out)
        self.assertEqual(out["data"]["missing"], ["terminal/0001: no recording was collected"])
        self.assertIn("artifact_missing", codes(out))
        public = self.lab.state / out["data"]["path"]
        self.assertTrue((public / "terminal" / "0001" / "handle.json").is_file())


class RedactorTests(unittest.TestCase):
    def test_length_is_kept_when_asked(self):
        r = evidence.Redactor({"/home/alice": "home"})
        data = b"cd /home/alice/x && ping 10.0.0.12"
        kept = r.bytes(data, keep_length=True)
        self.assertEqual(len(kept), len(data))
        self.assertNotIn(b"alice", kept)
        self.assertNotIn(b"10.0.0.12", kept)
        self.assertIn(b"<home>", kept)
        self.assertEqual(r.bytes(b"ip 1.2.3.4"), b"ip <ip>")


if __name__ == "__main__":
    unittest.main()
