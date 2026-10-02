"""Regression scenarios in a worker: stage the runner, run it bounded, keep its result.

See docs/design/agent-lab.md §Test the real startup path. The runner and the
scenario definitions come from the controller's checkout (tests/scenarios),
not the worker's: a red run against an older, affected revision uses today's
scenario. They are staged into the guest under a content-addressed directory
and run as one exec, in the worker checkout's development shell, against the
binaries it built. The exec is bounded by the scenario deadline plus the
runner's cleanup window and the shell's startup.

The scenario's status is the runner's own result, checked against its exit
status and its assertions. Output that is not a valid result is an environment
failure, never a product failure; an exec the controller had to end is a
timeout. Each run's record goes to `runs/<run>/scenarios/<exec>/result.json`
beside the exec's logs, and `collect` lists them with the coverage of each
scenario's required fixture modes.
"""
import hashlib
import io
import json
import re
import shlex
import subprocess
import sys
import tarfile

import contracts
import evidence
import guest
import screen
import template
import worker

SCENARIOS = "tests/scenarios"
STAGE = "/var/tmp/busybee-scenarios"
RECORD_SCHEMA = "busybee.vm.scenario/v1"
# The runner's own cleanup window, and the development shell's startup.
CLEANUP_S = 30
START_S = 60
# What a runner may name as its terminal directory, and its terminals.
TERMINAL_DIR = re.compile(r"^/var/tmp/bzt-[A-Za-z0-9_]+$")
TERMINAL_NAME = re.compile(r"^[a-z0-9][a-z0-9-]*$")


def _runner(repo):
    path = str(repo / SCENARIOS)
    if path not in sys.path:
        sys.path.insert(0, path)
    import runner
    return runner


def load(repo, scenario_id):
    runner = _runner(repo)
    try:
        return runner.load_meta(scenario_id, repo / SCENARIOS)
    except runner.MetaError as err:
        raise worker.Refused("scenario_invalid", str(err)) from err


def archive(repo):
    """The runner and scenario definitions as a reproducible tar, and its sha256."""
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w", format=tarfile.PAX_FORMAT) as tar:
        for path in sorted((repo / SCENARIOS).iterdir()):
            if path.is_file() and path.suffix in (".py", ".toml"):
                data = path.read_bytes()
                info = tarfile.TarInfo(path.name)
                info.size, info.mode = len(data), 0o644
                tar.addfile(info, io.BytesIO(data))
    data = buf.getvalue()
    return data, hashlib.sha256(data).hexdigest()


def stage_runner(workers, run_id, record):
    """Stage the controller's runner, scenarios and terminal host into the
    guest; returns the staged directory and the archive's digest."""
    data, digest = archive(workers.repo)
    stage = f"{STAGE}/{digest[:16]}"
    with worker.locked(workers._lock(run_id)):
        workers._window()
        workers._guest(record).run(f"mkdir -p {stage} && tar -xf - -C {stage}", workers._bound("command"),
                                   stdin=data)
    return stage, digest


def terminals(workers, run_id, record, name, reported):
    """Fetch, render and summarise the terminals a scenario reported; returns
    (summaries by handle, problems)."""
    gdir, names = reported.get("dir"), reported.get("names")
    if not isinstance(gdir, str) or not TERMINAL_DIR.match(gdir) or not isinstance(names, list) \
            or not all(isinstance(n, str) and TERMINAL_NAME.match(n) for n in names):
        return {}, ["the runner reported its terminals in an unexpected form"]
    found, problems = {}, []
    try:
        workers._window()
        g = workers._guest(record)
        for terminal in names:
            hdir = worker.run_dir(workers.state, run_id) / "terminal" / f"{name}-{terminal}"
            try:
                worker.fetch_terminal(g, workers._bound("command"), f"{gdir}/{terminal}", hdir)
                screen.render_handle(hdir)
            except (screen.RecordingError, screen.RenderError, tarfile.TarError, OSError) as err:
                problems.append(f"terminal {terminal}: {worker._reason(err)}")
            found[hdir.name] = screen.summary(hdir)
        if not problems:  # otherwise the guest keeps the only copy
            g.run(f"rm -rf {shlex.quote(gdir)}", workers._bound("command"))
    except (guest.GuestError, worker.Refused, template.DeadlineExceeded) as err:
        problems.append(f"terminals: {worker._reason(err)}")
    return found, problems


def _controller(repo):
    """The controller checkout the runner came from."""
    def git(*args):
        return subprocess.run(["git", *args], cwd=repo, capture_output=True, text=True).stdout.strip()
    return {"head": git("rev-parse", "HEAD") or None,
            "scenarios_dirty": bool(git("status", "--porcelain", "--", SCENARIOS))}


