#!/usr/bin/env python3
"""Run one regression scenario inside a lab worker and print its result.

See docs/design/agent-lab.md §Test the real startup path. The controller stages
this directory into the guest and runs, inside the checkout's development shell:

    python3 runner.py run SCENARIO --mode cold|prepared --bin-dir DIR --cleanup-s N

A scenario is `<id>.toml` here (META_SCHEMA): its platforms, fixture modes and
the modes a verification requires, the tools it needs, its deadline, and the
procedure (procedures.py) with the assertions it checks. The runner:

1. Preflight: the platform, the workspace-built busybee and the bzbd beside it
   (busybee starts that one), and every other tool at its minimum version. A
   missing or mismatched tool is an environment failure before any fixture
   exists, never a skipped check.
2. Fixture: a private root holding copies of those binaries, an explicit
   busybee config and a Pueue YAML file routing every daemon path into the
   root. Cold mode writes nothing else: busybee's own client path starts both
   daemons. Prepared mode starts them deliberately, under umask 0022.
3. The procedure, as an unprivileged user (root would bypass the permission
   checks a scenario is about), bounded by the scenario deadline.
4. Diagnostics, then a cleanup bounded by --cleanup-s that stops every process
   carrying the fixture's marker and removes the root.

stdout carries exactly one JSON result (RESULT_SCHEMA); progress goes to stderr.
Exit status: 0 success, 1 product_failure, 2 environment_failure, 3 timeout,
64 usage. Only success is a pass.
"""
import argparse
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import tomllib
import traceback

import procedures

HERE = Path(__file__).resolve().parent
META_SCHEMA = "busybee.scenario/v1"
RESULT_SCHEMA = "busybee.scenario.result/v1"
MODES = ("cold", "prepared")
PLATFORMS = ("linux", "macos")
# Where this runner can drop privileges and account for processes.
SUPPORTED = ("linux",)
EXIT = {"success": 0, "product_failure": 1, "environment_failure": 2, "timeout": 3}
EXIT_USAGE = 64
WORKSPACE = ("busybee", "bzbd")
# The name each tool's --version reports; busybee and bzb are one binary.
VERSION_NAMES = {"busybee": "bzb"}
# sun_path, including its terminating NUL.
SOCKET_LIMIT = {"linux": 108, "macos": 104}
# Set on everything the fixture starts and inherited by the daemons and their
# tasks; cleanup stops exactly the processes that carry it.
MARKER = "BUSYBEE_SCENARIO_ROOT"
DAEMONS = ("bzbd", "pueued")
TASK_UMASK = 0o022
DAEMON_START_S = 10
LOG_TAIL = 64 * 1024
MAX_DEADLINE_S = 3600
SCENARIO_ID = re.compile(r"^[a-z0-9][a-z0-9-]*$")
REQUIREMENT = re.compile(r"^>=(\d+)\.(\d+)$")

monotonic = time.monotonic


class MetaError(Exception):
    pass


class HarnessError(Exception):
    """The fixture could not be set up: an environment failure."""

    def __init__(self, code, message):
        super().__init__(message)
        self.code = code


class Deadline(Exception):
    """The scenario deadline passed during `step`."""


def finding(code, message):
    return {"code": code, "message": message}


# Metadata

def meta_errors(meta, stem):
    fields = {"schema", "id", "summary", "platforms", "modes", "required_modes", "deadline_s", "procedure",
              "assertions", "tools"}
    optional = {"issue", "affected_revision"}
    errors = [f"unknown field {k!r}" for k in sorted(set(meta) - fields - optional)]
    errors += [f"missing field {k!r}" for k in sorted(fields - set(meta))]
    if errors:
        return errors
    if meta["schema"] != META_SCHEMA:
        errors.append(f"schema must be {META_SCHEMA}")
    if meta["id"] != stem:
        errors.append("id must match the file name")
    for key, allowed in (("platforms", PLATFORMS), ("modes", MODES)):
        value = meta[key]
        if not isinstance(value, list) or not value or not set(value) <= set(allowed):
            errors.append(f"{key} must be a non-empty subset of {', '.join(allowed)}")
    if not isinstance(meta["required_modes"], list) or not set(meta["required_modes"]) <= set(meta["modes"] or []):
        errors.append("required_modes must be a subset of modes")
    deadline = meta["deadline_s"]
    if not isinstance(deadline, int) or isinstance(deadline, bool) or not 1 <= deadline <= MAX_DEADLINE_S:
        errors.append(f"deadline_s must be an integer within 1..{MAX_DEADLINE_S}")
    procedure = procedures.PROCEDURES.get(meta["procedure"])
    if procedure is None:
        errors.append(f"unknown procedure {meta['procedure']!r}")
    elif meta["assertions"] != list(procedure.assertions):
        errors.append(f"assertions must be exactly what procedure {meta['procedure']} checks")
    tools = meta["tools"]
    if not isinstance(tools, dict) or not all(isinstance(v, str) and (v == "workspace" or REQUIREMENT.match(v))
                                              for v in tools.values()):
        errors.append("tools must map a name to 'workspace' or '>=MAJOR.MINOR'")
    elif any(v == "workspace" and k not in WORKSPACE for k, v in tools.items()):
        errors.append(f"only {', '.join(WORKSPACE)} are workspace tools")
    if "affected_revision" in meta and not re.match(r"^[0-9a-f]{40}$", str(meta["affected_revision"])):
        errors.append("affected_revision must be a full commit id")
    return errors


