"""Versioned contracts shared by every controller operation.

Local configuration (TOML), template manifests, worker ownership records and
operation results. See docs/design/agent-lab.md §Controller interface. Pure:
nothing here touches Parallels or the host beyond resolving paths.
"""
from datetime import datetime, timezone
from pathlib import Path
import re
import secrets

CONFIG_SCHEMA = 1
TEMPLATE_SCHEMA = "busybee.vm.template/v2"
WORKER_SCHEMA = "busybee.vm.worker/v1"
RESULT_SCHEMA = "busybee.vm.result/v1"

# What a controller operation can end as. Only `success` is a pass.
RESULT_STATES = ("success", "product_failure", "environment_failure", "timeout", "cancelled",
                 "incomplete_collection", "unsupported")

CLONE_STRATEGIES = ("linked", "full")
GUEST_OS = ("linux", "macos")
STATE_ROOT = Path("build/vm")

# Inclusive bounds. Deadlines are seconds; every one is finite and nested:
# a command fits in its scenario, a scenario in its run.
DEADLINE_BOUNDS = {"command": (1, 6 * 3600), "scenario": (1, 12 * 3600), "run": (1, 24 * 3600),
                   "cleanup": (1, 3600)}
BUDGET_BOUNDS = {"cpus": (1, 256), "memory_mib": (1024, 1024 * 1024), "storage_gib": (8, 16 * 1024)}

UUID = re.compile(r"^\{[0-9a-f]{8}(-[0-9a-f]{4}){3}-[0-9a-f]{12}\}$")
RUN_ID = re.compile(r"^r-\d{8}T\d{6}Z-[0-9a-f]{6}$")
TIMESTAMP = "%Y-%m-%dT%H:%M:%SZ"


def _integer(value):
    # bool is an int in Python; a budget of `true` is a typo, not 1.
    return isinstance(value, int) and not isinstance(value, bool)


def _bounded(section, values, bounds, errors):
    if not isinstance(values, dict):
        errors.append(("config_invalid", f"[{section}] must be a table"))
        return
    for key in sorted(set(values) - set(bounds)):
        errors.append(("config_invalid", f"[{section}] has unknown key {key!r}"))
    for key, (low, high) in bounds.items():
        value = values.get(key)
        if value is None:
            errors.append(("config_invalid", f"[{section}] {key} is required"))
        elif not _integer(value):
            errors.append(("config_invalid", f"[{section}] {key} must be an integer"))
        elif not low <= value <= high:
            errors.append(("config_invalid", f"[{section}] {key} = {value} is outside {low}..{high}"))


def state_dir(config, repo):
    return (repo / config["state_dir"]).resolve()


def config_errors(config, repo):
    """Every problem with a parsed local config, as (code, message) pairs."""
    errors = []
    known = {"schema", "state_dir", "clone_strategy", "parallels", "deadlines", "budget", "templates"}
    for key in sorted(set(config) - known):
        errors.append(("config_invalid", f"unknown key {key!r}"))
    if config.get("schema") != CONFIG_SCHEMA:
        errors.append(("config_invalid", f"schema must be {CONFIG_SCHEMA}"))

    raw_state = config.get("state_dir")
    state = None
    if not isinstance(raw_state, str) or Path(raw_state).is_absolute():
        errors.append(("config_invalid", "state_dir must be a path relative to the repository"))
    else:
        # Resolved, so `..` and symlinks cannot carry state out of the ignored tree.
        state = (repo / raw_state).resolve()
        if not state.is_relative_to((repo / STATE_ROOT).resolve()) or not state.is_relative_to(repo.resolve()):
            errors.append(("config_invalid", f"state_dir must stay inside {STATE_ROOT}/"))
            state = None

    strategy = config.get("clone_strategy")
    if strategy not in CLONE_STRATEGIES:
        errors.append(("clone_mode_unsupported",
                       f"clone_strategy {strategy!r} is not one of {', '.join(CLONE_STRATEGIES)}"))

    tools = config.get("parallels", {})
    if not isinstance(tools, dict) or set(tools) - {"prlctl", "prlsrvctl"} or \
            not all(isinstance(v, str) and Path(v).is_absolute() for v in tools.values()):
        errors.append(("config_invalid", "[parallels] may only set absolute prlctl/prlsrvctl paths"))

    _bounded("deadlines", config.get("deadlines"), DEADLINE_BOUNDS, errors)
    deadlines = config.get("deadlines")
    if isinstance(deadlines, dict) and all(_integer(deadlines.get(k)) for k in ("command", "scenario", "run")):
        if not deadlines["command"] <= deadlines["scenario"] <= deadlines["run"]:
            errors.append(("config_invalid", "deadlines must nest: command <= scenario <= run"))
    _bounded("budget", config.get("budget"), BUDGET_BOUNDS, errors)

    templates = config.get("templates", {})
    if not isinstance(templates, dict):
        errors.append(("config_invalid", "[templates] must be a table"))
        templates = {}
    for name, entry in templates.items():
        if name not in GUEST_OS or not isinstance(entry, dict) or set(entry) != {"manifest"} \
                or not isinstance(entry["manifest"], str):
            errors.append(("config_invalid", f"[templates.{name}] needs exactly a manifest path, "
                                             f"and the name must be one of {', '.join(GUEST_OS)}"))
        elif state and not (state / entry["manifest"]).resolve().is_relative_to(state):
            errors.append(("config_invalid", f"[templates.{name}] manifest must stay inside state_dir"))
    return errors