def interpret(runner, meta, mode, done, stdout):
    """The scenario's status, from the exec's result and the runner's output."""
    declared = meta["assertions"]
    unreached = [{"name": n, "status": "not_reached", "detail": "no result from the runner"} for n in declared]
    if done.get("operation") != "exec":
        return "environment_failure", [contracts.finding("scenario_unfinished", done["summary"])], unreached, None
    if done["status"] in ("timeout", "cancelled", "environment_failure"):
        return done["status"], [dict(f) for f in done["findings"]], unreached, None
    try:
        parsed = json.loads(stdout)
    except ValueError:
        parsed = None
    problems = []
    if not isinstance(parsed, dict) or parsed.get("schema") != runner.RESULT_SCHEMA:
        problems.append(f"the runner printed no {runner.RESULT_SCHEMA} result (exec {done['status']}, "
                        f"exit {done['data'].get('exit_code')})")
    elif (parsed.get("scenario"), parsed.get("mode")) != (meta["id"], mode):
        problems.append(f"the result is for {parsed.get('scenario')} ({parsed.get('mode')})")
    elif parsed.get("status") not in runner.EXIT:
        problems.append(f"unknown status {parsed.get('status')!r}")
    elif runner.EXIT[parsed["status"]] != done["data"].get("exit_code"):
        problems.append(f"status {parsed['status']} but the runner exited {done['data'].get('exit_code')}")
    elif [a.get("name") for a in parsed.get("assertions", [])] != declared:
        problems.append("the result does not list the declared assertions")
    elif parsed["status"] == "success" and any(a["status"] != "passed" for a in parsed["assertions"]):
        problems.append("a success with an assertion that did not pass")
    if problems:
        return "environment_failure", [contracts.finding("scenario_result_invalid", problems[0])], unreached, None
    findings = [contracts.finding(f["code"], f["message"]) for f in parsed["findings"]]
    return parsed["status"], findings, parsed["assertions"], parsed


def run(workers, run_id, scenario_id, mode, bin_dir="build/debug"):
    meta = load(workers.repo, scenario_id)
    if mode not in meta["modes"]:
        raise worker.Refused("mode_invalid", f"{scenario_id} declares modes {', '.join(meta['modes'])}, not {mode}")
    timeout = meta["deadline_s"] + CLEANUP_S + START_S
    _, record = workers._owned(run_id)
    if record["status"] != "ready":
        raise worker.Refused("worker_not_ready", f"worker {run_id} is {record['status']}")
    stage, digest = stage_runner(workers, run_id, record)
    argv = ["nix", "develop", "-c", "python3", f"{stage}/runner.py", "run", scenario_id, "--mode", mode,
            "--bin-dir", bin_dir, "--cleanup-s", str(CLEANUP_S)]
    done = workers.exec(run_id, argv, worker.CHECKOUT, {}, timeout)
    stdout_path = workers.state / done["data"]["stdout"] if "stdout" in done["data"] else None
    stdout = stdout_path.read_bytes() if stdout_path and stdout_path.is_file() else b""
    status, findings, assertions, parsed = interpret(_runner(workers.repo), meta, mode, done, stdout)
    name = done["data"].get("exec")
    if name is None:
        # Refused before anything was queued: no exec ran, so there is no run to record.
        return contracts.result("scenario", status, f"{scenario_id} ({mode}): {status}, nothing ran", findings, {
            "run_id": run_id, "scenario": scenario_id, "mode": mode, "exec": None, "path": None,
            "assertions": assertions, "coverage": None})
    shown, problems = terminals(workers, run_id, record, name, parsed["terminals"]) \
        if parsed and parsed.get("terminals") else ({}, [])
    findings += [contracts.finding("terminal_evidence_missing", p) for p in problems]
    if problems and status == "success":
        status = "environment_failure"  # a pass needs its terminal evidence
    entry = {"schema": RECORD_SCHEMA, "scenario": scenario_id, "issue": meta.get("issue"), "mode": mode,
             "required_modes": meta["required_modes"], "status": status, "exec": name,
             "exec_status": done["status"], "findings": findings, "assertions": assertions,
             "failed": [a["name"] for a in assertions if a["status"] == "failed"],
             "source": record["source"], "provenance": done["data"].get("provenance"),
             "runner": {"sha256": digest, **_controller(workers.repo)}, "terminals": shown, "result": parsed}
    path = worker.run_dir(workers.state, run_id) / "scenarios" / name / "result.json"
    evidence.durable(path, (json.dumps(entry, indent=2) + "\n").encode())
    covered = evidence.coverage(evidence.scenario_records(worker.run_dir(workers.state, run_id)))[scenario_id]
    notes = [contracts.finding("required_mode_missing", f"{scenario_id} is verified only by a {m} run, which "
                               f"this run has not had; a {mode} result cannot stand in for it", "warning")
             for m in covered["missing"]]
    summary = f"{scenario_id} ({mode}): {status}" + (f", failed {', '.join(entry['failed'])}" if entry["failed"] else "")
    return contracts.result("scenario", status, summary, findings + notes, {
        "run_id": run_id, "scenario": scenario_id, "mode": mode, "exec": name, "path": workers._rel(path),
        "assertions": assertions, "coverage": covered})