def load_meta(scenario_id, root=HERE):
    if not SCENARIO_ID.match(scenario_id):
        raise MetaError(f"{scenario_id!r} is not a scenario id")
    path = Path(root) / f"{scenario_id}.toml"
    if not path.is_file():
        raise MetaError(f"no scenario {scenario_id}")
    try:
        meta = tomllib.loads(path.read_text())
    except tomllib.TOMLDecodeError as err:
        raise MetaError(f"{scenario_id}.toml is not valid TOML: {err}") from err
    errors = meta_errors(meta, scenario_id)
    if errors:
        raise MetaError(f"{scenario_id}.toml: {'; '.join(errors)}")
    return meta


# Preflight

def version_from_describe(text):
    """`[v]MAJOR.MINOR.PATCH-N-gSHA` to `MAJOR.MINOR.<PATCH+N>`, as crates/bzb/build.rs does."""
    match = re.match(r"^v?(\d+)\.(\d+)\.(\d+)-(\d+)-g[0-9a-f]+$", (text or "").strip())
    if not match:
        return None
    major, minor, patch, ahead = map(int, match.groups())
    return f"{major}.{minor}.{patch + ahead}"


def at_least(version, minimum):
    def parts(v):
        return [int(p) for p in re.findall(r"\d+", v)[:3]]
    return parts(version) >= parts(minimum)


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


class Probe:
    """Read-only tool queries in the guest."""

    def platform(self):
        return {"linux": "linux", "darwin": "macos"}.get(sys.platform, sys.platform)

    def euid(self):
        return os.geteuid()

    def which(self, name):
        return shutil.which(name)

    def version(self, path):
        try:
            done = subprocess.run([str(path), "--version"], capture_output=True, text=True, timeout=10)
        except (OSError, subprocess.TimeoutExpired):
            return None
        return done.stdout.strip() if done.returncode == 0 else None

    def describe(self, checkout):
        done = subprocess.run(["git", "-C", str(checkout), "describe", "--tags", "--long", "--match",
                               "[0-9]*.[0-9]*.[0-9]*", "--match", "v[0-9]*.[0-9]*.[0-9]*"],
                              capture_output=True, text=True, timeout=10)
        return done.stdout.strip() if done.returncode == 0 else None


