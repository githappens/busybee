#!/usr/bin/env python3
"""busybee VM lab controller. See docs/design/agent-lab.md §Controller interface.

Every operation prints one result (contracts.RESULT_SCHEMA): JSON with --json,
otherwise a short summary. Exit status: 0 success, 1 any other result, 2 usage,
3 an operation this revision does not implement yet.
"""
import argparse
import base64
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tomllib

import contracts
import gate
import guest
import macos
import parallels
import registry
import scenario
import session
import supervisor
import template
import terminal_ops
import verify
import worker

REPO = Path(__file__).resolve().parents[2]
DEFAULT_CONFIG = contracts.STATE_ROOT / "local.toml"
EXIT_OK, EXIT_FAILED, EXIT_UNSUPPORTED = 0, 1, 3

# Documented install location, used only when the tool is not on PATH and the
# config names no path. Reported as the source so the choice is visible.
APP_BUNDLE = Path("/Applications/Parallels Desktop.app/Contents/MacOS")

class Host:
    """Read-only facts about the machine running the controller."""

    def which(self, name):
        return shutil.which(name)

    def executable(self, path):
        return os.path.isfile(path) and os.access(path, os.X_OK)

    def cpus(self):
        return os.cpu_count()

    def memory_mib(self):
        if sys.platform == "darwin":
            out = subprocess.run(["sysctl", "-n", "hw.memsize"], capture_output=True, text=True, check=True)
            return int(out.stdout) // (1024 * 1024)
        with open("/proc/meminfo") as meminfo:
            for line in meminfo:
                if line.startswith("MemTotal:"):
                    return int(line.split()[1]) // 1024
        raise RuntimeError("MemTotal missing from /proc/meminfo")

    def free_storage_gib(self, path):
        # The state directory may not exist before the first worker; measure
        # the filesystem it will be created on.
        while not path.exists():
            path = path.parent
        return shutil.disk_usage(path).free // (1024 ** 3)

    def os_name(self):
        return platform.system()

    def arch(self):
        return platform.machine()


def display(path, repo):
    """A path for output: repository-relative, never a host-specific location."""
    try:
        return str(path.resolve().relative_to(repo.resolve()))
    except ValueError:
        return f"<outside the repository>/{path.name}"


def load_config(path, repo, findings):
    if not path.is_file():
        findings.append(contracts.finding("config_missing", f"no local config at {display(path, repo)}; "
                                          "start from infra/vm/local.example.toml"))
        return None
    try:
        config = tomllib.loads(path.read_text())
    except tomllib.TOMLDecodeError as err:
        findings.append(contracts.finding("config_invalid", f"{display(path, repo)} is not valid TOML: {err}"))
        return None
    errors = contracts.config_errors(config, repo)
    findings.extend(contracts.finding(code, message) for code, message in errors)
    return None if errors else config


def resolve_tool(name, config, host, findings):
    explicit = (config or {}).get("parallels", {}).get(name)
    if explicit:
        if host.executable(explicit):
            return explicit, "config"
        findings.append(contracts.finding("tool_missing", f"the configured {name} is not an executable file"))
        return None, None
    found = host.which(name)
    if found:
        return found, "PATH"
    bundled = APP_BUNDLE / name
    if host.executable(str(bundled)):
        return str(bundled), "app bundle"
    findings.append(contracts.finding("tool_missing", f"{name} is not on PATH or in the Parallels Desktop "
                                      "app bundle; set [parallels] in the local config"))
    return None, None


def check_parallels(adapter, findings):
    try:
        version = adapter.query(["--version"]).split()[2]
        server = json.loads(adapter.query(["info", "--json"], tool="prlsrvctl"))
        vms = json.loads(adapter.query(["list", "--all", "--json"]))
    except (parallels.ParallelsError, ValueError, IndexError) as err:
        findings.append(contracts.finding("parallels_unavailable", f"Parallels did not answer a query: {err}"))
        return None, None
    license_state = server.get("License", {}).get("state", "unknown")
    signed_in = server.get("Signed In") == "yes"
    if license_state != "valid":
        findings.append(contracts.finding("parallels_not_authorized", f"Parallels license is {license_state}"))
    if not signed_in:
        findings.append(contracts.finding("parallels_signed_out", "no Parallels account is signed in; "
                                          "unattended operations may stop at a sign-in prompt", "warning"))
    info = {"version": version, "license": license_state, "signed_in": signed_in, "vm_count": len(vms)}
    return info, {parallels.braced(vm["uuid"]) for vm in vms}


