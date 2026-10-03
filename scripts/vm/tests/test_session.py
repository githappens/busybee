"""Issue sessions without Parallels: a controller double whose guest is a
directory on this host, a real git origin, and a fake `gh`. The agent turns
run real shell commands and the real `lab` client against the real broker."""
from pathlib import Path
import io
import json
import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import contracts
import gate
import guest
import scenario
import session
import worker
from test_gate import FakeOps as GateOps, matrix

REPO = Path(__file__).resolve().parents[3]
CONFIG = {"deadlines": {"command": 60, "scenario": 120, "run": 600, "cleanup": 5}}
ISSUE = 42
BRANCH = f"sortie-lab/{ISSUE}"
IDENTITY = {"GIT_AUTHOR_NAME": "Fixture", "GIT_AUTHOR_EMAIL": "fixture@example.test",
            "GIT_COMMITTER_NAME": "Fixture", "GIT_COMMITTER_EMAIL": "fixture@example.test"}

FAKE_GH = r'''#!/usr/bin/env python3
import json, os, subprocess, sys
state_path = os.environ["FAKE_GH_STATE"]
state = json.load(open(state_path)) if os.path.exists(state_path) else {"prs": [], "calls": []}
args = sys.argv[1:]
state["calls"].append(args)
def save():
    json.dump(state, open(state_path, "w"))
def arg(name):
    return args[args.index(name) + 1]
out = ""
if args[:2] == ["pr", "list"]:
    out = json.dumps([{"number": p["number"], "url": p["url"], "isDraft": p["draft"], "headRefOid": ""}
                      for p in state["prs"] if p["head"] == arg("--head") and p["state"] == "open"])
elif args[:2] == ["pr", "create"]:
    number = 100 + len(state["prs"])
    state["prs"].append({"number": number, "head": arg("--head"), "title": arg("--title"),
                         "body": sys.stdin.read(), "draft": "--draft" in args, "state": "open",
                         "url": f"https://github.com/owner/repo/pull/{number}"})
    out = state["prs"][-1]["url"] + "\n"
elif args[:2] == ["pr", "ready"]:
    next(p for p in state["prs"] if p["number"] == int(args[2]))["draft"] = False
elif args[:2] == ["pr", "comment"]:
    next(p for p in state["prs"] if p["number"] == int(args[2])).setdefault("comments", []).append(sys.stdin.read())
elif args[:2] == ["pr", "view"]:
    p = next(p for p in state["prs"] if p["number"] == int(args[2]))
    out = json.dumps({"number": p["number"], "isDraft": p["draft"], "state": p["state"].upper(),
                      "comments": [{"body": c} for c in p.get("comments", [])]})
elif args[:2] == ["repo", "view"]:
    out = "owner/repo\n"
elif args[0] == "api":
    number = int(args[-1].rsplit("/", 1)[1])
    p = next(p for p in state["prs"] if p["number"] == number)
    sha = subprocess.run(["git", "ls-remote", "origin", "refs/heads/" + p["head"]], capture_output=True,
                         text=True).stdout.split()[0]
    repo = {"full_name": "owner/repo"}
    out = json.dumps({"number": number, "state": p["state"], "draft": p["draft"],
                      "head": {"sha": sha, "ref": p["head"], "repo": repo}, "base": {"repo": repo}})
else:
    sys.exit(f"fake gh: unsupported {args}")
save()
sys.stdout.write(out)
'''


# The guest's GNU timeout(1), for hosts without one (macOS CI): `timeout -k K
# -- S ARGV` leads its own process group, ends the command at S seconds and
# exits 124 then.
TIMEOUT_SHIM = r'''#!/usr/bin/env python3
import os, signal, subprocess, sys
args = sys.argv[1:]
grace = int(args[args.index("-k") + 1]) if "-k" in args else 0
rest = args[args.index("--") + 1:]
seconds, argv = float(rest[0]), rest[1:]
os.setpgid(0, 0)
try:
    proc = subprocess.Popen(argv)
except OSError:
    sys.exit(127)
try:
    sys.exit(proc.wait(timeout=seconds))
except subprocess.TimeoutExpired:
    proc.terminate()
    try:
        proc.wait(timeout=grace or None)
    except subprocess.TimeoutExpired:
        proc.kill()
    sys.exit(124)
'''


def gnu_timeout():
    found = shutil.which("timeout")
    if not found:
        return False
    out = subprocess.run([found, "--version"], capture_output=True, text=True)
    return "GNU" in out.stdout


class LocalGuest:
    """A guest whose filesystem is this host's: commands run under sh."""

    def __init__(self, env):
        self.env = env

    def run(self, command, timeout, stdin=None, tty=False, check=True, raw=False):
        done = subprocess.run(["sh", "-c", command], input=stdin, capture_output=True, timeout=timeout, env=self.env)
        out = done.stdout if raw else done.stdout.decode(errors="replace")
        err = done.stderr.decode(errors="replace")
        if check and done.returncode != 0:
            raise guest.GuestError(f"`{command[:80]}` exited {done.returncode}: {err.strip()[-300:]}")
        return done.returncode, out, err

    def session(self, command, forward, stdin=None, stdout=None, stderr=None):
        remote, local = forward
        os.symlink(local, remote)  # what ssh's -R gives the guest
        return subprocess.Popen(["sh", "-c", command], stdin=subprocess.DEVNULL, stdout=stdout, stderr=stderr,
                                env=self.env)