def preflight(meta, bin_dir, checkout, probe):
    """What the scenario will run with, and every reason it cannot."""
    findings = []
    platform = probe.platform()
    if platform not in meta["platforms"]:
        findings.append(finding("platform_not_applicable", f"{meta['id']} declares {', '.join(meta['platforms'])}, "
                                f"not {platform}"))
    if platform not in SUPPORTED:
        findings.append(finding("platform_unsupported", f"this runner supports {', '.join(SUPPORTED)}, not {platform}"))
    if probe.euid() != 0:
        findings.append(finding("runner_not_root", "the runner drops to an unprivileged user and must start as root"))
    if not probe.which("setpriv"):
        findings.append(finding("tool_missing", "setpriv is not on PATH; the runner needs it to drop privileges"))
    expected = version_from_describe(probe.describe(checkout))
    if expected is None:
        findings.append(finding("revision_version_unknown", "git describe found no semver tag in the checkout"))
    tools = {}
    for name, requirement in meta["tools"].items():
        path = Path(bin_dir) / name if requirement == "workspace" else probe.which(name)
        if path is None or not (Path(path).is_file() and os.access(path, os.X_OK)):
            findings.append(finding("tool_missing", f"{name} ({requirement}) is not an executable "
                                    + ("in the build directory" if requirement == "workspace" else "on PATH")))
            continue
        fact = {"path": str(path), "requirement": requirement, "sha256": sha256(path), "version": None}
        tools[name] = fact
        if name == "bzbd":
            continue  # no version flag: paired by sitting beside busybee, which starts it, and by digest
        reported = probe.version(path)
        words = (reported or "").split()
        fact["version"] = words[1] if len(words) == 2 and words[0] == VERSION_NAMES.get(name, name) else None
        if fact["version"] is None:
            findings.append(finding("tool_version_unknown", f"{name} --version printed {reported!r}"))
        elif requirement == "workspace":
            if expected is not None and fact["version"] != expected:
                findings.append(finding("tool_version_mismatch", f"{name} is {fact['version']}; the checkout "
                                        f"builds {expected}"))
        elif not at_least(fact["version"], requirement[2:]):
            findings.append(finding("tool_version_too_old", f"{name} is {fact['version']}; {meta['id']} needs "
                                    f"{requirement}"))
    return {"platform": platform, "expected_version": expected, "tools": tools}, findings


# Fixture

class User:
    """Who scenario commands run as, and the argv prefix that becomes them."""

    def __init__(self, name, uid, gid, prefix):
        self.name, self.uid, self.gid, self.prefix = name, uid, gid, list(prefix)

    def describe(self):
        return {"name": self.name, "uid": self.uid, "gid": self.gid}


def scenario_user():
    import pwd
    user = pwd.getpwnam("nobody")
    return User("nobody", user.pw_uid, user.pw_gid,
                ["setpriv", f"--reuid={user.pw_uid}", f"--regid={user.pw_gid}", "--clear-groups", "--"])


def _proc_text(pid, name):
    try:
        return Path(f"/proc/{pid}/{name}").read_bytes()
    except OSError:
        return None


def scenario_processes(root, uid):
    """Processes carrying the fixture's marker, and any bzbd or pueued of the
    scenario user (Linux /proc)."""
    marker = f"{MARKER}={root}".encode()
    found = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit() or int(entry.name) == os.getpid():
            continue
        environ, status = _proc_text(entry.name, "environ"), _proc_text(entry.name, "status")
        if environ is None or status is None:
            continue
        fields = dict(line.split(":", 1) for line in status.decode(errors="replace").splitlines() if ":" in line)
        comm = fields.get("Name", "").strip()
        owner = int(fields.get("Uid", "-1").split()[0])
        if marker in environ.split(b"\0") or (owner == uid and comm in DAEMONS):
            found.append({"pid": int(entry.name), "comm": comm, "uid": owner,
                          "umask": fields.get("Umask", "").strip() or None})
    return sorted(found, key=lambda p: p["pid"])


def _kill(pid, sig):
    try:
        os.kill(pid, sig)
    except ProcessLookupError:
        pass


def _mode(path):
    st = path.lstat()
    kind = {0o040000: "dir", 0o100000: "file", 0o140000: "socket", 0o010000: "fifo", 0o120000: "symlink"}
    return {"type": kind.get(st.st_mode & 0o170000, "other"), "mode": f"{st.st_mode & 0o7777:04o}"}


def _tail(path):
    try:
        with open(path, "rb") as f:
            f.seek(max(0, f.seek(0, os.SEEK_END) - LOG_TAIL))
            return f.read().decode(errors="replace")
    except OSError as err:
        return f"<unreadable: {err.strerror}>"