def check_resources(repo, config, host, findings):
    state = contracts.state_dir(config, repo) if config else repo / contracts.STATE_ROOT
    available = {"cpus": host.cpus(), "memory_mib": host.memory_mib(),
                 "storage_gib": host.free_storage_gib(state)}
    for key, have in available.items():
        want = config["budget"][key] if config else None
        if want is not None and want > have:
            findings.append(contracts.finding("budget_exceeds_host", f"budget {key} = {want} (one worker's "
                                              f"ceiling) but the host has {have}"))
    return available


def check_concurrency(config, available, findings):
    """The resources every worker active at once needs: the configured number
    of Linux workers and, when configured, the macOS slot, each with the
    [worker] allocation. The slot guest's disk is its baseline's own, so it
    adds no storage. Workers are admitted one at a time against the host, so
    a peak beyond it is a warning, not a failure."""
    cap, allocation = contracts.linux_workers(config), config["worker"]
    slot = 1 if "macos" in config.get("templates", {}) else 0
    peak = {"cpus": (cap + slot) * allocation["cpus"], "memory_mib": (cap + slot) * allocation["memory_mib"],
            "storage_gib": cap * allocation["storage_gib"]}
    for key, need in peak.items():
        if need > available[key]:
            findings.append(contracts.finding("concurrency_exceeds_host", f"{cap} Linux worker(s)"
                                              f"{' and the macOS slot' if slot else ''} at once need {key} = "
                                              f"{need}; the host has {available[key]}", "warning"))
    return peak


def check_template(repo, name, entry, config, adapter, vms, findings):
    path = contracts.state_dir(config, repo) / entry["manifest"]
    if not path.is_file():
        findings.append(contracts.finding("baseline_missing", f"no {name} template manifest at {display(path, repo)}; "
                                          "provision and validate a baseline first"))
        return {"state": "missing"}
    try:
        manifest = json.loads(path.read_text())
    except ValueError as err:
        findings.append(contracts.finding("baseline_invalid", f"{display(path, repo)} is not JSON: {err}"))
        return {"state": "invalid"}
    errors = contracts.manifest_errors(manifest)
    if errors:
        findings.append(contracts.finding("baseline_invalid", f"{display(path, repo)}: {'; '.join(errors)}"))
        return {"state": "invalid"}
    summary = {"os": manifest["os"], "arch": manifest["arch"], "clone_modes": manifest["clone_modes"],
               "provisioning_revision": manifest["provisioning_revision"]}
    # Present, but not for the configured clone strategy: not a worker source.
    strategy = contracts.clone_strategy(config, name)
    eligible = strategy in manifest["clone_modes"]
    if not eligible:
        findings.append(contracts.finding("clone_mode_unsupported", f"clone_strategy {strategy} "
                                          f"was not validated for the {name} baseline ({', '.join(manifest['clone_modes'])})"))
    if vms is None:
        return {**summary, "state": "unverified"}
    snapshots = {}
    if manifest["vm_id"] in vms:
        try:
            snapshots = json.loads(adapter.query(["snapshot-list", manifest["vm_id"], "--json"]) or "{}")
        except (parallels.ParallelsError, ValueError) as err:
            findings.append(contracts.finding("parallels_unavailable", f"cannot list {name} snapshots: {err}"))
            return {**summary, "state": "unverified"}
    if manifest["snapshot_id"] not in snapshots:
        findings.append(contracts.finding("baseline_missing", f"the recorded {name} baseline VM or snapshot "
                                          "no longer exists in Parallels"))
        return {**summary, "state": "missing"}
    return {**summary, "state": "ready" if eligible else "ineligible"}