def manifest_errors(manifest):
    """Problems with a template manifest recorded by template validation."""
    fields = {"schema", "name", "candidate", "os", "arch", "vm_id", "snapshot_id", "provisioning_revision",
              "lock_hashes", "tools", "parallels_version", "clone_modes", "validated_at"}
    if not isinstance(manifest, dict):
        return ["manifest must be an object"]
    errors = [f"unknown field {k!r}" for k in sorted(set(manifest) - fields)]
    errors += [f"missing field {k!r}" for k in sorted(fields - set(manifest))]
    if errors:
        return errors
    if manifest["schema"] != TEMPLATE_SCHEMA:
        errors.append(f"schema must be {TEMPLATE_SCHEMA}")
    if manifest["os"] not in GUEST_OS:
        errors.append(f"os must be one of {', '.join(GUEST_OS)}")
    for key in ("vm_id", "snapshot_id"):
        if not isinstance(manifest[key], str) or not UUID.match(manifest[key]):
            errors.append(f"{key} must be a Parallels UUID")
    if not valid_run_id(manifest["candidate"]):
        errors.append("candidate must be the run id of the build that produced it")
    modes = manifest["clone_modes"]
    if not isinstance(modes, list) or not modes or not set(modes) <= set(CLONE_STRATEGIES):
        errors.append(f"clone_modes must be a non-empty subset of {', '.join(CLONE_STRATEGIES)}")
    for key in ("lock_hashes", "tools"):
        if not isinstance(manifest[key], dict):
            errors.append(f"{key} must be an object")
    return errors


def new_run_id(now=None):
    now = now or datetime.now(timezone.utc)
    return f"r-{now.strftime('%Y%m%dT%H%M%SZ')}-{secrets.token_hex(3)}"


def valid_run_id(value):
    return isinstance(value, str) and bool(RUN_ID.match(value))


def worker_name(run_id):
    """The Parallels VM name of a run's worker. Ownership is the registry
    record, not this name; the prefix only makes owned clones recognisable."""
    return f"busybee-lab-{run_id}"


def worker_errors(record):
    """Problems with a worker ownership record, written before guest work starts."""
    fields = {"schema", "run_id", "worker", "vm_id", "template", "snapshot_id", "clone_strategy",
              "created_at", "deadline"}
    errors = [f"unknown field {k!r}" for k in sorted(set(record) - fields)]
    errors += [f"missing field {k!r}" for k in sorted(fields - set(record))]
    if errors:
        return errors
    if record["schema"] != WORKER_SCHEMA:
        errors.append(f"schema must be {WORKER_SCHEMA}")
    if not valid_run_id(record["run_id"]):
        errors.append("run_id is malformed")
    elif record["worker"] != worker_name(record["run_id"]):
        errors.append("worker name does not belong to this run")
    for key in ("vm_id", "snapshot_id"):
        if not isinstance(record[key], str) or not UUID.match(record[key]):
            errors.append(f"{key} must be a Parallels UUID")
    if record["template"] not in GUEST_OS or record["clone_strategy"] not in CLONE_STRATEGIES:
        errors.append("template or clone_strategy is not a supported value")
    try:
        created = datetime.strptime(record["created_at"], TIMESTAMP)
        deadline = datetime.strptime(record["deadline"], TIMESTAMP)
        if deadline <= created:
            errors.append("deadline must be after created_at")
    except (TypeError, ValueError):
        errors.append(f"timestamps must be {TIMESTAMP}")
    return errors


def result(operation, status, summary, findings=(), data=None):
    if status not in RESULT_STATES:
        raise ValueError(f"unknown result state {status!r}")
    return {"schema": RESULT_SCHEMA, "operation": operation, "status": status, "summary": summary,
            "findings": [dict(f) for f in findings], "data": data or {}}


def finding(code, message, severity="error"):
    return {"code": code, "severity": severity, "message": message}