class Fixture:
    """A private root for one scenario run. Cold mode writes only routing
    config and the binaries; the daemons create their own state."""

    def __init__(self, mode, platform, bin_dir, workspace, user, base="/tmp", procs=scenario_processes, kill=_kill):
        self.mode, self.platform, self.source_bin = mode, platform, Path(bin_dir)
        self.workspace, self.user, self.base = list(workspace), user, Path(base)
        self.procs, self.kill = procs, kill
        self.root = None
        self.written = []

    @property
    def bin(self):
        return self.root / "bin"

    @property
    def state(self):
        return self.root / "state"

    @property
    def pueue(self):
        return self.root / "pueue"

    @property
    def config(self):
        return self.root / "config"

    def env(self):
        return {"PATH": os.environ.get("PATH", "/usr/bin:/bin"), "HOME": str(self.root / "home"), "LANG": "C.UTF-8",
                "BUSYBEE_STATE_DIR": str(self.state), "BUSYBEE_CONFIG": str(self.config / "busybee.toml"),
                "PUEUE_CONFIG_PATH": str(self.config / "pueue.yml"), MARKER: str(self.root)}

    def write(self):
        self.root = Path(tempfile.mkdtemp(prefix="bzs-", dir=self.base))
        sockets = (self.state / "bzbd.sock", self.pueue / "pueue.sock")
        long = [s for s in sockets if len(str(s)) + 1 > SOCKET_LIMIT[self.platform]]
        if long:
            raise HarnessError("socket_path_too_long", f"{long[0]} exceeds the {SOCKET_LIMIT[self.platform]}-byte "
                               f"socket path limit on {self.platform}")
        self.root.chmod(0o755)
        for name in ("bin", "config", "home", "work", "logs"):
            (self.root / name).mkdir()
        for name in self.workspace:
            shutil.copy2(self.source_bin / name, self.bin / name)
        (self.config / "busybee.toml").write_text("pool_size = 2\n")
        # pueued creates its pueue_directory but not a separate runtime_directory,
        # so the socket and pid file live inside the former.
        (self.config / "pueue.yml").write_text(
            f"shared:\n  pueue_directory: {self.pueue}\n  runtime_directory: {self.pueue}\n"
            f"  use_unix_socket: true\n  unix_socket_path: {self.pueue / 'pueue.sock'}\n")
        for path in [self.root, *self.root.rglob("*")]:
            os.chown(path, self.user.uid, self.user.gid)
        # What setup wrote, before any daemon or task ran.
        self.written = sorted(str(p.relative_to(self.root)) for p in self.root.rglob("*") if p.is_file())

    def describe(self):
        return {"mode": self.mode, "root": str(self.root) if self.root else None, "user": self.user.describe(),
                "umask": f"{TASK_UMASK:04o}", "routing": self.env() if self.root else {}, "written": self.written}

    def cold_checks(self):
        """Both daemons absent and none of their runtime state precreated."""
        problems = [f"{p['comm']} (pid {p['pid']}) is already running" for p in self.procs(self.root, self.user.uid)]
        problems += [f"{p.name}/ already exists" for p in (self.state, self.pueue) if p.exists()]
        return [{"name": "cold_fixture_starts_no_daemons", "status": "failed" if problems else "passed",
                 "detail": "; ".join(problems) or "no daemon running and no runtime state before the client starts"}]

    def _spawn(self, step, argv, umask=TASK_UMASK):
        out = open(self.root / "logs" / f"{step}.stdout", "wb")
        err = open(self.root / "logs" / f"{step}.stderr", "wb")
        with out, err:
            return subprocess.Popen([*self.user.prefix, *argv], env=self.env(), cwd=self.root / "work",
                                    stdin=subprocess.DEVNULL, stdout=out, stderr=err, start_new_session=True,
                                    umask=umask)

    def run(self, step, argv, deadline, cap=None):
        """Run argv as the scenario user; its output never reaches the runner's stdout."""
        left = deadline - monotonic()
        if left <= 0:
            raise Deadline(step)
        started = monotonic()
        proc = self._spawn(step, argv)
        try:
            code = proc.wait(timeout=min(left, cap) if cap else left)
        except subprocess.TimeoutExpired:
            os.killpg(proc.pid, signal.SIGKILL)
            proc.wait()
            if cap and cap < left:
                code = None  # its own bound, inside the scenario's
            else:
                raise Deadline(step)
        logs = self.root / "logs"
        return {"step": step, "exit_code": code, "elapsed_s": round(monotonic() - started, 3),
                "stdout": (logs / f"{step}.stdout").read_text(errors="replace"),
                "stderr": (logs / f"{step}.stderr").read_text(errors="replace")}

    def prepare(self, deadline):
        """Start both daemons deliberately, under the task umask."""
        started = []
        for name, argv, ready in (("pueued", ["pueued", "-d", "-c", str(self.config / "pueue.yml")],
                                   self.pueue / "pueue.sock"),
                                  ("bzbd", [str(self.bin / "bzbd")], self.state / "bzbd.sock")):
            done = self.run(f"prepare-{name}", argv, deadline, cap=DAEMON_START_S)
            until = monotonic() + DAEMON_START_S
            while not ready.exists() and monotonic() < until:
                time.sleep(0.1)
            if done["exit_code"] != 0 or not ready.exists():
                raise HarnessError("prepared_daemon_failed", f"{name} did not start: exit {done['exit_code']}, "
                                   f"{done['stderr'].strip()[-500:]}")
            running = [p for p in self.procs(self.root, self.user.uid) if p["comm"] == name]
            started.append({"name": name, "pid": running[0]["pid"] if running else None,
                            "umask": running[0]["umask"] if running else None})
        return started

    def diagnostics(self):
        """Modes of everything the run created, the routing config and the
        daemon and step logs, and the processes still running."""
        if self.root is None or not self.root.exists():
            return {"modes": {}, "files": {}, "processes": []}
        modes = {str(p.relative_to(self.root)): _mode(p) for p in sorted(self.root.rglob("*"))
                 if p.parts[len(self.root.parts)] not in ("bin", "home")}
        files = {str(p.relative_to(self.root)): _tail(p) for p in sorted(self.root.rglob("*"))
                 if p.is_file() and (p.parent in (self.config, self.root / "logs") or p.suffix == ".log")}
        return {"modes": modes, "files": files, "processes": self.procs(self.root, self.user.uid)}

    def cleanup(self, seconds):
        """Stop every scenario process within `seconds`: TERM, then KILL."""
        started = monotonic()
        until = started + seconds
        stopped = self.procs(self.root, self.user.uid) if self.root else []
        for sig, share in ((signal.SIGTERM, 0.5), (signal.SIGKILL, 1.0)):
            for p in self.procs(self.root, self.user.uid):
                self.kill(p["pid"], sig)
            step_until = started + seconds * share
            while self.procs(self.root, self.user.uid) and monotonic() < min(step_until, until):
                time.sleep(0.1)
        remaining = self.procs(self.root, self.user.uid) if self.root else []
        error = "scenario processes are still running" if remaining else None
        if not remaining:
            try:
                self.remove()
            except OSError as err:
                error = f"{err.strerror}: {err.filename}"
        return {"stopped": stopped, "remaining": remaining, "removed": error is None, "remove_error": error,
                "elapsed_s": round(monotonic() - started, 3)}

    def remove(self):
        if self.root is not None and self.root.exists():
            shutil.rmtree(self.root)


