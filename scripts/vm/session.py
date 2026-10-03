"""Issue sessions: one issue agent, its assigned worker, its turns and its exits.

See docs/design/agent-lab.md §Agent sessions. The dispatcher's hooks open an
attempt (`start`) before the agent runs and close it (`end`) afterwards; every
agent turn is `agent`, which runs the agent's own command inside the attempt's
Linux worker over SSH. Nothing here is executed from the issue's branch: the
controller and guard policy come from the trusted revision this file was
loaded from.

- **Source.** The issue's host workspace is the durable checkpoint, outside any
  disposable disk. An attempt transfers its branch head and uncommitted
  changes into a fresh worker; after every turn, and whenever the agent asks,
  the worker's commits and changes come back to the workspace. The workspace
  is changed only when it still holds what was last transferred; otherwise
  the worker's copy is kept aside as a conflict.
- **Guard.** Changes that touch a path the session's profile forbids
  (`sortie/guard-policy.json`) are never adopted by the workspace and never
  pushed; they are kept aside and the attempt ends `blocked`.
- **Broker.** During a turn the agent reaches the controller only through a
  socket forwarded into its guest. Each request is served for the attempt's
  own run: a request naming another run, the host, or an operation outside
  the session's set is refused. Git pushes and pull-request operations run on
  the host from the workspace, so the guest never holds GitHub credentials.
  The agent's model credentials reach it per turn in a tmpfs file it removes
  before starting.
- **Handoff.** An agent's request for review is a claim, not evidence. When
  its turn ends, the attempt's worker is released and the evidence gate
  (gate.py) verifies the pushed head and its merge base with main on Linux and
  macOS in the lab's own workers. Only a head the gate accepts is handed to
  review, with its public evidence posted on the PR; otherwise the result is
  recorded for the agent's next turn, in a fresh worker. Verification that
  cannot complete for the same head twice blocks the attempt with its cause.
- **Exits.** Every attempt ends with an outcome, its worker destroyed (or
  retained with what it could not collect) and its public evidence exported.
  An attempt nobody closed is closed as `interrupted` by the next `start`.
"""
from contextlib import contextmanager
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import signal
import socketserver
import subprocess
import sys
import tempfile
import threading
import time

import contracts
import evidence
import gate
import guest
import parallels
import scenario
import template
import terminal_ops
import worker

SCHEMA = "busybee.lab.session/v1"
BRANCH_PREFIX = "sortie-lab/"
# How an attempt ended. Only `success` handed a pushed head to review;
# `unverified` asked for review of a head the evidence gate did not accept.
OUTCOMES = ("success", "blocked", "unverified", "no_handoff", "timeout", "cancelled", "adapter_failure",
            "interrupted")
# How often verification of one head may end incomplete before the attempt is
# blocked on its environment rather than asked to try again.
VERIFY_LIMIT = 2
# The agent's own conversation state, kept across worker replacement so a
# resumed turn finds its session (relative to the guest account's home).
AGENT_STATE = (".claude/projects", ".codex/sessions")
# Model credentials an agent runner needs; passed per turn, never stored.
CREDENTIALS = {"claude": ("CLAUDE_CODE_OAUTH_TOKEN", "ANTHROPIC_API_KEY", "ANTHROPIC_AUTH_TOKEN"),
               "codex": ("OPENAI_API_KEY", "CODEX_API_KEY")}
# The development shell each runner's command runs in; others use the default one.
AGENT_SHELL = {"claude": ".#worker-agent", "codex": ".#worker-agent"}
# What a runner needs to be told about the guest it runs in. The agent is root
# there, and Claude Code skips its permission prompts as root only when told
# it runs in a sandbox, which a disposable worker is.
RUNNER_ENV = {"claude": {"IS_SANDBOX": "1"}}
PROTOCOL_LIMIT = 64 * 1024 * 1024
MARGIN_S = worker.KILL_GRACE_S + 30
LAB_CLIENT = Path(__file__).resolve().parent / "lab_client.py"
TURN_RELAY = Path(__file__).resolve().parent / "turn_relay.py"
SORTIE_FILES = ("status", "blocker.md")
# A path a test overlay may name: relative, inside the checkout.
OVERLAY_PATH = re.compile(r"^(?!/)(?!.*(?:^|/)\.\.(?:/|$)).+$")


class SessionError(Exception):
    """A session operation that could not proceed; carries its result."""

    def __init__(self, result):
        super().__init__(result["summary"])
        self.result = result


def _now():
    return datetime.now(timezone.utc).strftime(contracts.TIMESTAMP)


def _sha256(data):
    return hashlib.sha256(data).hexdigest()


def _write(path, value):
    data = value if isinstance(value, bytes) else (json.dumps(value, indent=2) + "\n").encode()
    evidence.durable(path.with_name(path.name + ".tmp"), data)
    os.replace(path.with_name(path.name + ".tmp"), path)


def _load(path):
    return json.loads(path.read_text()) if path.is_file() else None


def _git(workspace, *args, stdin=None, env=None, check=True):
    done = subprocess.run(["git", *args], cwd=workspace, input=stdin, capture_output=True,
                          env={**os.environ, **(env or {})}, timeout=300)
    if check and done.returncode != 0:
        raise RuntimeError(f"git {args[0]} failed: {done.stderr.decode(errors='replace').strip()[-300:]}")
    return done


def workspace_diff(workspace):
    """Uncommitted and untracked changes (ignored and excluded files aside), as a
    binary diff, without touching the workspace's own index."""
    index = Path(_git(workspace, "rev-parse", "--git-path", "index").stdout.decode().strip())
    index = index if index.is_absolute() else Path(workspace) / index
    with tempfile.TemporaryDirectory() as tmp:
        temp = Path(tmp) / "index"
        if index.is_file():
            shutil.copyfile(index, temp)
        env = {"GIT_INDEX_FILE": str(temp)}
        _git(workspace, "add", "-A", env=env)
        return _git(workspace, "diff", "--cached", "--binary", "HEAD", env=env).stdout


def diff_paths(workspace, diff):
    """Every path a patch touches, both sides of a rename, exactly as named."""
    if not diff:
        return set()
    fields = _git(workspace, "apply", "--numstat", "-z", "-", stdin=diff).stdout.split(b"\0")
    paths, i = set(), 0
    while i < len(fields) - 1:
        name = fields[i].split(b"\t", 2)[2]
        if name:
            paths.add(name)
            i += 1
        else:  # a rename or copy: both of its paths follow
            paths.update(fields[i + 1:i + 3])
            i += 3
    return {p.decode(errors="surrogateescape") for p in paths}


def load_policy(code_root):
    """The guard policy of the trusted revision; a missing one is an error."""
    policy = json.loads((Path(code_root) / "sortie" / "guard-policy.json").read_text())
    if policy.get("schema") != "busybee.lab.guard/v1" or not isinstance(policy.get("profiles"), dict):
        raise ValueError("sortie/guard-policy.json is not a busybee.lab.guard/v1 policy")
    return policy


def forbidden(paths, prefixes):
    return sorted(p for p in paths if any(p == x.rstrip("/") or p.startswith(x) for x in prefixes))


def strip_host_arguments(argv):
    """Drop arguments naming host files the guest cannot read: Sortie's
    generated MCP configuration declares a tool server that runs on the host."""
    out, dropped, skip = [], [], False
    for arg in argv:
        if skip:
            dropped.append(arg)
            skip = False
        elif arg == "--mcp-config":
            dropped.append(arg)
            skip = True
        elif arg.startswith("--mcp-config="):
            dropped.append(arg)
        else:
            out.append(arg)
    return out, dropped