# Controller operations an issue session may rely on; lab dispatch requires
# `controller:<name>` for the ones an issue declares (sortie/lab.py).
CAPABILITIES = ("session", "exec", "terminal", "scenario", "inspect", "signal", "console", "collect", "reset",
                "verify", "gate")


def doctor(repo, config_path, host, runner=parallels.run):
    """Read-only preflight. Starts, creates and changes nothing."""
    findings = []
    config = load_config(config_path, repo, findings)
    prlctl, prlctl_source = resolve_tool("prlctl", config, host, findings)
    prlsrvctl, prlsrvctl_source = resolve_tool("prlsrvctl", config, host, findings)
    data = {"host": {"os": host.os_name(), "arch": host.arch()},
            "tools": {"prlctl": prlctl_source or "missing", "prlsrvctl": prlsrvctl_source or "missing"},
            "config": {"path": display(config_path, repo), "valid": config is not None},
            "controller": {"capabilities": list(CAPABILITIES)}}

    adapter, vms = None, None
    if prlctl and prlsrvctl:
        adapter = parallels.Parallels(prlctl, prlsrvctl, runner)
        data["parallels"], vms = check_parallels(adapter, findings)
    data["host"]["available"] = check_resources(repo, config, host, findings)

    if config:
        data["config"].update({"clone_strategy": config["clone_strategy"], "deadlines": config["deadlines"],
                               "budget": config["budget"],
                               "concurrency": {"linux_workers": contracts.linux_workers(config)},
                               "peak": check_concurrency(config, data["host"]["available"], findings)})
        templates = config.get("templates", {})
        if not templates:
            findings.append(contracts.finding("baseline_missing", "no template is configured"))
        data["templates"] = {name: check_template(repo, name, entry, config, adapter, vms, findings)
                             for name, entry in templates.items()}

    errors = [f for f in findings if f["severity"] == "error"]
    status = "environment_failure" if errors else "success"
    summary = f"{len(errors)} blocking finding(s)" if errors else "host is ready for lab workers"
    return contracts.result("doctor", status, summary, findings, data)


def connect(repo, args, host, operation):
    """The config, owned registry and Parallels adapter an operation needs, or
    the result explaining why there are none."""
    findings = []
    config = load_config(args.config, repo, findings)
    name = getattr(args, "name", None)
    if config and name and name not in config.get("templates", {}):
        findings.append(contracts.finding("config_invalid", f"no [templates.{name}] in the local config"))
    if findings:
        return None, contracts.result(operation, "environment_failure", "cannot run without a valid config", findings)
    prlctl, _ = resolve_tool("prlctl", config, host, findings)
    prlsrvctl, _ = resolve_tool("prlsrvctl", config, host, findings)
    if findings:
        return None, contracts.result(operation, "environment_failure", "Parallels tools are missing", findings)
    reg = registry.Registry(contracts.state_dir(config, repo))
    return (config, reg, parallels.Parallels(prlctl, prlsrvctl, owned=reg)), None


def template_operation(repo, args, host):
    operation = f"template {args.action}"
    ready, failed = connect(repo, args, host, operation)
    if failed:
        return failed
    config, reg, prl = ready
    state = contracts.state_dir(config, repo)
    if args.action == "promote":
        return template.promote(state, args.name, args.candidate)
    if args.action == "prune":
        return template.prune(state, args.name, prl, reg)
    lab = (macos.MacLab if args.name == "macos" else template.Lab)(REPO, config, prl, reg, root=repo)
    if args.action == "build":
        return lab.build(args.name, args.arch)
    return lab.validate(args.name, args.candidate)