class FakeController:
    """The controller's worker operations, with each worker's guest a directory."""

    def __init__(self, root):
        self.root = root
        self.state = root / "state"
        self.config = CONFIG
        # The guest's /run: short, like the real one, for its socket path.
        self.guest_run = tempfile.mkdtemp(prefix="bzg-", dir="/tmp")
        self.guests = root / "guests"
        self.calls = []
        self.fail_create = None
        self.env = dict(os.environ, **IDENTITY)
        # Verification writes its matrices as verify.run does; tests change their outcomes.
        self.gate_ops = GateOps(self.state)

    def _path(self, run_id):
        return worker.run_dir(self.state, run_id) / "worker.json"

    def _save(self, record):
        self._path(record["run_id"]).parent.mkdir(parents=True, exist_ok=True)
        self._path(record["run_id"]).write_text(json.dumps(record))

    def create(self, revision, patch, source_repo, branch):
        self.calls.append(("create", revision, str(source_repo)))
        if self.fail_create:
            return contracts.result("worker create", "environment_failure", "worker limit reached",
                                    [contracts.finding("worker_limit", self.fail_create)])
        run_id = contracts.new_run_id()
        co = self.guests / run_id / "checkout"
        (self.guests / run_id / "home").mkdir(parents=True)
        subprocess.run(["git", "clone", "-q", "--no-checkout", str(source_repo), str(co)], check=True)
        subprocess.run(["git", "-C", str(co), "checkout", "-q", "--detach", revision], check=True)
        if patch:
            subprocess.run(["git", "-C", str(co), "apply", "--binary", str(patch)], check=True)
        self._save({"run_id": run_id, "status": "ready", "template": "linux", "revision": revision})
        return contracts.result("worker create", "success", "ready", data={"run_id": run_id})

    def record(self, run_id):
        path = self._path(run_id)
        return json.loads(path.read_text()) if path.is_file() else None

    def guest(self, run_id):
        if self.record(run_id)["status"] != "ready":
            raise worker.Refused("worker_stopped", f"worker {run_id} is {self.record(run_id)['status']}")
        return LocalGuest(self.env)

    def checkout(self, run_id):
        return str(self.guests / run_id / "checkout")

    def home(self, run_id):
        return str(self.guests / run_id / "home")

    def guest_workspace(self, workspace):
        return str(self.root / "guest-workspace" / Path(workspace).name)

    def run_left(self, run_id):
        return 3600

    def reset(self, run_id, source_repo, branch):
        self.calls.append(("reset", run_id))
        record = self.record(run_id)
        shutil.rmtree(self.guests / run_id)
        (self.guests / run_id / "home").mkdir(parents=True)
        co = self.guests / run_id / "checkout"
        # As the controller does: the recorded revision comes from the source repository.
        subprocess.run(["git", "clone", "-q", "--no-checkout", str(source_repo), str(co)], check=True)
        subprocess.run(["git", "-C", str(co), "checkout", "-q", "--detach", record["revision"]], check=True)
        return contracts.result("worker reset", "success", "restored")

    def destroy(self, run_id):
        self.calls.append(("destroy", run_id))
        record = self.record(run_id)
        cdir = worker.run_dir(self.state, run_id) / "collect" / "0001"
        cdir.mkdir(parents=True, exist_ok=True)
        (cdir / "collected.json").write_text(json.dumps({"run_id": run_id}))
        record["status"] = "destroyed"
        self._save(record)
        shutil.rmtree(self.guests / run_id, ignore_errors=True)
        return contracts.result("worker destroy", "success", "destroyed", data={"run_id": run_id})

    def export(self, run_id):
        out = worker.run_dir(self.state, run_id) / "public"
        out.mkdir(parents=True, exist_ok=True)
        (out / "manifest.json").write_text("{}")
        return contracts.result("export", "success", "exported", data={"path": str(out.relative_to(self.state))})

    def collected(self, run_id):
        path = worker.run_dir(self.state, run_id) / "collect" / "0001" / "collected.json"
        return path if path.is_file() else None

    def gate(self, issue, revision, base, overlay, source_repo, branch):
        # The lab's workers verify; the session's own must be gone by now.
        held = [r["run_id"] for r in map(json.loads, (p.read_text() for p in self.state.glob("runs/*/worker.json")))
                if r["status"] == "ready"]
        self.calls.append(("gate", revision, base, held))
        return gate.evaluate(self.gate_ops, issue, revision, base, overlay)

    def operation(self, op, run_id, **args):
        self.calls.append((op, run_id, args))
        return contracts.result(op, "success", f"{op} on {run_id}", data={"run_id": run_id, "op": op})


