"""Terminal acceptance against the installed Parallels: opt in with BUSYBEE_VM_LAB=1.

Uses the developer's build/vm/local.toml and its promoted Linux baseline, which
must carry zellij. Each named test of #77 runs here against a real worker at
this checkout's HEAD: the deterministic fixture program
(tests/scenarios/pty_fixture.py) and the workspace-built monitor in the
live-monitor scenario. Every worker a test creates is destroyed through the
controller. CI has no Parallels and skips.
"""
from pathlib import Path
import json
import os
import subprocess
import sys
import time
import unittest

REPO = Path(__file__).resolve().parents[3]
VMCTL = REPO / "scripts" / "vm" / "vmctl.py"
STATE = REPO / "build" / "vm"
REAL = os.environ.get("BUSYBEE_VM_LAB") == "1"
sys.path.insert(0, str(REPO / "scripts" / "vm"))
import scenario  # noqa: E402
import screen  # noqa: E402

BUILD_S = "1800"
# What q or Ctrl-C may take to end a program and its terminal, unread output included.
QUIT_S = 30


def vmctl(*args):
    out = subprocess.run([sys.executable, str(VMCTL), "--json", *args], capture_output=True, text=True)
    return json.loads(out.stdout)


def codes(result):
    return {f["code"] for f in result["findings"]}


def head():
    return subprocess.run(["git", "-C", str(REPO), "rev-parse", "HEAD"], capture_output=True, text=True,
                          check=True).stdout.strip()


def registry():
    return json.loads((STATE / "registry.json").read_text())["vms"]


def workers():
    return {e["run_id"] for e in registry().values() if e["role"] == "worker"}


def host_vms():
    listed = subprocess.run(["prlctl", "list", "--all", "--json"], capture_output=True, text=True, check=True)
    return {"{" + vm["uuid"].strip("{}") + "}": vm["status"] for vm in json.loads(listed.stdout)}