def worker_operation(repo, args, host, operation):
    ready, failed = connect(repo, args, host, operation)
    if failed:
        return failed
    config, reg, prl = ready
    state = contracts.state_dir(config, repo)
    workers = worker.Workers(REPO, config, prl, reg, host.free_storage_gib,
                             supervise=lambda run_id, lease=None: supervisor.ensure(
                                 state, run_id, supervisor.argv(REPO, args.config, run_id, repo), lease=lease),
                             slot_wait_s=getattr(args, "wait", None), root=repo)
    try:
        if operation.startswith("session "):
            return session_operation(workers, args, operation)
        if operation == "supervise":
            supervisor.serve(workers, args.run_id, args.lease_fd)
            return None
        if operation == "worker create":
            return workers.create(args.name, args.revision, args.patch)
        if operation == "verify":
            revision = workers._source(args.revision, args.patch)["revision"]
            return verify.run(verify.WorkerOps(workers), args.platform or list(contracts.GUEST_OS), revision,
                              args.patch, state)
        if operation == "gate":
            revision = workers._source(args.revision, None)["revision"]
            base = args.base or subprocess.run(["git", "merge-base", "refs/remotes/origin/main", revision], cwd=repo,
                                               capture_output=True, text=True).stdout.strip()
            if not base:
                raise worker.Refused("source_invalid", f"no merge base of {revision} with origin/main; name --base")
            return gate.evaluate(gate.WorkerOps(workers), args.issue, revision,
                                 workers._source(base, args.overlay)["revision"], args.overlay,
                                 args.platform or list(contracts.GUEST_OS))
        if operation in ("exec", "terminal open"):
            malformed = [item for item in args.env if "=" not in item]
            if malformed:
                raise worker.Refused("env_invalid", f"--env takes NAME=VALUE, not {', '.join(malformed)}")
            env = dict(item.partition("=")[::2] for item in args.env)
            timeout = config["deadlines"]["command"] if args.timeout is None else args.timeout
            if operation == "exec":
                return workers.exec(args.run_id, args.command, args.cwd, env, timeout, args.detach)
            return terminal_ops.open_terminal(workers, args.run_id, args.command, args.cols, args.rows, args.cwd,
                                              env, timeout)
        if operation == "signal":
            return workers.signal(args.run_id, args.signal, args.pid)
        if operation == "status":
            return workers.status(args.run_id, args.exec)
        if operation == "wait":
            return workers.wait(args.run_id, args.exec, args.timeout)
        if operation == "read":
            return workers.read(args.run_id, args.exec, args.stream, args.offset, args.limit)
        if operation == "scenario":
            return scenario.run(workers, args.run_id, args.scenario, args.mode, args.bin_dir)
        if operation == "terminal send":
            return terminal_ops.send(workers, args.run_id, args.handle, args.text, args.key or (), args.bytes)
        if operation == "terminal resize":
            return terminal_ops.resize(workers, args.run_id, args.handle, args.cols, args.rows)
        if operation == "terminal capture":
            return terminal_ops.capture(workers, args.run_id, args.handle, args.expect, args.timeout)
        method = {"worker reset": workers.reset, "worker destroy": workers.destroy, "inspect": workers.inspect,
                  "collect": workers.collect, "console capture": workers.console_capture,
                  "export": workers.export}[operation]
        return method(args.run_id)
    except worker.Refused as err:
        return contracts.result(operation, "environment_failure", "refused", [contracts.finding(err.code, str(err))])
    except template.DeadlineExceeded as err:
        return contracts.result(operation, "timeout", "deadline passed", [contracts.finding("deadline", str(err))])
    except (guest.GuestError, parallels.ParallelsError) as err:
        return contracts.result(operation, "environment_failure", "the worker did not answer",
                                [contracts.finding("worker_unreachable", str(err))])


def session_operation(workers, args, operation):
    """An issue session (§Agent sessions), run by the controller of this
    checkout, which the dispatcher snapshots from a trusted revision."""
    sessions = session.Sessions(session.Controller(workers), REPO)
    if operation == "session start":
        return sessions.start(args.issue, args.workspace, args.profile)
    if operation == "session agent":
        return sessions.agent(Path.cwd(), args.command, args.timeout)
    if operation == "session end":
        return sessions.end(args.workspace, args.outcome, args.reason)
    return sessions.status(args.issue)