# Running a scenario

class Check:
    """The assertions one procedure run records, and what it observed."""

    def __init__(self, declared):
        self.declared = list(declared)
        self.results = {}
        self.observations = {}

    def record(self, name, ok, detail):
        if name not in self.declared:
            raise ValueError(f"{name} is not a declared assertion")
        self.results[name] = {"name": name, "status": "passed" if ok else "failed", "detail": detail}

    def not_reached(self, name, why):
        self.results.setdefault(name, {"name": name, "status": "not_reached", "detail": why})

    def observe(self, key, value):
        self.observations[key] = value

    def listed(self):
        return [self.results.get(n, {"name": n, "status": "not_reached", "detail": "the procedure did not get there"})
                for n in self.declared]


# No fixture was written: nothing to stop or remove.
NO_CLEANUP = {"stopped": [], "remaining": [], "removed": None, "remove_error": None, "elapsed_s": 0.0}


def classify(harness, timed_out, assertions, declared, cleanup):
    """The scenario's status. A failed assertion stays a product failure;
    nothing that went unchecked becomes a pass."""
    findings = list(harness)
    if cleanup.get("remaining"):
        findings.append(finding("cleanup_incomplete", f"{len(cleanup['remaining'])} scenario process(es) survived "
                                "cleanup"))
    elif cleanup["removed"] is False:
        findings.append(finding("cleanup_incomplete", f"the fixture root was not removed: {cleanup['remove_error']}"))
    failed = [a["name"] for a in assertions if a["status"] == "failed"]
    unchecked = [n for n in declared if n not in {a["name"] for a in assertions if a["status"] == "passed"}]
    # An observed product failure stays one, whatever went wrong after it.
    if failed:
        return "product_failure", findings + [finding("assertion_failed", f"failed: {', '.join(failed)}")]
    if harness:
        return "environment_failure", findings
    if timed_out:
        return "timeout", findings
    if unchecked:
        return "environment_failure", findings + [finding("assertion_not_evaluated",
                                                          f"not evaluated: {', '.join(unchecked)}")]
    if cleanup["removed"] is False:
        return "environment_failure", findings
    return "success", findings