class Harness(unittest.TestCase):
    def setUp(self):
        self.tmp = Path(tempfile.mkdtemp(prefix="bzs-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.git_env = dict(os.environ, **IDENTITY, GIT_CONFIG_GLOBAL=str(self.tmp / "gitconfig"),
                            GIT_CONFIG_NOSYSTEM="1")
        (self.tmp / "gitconfig").write_text("[init]\n\tdefaultBranch = main\n")
        os.environ.update(GIT_CONFIG_GLOBAL=str(self.tmp / "gitconfig"), GIT_CONFIG_NOSYSTEM="1")
        self.git("init", "-q", "--bare", str(self.tmp / "origin.git"))
        seed = self.tmp / "seed"
        self.git("init", "-q", str(seed))
        (seed / "README").write_text("busybee\n")
        (seed / "sortie").mkdir()
        (seed / "sortie" / "lab.py").write_text("# dispatch\n")
        self.git("-C", str(seed), "add", "-A")
        self.git("-C", str(seed), "commit", "-q", "-m", "base")
        self.git("-C", str(seed), "push", "-q", str(self.tmp / "origin.git"), "HEAD:refs/heads/main")
        self.workspace = self.tmp / "workspace"
        self.git("clone", "-q", str(self.tmp / "origin.git"), str(self.workspace))
        self.git("-C", str(self.workspace), "checkout", "-q", "-b", BRANCH)
        with open(self.workspace / ".git" / "info" / "exclude", "a") as f:
            f.write("/.sortie/\n")
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        (bin_dir / "gh").write_text(FAKE_GH)
        (bin_dir / "gh").chmod(0o755)
        if not gnu_timeout():
            (bin_dir / "timeout").write_text(TIMEOUT_SHIM)
            (bin_dir / "timeout").chmod(0o755)
        # An agent runner by name, to show credentials reach the turn.
        (bin_dir / "claude").write_text('#!/bin/sh\nexec sh "$@"\n')
        (bin_dir / "claude").chmod(0o755)
        self.gh_state = self.tmp / "gh.json"
        os.environ["FAKE_GH_STATE"] = str(self.gh_state)
        self.addCleanup(os.environ.pop, "FAKE_GH_STATE", None)
        self.c = FakeController(self.tmp)
        self.addCleanup(shutil.rmtree, self.c.guest_run, True)
        self.c.env["PATH"] = f"{bin_dir}{os.pathsep}{os.environ['PATH']}"
        self.err = io.StringIO()
        self.sessions = self.make_sessions()

    def make_sessions(self, env=None):
        out = open(os.devnull, "w")
        self.addCleanup(out.close)
        return session.Sessions(self.c, REPO, env=env or {}, shell=lambda runner: [], gh=str(self.tmp / "bin" / "gh"),
                                out=out, err=self.err)

    def git(self, *args, cwd=None):
        return subprocess.run(["git", *args], cwd=cwd, env=self.git_env, check=True, capture_output=True,
                              text=True).stdout.strip()

    def start(self, profile="product"):
        result = self.sessions.start(ISSUE, self.workspace, profile)
        self.assertEqual(result["status"], "success", result)
        return result["data"]["run_id"]

    def turn(self, script, timeout=None, argv=None):
        """One agent turn whose agent is `script` under sh, in the guest."""
        out = self.tmp / "out"
        out.mkdir(exist_ok=True)
        body = f"set -e\nOUT={out}\nexport PATH=\"$PATH\"\n" + textwrap.dedent(script)
        return self.sessions.agent(self.workspace, argv or ["sh", "-c", body], timeout)

    def reply(self, name):
        return json.loads((self.tmp / "out" / name).read_text())

    def attempt(self, name=None):
        s = self.sessions.load(ISSUE)
        return self.sessions.attempt(s, name or s["attempt"] or f"{s['attempts']:04d}")

    def gh(self):
        return json.loads(self.gh_state.read_text()) if self.gh_state.is_file() else {"prs": [], "calls": []}

    def head(self):
        return self.git("-C", str(self.workspace), "rev-parse", "HEAD")

    def assert_accounted(self, end, outcome):
        self.assertEqual(end["outcome"], outcome, end)
        self.assertEqual(end["worker_status"], "destroyed")
        self.assertEqual(end["destroy"], "success")
        self.assertTrue((self.c.state / end["collected"]).is_file())
        self.assertTrue((self.c.state / end["exported"] / "manifest.json").is_file())
        self.assertIn(("destroy", end["run_id"]), self.c.calls)


class ScopeTests(Harness):
    def test_agent_controls_only_assigned_worker(self):
        run_id = self.start()
        other = contracts.new_run_id()
        code = self.turn(f"""
            # Administer the guest: a temporary tool and a restarted daemon live in it.
            tools="$(cd "$(pwd -P)/.." && pwd)/home/tools"
            mkdir -p "$tools" && printf 'tool\\n' > "$tools/probe" && test -f "$tools/probe"
            sleep 300 & daemon=$!; kill $daemon; sleep 300 & kill $!
            lab exec -- sh -c 'restart the daemon' > $OUT/exec.json
            lab terminal open -- busybee monitor > $OUT/topen.json
            lab terminal capture 0001 --expect busybee > $OUT/capture.json
            lab inspect > $OUT/inspect.json
            lab signal TERM 1234 > $OUT/signal.json
            ! lab --run {other} inspect > $OUT/wrong.json
            ! lab --target host exec -- true > $OUT/host.json
            ! lab request '{{"op": "worker-create", "args": {{}}}}' > $OUT/create.json
            ! lab request '{{"op": "inspect", "run": "{other}"}}' > $OUT/raw-wrong.json
            git -c user.name=F -c user.email=f@example.test commit -q --allow-empty -m kept
            lab reset > $OUT/reset.json
        """)
        self.assertEqual(code, 0, self.err.getvalue())
        for name, op in (("exec", "exec"), ("topen", "terminal-open"), ("capture", "terminal-capture"),
                         ("inspect", "inspect"), ("signal", "signal")):
            self.assertEqual(self.reply(f"{name}.json")["data"], {"run_id": run_id, "op": op})
        worker_ops = [c for c in self.c.calls if c[0] not in ("create", "reset", "destroy")]
        self.assertEqual({c[1] for c in worker_ops}, {run_id})
        self.assertEqual(worker_ops[0][2]["argv"], ["sh", "-c", "restart the daemon"])
        for name, code_ in (("wrong", "target_not_assigned"), ("raw-wrong", "target_not_assigned"),
                            ("host", "host_target_refused"), ("create", "operation_not_permitted")):
            self.assertEqual([f["code"] for f in self.reply(f"{name}.json")["findings"]], [code_])
        self.assertTrue(self.reply("reset.json")["data"]["scheduled"])
        # The reset ran once the turn ended: the worker is back at its baseline,
        # the temporary tool is gone, and the agent's commit was kept.
        self.assertIn(("reset", run_id), self.c.calls)
        self.assertFalse((self.c.guests / run_id / "home" / "tools").exists())
        self.assertEqual(self.git("-C", str(self.workspace), "log", "-1", "--format=%s"), "kept")
        self.assertEqual(self.git("-C", self.c.checkout(run_id), "rev-parse", "HEAD"), self.head())
        self.assertEqual(self.attempt()["resets"][0]["status"], "success")

    def test_a_reset_restores_a_revision_only_the_workspace_has(self):
        (self.workspace / "local.txt").write_text("never pushed\n")
        self.git("-C", str(self.workspace), "add", "local.txt")
        self.git("-C", str(self.workspace), "commit", "-q", "-m", "local only")
        run_id = self.start()
        self.assertEqual(self.turn("lab reset > /dev/null"), 0, self.err.getvalue())
        self.assertEqual(self.attempt()["resets"][0]["status"], "success", self.attempt()["resets"])
        self.assertEqual((Path(self.c.checkout(run_id)) / "local.txt").read_text(), "never pushed\n")

    def test_a_reset_that_fails_is_recorded_and_the_turn_still_ends(self):
        self.start()

        def refused(run_id, source_repo, branch):
            raise worker.Refused("run_deadline_passed", "the run ended")
        self.c.reset = refused
        self.assertEqual(self.turn("lab reset > /dev/null"), 0, self.err.getvalue())
        turn = self.attempt()["turns"][-1]
        self.assertTrue(turn["finished"])
        self.assertEqual(turn["reset"]["status"], "environment_failure")
        self.assertIn("the run ended", turn["reset"]["summary"])

    def test_a_reset_waits_for_a_checkpoint_that_kept_the_work(self):
        self.start()
        (self.workspace / "README").write_text("edited on the host\n")
        self.assertEqual(self.turn("printf 'guest\n' > README; lab reset > /dev/null"), 0)
        turn = self.attempt()["turns"][-1]
        self.assertEqual((turn["sync"]["status"], turn["reset"]["status"]), ("conflict", "skipped"))
        self.assertNotIn("reset", [c[0] for c in self.c.calls])

    def test_a_daemon_the_agent_leaves_running_does_not_hold_its_turn(self):
        self.start()
        started = time.monotonic()
        # The daemon inherits the agent's stdout and stderr and outlives it.
        self.assertEqual(self.turn("echo before; (sleep 120; echo late) & echo after"), 0, self.err.getvalue())
        self.assertLess(time.monotonic() - started, 30)
        self.assertEqual(self.attempt()["turns"][-1]["kind"], "exited")

    def test_no_operation_reaches_an_unassigned_worker(self):
        self.start()
        broker = session.Broker(self.sessions, self.sessions.load(ISSUE), self.attempt())
        for request in (None, [], {"op": 7}, {"op": "inspect", "args": []},
                        {"op": "template", "args": {}}, {"op": "verify"}, {"op": "worker-destroy"},
                        {"op": "inspect", "target": "localhost"}, {"op": "exec", "run": "r-x"}):
            self.assertNotEqual(broker.handle(request)["status"], "success", request)
        self.assertEqual([c for c in self.c.calls if c[0] not in ("create",)], [])

    def test_an_operation_that_fails_still_answers(self):
        self.start()
        broker = session.Broker(self.sessions, self.sessions.load(ISSUE), self.attempt())

        def broken(op, run_id, **args):
            raise FileNotFoundError(2, "No such file or directory")
        self.c.operation = broken
        reply = broker.handle({"op": "inspect"})
        self.assertEqual(reply["status"], "environment_failure")
        self.assertEqual([f["code"] for f in reply["findings"]], ["operation_failed"])


class ContinuationTests(Harness):
    def test_continuation_preserves_branch_and_pr(self):
        first = self.start()
        (self.tmp / "body.md").write_text("Closes #42\n")
        code = self.turn(f"""
            printf 'one\\n' > a.txt && git add a.txt && git -c user.name=F -c user.email=f@example.test commit -q -m a
            printf 'unfinished\\n' > b.txt
            lab push > $OUT/push.json
            lab pr create --title 'lab: #42' --body-file {self.tmp}/body.md > $OUT/pr.json
        """)
        self.assertEqual(code, 0, self.err.getvalue())
        pr = self.reply("pr.json")["data"]["pr"]
        self.assertFalse(self.reply("pr.json")["data"]["reused"])
        # The runner is killed (no end), and its worker is gone with it.
        self.c.destroy(first)
        second = self.start()
        interrupted = self.attempt("0001")["end"]
        self.assertEqual((interrupted["outcome"], interrupted["worker_status"]), ("interrupted", "destroyed"))
        self.assertNotEqual(first, second)
        code = self.turn(f"""
            test "$(git branch --show-current)" = {BRANCH}
            test "$(cat a.txt)" = one && test "$(cat b.txt)" = unfinished
            git log -1 --format=%s | grep -qx a
            test "$BUSYBEE_LAB_PR" = {pr}
            lab pr create --title again --body-file {self.tmp}/body.md > $OUT/pr2.json
            printf 'two\\n' >> b.txt
        """)
        self.assertEqual(code, 0, self.err.getvalue())
        self.assertEqual(self.reply("pr2.json")["data"], {"pr": pr, "reused": True})
        self.assertEqual(len([c for c in self.gh()["calls"] if c[:2] == ["pr", "create"]]), 1)
        self.assertEqual(self.sessions.load(ISSUE)["pr"], pr)
        # The worker is replaced again mid-attempt; the next turn resumes in a fresh one.
        record = self.c.record(second)
        record["status"] = "expired"
        self.c._save(record)
        self.assertEqual(self.turn("test \"$(cat b.txt)\" = \"$(printf 'unfinished\\ntwo')\""), 0, self.err.getvalue())
        self.assertEqual(self.attempt("0002")["end"]["outcome"], "timeout")
        self.assertEqual(self.git("-C", str(self.workspace), "branch", "--show-current"), BRANCH)
        self.assertEqual((self.workspace / "b.txt").read_text(), "unfinished\ntwo\n")

    def test_a_workspace_changed_behind_the_worker_is_not_overwritten(self):
        self.start()
        (self.workspace / "README").write_text("edited on the host\n")
        self.assertEqual(self.turn("printf 'guest\\n' > README"), 0)
        self.assertEqual((self.workspace / "README").read_text(), "edited on the host\n")
        turn = self.attempt()["turns"][-1]
        self.assertEqual(turn["sync"]["status"], "conflict")
        kept = self.c.state / turn["sync"]["kept"]
        self.assertIn(b"+guest", (kept / "worktree.diff").read_bytes())


class SupervisorTests(Harness):
    def test_branch_cannot_replace_supervisor(self):
        trusted = session.load_policy(REPO)
        # The candidate branch replaces the controller and the guard policy.
        (self.workspace / "sortie" / "guard-policy.json").write_text(json.dumps(
            {"schema": "busybee.lab.guard/v1", "profiles": {"product": {"forbidden_paths": []}}}))
        (self.workspace / "scripts" / "vm").mkdir(parents=True)
        (self.workspace / "scripts" / "vm" / "session.py").write_text("raise SystemExit('candidate code ran')\n")
        self.git("-C", str(self.workspace), "add", "-A")
        self.git("-C", str(self.workspace), "commit", "-q", "-m", "replace the supervisor")
        start = self.head()
        self.start("product")
        code = self.turn("""
            printf '# looser\\n' >> sortie/lab.py
            git -c user.name=F -c user.email=f@example.test commit -qam 'edit dispatch'
            ! lab push > $OUT/push.json
        """)
        self.assertEqual(code, 0, self.err.getvalue())
        # The running policy is the trusted one, whatever the branch holds.
        self.assertEqual(self.sessions.policy, trusted)
        self.assertIn("sortie/", self.sessions.policy["profiles"]["product"]["forbidden_paths"])
        self.assertEqual([f["code"] for f in self.reply("push.json")["findings"]], ["checkpoint_violation"])
        self.assertEqual(self.head(), start)
        self.assertEqual(self.git("ls-remote", str(self.tmp / "origin.git"), f"refs/heads/{BRANCH}"), "")
        violation = self.attempt()["violation"]
        self.assertEqual(violation["paths"], ["scripts/vm/session.py", "sortie/guard-policy.json", "sortie/lab.py"])
        end = self.sessions.end(self.workspace)["data"]
        self.assert_accounted(end, "blocked")
        self.assertEqual((self.workspace / ".sortie" / "status").read_text(), "blocked\n")

    def test_renamed_and_unusual_paths_do_not_escape_the_guard(self):
        self.start("product")
        self.assertEqual(self.turn("""
            git mv sortie/lab.py lab.py
            git -c user.name=F -c user.email=f@example.test commit -qm 'move dispatch out'
            mkdir -p .github/workflows && printf 'on: push\\n' > ".github/workflows/é x.yml"
        """), 0, self.err.getvalue())
        violation = self.attempt()["violation"]
        self.assertIn("sortie/lab.py", violation["paths"])
        self.assertIn(".github/workflows/\u00e9 x.yml", violation["paths"])
        self.assertTrue((self.workspace / "sortie" / "lab.py").is_file())

    def test_a_workspace_without_origin_main_is_refused_not_unguarded(self):
        self.git("-C", str(self.workspace), "update-ref", "-d", "refs/remotes/origin/main")
        self.start("product")
        self.assertEqual(self.turn("echo x > y.txt"), 0)
        self.assertEqual(self.attempt()["turns"][-1]["sync"]["status"], "failed")
        self.assertFalse((self.workspace / "y.txt").exists())

    def test_an_infrastructure_session_may_change_dispatch_but_not_its_supervisor(self):
        self.start("infrastructure")
        self.assertEqual(self.turn("""
            printf '{"schema": "busybee.lab.guard/v1", "profiles": {}}' > sortie/guard-policy.json
            git -c user.name=F -c user.email=f@example.test add -A
            git -c user.name=F -c user.email=f@example.test commit -qm 'change policy'
        """), 0, self.err.getvalue())
        self.assertEqual(self.attempt()["turns"][-1]["sync"]["status"], "synced")
        self.assertEqual(self.git("-C", str(self.workspace), "log", "-1", "--format=%s"), "change policy")
        # Adopted by the branch for review, never by the running controller.
        self.assertEqual(self.sessions.policy, session.load_policy(REPO))

    def test_the_trusted_snapshot_never_comes_from_the_working_tree(self):
        root = self.tmp / "operator"
        shutil.copytree(REPO / "sortie", root / "sortie")
        self.git("init", "-q", str(root))
        for path in ("scripts/vm/session.py", "tests/scenarios/runner.py", "skills/x/SKILL.md", "infra/vm/linux/facts.sh",
                     "docs/development/agent-review.md", "AGENTS.md", "CLAUDE.md",
                     ".github/workflows/agent-review-gate.yml"):
            (root / path).parent.mkdir(parents=True, exist_ok=True)
            (root / path).write_text("reviewed\n")
        self.git("-C", str(root), "add", "-A")
        self.git("-C", str(root), "commit", "-q", "-m", "reviewed")
        (root / "scripts/vm/session.py").write_text("unreviewed\n")
        (root / "sortie/guard-policy.json").write_text("{}\n")
        done = subprocess.run(["bash", str(REPO / "sortie" / "snapshot.sh"), "HEAD", str(self.tmp / "state")],
                              cwd=root, capture_output=True, text=True, env=self.git_env)
        self.assertEqual(done.returncode, 0, done.stderr)
        trusted = Path(done.stdout.strip())
        self.assertEqual((trusted / "scripts/vm/session.py").read_text(), "reviewed\n")
        # A snapshot left without its revision (older layout, interrupted) is rebuilt.
        (trusted / ".revision").unlink()
        (trusted / "scripts/vm/session.py").unlink()
        again = subprocess.run(["bash", str(REPO / "sortie" / "snapshot.sh"), "HEAD", str(self.tmp / "state")],
                               cwd=root, capture_output=True, text=True, env=self.git_env)
        self.assertEqual((again.returncode, again.stdout), (0, done.stdout), again.stderr)
        self.assertEqual((trusted / "scripts/vm/session.py").read_text(), "reviewed\n")
        self.assertEqual(json.loads((trusted / "sortie/guard-policy.json").read_text()),
                         session.load_policy(REPO))
        self.assertTrue((trusted / "tests/scenarios/runner.py").is_file())
        # Everything the controller reads from its own checkout is in the snapshot.
        self.assertTrue((trusted / "infra/vm/linux/facts.sh").is_file())
        self.assertEqual(scenario._controller(trusted), {"head": self.git("-C", str(root), "rev-parse", "HEAD"),
                                                          "scenarios_dirty": False})


class ExitTests(Harness):
    def end(self):
        result = self.sessions.end(self.workspace)
        return result["data"]

    def test_every_runner_exit_accounts_for_worker(self):
        (self.tmp / "body.md").write_text("Closes #42\n")
        cases = {
            "success": (f"""
                printf 'fix\\n' > fix.txt && git add fix.txt
                git -c user.name=F -c user.email=f@example.test commit -qm fix
                lab push > $OUT/push.json
                lab pr create --title t --body-file {self.tmp}/body.md > /dev/null
                lab pr ready > /dev/null
                lab handoff > $OUT/handoff.json
            """, None, None),
            "blocked": ("mkdir -p .sortie && printf 'pueued is missing\\n' > .sortie/blocker.md && "
                        "printf 'blocked\\n' > .sortie/status", None, None),
            "timeout": ("sleep 30", 1, None),
            "cancelled": ("sleep 30", None, "cancel"),
            "adapter_failure": (None, None, ["definitely-not-an-agent-command"]),
            "no_handoff": ("true", None, None),
        }
        for outcome, (script, timeout, how) in cases.items():
            with self.subTest(outcome=outcome):
                self.sessions.cancel.clear()
                run_id = self.start()
                if how == "cancel":
                    threading.Timer(1, self.sessions.cancel.set).start()
                argv = how if isinstance(how, list) else None
                code = self.turn(script or "", timeout, argv)
                turn = self.attempt()["turns"][-1]
                self.assertTrue(turn["finished"])
                self.assertTrue((self.c.state / "sessions" / str(ISSUE) / "attempts" / self.attempt()["attempt"] /
                                 "turns" / f"{turn['turn']}.stderr").is_file())
                end = self.end()
                self.assert_accounted(end, outcome)
                self.assertEqual(end["run_id"], run_id)
                self.assertEqual({"timeout": 124, "cancelled": 143}.get(outcome, code), code)
                if outcome == "success":
                    self.assertEqual(self.reply("handoff.json")["status"], "success", self.reply("handoff.json"))
                    self.assertEqual((self.workspace / ".sortie" / "status").read_text(), "needs-human-review\n")

    def test_missing_credentials_and_a_dead_runner_are_accounted(self):
        self.start()
        self.assertEqual(self.sessions.agent(self.workspace, ["claude", "-p"]), 1)
        self.assertEqual(self.attempt()["turns"][-1]["reason"], "credentials_missing")
        self.assert_accounted(self.end(), "adapter_failure")
        # A runner killed mid-turn leaves the turn unfinished; its end says so.
        self.start()
        s = self.sessions.load(ISSUE)
        a = self.sessions.attempt(s)
        a["turns"].append({"turn": "0001", "finished": False})
        self.sessions._save(s, a)
        self.assert_accounted(self.end(), "cancelled")

    def test_a_worker_that_cannot_be_created_is_an_accounted_failure(self):
        self.c.fail_create = "1 worker(s) may be active"
        result = self.sessions.start(ISSUE, self.workspace, "product")
        self.assertEqual(result["status"], "environment_failure")
        end = result["data"]["end"]
        self.assertEqual((end["outcome"], end["worker_status"]), ("adapter_failure", "none"))
        self.assertIn("1 worker(s) may be active", end["reason"])
        self.assertIsNone(self.sessions.load(ISSUE)["attempt"])

    def test_claude_runs_as_the_worker_root_it_is_given(self):
        # The agent is root in its disposable guest; Claude Code refuses to skip
        # its permission prompts as root unless told it runs in a sandbox.
        self.sessions = self.make_sessions(env={"CLAUDE_CODE_OAUTH_TOKEN": "sk-secret-value"})
        self.start()
        self.assertEqual(self.sessions.agent(self.workspace, ["claude", "-c", 'test "$IS_SANDBOX" = 1']), 0,
                         self.err.getvalue())
        self.assertEqual(self.turn('test -z "${IS_SANDBOX-}"'), 0, self.err.getvalue())

    def test_credentials_reach_the_turn_and_never_its_records(self):
        self.sessions = self.make_sessions(env={"CLAUDE_CODE_OAUTH_TOKEN": "sk-secret-value",
                                                "GITHUB_TOKEN": "gh-secret-value", "SORTIE_X": "1"})
        self.start()
        # Only the runner's own credential reaches the worker; nothing else from the dispatcher.
        code = self.sessions.agent(self.workspace, ["claude", "-c", 'test "$CLAUDE_CODE_OAUTH_TOKEN" = '
                                                    'sk-secret-value && test -z "${GITHUB_TOKEN-}${SORTIE_X-}"'],
                                   None)
        # `claude` here is sh under another name.
        self.assertEqual(code, 0, self.err.getvalue())
        state = self.c.state
        for path in state.rglob("*"):
            if path.is_file():
                self.assertNotIn(b"sk-secret-value", path.read_bytes(), path)
                self.assertNotIn(b"gh-secret-value", path.read_bytes(), path)
        self.assertEqual(list(Path(self.c.guest_run).glob("*/env")), [])


class GateTests(Harness):
    """The handoff goes to review only on the controller's own evidence."""

    def setUp(self):
        super().setUp()
        (self.tmp / "body.md").write_text("Closes #42\n")

    def handoff(self, change, push="lab push"):
        return self.turn(f"""
            {change}
            {push} > $OUT/push.json
            lab pr create --title 'lab: #42' --body-file {self.tmp}/body.md > $OUT/pr.json
            lab pr ready > /dev/null
            lab handoff > $OUT/handoff.json
        """)

    def commit(self, name, text):
        return (f"printf '{text}\\n' > {name} && git add {name} && "
                f"git -c user.name=F -c user.email=f@example.test commit -qm '{name}: {text}'")

    def gates(self):
        return [c for c in self.c.calls if c[0] == "gate"]

    def comments(self, pr):
        return next(p for p in self.gh()["prs"] if p["number"] == pr).get("comments", [])

    def status(self):
        path = self.workspace / ".sortie" / "status"
        return path.read_text() if path.is_file() else None

    def test_review_resumes_same_task_with_current_evidence(self):
        self.start()
        self.assertEqual(self.handoff(self.commit("fix.txt", "one")), 0, self.err.getvalue())
        pr = self.reply("pr.json")["data"]["pr"]
        self.assertTrue(self.reply("handoff.json")["data"]["scheduled"])
        self.assertEqual(self.status(), "needs-human-review\n")
        main = self.git("-C", str(self.workspace), "rev-parse", "origin/main")
        self.assertEqual([g[1:3] for g in self.gates()], [(self.head(), main)])
        self.assert_accounted(self.sessions.end(self.workspace)["data"], "success")
        # Review findings, a failing CI job, then a main that moved under the
        # branch: each resumes the same issue, branch and PR in a fresh worker,
        # and its new head goes to review only on evidence for that head.
        upstream = self.tmp / "upstream"
        self.git("clone", "-q", str(self.tmp / "origin.git"), str(upstream))
        for event, change, push in (
                ("review", self.commit("fix.txt", "two"), "lab push"),
                ("ci", self.commit("test.txt", "fixed"), "lab push"),
                ("conflict", "lab fetch > /dev/null && git -c user.name=F -c user.email=f@example.test rebase -q "
                             "origin/main", "lab push --force-with-lease")):
            with self.subTest(event):
                if event == "conflict":
                    (upstream / "main.txt").write_text("moved on\n")
                    self.git("-C", str(upstream), "add", "main.txt")
                    self.git("-C", str(upstream), "commit", "-qm", "main moved")
                    self.git("-C", str(upstream), "push", "-q", "origin", "HEAD:main")
                    main = self.git("-C", str(upstream), "rev-parse", "HEAD")
                before = self.head()
                run_id = self.start()
                self.assertEqual(self.handoff(change, push), 0, self.err.getvalue())
                self.assertNotEqual(self.head(), before)
                self.assertEqual(self.reply("pr.json")["data"], {"pr": pr, "reused": True})
                self.assertEqual(self.sessions.load(ISSUE)["pr"], pr)
                verified = self.gates()[-1]
                self.assertEqual(verified[1:3], (self.head(), main))
                self.assertEqual(verified[3], [], "the session's worker was still held during verification")
                self.assertEqual(self.status(), "needs-human-review\n")
                scm = json.loads((self.workspace / ".sortie" / "scm.json").read_text())
                self.assertEqual((scm["sha"], scm["pr_number"]), (self.head(), pr))
                # The evidence on the PR is for this head.
                marker = json.loads(self.comments(pr)[-1].splitlines()[0][len(f"<!-- {gate.MARKER} "):-len(" -->")])
                self.assertEqual((marker["head"], marker["base"]), (self.head(), main))
                end = self.sessions.end(self.workspace)["data"]
                self.assert_accounted(end, "success")
                self.assertEqual(end["run_id"], run_id)
        self.assertEqual(len([c for c in self.gh()["calls"] if c[:2] == ["pr", "create"]]), 1)
        # Only the new heads, and the new main, were verified again.
        self.assertEqual([r for r in self.c.gate_ops.runs if r[0] == "base"], [("base", m) for m in
                                                                              dict.fromkeys(g[2] for g in self.gates())])

    def test_an_agents_claim_of_success_is_not_a_handoff(self):
        # The candidate breaks formatting on macOS: the agent still asks for review.
        broken = matrix("candidate", "x")
        self.c.gate_ops.candidate = lambda rev: dict(
            matrix("candidate", rev), platforms={**matrix("candidate", rev)["platforms"], "macos": dict(
                broken["platforms"]["macos"], head=rev,
                checks={**broken["platforms"]["macos"]["checks"], "fmt": {"status": "product_failure",
                                                                          "exit_code": 1}})})
        first = self.start()
        self.assertEqual(self.handoff(self.commit("fix.txt", "one")), 0, self.err.getvalue())
        pr = self.reply("pr.json")["data"]["pr"]
        self.assertIsNone(self.status())
        self.assertFalse((self.workspace / ".sortie" / "scm.json").exists())
        self.assertEqual(self.comments(pr), [])
        attempt = self.attempt()
        self.assertEqual(attempt["verification"]["verdict"], "failed")
        self.assertIsNone(attempt["handoff"])
        self.assertIn("verification of", self.err.getvalue())
        # The next turn continues in a fresh worker, where the agent reads why.
        code = self.turn("lab verification > $OUT/verification.json")
        self.assertEqual(code, 0, self.err.getvalue())
        self.assertEqual(self.attempt("0001")["end"]["outcome"], "unverified")
        self.assertNotEqual(self.attempt()["run_id"], first)
        reply = self.reply("verification.json")
        self.assertEqual((reply["data"]["verdict"], reply["data"]["head"]), ("failed", self.head()))
        self.assertIn("new_failure", [f["code"] for f in reply["data"]["findings"]])
        self.assertIn("macos fmt", " ".join(f["message"] for f in reply["data"]["findings"]))
        # This attempt only read the result; it asked for no review.
        self.assert_accounted(self.sessions.end(self.workspace)["data"], "no_handoff")
        self.assertEqual(self.sessions.status(ISSUE)["data"]["latest_verification"]["verdict"], "failed")

    def test_unchanged_evidence_is_not_verified_or_posted_again(self):
        self.start()
        self.assertEqual(self.handoff(self.commit("fix.txt", "one")), 0, self.err.getvalue())
        pr = self.reply("pr.json")["data"]["pr"]
        self.sessions.end(self.workspace)
        runs, comments = list(self.c.gate_ops.runs), list(self.comments(pr))
        # A continuation that changes nothing hands the same head off again.
        self.start()
        self.assertEqual(self.handoff("true"), 0, self.err.getvalue())
        self.assertEqual(self.status(), "needs-human-review\n")
        self.assertEqual(self.c.gate_ops.runs, runs)
        self.assertEqual(self.comments(pr), comments)
        self.assertEqual(len(comments), 1)

    def test_verification_that_cannot_complete_is_bounded_and_blocks(self):
        self.c.gate_ops.candidate = lambda rev: dict(matrix("candidate", rev), required_platforms=["linux"],
                                                     verdict="incomplete", platforms={
            "linux": matrix("candidate", rev)["platforms"]["linux"],
            "macos": {"status": "unavailable", "reason": [{"code": "baseline_missing"}]}})
        self.start()
        self.assertEqual(self.handoff(self.commit("fix.txt", "one")), 0, self.err.getvalue())
        self.assertIsNone(self.status())
        self.assertEqual(self.attempt()["verification"]["verdict"], "incomplete")
        # Asking again for the same head cannot help: the environment is the cause.
        self.assertEqual(self.handoff("true"), 0, self.err.getvalue())
        self.assertEqual(self.status(), "blocked\n")
        blocker = (self.workspace / ".sortie" / "blocker.md").read_text()
        self.assertIn("platform_missing", blocker)
        self.assertIn("sessions/42/verifications/", blocker)
        self.assertEqual(len([r for r in self.c.gate_ops.runs if r[0] == "candidate"]), 2)
        self.assert_accounted(self.sessions.end(self.workspace)["data"], "blocked")

    def test_verification_that_raises_is_recorded_not_lost(self):
        def broken(*args):
            raise worker.Refused("source_invalid", "the revision is not a commit in this repository")
        self.c.gate = broken
        self.start()
        self.assertEqual(self.handoff(self.commit("fix.txt", "one")), 0, self.err.getvalue())
        self.assertIsNone(self.status())
        record = self.sessions._verifications(ISSUE)[-1]
        self.assertEqual(record["verdict"], "incomplete")
        self.assertEqual([f["code"] for f in record["findings"]], ["gate_failed"])
        self.assert_accounted(self.sessions.end(self.workspace)["data"], "unverified")

    def test_the_evidence_marker_is_reserved_for_the_controller(self):
        self.start()
        (self.tmp / "forged.md").write_text(f"<!-- {gate.MARKER} {{\"verdict\": \"verified\"}} -->\n")
        code = self.turn(f"""
            {self.commit("fix.txt", "one")}
            lab push > /dev/null
            ! lab pr create --title t --body-file {self.tmp}/forged.md > $OUT/create.json
            lab pr create --title t --body-file {self.tmp}/body.md > /dev/null
            ! lab pr comment --body-file {self.tmp}/forged.md > $OUT/comment.json
        """)
        self.assertEqual(code, 0, self.err.getvalue())
        for name in ("create", "comment"):
            self.assertEqual([f["code"] for f in self.reply(f"{name}.json")["findings"]], ["evidence_marker_reserved"])
        self.assertEqual(self.gh()["prs"][0].get("comments", []), [])


if __name__ == "__main__":
    unittest.main()