def summary(result):
    lines = [f"{result['operation']}: {result['status']} ({result['summary']})"]
    lines += [f"  {f['severity']:<7} {f['code']}: {f['message']}" for f in result["findings"]]
    data = result["data"]
    if "parallels" in data and data["parallels"]:
        p = data["parallels"]
        lines.append(f"  parallels {p['version']}, license {p['license']}, "
                     f"{'signed in' if p['signed_in'] else 'signed out'}, {p['vm_count']} VM(s) on the host")
    if "available" in data.get("host", {}):
        a = data["host"]["available"]
        lines.append(f"  host {data['host']['os']} {data['host']['arch']}: {a['cpus']} cpus, "
                     f"{a['memory_mib']} MiB, {a['storage_gib']} GiB free")
    if "peak" in data.get("config", {}):
        c = data["config"]
        b, peak = c["budget"], c["peak"]
        slot = " and the macOS slot" if "macos" in data.get("templates", {}) else ""
        lines.append(f"  workers: up to {c['concurrency']['linux_workers']} Linux at once{slot}, each within the "
                     f"per-worker budget of {b['cpus']} cpus, {b['memory_mib']} MiB, {b['storage_gib']} GiB; "
                     f"together {peak['cpus']} cpus, {peak['memory_mib']} MiB, {peak['storage_gib']} GiB")
    for name, template in data.get("templates", {}).items():
        lines.append(f"  template {name}: {template['state']}")
    lines += [f"  {line}" for line in data.get("view", [])]
    return "\n".join(lines)


def seconds(text):
    value = int(text)
    if value < 0:
        raise argparse.ArgumentTypeError(f"{text} is not a number of seconds")
    return value


