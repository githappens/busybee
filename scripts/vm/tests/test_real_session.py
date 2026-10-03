"""Issue sessions against the installed Parallels: opt in with BUSYBEE_VM_LAB=1.

Runs the named tests of the issue-agent integration in real Linux workers at
this checkout's HEAD, the way the lab dispatcher does: the controller runs from
a trusted snapshot of the committed revision (sortie/snapshot.sh), the agent's
turns are `session agent` processes whose command runs in the worker, and the
issue workspace is a scratch clone whose origin is a local bare repository.
Pull-request operations go to a fake `gh`; nothing reaches GitHub. Every
attempt is closed through `session end`, which destroys its worker. CI has no
Parallels and skips.
"""
from pathlib import Path
import json
import os
import shutil
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

REPO = Path(__file__).resolve().parents[3]
# The checkout holding the lab state (build/vm); another worktree of the same
# repository can run these tests against it.
ROOT = Path(os.environ.get("BUSYBEE_VM_LAB_ROOT") or REPO).resolve()
STATE = ROOT / "build" / "vm"
REAL = os.environ.get("BUSYBEE_VM_LAB") == "1"
ISSUE = 990079  # no real issue; its session records sit beside the real ones
BRANCH = f"sortie-lab/{ISSUE}"
sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_session import FAKE_GH, IDENTITY  # noqa: E402


def git(*args, cwd=None):
    return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, check=True,
                          env={**os.environ, **IDENTITY}).stdout.strip()


def registry():
    return json.loads((STATE / "registry.json").read_text())["vms"]


def workers():
    return {e["run_id"] for e in registry().values() if e["role"] == "worker"}