def _stamp():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def run_scenario(meta, mode, bin_dir, checkout, cleanup_s, probe, make_fixture, procs=None):
    procs = procedures.PROCEDURES if procs is None else procs
    started, started_at = monotonic(), _stamp()
    deadline = started + meta["deadline_s"]
    check = Check(meta["assertions"])
    result = {"schema": RESULT_SCHEMA, "scenario": meta["id"], "issue": meta.get("issue"), "mode": mode,
              "required_modes": meta["required_modes"], "satisfies_required_mode": mode in meta["required_modes"],
              "deadline_s": meta["deadline_s"], "started_at": started_at}
    timed_out, fixture, fx = None, {"mode": mode, "checks": [], "daemons": []}, None
    facts, harness = preflight(meta, bin_dir, checkout, probe)
    result["preflight"] = facts
    if not harness:
        try:
            workspace = [n for n, r in meta["tools"].items() if r == "workspace"]
            fx = make_fixture(mode, facts["platform"], bin_dir, workspace)
            fx.write()
            if mode == "cold":
                fixture["checks"] = fx.cold_checks()
                if any(c["status"] != "passed" for c in fixture["checks"]):
                    raise HarnessError("fixture_not_cold", fixture["checks"][0]["detail"])
            else:
                fixture["daemons"] = fx.prepare(deadline)
            print(f"runner: {meta['id']} ({mode}) running {meta['procedure']}", file=sys.stderr)
            procs[meta["procedure"]].fn(fx, check, deadline)
        except HarnessError as err:
            harness.append(finding(err.code, str(err)))
        except Deadline as err:
            timed_out = finding("scenario_timeout", f"the {meta['deadline_s']}s deadline passed during {err}")
        except Exception as err:  # reported, never mistaken for a product result
            harness.append(finding("runner_error", "".join(traceback.format_exception(err))[-2000:]))
    if fx is not None:
        fixture.update(fx.describe())
        try:
            result["diagnostics"] = fx.diagnostics()
        except Exception as err:  # the cleanup still runs
            result["diagnostics"] = {"error": str(err)}
        cleanup = fx.cleanup(cleanup_s)
    else:
        result["diagnostics"] = {}
        cleanup = NO_CLEANUP
    assertions = check.listed()
    status, findings = classify(harness, bool(timed_out), assertions, meta["assertions"], cleanup)
    if timed_out:
        findings.insert(0, timed_out)
    failed = [a["name"] for a in assertions if a["status"] == "failed"]
    summary = {"success": "every assertion passed", "product_failure": f"failed: {', '.join(failed)}",
               "timeout": f"deadline of {meta['deadline_s']}s passed",
               "environment_failure": findings[0]["message"] if findings else "harness failure"}[status]
    result.update(status=status, summary=summary, findings=findings, assertions=assertions, fixture=fixture,
                  observations=check.observations, cleanup=cleanup, elapsed_s=round(monotonic() - started, 3))
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(prog="runner", description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = parser.add_subparsers(dest="command", required=True)
    run = sub.add_parser("run", help="run one scenario and print its result")
    run.add_argument("scenario")
    run.add_argument("--mode", required=True, choices=MODES)
    run.add_argument("--bin-dir", required=True, type=Path)
    run.add_argument("--checkout", type=Path, default=Path.cwd())
    run.add_argument("--cleanup-s", type=int, default=30)
    try:
        args = parser.parse_args(argv)
    except SystemExit as err:
        return EXIT_USAGE if err.code else 0
    try:
        meta = load_meta(args.scenario)
    except MetaError as err:
        print(f"runner: {err}", file=sys.stderr)
        return EXIT_USAGE
    if args.mode not in meta["modes"]:
        print(f"runner: {meta['id']} declares modes {', '.join(meta['modes'])}", file=sys.stderr)
        return EXIT_USAGE

    def make(mode, platform, bin_dir, workspace):
        return Fixture(mode, platform, bin_dir, workspace, scenario_user())

    result = run_scenario(meta, args.mode, args.bin_dir.resolve(), args.checkout, args.cleanup_s, Probe(), make)
    sys.stdout.write(json.dumps(result, indent=2) + "\n")
    print(f"runner: {meta['id']} ({args.mode}): {result['status']} ({result['summary']})", file=sys.stderr)
    return EXIT[result["status"]]


if __name__ == "__main__":
    sys.exit(main())