class Controller:
    """What a session needs from the lab controller: the worker operations,
    for one run at a time. Tests substitute a local double."""

    guest_run = "/run/busybee-lab"

    def __init__(self, workers):
        self.w = workers
        self.state = workers.state
        self.config = workers.config

    def create(self, revision, patch, source_repo, branch):
        self.w.source_repo, self.w.transfer_refs = Path(source_repo), (f"refs/heads/{branch}",)
        return self.w.create("linux", revision, patch)

    def record(self, run_id):
        return worker._load(worker.run_dir(self.state, run_id) / "worker.json")

    def guest(self, run_id):
        _, record = self.w._owned(run_id)
        self.w._window()
        return self.w._guest(record)

    def checkout(self, run_id):
        return self.w.checkout(self.record(run_id))

    def home(self, run_id):
        # The guest account's home holds its checkout.
        return str(Path(self.checkout(run_id)).parent)

    def guest_workspace(self, workspace):
        # The same absolute path as on the host, so an agent's recorded working
        # directory (Codex sends it over its protocol) resolves in the guest.
        return str(workspace)

    def run_left(self, run_id):
        return worker.run_deadline(self.record(run_id)) - time.time()

    def reset(self, run_id, source_repo, branch):
        # The recorded revision may exist only in the session's workspace.
        self.w.source_repo, self.w.transfer_refs = Path(source_repo), (f"refs/heads/{branch}",)
        return self.w.reset(run_id)

    def destroy(self, run_id):
        return self.w.destroy(run_id)

    def export(self, run_id):
        return self.w.export(run_id)

    def gate(self, issue, revision, base, overlay, source_repo, branch, profile):
        """The evidence gate on the lab's own workers; the revisions come from
        the session's workspace."""
        self.w.source_repo, self.w.transfer_refs = Path(source_repo), (f"refs/heads/{branch}",)
        return gate.evaluate(gate.WorkerOps(self.w), issue, revision, base, overlay, profile=profile)

    def collected(self, run_id):
        latest = worker._latest_dir(worker.run_dir(self.state, run_id) / "collect")
        return latest / "collected.json" if latest and (latest / "collected.json").is_file() else None

    def operation(self, op, run_id, **args):
        """One of the worker operations a session's agent may request."""
        w = self.w
        if op in ("exec", "terminal-open"):
            env = args.get("env") or {}
            if not isinstance(env, dict):
                raise worker.Refused("env_invalid", "env must map names to values")
            timeout = args.get("timeout") or self.config["deadlines"]["command"]
            if op == "exec":
                return w.exec(run_id, args["argv"], args.get("cwd") or self.checkout(run_id), env, timeout,
                              bool(args.get("detach")))
            return terminal_ops.open_terminal(w, run_id, args["argv"], args.get("cols", 120), args.get("rows", 40),
                                              args.get("cwd") or self.checkout(run_id), env, timeout)
        calls = {
            "inspect": lambda: w.inspect(run_id),
            "signal": lambda: w.signal(run_id, args["signal"], str(args["pid"])),
            "status": lambda: w.status(run_id, args.get("exec")),
            "wait": lambda: w.wait(run_id, args["exec"], args.get("timeout")),
            "read": lambda: w.read(run_id, args["exec"], args["stream"], args.get("offset", 0),
                                   args.get("limit", worker.READ_LIMIT)),
            "terminal-send": lambda: terminal_ops.send(w, run_id, args["handle"], args.get("text"),
                                                       args.get("keys") or (), args.get("bytes")),
            "terminal-resize": lambda: terminal_ops.resize(w, run_id, args["handle"], args["cols"], args["rows"]),
            "terminal-capture": lambda: terminal_ops.capture(w, run_id, args["handle"], args.get("expect"),
                                                             args.get("timeout", 10)),
            "console-capture": lambda: w.console_capture(run_id),
            "scenario": lambda: scenario.run(w, run_id, args["scenario"], args["mode"],
                                             args.get("bin_dir", "build/debug")),
            "collect": lambda: w.collect(run_id),
        }
        return calls[op]()