@unittest.skipUnless(REAL, "needs Parallels and a local config; set BUSYBEE_VM_LAB=1")
class RealSessionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.head = git("-C", str(REPO), "rev-parse", "HEAD")
        cls.scratch = Path(tempfile.mkdtemp(prefix="session-", dir=ROOT / "build"))
        done = subprocess.run(["bash", str(REPO / "sortie" / "snapshot.sh"), cls.head, str(cls.scratch / "state")],
                              cwd=REPO, capture_output=True, text=True, check=True)
        cls.trusted = Path(done.stdout.strip())
        cls.bin = cls.scratch / "bin"
        cls.bin.mkdir()
        (cls.bin / "gh").write_text(FAKE_GH)
        (cls.bin / "gh").chmod(0o755)

    @classmethod
    def tearDownClass(cls):
        shutil.rmtree(cls.scratch, ignore_errors=True)

    def setUp(self):
        self.assertEqual(workers(), set(), "a worker is already registered; destroy it first")
        # The synthetic issue's records from earlier runs; every test starts it afresh.
        shutil.rmtree(STATE / "sessions" / str(ISSUE), ignore_errors=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="t-", dir=self.scratch))
        origin = self.tmp / "origin.git"
        git("init", "-q", "--bare", str(origin))
        # main is this revision, so the guard sees only what the agent changes.
        git("-C", str(REPO), "push", "-q", str(origin), f"{self.head}:refs/heads/main", f"{self.head}:refs/heads/seed")
        # A task checkout clones the repository with its tags, which `git describe`
        # versions the build from; the scenarios' preflight checks that version.
        git("-C", str(REPO), "push", "-q", str(origin), "--tags")
        self.workspace = self.tmp / "ws"
        git("clone", "-q", "--branch", "main", str(origin), str(self.workspace))
        git("-C", str(self.workspace), "checkout", "-q", "-b", BRANCH, "origin/seed")
        with open(self.workspace / ".git" / "info" / "exclude", "a") as f:
            f.write("/.sortie/\n")
        self.env = {**os.environ, **IDENTITY, "PATH": f"{self.bin}{os.pathsep}{os.environ['PATH']}",
                    "FAKE_GH_STATE": str(self.tmp / "gh.json")}
        self.out = self.tmp / "out"
        self.out.mkdir()
        self.addCleanup(self.close)

    def vmctl(self, *args):
        done = subprocess.run([sys.executable, str(self.trusted / "scripts/vm/vmctl.py"), "--json", "--root",
                               str(ROOT), *args], capture_output=True, text=True, env=self.env, cwd=self.workspace)
        return json.loads(done.stdout)

    def close(self):
        if (STATE / "sessions" / str(ISSUE) / "session.json").is_file():
            ended = self.vmctl("session", "end", "--workspace", str(self.workspace))
            self.assertIn(ended["status"], ("success",), ended)

    def start(self, profile="product"):
        started = self.vmctl("session", "start", "--issue", str(ISSUE), "--workspace", str(self.workspace),
                             "--profile", profile)
        self.assertEqual(started["status"], "success", started)
        return started["data"]

    def agent(self, script, timeout=None, argv=None, background=False):
        body = f"set -euo pipefail\nOUT=/var/tmp/agent-out; mkdir -p $OUT\n" + textwrap.dedent(script)
        cmd = [sys.executable, str(self.trusted / "scripts/vm/vmctl.py"), "--root", str(ROOT), "session", "agent",
               *(["--timeout", str(timeout)] if timeout else []), "--", *(argv or ["bash", "-c", body])]
        log = open(self.tmp / f"agent-{time.monotonic_ns()}.log", "wb")
        self.addCleanup(log.close)
        proc = subprocess.Popen(cmd, cwd=self.workspace, env=self.env, stdin=subprocess.DEVNULL, stdout=log,
                                stderr=subprocess.STDOUT, start_new_session=True)
        if background:
            return proc
        code = proc.wait(timeout=3600)
        log.flush()
        self.last_log = Path(log.name).read_text(errors="replace")
        return code

    def session(self):
        return json.loads((STATE / "sessions" / str(ISSUE) / "session.json").read_text())

    def attempt(self, name=None):
        s = self.session()
        name = name or s["attempt"] or f"{s['attempts']:04d}"
        return json.loads((STATE / "sessions" / str(ISSUE) / "attempts" / name / "attempt.json").read_text())

    def end(self):
        ended = self.vmctl("session", "end", "--workspace", str(self.workspace))
        self.assertEqual(ended["status"], "success", ended)
        data = ended["data"]
        self.assertEqual((data["worker_status"], data["destroy"]), ("destroyed", "success"))
        self.assertTrue((STATE / data["collected"]).is_file())
        self.assertTrue((STATE / data["exported"] / "manifest.json").is_file())
        self.assertNotIn(data["run_id"], workers())
        return data

    def test_agent_controls_only_assigned_worker(self):
        started = self.start()
        other = "r-20990101T000000Z-abcdef"
        code = self.agent(f"""
            # A temporary tool, installed into the guest's own profile.
            nix profile add "$(dirname "$(dirname "$(command -v jq)")")"
            test -x /root/.nix-profile/bin/jq
            # A daemon restarted: a private pueued, stopped and started again.
            cfg=$(mktemp -d)
            printf 'shared:\\n  pueue_directory: %s\\n  runtime_directory: %s\\n  use_unix_socket: true\\n  unix_socket_path: %s\\n' \\
                "$cfg/d" "$cfg/d" "$cfg/d/pueue.sock" > "$cfg/pueue.yml"
            pueued -c "$cfg/pueue.yml" -d && sleep 2 && pueue -c "$cfg/pueue.yml" status
            pkill -f "$cfg/pueue.yml"; sleep 1
            ! pueue -c "$cfg/pueue.yml" status 2>/dev/null
            pueued -c "$cfg/pueue.yml" -d && sleep 2 && pueue -c "$cfg/pueue.yml" status > $OUT/pueue-restarted
            # A terminal, inspected through the controller.
            lab terminal open --cols 80 --rows 24 -- bash -c 'echo SESSION-TERMINAL-READY; sleep 600' > $OUT/topen.json
            handle=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["data"]["handle"])' $OUT/topen.json)
            lab terminal capture "$handle" --expect SESSION-TERMINAL-READY > $OUT/capture.json
            lab terminal send "$handle" --key 'Ctrl c' > /dev/null
            lab inspect > $OUT/inspect.json
            ! lab --run {other} inspect > $OUT/wrong.json
            ! lab --target host exec -- true > $OUT/host.json
            ! lab request '{{"op": "worker-create", "args": {{}}}}' > $OUT/create.json
            # Work that must survive the reset.
            echo kept > session-probe.txt && git add session-probe.txt
            git -c user.name=Fixture -c user.email=fixture@example.test commit -qm 'session probe'
            for f in capture wrong host create inspect; do printf '%s ' $f; cat $OUT/$f.json | python3 -c \\
                'import json,sys; r=json.load(sys.stdin); print(r["status"], [f["code"] for f in r["findings"]])'; done
            lab reset > $OUT/reset.json
        """)
        self.assertEqual(code, 0, self.last_log)
        for line, expected in (("capture success []", True), ("wrong environment_failure ['target_not_assigned']", True),
                               ("host environment_failure ['host_target_refused']", True),
                               ("create environment_failure ['operation_not_permitted']", True),
                               ("inspect success []", True)):
            self.assertIn(line, self.last_log)
        resets = self.attempt()["resets"]
        self.assertEqual([r["status"] for r in resets], ["success"], resets)
        # After the reset: the tool and the daemon are gone, the committed work is not.
        code = self.agent("""
            test ! -e /root/.nix-profile/bin/jq
            ! pgrep -f pueued
            test "$(cat session-probe.txt)" = kept
            test "$(git log -1 --format=%s)" = 'session probe'
            test "$(git branch --show-current)" = sortie-lab/990079
        """)
        self.assertEqual(code, 0, self.last_log)
        self.assertEqual(git("-C", str(self.workspace), "log", "-1", "--format=%s"), "session probe")
        self.assertEqual(self.attempt()["run_id"], started["run_id"])
        end = self.end()
        self.assertEqual(end["outcome"], "no_handoff")

    def test_continuation_preserves_branch_and_pr(self):
        first = self.start()["run_id"]
        code = self.agent(f"""
            echo one > a.txt && git add a.txt
            git -c user.name=Fixture -c user.email=fixture@example.test commit -qm 'checkpointed commit'
            echo unfinished > b.txt
            lab push > /dev/null
            printf 'Closes #990079\\n' > /var/tmp/body.md
            lab pr create --title 'lab: probe' --body-file /var/tmp/body.md
        """)
        self.assertEqual(code, 0, self.last_log)
        pr = self.session()["pr"]
        self.assertIsNotNone(pr)
        # Interrupt: the runner is killed mid-turn and never closes its attempt.
        runner = self.agent("echo more > c.txt; sleep 900", background=True)
        time.sleep(45)
        os.killpg(runner.pid, signal.SIGKILL)
        runner.wait()
        # The next attempt closes it, replaces the worker and resumes the branch.
        second = self.start()
        self.assertNotEqual(second["run_id"], first)
        closed = self.attempt("0001")["end"]
        self.assertEqual((closed["outcome"], closed["worker_status"]), ("interrupted", "destroyed"), closed)
        code = self.agent(f"""
            test "$(git branch --show-current)" = {BRANCH}
            test "$(cat a.txt)" = one && test "$(cat b.txt)" = unfinished && test "$(cat c.txt)" = more
            test "$(git log -1 --format=%s)" = 'checkpointed commit'
            test "$BUSYBEE_LAB_PR" = {pr}
            printf 'Closes #990079\\n' > /var/tmp/body.md
            lab pr create --title again --body-file /var/tmp/body.md
        """)
        self.assertEqual(code, 0, self.last_log)
        self.assertIn('"reused": true', self.last_log)
        calls = json.loads((self.tmp / "gh.json").read_text())["calls"]
        self.assertEqual(len([c for c in calls if c[:2] == ["pr", "create"]]), 1)
        self.assertEqual(self.session()["pr"], pr)
        self.assertEqual(git("ls-remote", str(self.tmp / "origin.git"), f"refs/heads/{BRANCH}").split()[0],
                         git("-C", str(self.workspace), "rev-parse", "HEAD"))
        self.end()

    def test_branch_cannot_replace_supervisor(self):
        policy = (self.trusted / "sortie" / "guard-policy.json").read_text()
        self.start("product")
        code = self.agent("""
            printf '{"schema": "busybee.lab.guard/v1", "profiles": {"product": {"forbidden_paths": []}}}\\n' \\
                > sortie/guard-policy.json
            printf '# looser dispatch\\n' >> sortie/lab.py
            git -c user.name=Fixture -c user.email=fixture@example.test commit -qam 'replace the guard'
            ! lab push > $OUT/push.json
            python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); print("push", [f["code"] for f in r["findings"]])' \\
                $OUT/push.json
        """)
        self.assertEqual(code, 0, self.last_log)
        self.assertIn("push ['checkpoint_violation']", self.last_log)
        self.assertEqual((self.trusted / "sortie" / "guard-policy.json").read_text(), policy)
        self.assertEqual(self.attempt()["violation"]["paths"], ["sortie/guard-policy.json", "sortie/lab.py"])
        self.assertEqual(git("-C", str(self.workspace), "rev-parse", "HEAD"), self.head)
        self.assertEqual(git("ls-remote", str(self.tmp / "origin.git"), f"refs/heads/{BRANCH}"), "")
        end = self.end()
        self.assertEqual(end["outcome"], "blocked")
        self.assertEqual((self.workspace / ".sortie" / "status").read_text(), "blocked\n")

    def test_every_runner_exit_accounts_for_worker(self):
        cases = [
            ("success", """
                echo fixed > fix.txt && git add fix.txt
                git -c user.name=Fixture -c user.email=fixture@example.test commit -qm fix
                lab push > /dev/null
                printf 'Closes #990079\\n' > /var/tmp/body.md
                lab pr create --title fix --body-file /var/tmp/body.md > /dev/null
                lab pr ready > /dev/null
                lab handoff
            """, {}),
            ("blocked", "printf 'a prerequisite tool is missing\\n' > .sortie/blocker.md; echo blocked > .sortie/status",
             {}),
            ("timeout", "sleep 900", {"timeout": 60}),
            ("cancelled", "sleep 900", {"cancel_after": 60}),
            ("adapter_failure", None, {"argv": ["no-such-agent-runner"]}),
        ]
        for outcome, script, how in cases:
            with self.subTest(outcome=outcome):
                # No scenario names the synthetic issue: only an infrastructure
                # session hands off a head on passing checks alone.
                run_id = self.start("infrastructure" if outcome == "success" else "product")["run_id"]
                if "cancel_after" in how:
                    runner = self.agent(script, background=True)
                    time.sleep(how["cancel_after"])
                    os.kill(runner.pid, signal.SIGTERM)
                    code = runner.wait(timeout=300)
                else:
                    code = self.agent(script or "", timeout=how.get("timeout"), argv=how.get("argv"))
                turn = self.attempt()["turns"][-1]
                self.assertTrue(turn["finished"], turn)
                end = self.end()
                self.assertEqual((end["outcome"], end["run_id"]), (outcome, run_id), end)
                if outcome == "success":
                    # Handed off only once the gate accepted the head on both platforms.
                    self.assertEqual(self.attempt()["verification"]["verdict"], "checks_only")
                    self.assertEqual((self.workspace / ".sortie" / "status").read_text(), "needs-human-review\n")
                self.assertEqual(code, {"timeout": 124, "cancelled": 143, "adapter_failure": 127}.get(outcome, 0))
                public = json.loads((STATE / end["exported"] / "manifest.json").read_text())
                self.assertEqual(public["missing"], [])

    def test_a_claim_of_success_is_handed_off_only_on_evidence(self):
        self.start("infrastructure")
        # The agent claims success for a head that breaks formatting.
        code = self.agent("""
            sed -i 's/^fn main() {/fn  main() {/' crates/bzbd/src/main.rs
            git -c user.name=Fixture -c user.email=fixture@example.test commit -qam 'misformatted'
            lab push > /dev/null
            printf 'Closes #990079\\n' > /var/tmp/body.md
            lab pr create --title fix --body-file /var/tmp/body.md > /dev/null
            lab pr ready > /dev/null
            lab handoff
        """)
        self.assertEqual(code, 0, self.last_log)
        status = self.workspace / ".sortie" / "status"
        self.assertFalse(status.exists(), status.read_text() if status.exists() else None)
        verification = self.attempt()["verification"]
        self.assertEqual(verification["verdict"], "failed")
        record = json.loads((STATE / verification["record"]).read_text())
        refused = [f["message"] for f in record["findings"] if f["code"] == "new_failure"]
        self.assertTrue(any("linux fmt" in m for m in refused) and any("macos fmt" in m for m in refused), refused)
        gh = json.loads((self.tmp / "gh.json").read_text())
        self.assertEqual(gh["prs"][0].get("comments", []), [])
        # The next turn is a fresh worker: the agent reads why, fixes it and asks again.
        code = self.agent("""
            lab verification > $OUT/verification.json
            python3 -c 'import json,sys; r=json.load(open(sys.argv[1])); print("verdict", r["data"]["verdict"])' \\
                $OUT/verification.json
            git -c user.name=Fixture -c user.email=fixture@example.test revert --no-edit HEAD > /dev/null
            lab push > /dev/null
            lab handoff
        """)
        self.assertEqual(code, 0, self.last_log)
        self.assertIn("verdict failed", self.last_log)
        self.assertEqual(self.attempt("0001")["end"]["outcome"], "unverified")
        self.assertEqual(status.read_text(), "needs-human-review\n")
        comments = json.loads((self.tmp / "gh.json").read_text())["prs"][0]["comments"]
        self.assertEqual(len(comments), 1)
        header = json.loads(comments[0].splitlines()[0].split(" ", 2)[2].rsplit(" -->", 1)[0])
        self.assertEqual(header["head"], git("-C", str(self.workspace), "rev-parse", "HEAD"))
        self.assertEqual((header["verdict"], header["profile"]), ("checks_only", "infrastructure"))
        self.assertEqual(self.end()["outcome"], "success")

    @unittest.skipUnless(os.environ.get("CLAUDE_CODE_OAUTH_TOKEN"), "needs a Claude credential in the environment")
    def test_a_model_agent_hands_off_through_the_gate(self):
        # A real model-backed Claude turn in the worker: it changes the branch,
        # opens and readies the PR and asks for review; the gate decides.
        self.start("infrastructure")
        prompt = ("You work in a disposable VM on branch sortie-lab/990079 of busybee; `lab` is on your PATH. "
                  "Do exactly this with the Bash tool, then stop: create PILOT.md containing the line "
                  "'model-backed lab turn'; commit it with `git -c user.name=Lab -c user.email=lab@example.test "
                  "commit`; run `lab push`; write 'Closes #990079' to /var/tmp/body.md; run `lab pr create "
                  "--title 'lab: model pilot' --body-file /var/tmp/body.md`; run `lab pr ready`; run "
                  "`lab handoff`. Report the final output of `lab handoff`.")
        code = self.agent("", argv=["claude", "-p", prompt, "--permission-mode", "bypassPermissions",
                                      "--allowedTools", "Bash", "--max-turns", "30", "--output-format", "json"])
        self.assertEqual(code, 0, self.last_log)
        turn = self.attempt()["turns"][-1]
        self.assertEqual((turn["runner"], turn["kind"]), ("claude", "exited"), turn)
        self.assertIn('"type":"result"', self.last_log.replace(" ", ""))
        self.assertEqual(git("-C", str(self.workspace), "show", "HEAD:PILOT.md").strip(), "model-backed lab turn")
        verification = self.attempt()["verification"]
        self.assertEqual(verification["verdict"], "checks_only", verification)
        self.assertEqual((self.workspace / ".sortie" / "status").read_text(), "needs-human-review\n")
        comments = json.loads((self.tmp / "gh.json").read_text())["prs"][0]["comments"]
        self.assertEqual(len(comments), 1)
        # The credential reached the turn only: never the session's or the runs' records.
        secret = os.environ["CLAUDE_CODE_OAUTH_TOKEN"].encode()
        for root in (STATE / "sessions" / str(ISSUE), STATE / "runs" / self.attempt()["run_id"]):
            for path in root.rglob("*"):
                if path.is_file():
                    self.assertNotIn(secret, path.read_bytes(), path.name)
        self.assertEqual(self.end()["outcome"], "success")


if __name__ == "__main__":
    unittest.main()
