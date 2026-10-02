"""Supervision acceptance against the installed Parallels: opt in with BUSYBEE_VM_LAB=1.

Uses the developer's build/vm/local.toml and its promoted Linux baseline. Each
test kills real controller processes (in their own process groups, never the
supervisor's) and checks what the detached supervisor did. Every worker a test
creates is destroyed through the controller. CI has no Parallels and skips.
"""
from pathlib import Path
import base64
import getpass
import hashlib
import json
import os
import signal
import socket
import subprocess
import sys
import time
import unittest

REPO = Path(__file__).resolve().parents[3]
VMCTL = REPO / "scripts" / "vm" / "vmctl.py"
STATE = REPO / "build" / "vm"
REAL = os.environ.get("BUSYBEE_VM_LAB") == "1"
sys.path.insert(0, str(REPO / "scripts" / "vm"))
import worker  # noqa: E402


def vmctl(*args):
    out = subprocess.run([sys.executable, str(VMCTL), "--json", *args], capture_output=True, text=True)
    return json.loads(out.stdout)


def spawn(*args):
    """A controller process in its own process group, so the test can kill it and everything it started."""
    return subprocess.Popen([sys.executable, str(VMCTL), "--json", *args], stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL, start_new_session=True)


def kill(proc):
    os.killpg(proc.pid, signal.SIGKILL)
    proc.wait()


def until(check, seconds, what):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        found = check()
        if found:
            return found
        time.sleep(0.5)
    raise AssertionError(f"{what} within {seconds}s")


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


def record(run_id):
    return json.loads((STATE / "runs" / run_id / "worker.json").read_text())


def supervisor_pid(run_id):
    events = (STATE / "runs" / run_id / "events.jsonl").read_text().splitlines()
    return [json.loads(e)["pid"] for e in events if json.loads(e)["event"] == "supervisor_started"][-1]