class Sessions:
    """Issue sessions under `<state>/sessions/<issue>/`. `code_root` is the
    trusted revision the controller runs from; `shell(runner)` is the argv
    prefix a runner's command runs in."""

    def __init__(self, controller, code_root, env=None, shell=None, gh="gh", out=None, err=None):
        self.c = controller
        self.state = Path(controller.state)
        self.dir = self.state / "sessions"
        self.code = Path(code_root)
        self.policy = load_policy(self.code)
        self.env = dict(os.environ if env is None else env)
        self.shell = shell or (lambda runner: ["nix", "develop", AGENT_SHELL.get(runner, "."), "-c"])
        self.gh = gh
        self.out = out or sys.stdout
        self.err = err or sys.stderr
        self.cancel = threading.Event()

    # Records

    def _sdir(self, issue):
        return self.dir / str(issue)

    def _adir(self, issue, attempt):
        return self._sdir(issue) / "attempts" / attempt

    @contextmanager
    def _locked(self, issue):
        with worker.locked(self._sdir(issue) / "session.lock"):
            yield

    def load(self, issue):
        return _load(self._sdir(issue) / "session.json")

    def attempt(self, session, name=None):
        name = name or session["attempt"]
        return _load(self._adir(session["issue"], name) / "attempt.json") if name else None

    def _save(self, session, attempt=None):
        session["updated_at"] = _now()
        _write(self._sdir(session["issue"]) / "session.json", session)
        if attempt is not None:
            _write(self._adir(session["issue"], attempt["attempt"]) / "attempt.json", attempt)

    def by_workspace(self, workspace):
        here = Path(workspace).resolve()
        for path in sorted(self.dir.glob("*/session.json")) if self.dir.is_dir() else ():
            if Path(json.loads(path.read_text())["workspace"]) == here:
                return int(path.parent.name)
        raise worker.Refused("session_missing", "no session is bound to this workspace; the dispatcher's "
                             "before_run hook starts one")

    def note(self, message):
        print(f"busybee-lab: {message}", file=self.err, flush=True)

    # Starting an attempt

    def start(self, issue, workspace, profile):
        op = "session start"
        if not (isinstance(issue, int) and issue > 0):
            raise worker.Refused("issue_invalid", f"{issue!r} is not an issue number")
        if profile not in self.policy["profiles"]:
            raise worker.Refused("profile_invalid", f"{profile!r} is not a profile of the guard policy")
        workspace = Path(workspace).resolve()
        if not (workspace / ".git").exists():
            raise worker.Refused("workspace_invalid", "the workspace is not a git checkout")
        branch = _git(workspace, "branch", "--show-current").stdout.decode().strip()
        if branch != f"{BRANCH_PREFIX}{issue}":
            raise worker.Refused("branch_invalid", f"the workspace is on {branch or 'a detached HEAD'}, "
                                 f"not {BRANCH_PREFIX}{issue}")
        self._sdir(issue).mkdir(parents=True, exist_ok=True)
        with self._locked(issue):
            session = self.load(issue) or {"schema": SCHEMA, "issue": issue, "branch": branch, "pr": None,
                                           "attempt": None, "attempts": 0}
            ended = None
            if session["attempt"]:
                ended = self._end(session, "interrupted", "the previous runner ended without closing its attempt")
            # Rebinding happens only between attempts, so no worker's source is lost.
            session.update(workspace=str(workspace), profile=profile)
            attempt = self._open(session)
            data = {"issue": issue, "branch": branch, "profile": profile, "attempt": attempt["attempt"],
                    "pr": session["pr"], "closed_previous": ended}
            try:
                self._allocate(session, attempt)
            except SessionError as err:
                return contracts.result(op, err.result["status"], err.result["summary"], err.result["findings"],
                                        {**data, **err.result["data"]})
        return contracts.result(op, "success", f"attempt {attempt['attempt']} of #{issue} runs in worker "
                                f"{attempt['run_id']}", data={**data, "run_id": attempt["run_id"]})

    def _open(self, session):
        session["attempts"] += 1
        name = f"{session['attempts']:04d}"
        attempt = {"attempt": name, "run_id": None, "started_at": _now(), "profile": session["profile"],
                   "source": None, "synced": None, "turns": [], "resets": [], "reset_requested": False,
                   "handoff_requested": None, "released": None, "verification": None,
                   "handoff": None, "blocked": None, "violation": None, "conflict": None, "end": None}
        session["attempt"] = name
        self._save(session, attempt)
        return attempt

    def _allocate(self, session, attempt):
        """A fresh worker holding the workspace's branch, or a closed attempt."""
        workspace = Path(session["workspace"])
        revision = _git(workspace, "rev-parse", "HEAD").stdout.decode().strip()
        diff = workspace_diff(workspace)
        adir = self._adir(session["issue"], attempt["attempt"])
        patch = None
        if diff:
            patch = adir / "source.patch"
            _write(patch, diff)
        created = self.c.create(revision, patch, workspace, session["branch"])
        attempt["run_id"] = created["data"].get("run_id")
        attempt["source"] = {"revision": revision, "patch_sha256": _sha256(diff) if diff else None}
        self._save(session, attempt)
        if created["status"] != "success":
            reason = "; ".join(f["message"] for f in created["findings"]) or created["summary"]
            ended = self._end(session, "adapter_failure", f"no worker: {reason}")
            raise SessionError(contracts.result("session start", created["status"], "the attempt has no worker",
                                                created["findings"], {"end": ended}))
        try:
            self._load_workspace(session, attempt)
        except (guest.GuestError, parallels.ParallelsError, worker.Refused, template.DeadlineExceeded,
                RuntimeError) as err:
            ended = self._end(session, "adapter_failure", f"the worker did not take the workspace: {err}")
            raise SessionError(contracts.result("session start", "environment_failure", "the attempt has no worker",
                                                [contracts.finding("workspace_transfer_failed", str(err))],
                                                {"end": ended}))

    def _load_workspace(self, session, attempt):
        """Put the workspace's branch, its origin/main, its uncommitted changes
        and the agent's saved conversation state into the worker."""
        workspace, issue = Path(session["workspace"]), session["issue"]
        run_id, branch = attempt["run_id"], session["branch"]
        g, co, q = self.c.guest(run_id), self.c.checkout(run_id), shlex.quote
        refs = [f"refs/heads/{branch}"]
        if _git(workspace, "rev-parse", "-q", "--verify", "refs/remotes/origin/main", check=False).returncode == 0:
            refs.append("refs/remotes/origin/main")
        bundle = _git(workspace, "bundle", "create", "-", *refs).stdout
        head = _git(workspace, "rev-parse", "HEAD").stdout.decode().strip()
        diff = workspace_diff(workspace)
        timeout = self.c.config["deadlines"]["command"]
        g.run(f"t=$(mktemp) && cat > $t && cd {q(co)} && git bundle unbundle $t > $t.refs && "
              f"while read -r id ref; do git update-ref \"$ref\" \"$id\" || exit 1; done < $t.refs; s=$?; "
              f"rm -f $t $t.refs; [ $s = 0 ] && "
              f"git checkout -q -f {q(branch)} && git clean -fdq && "
              "{ grep -qxF /.sortie/ .git/info/exclude || printf '/.sortie/\\n' >> .git/info/exclude; } && "
              f"rm -rf .sortie && mkdir -p .sortie", timeout, stdin=bundle)
        if diff:
            g.run(f"cd {q(co)} && git apply --binary", timeout, stdin=diff)
        found = g.run(f"git -C {q(co)} rev-parse HEAD", timeout)[1].strip()
        if found != head:
            raise RuntimeError(f"the worker's branch is at {found}, not {head}")
        link = self.c.guest_workspace(workspace)
        if link != co:
            g.run(f"mkdir -p {q(str(Path(link).parent))} && ln -sfn {q(co)} {q(link)}", timeout)
        saved = self._sdir(issue) / "agent-state.tar"
        if saved.is_file():
            g.run(f"tar -xf - -C {q(self.c.home(run_id))}", timeout, stdin=saved.read_bytes())
        attempt["synced"] = {"head": head, "diff_sha256": _sha256(diff)}
        self._save(session, attempt)

    # Bringing the worker's work back

    def _guest_outputs(self, g, run_id, since):
        co, q = self.c.checkout(run_id), shlex.quote
        timeout = self.c.config["deadlines"]["command"]
        head = g.run(f"git -C {q(co)} rev-parse HEAD", timeout)[1].strip()
        bundle = None
        if head != since and g.run(f"git -C {q(co)} merge-base --is-ancestor HEAD {q(since)}", timeout,
                                   check=False)[0] != 0:
            bundle = g.run(f"git -C {q(co)} bundle create - HEAD ^{q(since)}", timeout, raw=True)[1]
        diff = g.run(f"cd {q(co)} && i=$(mktemp) && cp .git/index $i && GIT_INDEX_FILE=$i git add -A && "
                     f"GIT_INDEX_FILE=$i git diff --cached --binary HEAD; s=$?; rm -f $i; exit $s",
                     timeout, raw=True)[1]
        files = {}
        for name in SORTIE_FILES:
            status, out, _ = g.run(f"cat {q(co)}/.sortie/{name}", timeout, check=False, raw=True)
            if status == 0:
                files[name] = out
        home = q(self.c.home(run_id))
        names = " ".join(q(p) for p in AGENT_STATE)
        state = g.run(f"cd {home} && set -- && for p in {names}; do [ -e \"$p\" ] && set -- \"$@\" \"$p\"; done; "
                      f"[ $# -eq 0 ] || tar -cf - \"$@\"", timeout, raw=True)[1]
        return head, bundle, diff, files, state

    def sync(self, session, attempt):
        """Bring the worker's branch and changes into the workspace when the
        workspace still holds what it last gave the worker and the changes
        pass the guard. Returns what happened."""
        issue, workspace = session["issue"], Path(session["workspace"])
        run_id, synced = attempt["run_id"], attempt["synced"]
        g = self.c.guest(run_id)
        head, bundle, diff, files, state = self._guest_outputs(g, run_id, synced["head"])
        if state:
            _write(self._sdir(issue) / "agent-state.tar", state)
        here = _git(workspace, "rev-parse", "HEAD").stdout.decode().strip()
        on = _git(workspace, "branch", "--show-current").stdout.decode().strip()
        if on != session["branch"] or here != synced["head"] or \
                _sha256(workspace_diff(workspace)) != synced["diff_sha256"]:
            kept = self._keep(issue, "conflicts", head, bundle, diff)
            attempt["conflict"] = {"at": _now(), "kept": kept}
            self._save(session, attempt)
            return {"status": "conflict", "head": head, "kept": kept,
                    "message": "the workspace changed since it was transferred; the worker's work was kept aside"}
        if bundle:
            with tempfile.NamedTemporaryFile(suffix=".bundle") as f:
                f.write(bundle)
                f.flush()
                _git(workspace, "fetch", "-q", f.name, "HEAD")
        if _git(workspace, "cat-file", "-e", f"{head}^{{commit}}", check=False).returncode != 0:
            raise RuntimeError(f"the worker's head {head} did not reach the workspace")
        bad = forbidden(self._changed(workspace, head) | diff_paths(workspace, diff),
                        self.policy["profiles"][session["profile"]]["forbidden_paths"])
        if bad:
            kept = self._keep(issue, "violations", head, bundle, diff)
            attempt["violation"] = {"at": _now(), "paths": bad, "kept": kept}
            self._save(session, attempt)
            return {"status": "violation", "head": head, "paths": bad, "kept": kept,
                    "message": f"a {session['profile']} session may not change {', '.join(bad)}; "
                               "the workspace was not changed"}
        attempt["violation"] = None
        _git(workspace, "reset", "-q", "--hard", head)
        _git(workspace, "clean", "-fdq")
        if diff:
            _git(workspace, "apply", "--binary", stdin=diff)
        if files.get("status", b"").strip() == b"blocked":
            sortie = workspace / ".sortie"
            sortie.mkdir(exist_ok=True)
            reason = files.get("blocker.md", b"no reason given\n")
            _write(sortie / "blocker.md", reason)
            _write(sortie / "status", b"blocked\n")
            attempt["blocked"] = reason.decode(errors="replace").strip().splitlines()[0][:300] if reason.strip() \
                else "no reason given"
        changed = head != synced["head"] or _sha256(diff) != synced["diff_sha256"]
        attempt["synced"] = {"head": head, "diff_sha256": _sha256(workspace_diff(workspace))}
        self._save(session, attempt)
        return {"status": "synced" if changed else "unchanged", "head": head}

    def _changed(self, workspace, head):
        """Every path the branch changes since it left origin/main: both sides
        of a rename, unquoted. Without origin/main nothing can be checked."""
        main = _git(workspace, "rev-parse", "-q", "--verify", "refs/remotes/origin/main", check=False)
        if main.returncode != 0:
            raise RuntimeError("the workspace has no origin/main to check the branch's changes against")
        base = _git(workspace, "merge-base", main.stdout.decode().strip(), head).stdout.decode().strip()
        names = _git(workspace, "-c", "core.quotePath=false", "diff", "--name-only", "--no-renames", "-z",
                     base, head).stdout
        return {n.decode(errors="surrogateescape") for n in names.split(b"\0") if n}

    def _keep(self, issue, kind, head, bundle, diff):
        kdir = worker._next_dir(self._sdir(issue) / kind)
        _write(kdir / "source.json", {"head": head, "at": _now()})
        if bundle:
            _write(kdir / "commits.bundle", bundle)
        _write(kdir / "worktree.diff", diff)
        return str(kdir.relative_to(self.state))

    # One agent turn

    def agent(self, workspace, argv, timeout=None):
        """Run one turn of the agent's command in the attempt's worker; returns
        its exit status. stdin and stdout belong to the agent's protocol."""
        if not argv:
            raise worker.Refused("argv_empty", "session agent needs the agent's command")
        issue = self.by_workspace(workspace)
        with self._locked(issue):
            session = self.load(issue)
            attempt = self.attempt(session)
            if attempt is None:
                raise worker.Refused("attempt_missing", f"#{issue} has no open attempt; the before_run hook "
                                     "starts one")
            record = self.c.record(attempt["run_id"])
            if attempt.get("released"):
                # Released for verification: the attempt ends as the gate decided.
                self._end(session)
                attempt = self._open(session)
                try:
                    self._allocate(session, attempt)
                except SessionError as err:
                    self.note(err.result["summary"])
                    return 1
            elif record is None or record["status"] != "ready":
                state = record["status"] if record else "missing"
                self.note(f"worker {attempt['run_id']} is {state}; replacing it with a fresh one from the workspace")
                self._end(session, "timeout" if state == "expired" else "adapter_failure", f"the worker was {state}")
                attempt = self._open(session)
                try:
                    self._allocate(session, attempt)
                except SessionError as err:
                    self.note(err.result["summary"])
                    return 1
            return self._turn(session, attempt, argv, timeout)

    def _turn(self, session, attempt, argv, timeout):
        issue, run_id = session["issue"], attempt["run_id"]
        runner = Path(argv[0]).name
        argv, dropped = strip_host_arguments(argv)
        if dropped:
            self.note(f"dropped {' '.join(dropped[:1])}: the orchestrator's agent tools run on the host, "
                      "not in the worker")
        name = f"{len(attempt['turns']) + 1:04d}"
        turn = {"turn": name, "runner": runner, "started_at": _now(), "finished": False, "kind": None,
                "exit_code": None, "sync": None, "dropped": dropped}
        attempt["turns"].append(turn)
        self._save(session, attempt)
        tdir = self._adir(issue, attempt["attempt"]) / "turns"
        tdir.mkdir(parents=True, exist_ok=True)

        def finish(kind, code, reason=None):
            turn.update(finished=True, kind=kind, exit_code=code, finished_at=_now(), reason=reason)
            self._save(session, attempt)
            return code

        needed = CREDENTIALS.get(runner, ())
        credentials = {k: self.env[k] for k in needed if self.env.get(k)}
        if needed and not credentials:
            self.note(f"{runner} needs one of {', '.join(needed)} in the dispatcher's environment; the worker "
                      "holds no login")
            return finish("adapter_failure", 1, "credentials_missing")
        bound = int(min(timeout or float("inf"), self.c.run_left(run_id) - self.c.config["deadlines"]["cleanup"]))
        if bound < 1:
            return finish("timeout", 124, "the worker's run deadline leaves no time for a turn")
        gdir = f"{self.c.guest_run}/{run_id}-{name}"
        q = shlex.quote
        try:
            g = self.c.guest(run_id)
            g.run(f"umask 077 && mkdir -p {q(gdir)}/bin && cat > {q(gdir)}/env", 60,
                  stdin="".join(f"{k}={q(v)}\n" for k, v in credentials.items()).encode())
            g.run(f"cat > {q(gdir)}/bin/lab && chmod 0755 {q(gdir)}/bin/lab", 60, stdin=LAB_CLIENT.read_bytes())
            g.run(f"cat > {q(gdir)}/relay.py", 60, stdin=TURN_RELAY.read_bytes())
        except (guest.GuestError, parallels.ParallelsError, worker.Refused, template.DeadlineExceeded) as err:
            self._forget(gdir, attempt)
            return finish("adapter_failure", 1, f"the worker did not take the turn: {err}")
        exports = {"BUSYBEE_SORTIE_WORKER": "1", "BUSYBEE_LAB_SOCKET": f"{gdir}/broker.sock",
                   "BUSYBEE_LAB_ISSUE": str(issue), "BUSYBEE_LAB_RUN": run_id, **RUNNER_ENV.get(runner, {})}
        if session["pr"]:
            exports["BUSYBEE_LAB_PR"] = str(session["pr"])
        command = (f"d={q(gdir)}; set -a; . \"$d/env\"; set +a; rm -f \"$d/env\"; "
                   f"export PATH=\"$d/bin:$PATH\" {' '.join(f'{k}={q(v)}' for k, v in exports.items())}; "
                   f"cd {q(self.c.guest_workspace(session['workspace']))} || exit 125; "
                   # The relay ends the turn with the runner, whatever daemons it left.
                   + " ".join(q(w) for w in ["exec", *self.shell(runner), "python3", f"{gdir}/relay.py", "sh", "-c",
                                             'echo $$ > "$0"; exec "$@"', f"{gdir}/pid", "timeout", "-k",
                                             str(worker.KILL_GRACE_S), "--", str(bound), *argv]))
        broker = Broker(self, session, attempt)
        log = open(tdir / f"{name}.stderr", "wb")
        previous = {}
        try:
            host_socket = broker.start()
            if threading.current_thread() is threading.main_thread():
                for sig in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
                    previous[sig] = signal.signal(sig, lambda *_: self.cancel.set())
            started = time.monotonic()
            try:
                proc = g.session(command, (f"{gdir}/broker.sock", host_socket), stdin=None, stdout=self.out,
                                 stderr=subprocess.PIPE)
            except OSError as err:
                self._forget(gdir, attempt)
                return finish("adapter_failure", 1, f"the turn could not start: {err}")
            tee = threading.Thread(target=self._tee, args=(proc.stderr, log), daemon=True)
            tee.start()
            kind, reason = None, None
            while proc.poll() is None:
                if self.cancel.is_set():
                    kind, reason = "cancelled", "the runner was asked to stop"
                elif time.monotonic() - started > bound + MARGIN_S:
                    kind, reason = "timeout", f"the turn outlived its {bound}s bound and was killed by the host"
                if kind:
                    self._kill(g, gdir, proc)
                    break
                time.sleep(0.2)
            code = proc.wait()
            tee.join(timeout=5)
            proc.stderr.close()
            elapsed = time.monotonic() - started
            if kind is None:
                ran = self._runner_status(run_id, gdir)
                if ran is None:
                    kind, reason = "adapter_failure", (f"the runner never reported an exit (ssh or its shell ended "
                                                       f"the turn with {code})" if self._answers(run_id)
                                                       else "the worker stopped answering")
                elif ran == 124 and elapsed >= bound - 1:
                    kind, reason = "timeout", f"the turn reached its {bound}s bound"
                elif ran in (126, 127):
                    kind, reason = "adapter_failure", f"the agent's command did not start in the worker (exit {ran})"
                else:
                    kind = "exited"
            result = {"timeout": 124, "cancelled": 143}.get(kind, code if code else (1 if kind != "exited" else 0))
        finally:
            broker.stop()
            log.close()
            for sig, handler in previous.items():
                signal.signal(sig, handler)
        turn["sync"] = self._after_turn(session, attempt, gdir)
        if attempt["handoff_requested"]:
            if attempt["reset_requested"]:  # the worker is released instead
                attempt["reset_requested"] = False
                turn["reset"] = {"at": _now(), "status": "skipped", "summary": "the turn asked for a handoff"}
            turn["handoff"] = self._gated_handoff(session, attempt, turn["sync"])
        if attempt["reset_requested"]:
            if turn["sync"]["status"] in ("synced", "unchanged"):
                turn["reset"] = self._reset(session, attempt)
            else:  # the worker still holds work the workspace did not take
                attempt["reset_requested"] = False
                turn["reset"] = {"at": _now(), "status": "skipped",
                                 "summary": f"the checkpoint before it was {turn['sync']['status']}"}
                self.note(f"reset skipped: {turn['reset']['summary']}")
        return finish(kind, result, reason)

    def _forget(self, gdir, attempt):
        """Remove a turn's guest files, credentials included, when it ends early."""
        try:
            self.c.guest(attempt["run_id"]).run(f"rm -rf {shlex.quote(gdir)}", 60, check=False)
        except (guest.GuestError, parallels.ParallelsError, worker.Refused, template.DeadlineExceeded):
            pass  # the worker is gone or unreachable, and its /run with it on destroy

    def _tee(self, stream, log):
        for chunk in iter(lambda: stream.read1(65536), b""):
            log.write(chunk)
            log.flush()
            self.err.buffer.write(chunk) if hasattr(self.err, "buffer") else self.err.write(chunk.decode(errors="replace"))
            self.err.flush()

    def _kill(self, g, gdir, proc):
        try:
            g.run(worker.kill_command(f"{gdir}/pid"), 30, check=False)
        except guest.GuestError:
            pass  # the host side ends below whether or not the guest answered
        proc.terminate()
        try:
            proc.wait(timeout=worker.KILL_GRACE_S)
        except subprocess.TimeoutExpired:
            proc.kill()

    def _runner_status(self, run_id, gdir):
        """The runner's exit status as the turn's relay recorded it, or None."""
        try:
            status, out, _ = self.c.guest(run_id).run(f"cat {shlex.quote(gdir)}/status", 30, check=False)
        except (guest.GuestError, parallels.ParallelsError, worker.Refused, template.DeadlineExceeded):
            return None
        return int(out.strip()) if status == 0 and out.strip().lstrip("-").isdigit() else None

    def _answers(self, run_id):
        try:
            self.c.guest(run_id).run("true", 30)
            return True
        except (guest.GuestError, parallels.ParallelsError, worker.Refused, template.DeadlineExceeded):
            return False

    def _after_turn(self, session, attempt, gdir):
        """Checkpoint the worker's work into the workspace; the turn's guest
        files (credentials already gone) are removed."""
        try:
            g = self.c.guest(attempt["run_id"])
            g.run(f"rm -rf {shlex.quote(gdir)}", 60, check=False)
            outcome = self.sync(session, attempt)
        except (guest.GuestError, parallels.ParallelsError, worker.Refused, template.DeadlineExceeded,
                RuntimeError) as err:
            outcome = {"status": "failed", "message": str(err)}
        if outcome["status"] not in ("synced", "unchanged"):
            self.note(f"checkpoint {outcome['status']}: {outcome.get('message', '')}")
        return outcome

    def _reset(self, session, attempt):
        """The reset an agent asked for, once its turn ended: the worker returns
        to its baseline and takes the workspace's branch again."""
        attempt["reset_requested"] = False
        try:
            result = self.c.reset(attempt["run_id"], session["workspace"], session["branch"])
        except (worker.Refused, guest.GuestError, parallels.ParallelsError, template.DeadlineExceeded) as err:
            result = {"status": "environment_failure", "summary": f"the reset was not done: {err}"}
        entry = {"at": _now(), "status": result["status"], "summary": result["summary"]}
        if result["status"] == "success":
            try:
                self._load_workspace(session, attempt)
            except (guest.GuestError, parallels.ParallelsError, worker.Refused, template.DeadlineExceeded,
                    RuntimeError) as err:
                entry.update(status="environment_failure", summary=f"the workspace did not return: {err}")
        attempt["resets"].append(entry)
        self._save(session, attempt)
        if entry["status"] != "success":
            self.note(f"reset of worker {attempt['run_id']}: {entry['summary']}")
        return entry

    # Handing a head to review, on evidence

    def _gated_handoff(self, session, attempt, sync):
        """The handoff an agent asked for, once its turn ended: release the
        worker, verify the head against its base, and hand it to review only
        when the evidence gate accepts it. Returns what happened."""
        request, attempt["handoff_requested"] = attempt["handoff_requested"], None
        workspace, issue = Path(session["workspace"]), session["issue"]
        head = _git(workspace, "rev-parse", "HEAD").stdout.decode().strip()
        entry = {"at": _now(), "requested": request["sha"], "status": None, "summary": None}

        def done(status, summary):
            entry.update(status=status, summary=summary)
            self._save(session, attempt)
            self.note(f"handoff {status}: {summary}")
            return entry
        if sync["status"] not in ("synced", "unchanged") or head != request["sha"] or workspace_diff(workspace):
            return done("skipped", f"the branch is not the requested {request['sha'][:12]} any more (checkpoint "
                        f"{sync['status']}); push and ask again")
        attempt["released"] = self._release(attempt)
        try:
            _git(workspace, "fetch", "-q", "origin", "main")
            base = _git(workspace, "merge-base", "refs/remotes/origin/main", head).stdout.decode().strip()
            overlay = self._overlay(session, base, head, request["overlay"])
        except (RuntimeError, ValueError) as err:
            return done("environment_failure", f"no base to verify against: {err}")
        try:
            result = self.c.gate(issue, head, base, overlay, workspace, session["branch"], session["profile"])
        except (worker.Refused, guest.GuestError, parallels.ParallelsError, template.DeadlineExceeded, OSError,
                ValueError, RuntimeError) as err:  # recorded as verification that could not complete
            result = contracts.result("gate", "environment_failure", "verification did not run", [
                contracts.finding("gate_failed", f"{type(err).__name__}: {err}")], {"verdict": "incomplete"})
        record = self._record_verification(session, head, base, request, result)
        verdict = result["data"].get("verdict")
        attempt["verification"] = {"head": head, "base": base, "verdict": verdict, "record": record,
                                   "gate": result["data"].get("gate")}
        self._save(session, attempt)
        if not gate.accepted(verdict, session["profile"]):
            failed = [f for f in result["findings"] if f["severity"] == "error"]
            summary = f"verification of {head[:12]} is {verdict}: " + "; ".join(
                f"{f['code']}: {f['message']}" for f in failed[:5])
            tries = [v for v in self._verifications(issue) if v["head"] == head and v["verdict"] in
                     ("incomplete", "stale")]
            if verdict in ("incomplete", "stale") and len(tries) >= VERIFY_LIMIT:
                self._block(session, attempt, f"Verification of {head} could not complete {len(tries)} times; the "
                            f"cause is the lab environment, not the change.\n\n" + "\n".join(
                                f"- {f['code']}: {f['message']}" for f in failed) +
                            f"\n\nRetained evidence: {record} and {result['data'].get('gate')} under the lab "
                            "state directory.\n")
                return done("blocked", summary)
            return done("refused", summary)
        try:
            self._post_evidence(session, request, result)
            self._handoff(session, request["pr"])
        except RuntimeError as err:
            return done("environment_failure", f"{head[:12]} is {verdict} but the handoff failed: {err}")
        attempt["handoff"] = {"sha": head, "pr": request["pr"], "at": _now(), "verdict": verdict}
        return done("success", f"{head[:12]} is {verdict}; handed PR #{request['pr']} to review")

    def _release(self, attempt):
        """Destroy the attempt's worker so verification can use the lab's."""
        try:
            destroyed = self.c.destroy(attempt["run_id"])
            return {"at": _now(), "status": destroyed["status"], "summary": destroyed["summary"]}
        except (worker.Refused, guest.GuestError, parallels.ParallelsError, template.DeadlineExceeded) as err:
            return {"at": _now(), "status": "environment_failure", "summary": str(err)}

    def _overlay(self, session, base, head, paths):
        """The candidate's changes to `paths` since `base`, as a patch file the
        base run applies: a new regression test on old code."""
        if not paths:
            return None
        diff = _git(Path(session["workspace"]), "diff", "--binary", base, head, "--", *paths).stdout
        if not diff:
            raise ValueError(f"the overlay paths {', '.join(paths)} have no changes since {base[:12]}")
        path = self._sdir(session["issue"]) / "overlays" / f"{_sha256(diff)}.patch"
        _write(path, diff)
        return path

    def _verifications(self, issue):
        vdir = self._sdir(issue) / "verifications"
        return [json.loads(p.read_text()) for p in sorted(vdir.glob("*.json"))] if vdir.is_dir() else []

    def _record_verification(self, session, head, base, request, result):
        vdir = self._sdir(session["issue"]) / "verifications"
        path = vdir / f"{len(self._verifications(session['issue'])) + 1:04d}.json"
        _write(path, {"at": _now(), "head": head, "base": base, "pr": request["pr"], "overlay": request["overlay"],
                      "verdict": result["data"].get("verdict"), "status": result["status"],
                      "summary": result["summary"], "findings": result["findings"], "gate": result["data"]})
        return str(path.relative_to(self.state))

    def _block(self, session, attempt, reason):
        sortie = Path(session["workspace"]) / ".sortie"
        sortie.mkdir(exist_ok=True)
        _write(sortie / "blocker.md", reason.encode())
        _write(sortie / "status", b"blocked\n")
        attempt["blocked"] = reason.splitlines()[0][:300]

    def _gh(self, session, *args, stdin=None):
        done = subprocess.run([self.gh, *args], cwd=session["workspace"], input=stdin, capture_output=True, text=True,
                              timeout=120)
        if done.returncode != 0:
            raise RuntimeError(f"gh {' '.join(args[:2])} failed: {done.stderr.strip()[-300:]}")
        return done.stdout

    def _post_evidence(self, session, request, result):
        """The gate's public record on the PR, once per piece of evidence."""
        body = gate.comment(json.loads((self.state / result["data"]["public"]).read_text()))
        marker = body.splitlines()[0]
        view = json.loads(self._gh(session, "pr", "view", str(request["pr"]), "--json", "author,comments"))
        author = (view.get("author") or {}).get("login")
        if any((c.get("body") or "").splitlines()[:1] == [marker] and (c.get("author") or {}).get("login") == author
               for c in view.get("comments", [])):
            return  # already there: unchanged evidence is not posted again
        self._gh(session, "pr", "comment", str(request["pr"]), "--body-file", "-", stdin=body)

    def _reviews(self, session, *args):
        env = dict(os.environ)
        if os.sep in self.gh:  # an explicit gh, which the trusted helper must use too
            env["PATH"] = f"{Path(self.gh).parent}{os.pathsep}{env.get('PATH', '')}"
        done = subprocess.run([sys.executable, str(self.code / "sortie" / "reviews.py"), "handoff", *args],
                              cwd=session["workspace"], capture_output=True, text=True, timeout=120, env=env)
        if done.returncode != 0:
            raise RuntimeError(done.stderr.strip()[-500:] or done.stdout.strip())
        return done.stdout.strip()

    def _handoff(self, session, pr):
        repo = self._gh(session, "repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner").strip()
        return self._reviews(session, "--repo", repo, "--pr", str(pr))

    # Ending an attempt

    def end(self, workspace, outcome=None, reason=None):
        op = "session end"
        if outcome is not None and outcome not in OUTCOMES:
            raise worker.Refused("outcome_invalid", f"{outcome!r} is not one of {', '.join(OUTCOMES)}")
        issue = self.by_workspace(workspace)
        with self._locked(issue):
            session = self.load(issue)
            if not session["attempt"]:
                return contracts.result(op, "success", f"#{issue} has no open attempt", data={"issue": issue})
            ended = self._end(session, outcome, reason)
        status = "success" if ended["worker_status"] in ("destroyed", "none") and not ended["missing"] \
            else "incomplete_collection"
        return contracts.result(op, status, f"attempt {ended['attempt']} of #{issue} ended {ended['outcome']}; "
                                f"worker {ended['worker_status']}", data={"issue": issue, **ended})

    def _derive(self, session, attempt, record):
        if attempt["violation"]:
            return "blocked", f"policy_violation: {', '.join(attempt['violation']['paths'])}"
        if attempt["blocked"]:
            return "blocked", attempt["blocked"]
        head = _git(Path(session["workspace"]), "rev-parse", "HEAD").stdout.decode().strip()
        if attempt["handoff"] and attempt["handoff"]["sha"] == head:
            return "success", f"handed {head} to review"
        if attempt.get("verification"):
            v = attempt["verification"]
            return "unverified", f"verification of {v['head']} was {v['verdict']} ({v['record']})"
        if record and record["status"] == "expired":
            return "timeout", "the worker reached its run deadline"
        if not attempt["turns"]:
            return "adapter_failure", "no agent turn ran"
        last = attempt["turns"][-1]
        if not last["finished"]:
            return "cancelled", f"the runner ended during turn {last['turn']}"
        if last["kind"] != "exited":
            return last["kind"], last.get("reason")
        return "no_handoff", "the agent stopped without a handoff or a blocker"

    def _end(self, session, outcome=None, reason=None):
        """Close the open attempt: checkpoint, decide its outcome, collect and
        destroy its worker, export its evidence. Returns the attempt's end."""
        attempt = self.attempt(session)
        run_id = attempt["run_id"]
        record = self.c.record(run_id) if run_id else None
        sync = None
        if record and record["status"] == "ready" and attempt["synced"]:
            try:
                sync = self.sync(session, attempt)
            except (guest.GuestError, parallels.ParallelsError, worker.Refused, template.DeadlineExceeded,
                    RuntimeError) as err:
                sync = {"status": "failed", "message": str(err)}
        derived, why = self._derive(session, attempt, record)
        outcome, reason = outcome or derived, reason or why
        end = {"attempt": attempt["attempt"], "outcome": outcome, "reason": reason, "at": _now(), "run_id": run_id,
               "sync": sync, "worker_status": None, "destroy": None, "collected": None, "exported": None,
               "missing": []}
        if run_id:
            try:
                destroyed = self.c.destroy(run_id)
                end["destroy"] = destroyed["status"]
                end["missing"] = destroyed["data"].get("missing", []) or \
                    [f["message"] for f in destroyed["findings"] if f["severity"] == "error"]
            except (worker.Refused, guest.GuestError, parallels.ParallelsError, template.DeadlineExceeded) as err:
                end["destroy"] = "environment_failure"
                end["missing"] = [str(err)]
            collected = self.c.collected(run_id)
            end["collected"] = str(collected.relative_to(self.state)) if collected else None
            try:
                exported = self.c.export(run_id)
                end["exported"] = exported["data"]["path"]
            except (worker.Refused, OSError, ValueError) as err:
                end["missing"].append(f"export: {err}")
            after = self.c.record(run_id)
            end["worker_status"] = after["status"] if after else "never_created"
        else:
            end["worker_status"] = "none"
        if outcome == "blocked" and attempt["violation"]:
            sortie = Path(session["workspace"]) / ".sortie"
            sortie.mkdir(exist_ok=True)
            _write(sortie / "blocker.md", (f"The worker's changes touch paths a {session['profile']} session may "
                                           f"not change: {', '.join(attempt['violation']['paths'])}. They are kept "
                                           "outside the branch.\n").encode())
            _write(sortie / "status", b"blocked\n")
        attempt["end"] = end
        session["attempt"] = None
        self._save(session, attempt)
        return end

    def status(self, issue):
        """The run-result view: every attempt's worker and outcome, and the
        latest verification with what it rests on."""
        session = self.load(issue)
        if session is None:
            raise worker.Refused("session_missing", f"#{issue} has no session")
        attempts, lines = [], []
        for path in sorted((self._sdir(issue) / "attempts").glob("*/attempt.json")):
            a = json.loads(path.read_text())
            attempts.append({"attempt": a["attempt"], "run_id": a["run_id"], "turns": len(a["turns"]),
                             "verification": a.get("verification"), "end": a["end"]})
            end = a["end"] or {}
            lines.append(f"attempt {a['attempt']}: {len(a['turns'])} turn(s), worker "
                         f"{end.get('worker_status') or 'held'}, {end.get('outcome') or 'open'}"
                         + (f" ({end['reason']})" if end.get("reason") else ""))
        verifications = self._verifications(issue)
        if verifications:
            v = verifications[-1]
            lines.append(f"verification {v['head'][:12]} against {v['base'][:12]}: {v['verdict']} "
                         f"(gate {v['gate'].get('gate')})")
            lines += [f"  {f['code']}: {f['message']}" for f in v["findings"] if f["severity"] == "error"][:10]
        summary = f"#{issue}, PR {session['pr'] or 'none'}: {len(attempts)} attempt(s), " \
                  f"{'one open' if session['attempt'] else 'none open'}"
        return contracts.result("session status", "success", summary, data={
            **session, "attempt_records": attempts, "view": lines,
            "latest_verification": verifications[-1] if verifications else None})