def parser():
    root = argparse.ArgumentParser(prog="vmctl", description=__doc__,
                                   formatter_class=argparse.RawDescriptionHelpFormatter)
    root.add_argument("--json", action="store_true", help="print the result as JSON")
    root.add_argument("--root", type=Path, default=REPO,
                      help="the checkout whose build/vm state and revisions to use; default this one")
    ops = root.add_subparsers(dest="operation", required=True)
    doc = ops.add_parser("doctor", help="read-only host and configuration preflight")
    doc.add_argument("--config", type=Path, help="default: build/vm/local.toml under --root")
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("name")
    common.add_argument("--config", type=Path, help="default: build/vm/local.toml under --root")
    actions = ops.add_parser("template", help="build, validate and promote worker templates") \
        .add_subparsers(dest="action", required=True)
    actions.add_parser("build", parents=[common], help="provision a candidate from the pinned installer") \
        .add_argument("--arch", required=True, help="guest architecture; must match the pinned installer")
    for action in ("validate", "promote"):
        actions.add_parser(action, parents=[common]) \
            .add_argument("--candidate", required=True, help="run id printed by template build")
    actions.add_parser("prune", parents=[common], help="delete retained baselines no worker depends on")

    target = argparse.ArgumentParser(add_help=False)
    target.add_argument("run_id", help="run id printed by worker create")
    target.add_argument("--config", type=Path, help="default: build/vm/local.toml under --root")
    workers = ops.add_parser("worker", help="create, reset and destroy owned workers") \
        .add_subparsers(dest="action", required=True)
    create = workers.add_parser("create", parents=[common],
                                help="clone the promoted baseline; macos: wait for and lease the macOS slot")
    create.add_argument("--revision", required=True, help="commit to check out in the worker")
    create.add_argument("--patch", type=Path, help="patch applied on top of the revision")
    create.add_argument("--wait", type=seconds, help="seconds to wait in line for a free Linux worker or the "
                        "macOS slot; default deadlines.run")
    workers.add_parser("reset", parents=[target], help="collect, then restore the recorded baseline")
    workers.add_parser("destroy", parents=[target], help="collect, then delete the clone")
    run = ops.add_parser("exec", parents=[target], help="run argv in a worker: exec RUN_ID --cwd DIR -- ARGV...")
    run.add_argument("--cwd", required=True)
    run.add_argument("--env", action="append", default=[], metavar="NAME=VALUE")
    run.add_argument("--timeout", type=int, help="seconds, at most deadlines.scenario; default deadlines.command")
    run.add_argument("--detach", action="store_true", help="return the exec's handle instead of waiting for it")
    handle = argparse.ArgumentParser(add_help=False, parents=[target])
    handle.add_argument("exec", help="exec number printed by exec")
    status = ops.add_parser("status", help="owned workers after reconciling them, or one worker or exec")
    status.add_argument("run_id", nargs="?")
    status.add_argument("exec", nargs="?")
    status.add_argument("--config", type=Path, help="default: build/vm/local.toml under --root")
    ops.add_parser("wait", parents=[handle], help="wait for an exec's result") \
        .add_argument("--timeout", type=int, help="seconds; default as long as the supervisor may take")
    read = ops.add_parser("read", parents=[handle], help="an exec's output from a byte offset")
    read.add_argument("stream", choices=worker.STREAMS)
    read.add_argument("--offset", type=int, default=0)
    read.add_argument("--limit", type=int, default=worker.READ_LIMIT)
    ops.add_parser("export", parents=[target], help="write the run's sanitized public evidence")
    check = ops.add_parser("verify", help="build, check and run the scenarios of one revision on every platform")
    check.add_argument("--revision", required=True, help="commit to verify")
    check.add_argument("--patch", type=Path, help="patch applied on top of the revision")
    check.add_argument("--platform", action="append", choices=contracts.GUEST_OS,
                       help="a required platform; repeatable; default all")
    check.add_argument("--config", type=Path, help="default: build/vm/local.toml under --root")
    evidence_gate = ops.add_parser("gate", help="verify (or reuse) a candidate and its base on every platform, "
                                   "and judge them as a fix of an issue")
    evidence_gate.add_argument("--issue", type=int, required=True, help="the issue the candidate fixes")
    evidence_gate.add_argument("--revision", required=True, help="the candidate commit")
    evidence_gate.add_argument("--base", help="the base commit; default: its merge base with origin/main")
    evidence_gate.add_argument("--overlay", type=Path, help="test patch the base's red run applies")
    evidence_gate.add_argument("--platform", action="append", choices=contracts.GUEST_OS,
                               help="a required platform; repeatable; default all")
    evidence_gate.add_argument("--config", type=Path, help="default: build/vm/local.toml under --root")
    run_scenario = ops.add_parser("scenario", parents=[target], help="run a tests/scenarios scenario in a worker")
    run_scenario.add_argument("scenario", help="scenario id: tests/scenarios/<id>.toml")
    run_scenario.add_argument("--mode", required=True, choices=("cold", "prepared"))
    run_scenario.add_argument("--bin-dir", default="build/debug",
                              help="the built busybee and bzbd, relative to the worker checkout")
    ops.add_parser("supervise", parents=[target], help="internal: the run's watchdog, started by the controller") \
        .add_argument("--lease-fd", type=int, help="the macOS slot lease it inherits from worker create")
    configured = argparse.ArgumentParser(add_help=False)
    configured.add_argument("--config", type=Path, help="default: build/vm/local.toml under --root")
    sessions = ops.add_parser("session", help="an issue agent's attempts in owned workers (dispatcher hooks)") \
        .add_subparsers(dest="action", required=True)
    sstart = sessions.add_parser("start", parents=[configured],
                                 help="close any unclosed attempt, then open one in a fresh worker")
    sstart.add_argument("--issue", type=int, required=True)
    sstart.add_argument("--workspace", type=Path, required=True, help="the issue's checkout on its branch")
    sstart.add_argument("--profile", required=True, help="a profile of sortie/guard-policy.json")
    sessions.add_parser("agent", parents=[configured],
                        help="one agent turn in the attempt's worker: session agent -- ARGV (cwd: the workspace)") \
        .add_argument("--timeout", type=int, help="seconds; default what the worker's run deadline leaves")
    send = sessions.add_parser("end", parents=[configured], help="checkpoint, collect, destroy and account")
    send.add_argument("--workspace", type=Path, required=True)
    send.add_argument("--outcome", choices=session.OUTCOMES, help="default: derived from the attempt")
    send.add_argument("--reason")
    sessions.add_parser("status", parents=[configured]).add_argument("--issue", type=int, required=True)
    sig = ops.add_parser("signal", parents=[target], help="signal a process in a worker")
    sig.add_argument("signal")
    sig.add_argument("pid")
    ops.add_parser("inspect", parents=[target], help="VM state, processes and daemons")
    ops.add_parser("collect", parents=[target], help="export source changes and log digests")
    ops.add_parser("console", help="capture a worker's display").add_subparsers(dest="action", required=True) \
        .add_parser("capture", parents=[target])
    term = ops.add_parser("terminal", help="a real PTY in a worker").add_subparsers(dest="action", required=True)
    topen = term.add_parser("open", parents=[target],
                            help="start argv in a terminal: terminal open RUN_ID [--cols C --rows R] -- ARGV...")
    topen.add_argument("--cols", type=int, default=120)
    topen.add_argument("--rows", type=int, default=40)
    topen.add_argument("--cwd", default=worker.CHECKOUT)
    topen.add_argument("--env", action="append", default=[], metavar="NAME=VALUE",
                       help="TERM and LANG default to xterm-256color and C.UTF-8")
    topen.add_argument("--timeout", type=int, help="seconds the terminal may live; default deadlines.command")
    terminal_handle = argparse.ArgumentParser(add_help=False, parents=[target])
    terminal_handle.add_argument("handle", help="terminal handle printed by terminal open")
    tsend = term.add_parser("send", parents=[terminal_handle], help="input to the terminal's program")
    what = tsend.add_mutually_exclusive_group(required=True)
    what.add_argument("--text", help="characters, as typed")
    what.add_argument("--key", action="append", help="a named key, e.g. Enter, Up, 'Ctrl c'; repeatable")
    what.add_argument("--bytes", help="raw bytes as hex, e.g. 1b5b41")
    tresize = term.add_parser("resize", parents=[terminal_handle], help="change the terminal's size")
    tresize.add_argument("--cols", type=int, required=True)
    tresize.add_argument("--rows", type=int, required=True)
    tcapture = term.add_parser("capture", parents=[terminal_handle],
                               help="settled screen text, cells and image, and the recording so far")
    tcapture.add_argument("--expect", help="wait until the screen shows this text first")
    tcapture.add_argument("--timeout", type=int, default=10, help="seconds to wait and settle")
    return root


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    # Everything after the first `--` is the guest argv of `exec`, verbatim.
    split = argv.index("--") if "--" in argv else len(argv)
    args = parser().parse_args(argv[:split])
    args.command = argv[split + 1:]
    args.root = args.root.resolve()
    if getattr(args, "config", None) is None:
        args.config = args.root / DEFAULT_CONFIG
    if args.operation == "doctor":
        outcome = doctor(args.root, args.config, Host())
    elif args.operation == "template":
        outcome = template_operation(args.root, args, Host())
    else:
        operation = " ".join(filter(None, (args.operation, getattr(args, "action", None))))
        outcome = worker_operation(args.root, args, Host(), operation)
        if outcome is None:  # the supervisor, which reports through the run's events
            return EXIT_OK
        if operation == "session agent":
            # stdout carries the agent's own protocol; the turn's status is the exit code.
            if isinstance(outcome, int):
                return outcome
            print(summary(outcome), file=sys.stderr)
            return EXIT_FAILED
    if args.operation == "read" and not args.json and outcome["status"] == "success":
        # Plain `read` is the bytes themselves, so output can be piped.
        sys.stdout.buffer.write(base64.b64decode(outcome["data"]["content_b64"]))
        return EXIT_OK
    print(json.dumps(outcome, indent=2) if args.json else summary(outcome))
    if outcome["status"] == "unsupported":
        return EXIT_UNSUPPORTED
    return EXIT_OK if outcome["status"] == "success" else EXIT_FAILED


if __name__ == "__main__":
    sys.exit(main())
