"""Scenario procedures: the steps and assertions a scenario's metadata names.

Each procedure takes the fixture, the Check that records its assertions and
observations, and the scenario deadline. Steps run through `fx.run`, as the
scenario user and bounded by that deadline; a step still running at the
deadline raises runner.Deadline. Assertions about the product are recorded
here; everything about the harness itself is the runner's business.
"""
from collections import namedtuple
import json
import os

Procedure = namedtuple("Procedure", "fn assertions")
STATUS_S = 15

# The task: report its umask, create a directory and a file inside it, and copy
# an executable without repairing its mode, as a build writes its output, then
# run the copy. Every step is attempted and its status recorded, so one failure
# does not hide the next; the probe itself exits 0, so busybee's exit status
# says only whether the task ran.
PROBE = """t=$1; o=probe.txt
echo "umask=$(umask)" > "$o"
mkdir d; echo "mkdir=$?" >> "$o"
touch d/f; echo "nested=$?" >> "$o"
cp "$t" t; echo "cp=$?" >> "$o"
./t; echo "exec=$?" >> "$o"
exit 0
"""


def parse_probe(text):
    return dict(line.split("=", 1) for line in text.splitlines() if "=" in line)


def released(status):
    """Whether `busybee status --json` shows every lease gone and the pool whole."""
    if not isinstance(status, dict):
        return False, "busybee status --json gave no parseable reply"
    leases, held, free, pool = status.get("leases"), status.get("held"), status.get("free"), status.get("pool_size")
    ok = leases == [] and held == 0 and free == pool
    return ok, f"{len(leases or [])} lease(s), {held} token(s) held, {free} of {pool} free"


def _mode(path):
    return f"{os.lstat(path).st_mode & 0o7777:04o}" if os.path.lexists(path) else None


def _last_line(text):
    lines = [line for line in text.strip().splitlines() if line.strip()]
    return lines[-1] if lines else ""


def status_snapshot(fx, check, deadline):
    """Record the broker's view and whether every lease was released."""
    done = fx.run("status", [str(fx.bin / "busybee"), "status", "--json"], deadline, cap=STATUS_S)
    try:
        status = json.loads(done["stdout"]) if done["exit_code"] == 0 else None
    except ValueError:
        status = None
    check.observe("status", status if status is not None else {"exit_code": done["exit_code"],
                                                              "stderr": done["stderr"][-2000:]})
    ok, detail = released(status)
    if status is None:
        detail += f" (exit {done['exit_code']}: {_last_line(done['stderr'])})"
    check.record("lease_released", ok, detail)


def task_permissions(fx, check, deadline):
    """#69: a task started through busybee creates a traversable directory and
    an executable it can run, and its lease is released afterwards."""
    work = fx.root / "work"
    for name, text, mode in (("probe.sh", PROBE, 0o644), ("tool", "#!/bin/sh\nexit 0\n", 0o755)):
        (work / name).write_text(text)
        (work / name).chmod(mode)
        os.chown(work / name, fx.user.uid, fx.user.gid)
    done = fx.run("task", [str(fx.bin / "busybee"), "--", "sh", "probe.sh", "tool"], deadline)
    facts = parse_probe((work / "probe.txt").read_text()) if (work / "probe.txt").is_file() else {}
    check.observe("task", {"exit_code": done["exit_code"], "stderr": done["stderr"][-2000:], "probe": facts,
                           "umask": facts.get("umask"), "directory_mode": _mode(work / "d"),
                           "executable_mode": _mode(work / "t"), "copied_from_mode": _mode(work / "tool")})
    ran = done["exit_code"] == 0 and "umask" in facts
    check.record("task_runs", ran, f"busybee exited {done['exit_code']}: {_last_line(done['stderr'])}")
    if "umask" not in facts:
        why = "the task never ran"
        check.not_reached("task_directory_traversable", why)
        check.not_reached("task_executable_output_runs", why)
    else:
        check.record("task_directory_traversable", facts.get("mkdir") == "0" and facts.get("nested") == "0",
                     f"umask {facts['umask']}: mkdir exited {facts.get('mkdir')}, a file inside it "
                     f"{facts.get('nested')}; directory mode {_mode(work / 'd')}")
        check.record("task_executable_output_runs", facts.get("cp") == "0" and facts.get("exec") == "0",
                     f"umask {facts['umask']}: copy exited {facts.get('cp')}, running it {facts.get('exec')}; "
                     f"mode {_mode(work / 't')}")
    status_snapshot(fx, check, deadline)


def wedged_task(fx, check, deadline):
    """A task that never finishes: the scenario deadline, not the task, ends it."""
    done = fx.run("task", [str(fx.bin / "busybee"), "--", "sleep", "86400"], deadline)
    check.record("task_completes", done["exit_code"] == 0, f"busybee exited {done['exit_code']}")


PROCEDURES = {
    "task_permissions": Procedure(task_permissions, ("task_runs", "task_directory_traversable",
                                                     "task_executable_output_runs", "lease_released")),
    "wedged_task": Procedure(wedged_task, ("task_completes",)),
}