@unittest.skipUnless(REAL, "needs Parallels and a local config; set BUSYBEE_VM_LAB=1")
class RealTerminalTests(unittest.TestCase):
    def setUp(self):
        self.assertEqual(workers(), set(), "a worker is already registered; destroy it first")
        before = host_vms()
        baselines = {e["vm_id"] for e in registry().values() if e["role"] != "worker"}

        def untouched():
            after = host_vms()
            self.assertLessEqual(set(before), set(after))
            self.assertEqual({k: after[k] for k in baselines}, {k: before[k] for k in baselines})
        self.addCleanup(untouched)
        self.fixture = f"{scenario.STAGE}/{scenario.archive(REPO)[1][:16]}/pty_fixture.py"

    def create(self):
        result = vmctl("worker", "create", "linux", "--revision", head())
        self.assertEqual(result["status"], "success", result["findings"])
        run_id = result["data"]["run_id"]
        self.addCleanup(self.destroy_if_owned, run_id)
        return run_id

    def destroy_if_owned(self, run_id):
        if f"busybee-lab-{run_id}" in registry():
            result = vmctl("worker", "destroy", run_id)
            self.assertEqual(result["status"], "success", result["findings"])

    def exec(self, run_id, *argv, timeout="60", detach=False):
        return vmctl("exec", run_id, "--cwd", "/root/busybee", "--timeout", timeout,
                     *(["--detach"] if detach else []), "--", *argv)

    def open_fixture(self, run_id, cols=100, rows=30):
        opened = vmctl("terminal", "open", run_id, "--cols", str(cols), "--rows", str(rows), "--timeout", "600",
                       "--", "python3", self.fixture)
        self.assertEqual(opened["status"], "success", opened["findings"])
        return opened["data"]

    def ok(self, result):
        self.assertEqual(result["status"], "success", result["findings"])
        return result["data"]

    def ended(self, run_id, opened, seconds):
        """The holder's exec result, which comes once the program and its terminal are gone."""
        started = time.monotonic()
        result = vmctl("wait", run_id, opened["exec"], "--timeout", str(seconds))
        return result, time.monotonic() - started

    def test_pty_records_input_resize_and_cells(self):
        run_id = self.create()
        t = self.open_fixture(run_id)
        handle = t["handle"]
        first = self.ok(vmctl("terminal", "capture", run_id, handle, "--expect", "SIZE 100x30"))
        self.assertTrue(first["agrees"], first)
        self.assertIn("TERM=xterm-256color LANG=C.UTF-8", first["text"])
        for how in (["--text", "hi"], ["--key", "Up"], ["--bytes", "1b5b41"]):
            self.ok(vmctl("terminal", "send", run_id, handle, *how))
        typed = self.ok(vmctl("terminal", "capture", run_id, handle, "--expect", "KEYS 1b 5b 41\nKEYS 1b 5b 41"))
        self.assertIn("KEYS 68 69\nKEYS 1b 5b 41\nKEYS 1b 5b 41", typed["text"])
        self.assertTrue(typed["agrees"], typed)
        self.ok(vmctl("terminal", "resize", run_id, handle, "--cols", "60", "--rows", "20"))
        narrow = self.ok(vmctl("terminal", "capture", run_id, handle, "--expect", "SIZE 60x20"))
        self.assertEqual((narrow["cols"], narrow["rows"]), (60, 20))
        self.assertTrue(narrow["agrees"], narrow)
        self.ok(vmctl("terminal", "send", run_id, handle, "--key", "Ctrl c"))
        result, _ = self.ended(run_id, t, QUIT_S)
        self.assertEqual(result["status"], "success", result["findings"])
        closed = self.ok(vmctl("terminal", "capture", run_id, handle))
        self.assertTrue(closed["text"].endswith("KEYS 03\nINTERRUPTED"))
        self.assertEqual(closed["app"], {"exited": True, "exit_status": 130})

        hdir = STATE / "runs" / run_id / "terminal" / handle
        recording = screen.Recording.load(hdir / "recording")
        self.assertEqual(recording.exit_code(), 130)
        self.assertEqual([(s["cols"], s["rows"]) for s in recording.sizes()], [(100, 30), (60, 20)])
        raw = (hdir / "recording" / "input").read_bytes()
        self.assertIn(b"hi\x1b[A\x1b[A\x03", raw)
        for capture in (first, typed, narrow):
            image = STATE / capture["image"]
            self.assertTrue(image.read_bytes().startswith(b"\x89PNG"))
            self.assertEqual(capture["missing_glyphs"], ["界"])
        self.assertNotIn("console", Path(first["image"]).parts)

    def test_pty_quit_drains_output_and_reaps_child(self):
        run_id = self.create()
        for quit_key, code, last in ((["--text", "q"], 0, "BYE"), (["--key", "Ctrl c"], 130, "INTERRUPTED")):
            with self.subTest(code=code):
                t = self.open_fixture(run_id)
                self.ok(vmctl("terminal", "capture", run_id, t["handle"], "--expect", "SIZE"))
                # f floods the terminal; the quit key follows at once, behind output nobody has read.
                self.ok(vmctl("terminal", "send", run_id, t["handle"], "--text", "f"))
                self.ok(vmctl("terminal", "send", run_id, t["handle"], *quit_key))
                result, took = self.ended(run_id, t, QUIT_S)
                self.assertEqual(result["status"], "success", result["findings"])
                self.assertLess(took, QUIT_S)
                closed = self.ok(vmctl("terminal", "capture", run_id, t["handle"]))
                self.assertEqual(closed["app"], {"exited": True, "exit_status": code})
                self.assertTrue(closed["text"].endswith(last), closed["text"][-200:])
                hdir = STATE / "runs" / run_id / "terminal" / t["handle"]
                state = json.loads((hdir / "state.json").read_text())
                self.assertEqual(state["closed"]["remaining"], [])
                self.assertEqual(state["closed"]["forced"], [])
                recording = screen.Recording.load(hdir / "recording")
                self.assertEqual(recording.exit_code(), code)
                self.assertIn(b"flood 19999", recording.body)
        left = self.exec(run_id, "sh", "-c", "pgrep -ax zellij; pgrep -af '[p]ty_fixture'; true")
        self.assertEqual((STATE / left["data"]["stdout"]).read_text().strip(), "")

    def test_live_monitor_is_observable_at_two_sizes(self):
        run_id = self.create()
        built = self.exec(run_id, "nix", "develop", "-c", "cargo", "build", "--bins", timeout=BUILD_S)
        self.assertEqual(built["status"], "success", built["findings"])
        result = vmctl("scenario", run_id, "live-monitor", "--mode", "prepared")
        self.assertEqual(result["status"], "success", result["findings"])
        record = json.loads((STATE / result["data"]["path"]).read_text())
        found = record["result"]
        self.assertEqual(found["preflight"]["tools"]["zellij"]["requirement"], ">=0.45")
        observed = found["observations"]
        self.assertEqual([lease["state"] for lease in observed["work"]["status"]["leases"]], ["running", "queued"])
        monitor = record["terminals"][f"{record['exec']}-monitor"]
        labels = {c["label"]: c for c in monitor["captures"]}
        for label in ("idle", "wide", "narrow", "stale"):
            self.assertTrue(labels[label]["agrees"], label)
        self.assertEqual([(s["cols"], s["rows"]) for s in monitor["sizes"]], [(120, 40), (60, 20)])
        hdir = STATE / "runs" / run_id / "terminal" / f"{record['exec']}-monitor" / "captures"
        wide = json.loads((hdir / f"{labels['wide']['capture']}.cells.json").read_text())
        narrow = json.loads((hdir / f"{labels['narrow']['capture']}.cells.json").read_text())
        self.assertEqual((wide["size"], narrow["size"]), ({"cols": 120, "rows": 40}, {"cols": 60, "rows": 20}))
        self.assertGreater(wide["render"]["image"]["width"], narrow["render"]["image"]["width"])
        for lease in observed["work"]["status"]["leases"]:
            self.assertIn(f"#{lease['id']}", wide["text"])
            self.assertIn(lease["label"][:40], wide["text"])  # a long label is readable when wide
            self.assertIn(f"#{lease['id']}", narrow["text"])

        collected = vmctl("collect", run_id)
        self.assertEqual(collected["status"], "success", collected["findings"])
        manifest = json.loads((STATE / collected["data"]["manifest"]).read_text())
        self.assertIn(f"{record['exec']}-monitor", manifest["terminals"])
        exported = vmctl("export", run_id)
        self.assertEqual(exported["status"], "success", exported["findings"])
        public = STATE / exported["data"]["path"]
        tdir = public / "terminal" / f"{record['exec']}-monitor"
        for label in ("wide", "narrow"):
            name = labels[label]["capture"]
            self.assertTrue((tdir / "captures" / f"{name}.png").read_bytes().startswith(b"\x89PNG"))
            self.assertTrue(json.loads((tdir / "captures" / f"{name}.cells.json").read_text())["agrees"])
        self.assertTrue((tdir / "recording" / "output").is_file())
        self.assertFalse(any(w.startswith("terminal/") for w in exported["data"]["withheld"]))
        public_record = json.loads((public / "scenarios" / record["exec"] / "result.json").read_text())
        self.assertIn("status", public_record["result"]["observations"]["wide"])
        self.assertNotIn(run_id, (public / "scenarios" / record["exec"] / "result.json").read_text())

    def test_slow_status_and_lost_guest_are_bounded(self):
        run_id = self.create()
        built = self.exec(run_id, "nix", "develop", "-c", "cargo", "build", "--bins", timeout=BUILD_S)
        self.assertEqual(built["status"], "success", built["findings"])
        # A status reply that never comes (bzbd stopped): captured within the
        # scenario's deadline, whatever the monitor does.
        result = vmctl("scenario", run_id, "live-monitor", "--mode", "prepared")
        record = json.loads((STATE / result["data"]["path"]).read_text())
        found = record["result"]
        self.assertLess(found["elapsed_s"], found["deadline_s"])
        stale = {a["name"]: a for a in found["assertions"]}
        for name in ("monitor_marks_unanswered_status_stale", "quit_during_slow_status_exits"):
            self.assertIn(stale[name]["status"], ("passed", "failed"), stale[name])  # evaluated, never skipped
        self.assertIn("stale", found["observations"])
        self.assertEqual(found["cleanup"]["remaining"], [])

        # Command access lost while a terminal is open: the supervisor keeps the
        # console, stops the VM and says so; the terminal reports it cannot answer.
        t = self.open_fixture(run_id)
        down = "sleep 2; for i in $(ls /sys/class/net); do [ $i = lo ] || ip link set $i down; done; sleep 600"
        self.assertEqual(self.exec(run_id, "sh", "-c", down, timeout="300", detach=True)["status"], "success")
        started = time.monotonic()
        lost, _ = self.ended(run_id, t, 300)
        self.assertEqual(lost["status"], "timeout", lost["findings"])
        self.assertIn("guest_unresponsive", codes(lost))
        self.assertLess(time.monotonic() - started, 180)
        rdir = STATE / "runs" / run_id
        shots = sorted(rdir.glob("console/*.png"))
        self.assertTrue(shots and shots[-1].read_bytes().startswith(b"\x89PNG"))
        after = vmctl("terminal", "capture", run_id, t["handle"])
        self.assertEqual(after["status"], "environment_failure")
        self.assertIn("worker_not_ready", codes(after))
        self.assertEqual(json.loads((rdir / "worker.json").read_text())["status"], "stopped")


if __name__ == "__main__":
    unittest.main()
