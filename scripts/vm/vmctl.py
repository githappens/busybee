#!/usr/bin/env python3
"""busybee VM lab controller. See docs/design/agent-lab.md §Controller interface.

Every operation prints one result (contracts.RESULT_SCHEMA): JSON with --json,
otherwise a short summary. Exit status: 0 success, 1 any other result, 2 usage,
3 an operation this revision does not implement yet.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tomllib

import contracts
import parallels

REPO = Path(__file__).resolve().parents[2]
DEFAULT_CONFIG = contracts.STATE_ROOT / "local.toml"
EXIT_OK, EXIT_FAILED, EXIT_UNSUPPORTED = 0, 1, 3

# Documented install location, used only when the tool is not on PATH and the
# config names no path. Reported as the source so the choice is visible.
APP_BUNDLE = Path("/Applications/Parallels Desktop.app/Contents/MacOS")

GROUPS = {"template": ("build", "validate", "promote"), "worker": ("create", "reset", "destroy"),
          "terminal": ("open", "send", "resize", "capture"), "console": ("capture",)}
SINGLE = ("exec", "inspect", "signal", "collect")


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
    return info, {vm.get("uuid") for vm in vms}


def check_resources(repo, config, host, findings):
    state = contracts.state_dir(config, repo) if config else repo / contracts.STATE_ROOT
    available = {"cpus": host.cpus(), "memory_mib": host.memory_mib(),
                 "storage_gib": host.free_storage_gib(state)}
    for key, have in available.items():
        want = config["budget"][key] if config else None
        if want is not None and want > have:
            findings.append(contracts.finding("budget_exceeds_host", f"budget {key} = {want} but the host has {have}"))
    return available


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
    eligible = config["clone_strategy"] in manifest["clone_modes"]
    if not eligible:
        findings.append(contracts.finding("clone_mode_unsupported", f"clone_strategy {config['clone_strategy']} "
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


def doctor(repo, config_path, host, runner=parallels.run):
    """Read-only preflight. Starts, creates and changes nothing."""
    findings = []
    config = load_config(config_path, repo, findings)
    prlctl, prlctl_source = resolve_tool("prlctl", config, host, findings)
    prlsrvctl, prlsrvctl_source = resolve_tool("prlsrvctl", config, host, findings)
    data = {"host": {"os": host.os_name(), "arch": host.arch()},
            "tools": {"prlctl": prlctl_source or "missing", "prlsrvctl": prlsrvctl_source or "missing"},
            "config": {"path": display(config_path, repo), "valid": config is not None}}

    adapter, vms = None, None
    if prlctl and prlsrvctl:
        adapter = parallels.Parallels(prlctl, prlsrvctl, runner)
        data["parallels"], vms = check_parallels(adapter, findings)
    data["host"]["available"] = check_resources(repo, config, host, findings)

    if config:
        data["config"].update({"clone_strategy": config["clone_strategy"], "deadlines": config["deadlines"],
                               "budget": config["budget"]})
        templates = config.get("templates", {})
        if not templates:
            findings.append(contracts.finding("baseline_missing", "no template is configured"))
        data["templates"] = {name: check_template(repo, name, entry, config, adapter, vms, findings)
                             for name, entry in templates.items()}

    errors = [f for f in findings if f["severity"] == "error"]
    status = "environment_failure" if errors else "success"
    summary = f"{len(errors)} blocking finding(s)" if errors else "host is ready for lab workers"
    return contracts.result("doctor", status, summary, findings, data)


def unsupported(operation):
    return contracts.result(operation, "unsupported",
                            f"{operation} is not implemented in this controller revision", [
                                contracts.finding("operation_unsupported", f"{operation} has not shipped; "
                                                  "see docs/design/agent-lab.md §Implementation sequence")])


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
    for name, template in data.get("templates", {}).items():
        lines.append(f"  template {name}: {template['state']}")
    return "\n".join(lines)


def parser():
    root = argparse.ArgumentParser(prog="vmctl", description=__doc__,
                                   formatter_class=argparse.RawDescriptionHelpFormatter)
    root.add_argument("--json", action="store_true", help="print the result as JSON")
    ops = root.add_subparsers(dest="operation", required=True)
    doc = ops.add_parser("doctor", help="read-only host and configuration preflight")
    doc.add_argument("--config", type=Path, default=REPO / DEFAULT_CONFIG)
    for group, actions in GROUPS.items():
        sub = ops.add_parser(group).add_subparsers(dest="action", required=True)
        for action in actions:
            sub.add_parser(action).add_argument("args", nargs=argparse.REMAINDER)
    for name in SINGLE:
        ops.add_parser(name).add_argument("args", nargs=argparse.REMAINDER)
    return root


def main(argv=None):
    args = parser().parse_args(argv)
    if args.operation == "doctor":
        outcome = doctor(REPO, args.config, Host())
    else:
        outcome = unsupported(" ".join(filter(None, (args.operation, getattr(args, "action", None)))))
    print(json.dumps(outcome, indent=2) if args.json else summary(outcome))
    if outcome["status"] == "unsupported":
        return EXIT_UNSUPPORTED
    return EXIT_OK if outcome["status"] == "success" else EXIT_FAILED


if __name__ == "__main__":
    sys.exit(main())