class Broker:
    """The controller as one turn's agent sees it: its own run, nothing else."""

    WORKER_OPS = ("inspect", "signal", "exec", "status", "wait", "read", "terminal-open", "terminal-send",
                  "terminal-resize", "terminal-capture", "console-capture", "scenario", "collect")
    SESSION_OPS = ("checkpoint", "reset", "fetch", "push", "pr-status", "pr-create", "pr-ready", "pr-comment",
                   "pr-view", "handoff", "verification")

    def __init__(self, sessions, session, attempt):
        self.s, self.session, self.attempt = sessions, session, attempt
        self.run_id = attempt["run_id"]
        self.lock = threading.Lock()
        self.server = None
        self.tmp = None

    def handle(self, request):
        op = request.get("op") if isinstance(request, dict) else None

        def refuse(code, message):
            return contracts.result(f"lab {op or 'request'}", "environment_failure", "refused",
                                    [contracts.finding(code, message)])
        if not isinstance(op, str):
            return refuse("request_invalid", "a request is a JSON object with an op")
        target = request.get("target", "worker")
        if target != "worker":
            return refuse("host_target_refused", f"{op} targets {target!r}; a session controls only its "
                          "assigned worker")
        run = request.get("run", self.run_id)
        if run != self.run_id:
            return refuse("target_not_assigned", f"run {run!r} is not this session's worker")
        args = request.get("args", {})
        if not isinstance(args, dict):
            return refuse("request_invalid", "args must be an object")
        if op not in self.WORKER_OPS + self.SESSION_OPS:
            return refuse("operation_not_permitted", f"{op!r} is not an operation a session may request")
        with self.lock:
            try:
                if op in self.WORKER_OPS:
                    return self.s.c.operation(op, self.run_id, **args)
                return getattr(self, "op_" + op.replace("-", "_"))(**args)
            except worker.Refused as err:
                return refuse(err.code, str(err))
            except (KeyError, TypeError) as err:
                return refuse("request_invalid", f"{op}: {err}")
            except Exception as err:  # every request gets a result; the failure is its finding
                return contracts.result(f"lab {op}", "environment_failure", "failed",
                                        [contracts.finding("operation_failed", f"{type(err).__name__}: {err}")])

    def _ok(self, op, summary, data=None):
        return contracts.result(f"lab {op}", "success", summary, data=data or {})

    def _failed(self, op, code, message, data=None):
        return contracts.result(f"lab {op}", "environment_failure", message, [contracts.finding(code, message)],
                                data or {})

    def _workspace(self):
        return Path(self.session["workspace"])

    # Source

    def op_checkpoint(self):
        outcome = self.s.sync(self.session, self.attempt)
        if outcome["status"] in ("synced", "unchanged"):
            return self._ok("checkpoint", f"the workspace holds {outcome['head']}", outcome)
        return self._failed("checkpoint", f"checkpoint_{outcome['status']}", outcome["message"], outcome)

    def op_reset(self):
        self.attempt["reset_requested"] = True
        self.s._save(self.session, self.attempt)
        return self._ok("reset", "scheduled: when this turn ends the worker is checkpointed, restored to its "
                        "baseline and given the branch again; end the turn now", {"scheduled": True})

    def op_fetch(self):
        workspace = self._workspace()
        _git(workspace, "fetch", "-q", "origin", "main")
        bundle = _git(workspace, "bundle", "create", "-", "refs/remotes/origin/main").stdout
        main = _git(workspace, "rev-parse", "refs/remotes/origin/main").stdout.decode().strip()
        co = shlex.quote(self.s.c.checkout(self.run_id))
        self.s.c.guest(self.run_id).run(
            f"t=$(mktemp) && cat > $t && cd {co} && git bundle unbundle $t >/dev/null && "
            f"git update-ref refs/remotes/origin/main {main}; s=$?; rm -f $t; exit $s",
            self.s.c.config["deadlines"]["command"], stdin=bundle)
        return self._ok("fetch", f"origin/main is {main} in the worker", {"origin_main": main})

    def op_push(self, force_with_lease=False):
        outcome = self.s.sync(self.session, self.attempt)
        if outcome["status"] not in ("synced", "unchanged"):
            return self._failed("push", f"checkpoint_{outcome['status']}", outcome["message"], outcome)
        branch = self.session["branch"]
        argv = ["push", "-q", "origin", f"HEAD:refs/heads/{branch}"]
        if force_with_lease:
            argv.insert(1, f"--force-with-lease=refs/heads/{branch}")
        done = _git(self._workspace(), *argv, check=False)
        if done.returncode != 0:
            return self._failed("push", "push_rejected", done.stderr.decode(errors="replace").strip()[-500:])
        return self._ok("push", f"pushed {outcome['head']} to {branch}", {"head": outcome["head"], "branch": branch})

    # Pull request, through the dispatcher's GitHub identity on the host

    def _gh(self, *args, stdin=None):
        done = subprocess.run([self.s.gh, *args], cwd=self._workspace(), input=stdin, capture_output=True,
                              text=True, timeout=120)
        if done.returncode != 0:
            raise RuntimeError(f"gh {args[0]} {args[1] if len(args) > 1 else ''} failed: {done.stderr.strip()[-300:]}")
        return done.stdout

    def _open_prs(self):
        return json.loads(self._gh("pr", "list", "--head", self.session["branch"], "--state", "open",
                                   "--json", "number,url,isDraft,headRefOid"))

    def _pr(self):
        if not self.session["pr"]:
            found = self._open_prs()
            if not found:
                raise worker.Refused("pr_missing", f"{self.session['branch']} has no open pull request; "
                                     "create one first")
            self._record_pr(found[0]["number"])
        return self.session["pr"]

    def _record_pr(self, number):
        self.session["pr"] = number
        self.s._save(self.session)

    def op_pr_status(self):
        found = self._open_prs()
        return self._ok("pr-status", f"{len(found)} open pull request(s) for {self.session['branch']}",
                        {"recorded": self.session["pr"], "open": found})

    def _reserved(self, body):
        # Only the controller posts evidence; an agent's text cannot pose as it.
        if gate.MARKER in body:
            raise worker.Refused("evidence_marker_reserved", f"{gate.MARKER} marks the controller's own "
                                 "verification evidence; an agent's text may not carry it")

    def op_pr_create(self, title, body):
        """Reuse the branch's pull request; open a draft only when there is none."""
        self._reserved(title + body)
        found = self._open_prs()
        if found:
            self._record_pr(found[0]["number"])
            return self._ok("pr-create", f"reusing pull request #{found[0]['number']}",
                            {"pr": found[0]["number"], "reused": True})
        url = self._gh("pr", "create", "--draft", "--base", "main", "--head", self.session["branch"],
                       "--title", title, "--body-file", "-", stdin=body).strip()
        number = int(re.search(r"/pull/(\d+)", url).group(1))
        self._record_pr(number)
        return self._ok("pr-create", f"opened draft pull request #{number}", {"pr": number, "reused": False})

    def op_pr_ready(self):
        number = self._pr()
        self._gh("pr", "ready", str(number))
        return self._ok("pr-ready", f"#{number} is ready for review", {"pr": number})

    def op_pr_comment(self, body):
        self._reserved(body)
        number = self._pr()
        self._gh("pr", "comment", str(number), "--body-file", "-", stdin=body)
        return self._ok("pr-comment", f"commented on #{number}", {"pr": number})

    def op_pr_view(self):
        number = self._pr()
        view = json.loads(self._gh("pr", "view", str(number), "--json",
                                   "number,url,state,isDraft,headRefOid,reviews,comments,statusCheckRollup"))
        return self._ok("pr-view", f"#{number}", view)

    def op_handoff(self, overlay=()):
        """Ask for review of the pushed head. Checked now, done when the turn
        ends: the worker is released and the head verified (gate.py) first.
        `overlay` names test files whose candidate version the base run uses."""
        if not isinstance(overlay, (list, tuple)) or not all(
                isinstance(p, str) and OVERLAY_PATH.match(p) for p in overlay):
            raise worker.Refused("request_invalid", "overlay names paths relative to the checkout")
        outcome = self.s.sync(self.session, self.attempt)
        if outcome["status"] not in ("synced", "unchanged"):
            return self._failed("handoff", f"checkpoint_{outcome['status']}", outcome["message"], outcome)
        if workspace_diff(self._workspace()):
            return self._failed("handoff", "handoff_refused", "the branch has uncommitted changes; commit and push "
                                "them, since only a pushed head is verified and reviewed")
        number = self._pr()
        repo = self._gh("repo", "view", "--json", "nameWithOwner", "-q", ".nameWithOwner").strip()
        try:
            self.s._reviews(self.session, "--check", "--repo", repo, "--pr", str(number))
        except RuntimeError as err:
            return self._failed("handoff", "handoff_refused", str(err))
        # An earlier head's handoff no longer stands for this branch.
        for name in ("status", "scm.json"):
            stale = self._workspace() / ".sortie" / name
            if name == "scm.json" or (stale.is_file() and stale.read_text().strip() == "needs-human-review"):
                stale.unlink(missing_ok=True)
        self.attempt["handoff_requested"] = {"sha": outcome["head"], "pr": number, "overlay": list(overlay),
                                             "at": _now()}
        self.s._save(self.session, self.attempt)
        return self._ok("handoff", f"scheduled: when this turn ends the worker is released and {outcome['head'][:12]} "
                        "is verified on Linux and macOS against its merge base with main; it goes to review only if "
                        "the evidence gate accepts it. End the turn now; the next turn's `lab verification` shows a "
                        "refusal and why.", {"scheduled": True, "pr": number, "sha": outcome["head"]})

    def op_verification(self):
        """The session's latest verification: verdict, findings and records."""
        found = self.s._verifications(self.session["issue"])
        if not found:
            return self._failed("verification", "verification_missing", "no head of this session was verified yet")
        latest = found[-1]
        return self._ok("verification", f"{latest['head'][:12]}: {latest['verdict']}", latest)

    # Transport: one JSON request and one JSON reply per connection

    def start(self):
        # A short directory: a socket path must fit sun_path (104 bytes on
        # macOS, whose per-user TMPDIR alone takes about half of it).
        self.tmp = tempfile.mkdtemp(prefix="bzl-", dir="/tmp")
        path = os.path.join(self.tmp, "broker.sock")
        broker = self

        class Handler(socketserver.StreamRequestHandler):
            def handle(self):
                line = self.rfile.readline(PROTOCOL_LIMIT)
                try:
                    request = json.loads(line)
                except ValueError:
                    request = None
                reply = broker.handle(request)
                self.wfile.write((json.dumps(reply) + "\n").encode())

        self.server = socketserver.ThreadingUnixStreamServer(path, Handler)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return path

    def stop(self):
        if self.server:
            self.server.shutdown()
            self.server.server_close()
        if self.tmp:
            shutil.rmtree(self.tmp, ignore_errors=True)