@unittest.skipUnless(REAL, "needs Parallels and a local config; set BUSYBEE_VM_LAB=1")
class RealSupervisionTests(unittest.TestCase):
    def setUp(self):
        # Every VM the suite did not create stays, and the registered baseline,
        # the sentinel it owns but must not touch, keeps its state.
        self.assertEqual(workers(), set(), "a worker is already registered; destroy it first")
        before = host_vms()
        baselines = {e["vm_id"] for e in registry().values() if e["role"] != "worker"}

        def untouched():
            after = host_vms()
            self.assertLessEqual(set(before), set(after))
            self.assertEqual({k: after[k] for k in baselines}, {k: before[k] for k in baselines})
        self.addCleanup(untouched)

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

    def exec(self, run_id, *argv, cwd="/root/busybee", env=(), timeout=60, detach=False):
        args = ["exec", run_id, "--cwd", cwd, "--timeout", str(timeout), *(f"--env={e}" for e in env)]
        return vmctl(*args, *(["--detach"] if detach else []), "--", *argv)

    def output(self, result, stream="stdout"):
        return (STATE / result["data"][stream]).read_bytes()

    def running(self, run_id, name):
        return vmctl("status", run_id, name)["data"]["state"] == "running"

    def test_watchdog_survives_agent_and_cli_exit(self):
        run_id = self.create()
        # The command stops its own timeout(1), so the guest's bound cannot end it.
        cli = spawn("exec", run_id, "--cwd", "/tmp", "--timeout", "5", "--", "sh", "-c", "kill -STOP $PPID; sleep 600")
        until(lambda: (STATE / "runs" / run_id / "exec" / "0001" / "state.json").exists() and
              self.running(run_id, "0001"), 60, "the exec did not start")
        kill(cli)
        started = time.monotonic()
        self.assertTrue(vmctl("status", run_id)["data"]["supervised"])

        result = vmctl("wait", run_id, "0001")
        waited = time.monotonic() - started
        self.assertEqual(result["status"], "timeout", result["findings"])
        self.assertIn("watchdog_deadline", codes(result))
        self.assertEqual(result["data"]["enforced_by"], "supervisor")
        self.assertLess(waited, 5 + worker.HOST_MARGIN_S + 30)
        self.assertGreaterEqual(result["data"]["elapsed_s"], 5 + worker.HOST_MARGIN_S)
        # The killed command and its stopped timeout(1) are gone; the worker works.
        # (This probe runs under a timeout(1) of its own, so stopped processes are what to look for.)
        left = self.exec(run_id, "sh", "-c", "pgrep -x sleep; ps -eo stat=,args= | awk '$1 ~ /^T/'; true")
        self.assertEqual(left["status"], "success", left["findings"])
        self.assertEqual(self.output(left), b"")
        self.assertEqual(record(run_id)["status"], "ready")

    def test_watchdog_handles_unreachable_guest(self):
        run_id = self.create()
        down = "sleep 2; for i in $(ls /sys/class/net); do [ $i = lo ] || ip link set $i down; done; sleep 600"
        handle = self.exec(run_id, "sh", "-c", down, timeout=300, detach=True)
        self.assertEqual(handle["status"], "success", handle["findings"])
        started = time.monotonic()
        result = vmctl("wait", run_id, handle["data"]["exec"])
        self.assertEqual(result["status"], "timeout", result["findings"])
        self.assertIn("guest_unresponsive", codes(result))
        # Bounded: ssh's keepalive notices within a minute, and the cleanup window bounds the rest.
        self.assertLess(time.monotonic() - started, 120)
        rec = record(run_id)
        self.assertEqual(rec["status"], "stopped")
        self.assertEqual(host_vms()[rec["vm_id"]], "stopped")
        self.assertIn(f"busybee-lab-{run_id}", registry())
        rdir = STATE / "runs" / run_id
        self.assertTrue(any(p.read_bytes().startswith(b"\x89PNG") for p in rdir.glob("console/*.png")))
        manifest = json.loads(sorted(rdir.glob("collect/*/collected.json"))[-1].read_text())
        self.assertEqual(manifest["missing"], ["source: the guest is not answering"])
        self.assertIn(f"runs/{run_id}/exec/0001/stdout", manifest["artifacts"])
        # Destroy boots it again (the link comes back up), finishes the collection and removes it.
        destroyed = vmctl("worker", "destroy", run_id)
        self.assertEqual(destroyed["status"], "success", destroyed["findings"])
        self.assertNotIn(rec["vm_id"], host_vms())

    def test_restart_reconciles_owned_workers(self):
        # 1. The controller dies while creating a worker.
        cli = spawn("worker", "create", "linux", "--revision", head())
        run_id = until(lambda: next(iter(workers()), None), 120, "the worker was never claimed")
        self.addCleanup(self.destroy_if_owned, run_id)
        time.sleep(5)  # into the clone
        kill(cli)
        status = until(lambda: (lambda s: s if s["data"]["workers"][0]["status"] in ("claimed", "failed") else None)(
            vmctl("status")), 120, "the interrupted creation was not reconciled")
        self.assertEqual([w["run_id"] for w in status["data"]["workers"]], [run_id])
        blocked = vmctl("worker", "create", "linux", "--revision", head())
        self.assertIn("worker_limit", codes(blocked))
        # Parallels may still be finishing the interrupted clone; a destroy retried until then removes it.
        until(lambda: vmctl("worker", "destroy", run_id)["status"] == "success", 120, "destroy never succeeded")
        self.assertEqual(workers(), set())

        # 2. The supervisor dies while a command runs; the next controller call restarts it.
        run_id = self.create()
        handle = self.exec(run_id, "sleep", "600", timeout=20, detach=True)["data"]
        first = supervisor_pid(run_id)
        os.kill(first, signal.SIGKILL)
        self.assertTrue(vmctl("status", run_id)["data"]["supervised"])
        self.assertNotEqual(supervisor_pid(run_id), first)
        result = vmctl("wait", run_id, handle["exec"])
        self.assertEqual(result["status"], "timeout", result["findings"])
        self.assertIn("command_timeout", codes(result))

        # 3. The controller dies while destroy collects.
        before = len(list((STATE / "runs" / run_id / "collect").glob("*")))
        cli = spawn("worker", "destroy", run_id)
        until(lambda: len(list((STATE / "runs" / run_id / "collect").glob("*"))) > before, 60, "no collection")
        kill(cli)
        status = vmctl("status")
        self.assertEqual([(w["run_id"], w["status"]) for w in status["data"]["workers"]], [(run_id, "ready")])
        self.assertEqual(vmctl("worker", "destroy", run_id)["status"], "success")
        self.assertEqual(vmctl("worker", "destroy", run_id)["status"], "success")  # repeated: still fine
        self.assertEqual(workers(), set())
        self.assertEqual(vmctl("status")["data"]["workers"], [])

    def test_collect_preserves_work_on_failure(self):
        run_id = self.create()
        rdir = STATE / "runs" / run_id
        changed = self.exec(run_id, "sh", "-c", "echo change >> README.md && echo built")
        self.assertEqual(changed["status"], "success", changed["findings"])
        self.assertIn(b"change", (rdir / "checkpoint" / "worktree.diff").read_bytes())
        # Until the next boot, nothing can write the guest's /tmp, which collection needs for the diff.
        broken = self.exec(run_id, "mount", "-t", "tmpfs", "-o", "ro", "tmpfs", "/tmp")
        self.assertEqual(broken["status"], "success", self.output(broken, "stderr"))

        failed = vmctl("worker", "destroy", run_id)
        self.assertEqual(failed["status"], "incomplete_collection", failed["findings"])
        self.assertTrue(any("worktree.diff" in m for m in failed["data"]["missing"]))
        rec = record(run_id)
        self.assertEqual(rec["status"], "retained")
        self.assertEqual(host_vms()[rec["vm_id"]], "stopped")
        first = json.loads(sorted(rdir.glob("collect/*/collected.json"))[-1].read_text())
        saved = set(first["artifacts"])
        self.assertIn(f"runs/{run_id}/exec/0001/stdout", saved)
        self.assertTrue(any(p.endswith("/source.json") and "/collect/" in p for p in saved), saved)
        # The checkpoint taken before the failure still holds the change.
        self.assertIn(b"change", (rdir / "checkpoint" / "worktree.diff").read_bytes())

        retried = vmctl("collect", run_id)
        self.assertEqual(retried["status"], "success", retried["findings"])
        self.assertLessEqual(saved, set(retried["data"]["acknowledged"]))
        self.assertEqual(len(list(rdir.glob("collect/*"))), 1)  # completed in place, not started over
        diff = next(p for p in sorted(rdir.glob("collect/*/worktree.diff")))
        self.assertIn(b"change", diff.read_bytes())
        self.assertEqual(host_vms()[rec["vm_id"]], "stopped")
        destroyed = vmctl("worker", "destroy", run_id)
        self.assertEqual(destroyed["status"], "success", destroyed["findings"])

    def test_incremental_logs_are_lossless(self):
        run_id = self.create()
        script = ('for i in $(seq 1 40); do printf "line %s\\r\\n" $i; printf "\\377\\000"; sleep 0.25; done; '
                  'echo done >&2; exit 3')
        handle = self.exec(run_id, "sh", "-c", script, cwd="/tmp", detach=True)["data"]
        name = handle["exec"]
        expected = b"".join(f"line {i}\r\n".encode() + b"\xff\x00" for i in range(1, 41))
        seen, reads, restarted = b"", 0, False
        while True:
            # Every read is a new client process: a reconnect each time.
            got = vmctl("read", run_id, name, "stdout", "--offset", str(len(seen)), "--limit", "7")
            self.assertEqual(got["status"], "success", got["findings"])
            seen += base64.b64decode(got["data"]["content_b64"])
            reads += 1
            if got["data"]["eof"]:
                break
            if not restarted and len(seen) > 100:
                os.kill(supervisor_pid(run_id), signal.SIGKILL)
                self.assertTrue(vmctl("status", run_id)["data"]["supervised"])
                restarted = True
            time.sleep(0.1)
        self.assertTrue(restarted)
        self.assertGreater(reads, 10)
        self.assertEqual(seen, expected)
        result = vmctl("wait", run_id, name)
        self.assertEqual((result["status"], result["data"]["exit_code"]), ("product_failure", 3))
        stderr = vmctl("read", run_id, name, "stderr")["data"]
        self.assertEqual((base64.b64decode(stderr["content_b64"]), stderr["eof"]), (b"done\n", True))
        collected = vmctl("collect", run_id)
        self.assertEqual(collected["status"], "success", collected["findings"])
        self.assertEqual(collected["data"]["artifacts"][f"runs/{run_id}/exec/{name}/stdout"],
                         hashlib.sha256(seen).hexdigest())

    def test_public_export_removes_private_values(self):
        run_id = self.create()
        rec = record(run_id)
        secret = "s3cr3t-" + os.urandom(8).hex()
        probe = ('echo "token=$API_TOKEN"; echo "repo=$1"; ip -4 -o addr show scope global; '
                 'cat /sys/class/net/*/address; echo "vm=$2"; echo "test result: FAILED. 3 passed; 1 failed"; exit 101')
        result = self.exec(run_id, "sh", "-c", probe, "sh", str(REPO), rec["worker"],
                           env=[f"API_TOKEN={secret}", "RUST_LOG=debug"])
        self.assertEqual(result["status"], "product_failure", result["findings"])
        raw = self.output(result)
        guest_ip = raw.split(b"inet ")[1].split(b"/")[0].decode()
        macs = [line.decode() for line in raw.splitlines() if line.count(b":") == 5]
        self.assertTrue(vmctl("console", "capture", run_id)["status"] == "success")
        self.assertEqual(vmctl("collect", run_id)["status"], "success")
        exported = vmctl("export", run_id)
        self.assertEqual(exported["status"], "success", exported["findings"])
        out = STATE / exported["data"]["path"]
        blob = b"".join(p.read_bytes() for p in sorted(out.rglob("*")) if p.is_file())
        private = [secret, str(REPO), str(Path.home()), getpass.getuser(), socket.gethostname().split(".")[0],
                   guest_ip, *macs, *(m.upper() for m in macs), rec["worker"], run_id, rec["vm_id"],
                   rec["vm_id"].strip("{}"), rec["reset_snapshot_id"], rec["baseline_vm_id"]]
        for value in private:
            with self.subTest(value=value):
                self.assertNotIn(value.encode(), blob)
        stdout = (out / "exec" / "0001" / "stdout").read_bytes()
        self.assertIn(b"test result: FAILED. 3 passed; 1 failed", stdout)
        self.assertIn(b"token=<redacted:API_TOKEN>", stdout)
        command = json.loads((out / "exec" / "0001" / "command.json").read_text())
        self.assertEqual(command["env"], {"API_TOKEN": "<redacted:API_TOKEN>", "RUST_LOG": "debug"})
        self.assertEqual(json.loads((out / "exec" / "0001" / "result.json").read_text())["data"]["exit_code"], 101)
        self.assertTrue(json.loads((out / "manifest.json").read_text())["withheld"])


if __name__ == "__main__":
    unittest.main()
