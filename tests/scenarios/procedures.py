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
import signal
import time

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


# The live monitor (#77). Long labels so a narrow terminal must cut them.
WIDE, NARROW = (120, 40), (60, 20)
LABELS = ("running-task-with-a-label-long-enough-to-be-cut-in-a-narrow-terminal",
          "queued-task-with-a-label-long-enough-to-be-cut-in-a-narrow-terminal")
# Seconds: the monitor polls status every second and gives up on a reply after one.
VIEW_S = 15
STALE_S = 10
QUIT_S = 5


def _lease_rows(screen, leases):
    """Each lease's row on a monitor screen (`#id state ...`), or None."""
    lines = screen.splitlines()
    return {lease["id"]: next((line for line in lines if f"#{lease['id']} " in line and lease["state"] in line), None)
            for lease in leases}


def _status(fx, deadline, step):
    done = fx.run(step, [str(fx.bin / "busybee"), "status", "--json"], deadline, cap=STATUS_S)
    try:
        return json.loads(done["stdout"]) if done["exit_code"] == 0 else None
    except ValueError:
        return None


def _until(check, seconds, deadline):
    """Poll `check` until it returns something truthy, for at most `seconds`."""
    until = min(deadline, time.monotonic() + seconds)
    while True:
        found = check()
        if found or time.monotonic() >= until:
            return found
        time.sleep(0.1)


def _exits(t, seconds, deadline):
    """Whether the terminal's program exited within `seconds`, how long it took,
    and the pane as zellij last reported it."""
    started = time.monotonic()
    last = {}

    def ended():
        last["pane"] = t.app()
        return last["pane"] is None or last["pane"].get("exited")

    exited = bool(_until(ended, seconds, deadline))
    pane = last.get("pane")
    return exited, round(time.monotonic() - started, 3), \
        {k: pane.get(k) for k in ("exited", "exit_status", "is_held")} if pane else None


def _states(leases):
    return sorted(lease["state"] for lease in leases)


def accounted(status):
    """One running lease holding every held token, one queued lease holding
    none, and the pool's tokens all accounted for."""
    leases = status.get("leases") or []
    running = [lease for lease in leases if lease["state"] == "running"]
    queued = [lease for lease in leases if lease["state"] == "queued"]
    return (len(running), len(queued)) == (1, 1) and status["held"] == running[0]["cores"] \
        and queued[0]["cores"] == 0 and status["held"] + status["free"] + status["approx_in_use"] == status["pool_size"]


def _listed(leases):
    return ", ".join(f"#{lease['id']} {lease['state']}" for lease in leases) or "no leases"


def live_monitor(fx, check, deadline):
    """#77: the workspace-built monitor, observed through a real terminal at two
    sizes, with an idle pool, running and queued work, a status reply that
    never comes and a daemon that is gone, and quit by q and by Ctrl-C. Every
    view is paired with `busybee status --json` taken beside it."""
    busybee = str(fx.bin / "busybee")
    t = fx.terminal("monitor", [busybee, "monitor"], *WIDE, deadline)
    idle = _status(fx, deadline, "status-idle")
    shown = t.wait_text("free", min(deadline, time.monotonic() + VIEW_S))
    capture = t.capture(min(deadline, time.monotonic() + VIEW_S), label="idle")
    check.observe("idle", {"status": idle, "capture": capture["capture"], "screen": capture["screen"]})
    pool = (idle or {}).get("pool_size")
    check.record("monitor_shows_idle_pool", shown and idle is not None and idle.get("leases") == []
                 and f"{pool} free" in capture["screen"],
                 f"status: {len((idle or {}).get('leases') or [])} lease(s), pool {pool}; the screen "
                 f"{'shows' if pool is not None and f'{pool} free' in capture['screen'] else 'does not show'} "
                 f"{pool} free")

    work = [fx.run(f"work-{n}", [busybee, "--detach", "--name", label, "--", "sleep", "600"], deadline, cap=STATUS_S)
            for n, label in enumerate(LABELS)]
    seen = {}

    def submitted():
        seen["status"] = _status(fx, deadline, "status-work")
        return _states((seen["status"] or {}).get("leases") or []) == ["queued", "running"]

    _until(submitted, VIEW_S, deadline)
    status = seen["status"]
    leases = (status or {}).get("leases") or []
    check.observe("work", {"submitted": [{"exit_code": w["exit_code"], "stdout": w["stdout"][-500:],
                                          "stderr": w["stderr"][-500:]} for w in work], "status": status})
    check.record("work_is_running_and_queued", status is not None and accounted(status),
                 f"status lists {_listed(leases)}; " + (f"{status['held']} held, {status['free']} free, "
                 f"~{status['approx_in_use']} in use of {status['pool_size']}" if status else "no status reply"))

    def views(name, size, since=0):
        # The monitor's own drawing at this size: output after `since`, then its rows.
        _until(lambda: t.output_bytes() > since and leases and all(_lease_rows(t.screen(), leases).values()),
               VIEW_S, deadline)
        capture = t.capture(min(deadline, time.monotonic() + VIEW_S), label=name)
        beside = _status(fx, deadline, f"status-{name}")
        rows = _lease_rows(capture["screen"], leases)
        check.observe(name, {"size": size, "capture": capture["capture"], "status": beside, "rows": rows,
                             "screen": capture["screen"]})
        return capture, beside, rows

    wide, beside, rows = views("wide", WIDE)
    check.record("monitor_shows_running_and_queued_work", bool(leases) and all(rows.values()),
                 "; ".join(f"#{i}: {'shown' if r else 'missing'}" for i, r in rows.items()) or "no leases to show")
    legend = (f"{beside['pool_size']} tokens · {beside['held']} held" if beside else None)
    check.record("monitor_matches_status", beside is not None and legend in wide["screen"]
                 and f"{beside['free']} free" in wide["screen"],
                 f"status says {legend}, {beside['free'] if beside else '?'} free; the screen "
                 f"{'agrees' if beside and legend in wide['screen'] else 'differs'}")

    t.resize(*NARROW, min(deadline, time.monotonic() + VIEW_S))
    narrow, _, rows = views("narrow", NARROW, since=t.output_bytes())
    check.record("narrow_monitor_shows_work", bool(leases) and all(rows.values()),
                 "; ".join(f"#{i}: {r.strip() if r else 'missing'}" for i, r in rows.items()) or "no leases")

    bzbd = next((p["pid"] for p in fx.procs(fx.root, fx.user.uid) if p["comm"] == "bzbd"), None)
    if bzbd is None:
        check.not_reached("monitor_marks_unanswered_status_stale", "bzbd was not running")
        check.not_reached("quit_during_slow_status_exits", "bzbd was not running")
    else:
        os.kill(bzbd, signal.SIGSTOP)  # status requests now get no reply
        stale = t.wait_text("stale", min(deadline, time.monotonic() + STALE_S))
        capture = t.capture(min(deadline, time.monotonic() + VIEW_S), label="stale")
        check.observe("stale", {"capture": capture["capture"], "screen": capture["screen"]})
        check.record("monitor_marks_unanswered_status_stale", stale,
                     f"within {STALE_S}s of bzbd stopping the screen {'marks' if stale else 'does not mark'} its data stale")
        t.send_text("q")
        exited, took, pane = _exits(t, QUIT_S, deadline)
        check.observe("quit", {"key": "q", "seconds": took, "pane": pane})
        check.record("quit_during_slow_status_exits", exited,
                     f"q {'ended' if exited else 'did not end'} the monitor within {QUIT_S}s ({took}s) "
                     "while status went unanswered")
        os.kill(bzbd, signal.SIGKILL)

    t2 = fx.terminal("monitor-gone", [busybee, "monitor"], *WIDE, deadline)
    gone = t2.wait_text("not running", min(deadline, time.monotonic() + VIEW_S))
    capture = t2.capture(min(deadline, time.monotonic() + VIEW_S), label="daemon-gone")
    check.observe("daemon_gone", {"capture": capture["capture"], "screen": capture["screen"],
                                  "status": _status(fx, deadline, "status-gone")})
    check.record("monitor_reports_daemon_not_running", gone,
                 f"with bzbd gone the screen {'says' if gone else 'does not say'} it is not running")
    t2.send_keys("Ctrl c")
    exited, took, pane = _exits(t2, QUIT_S, deadline)
    check.observe("ctrl_c", {"seconds": took, "pane": pane})
    check.record("ctrl_c_exits", exited, f"Ctrl-C {'ended' if exited else 'did not end'} the monitor within "
                 f"{QUIT_S}s ({took}s)")


PROCEDURES = {
    "task_permissions": Procedure(task_permissions, ("task_runs", "task_directory_traversable",
                                                     "task_executable_output_runs", "lease_released")),
    "wedged_task": Procedure(wedged_task, ("task_completes",)),
    "live_monitor": Procedure(live_monitor, ("monitor_shows_idle_pool", "work_is_running_and_queued",
                                             "monitor_shows_running_and_queued_work", "monitor_matches_status",
                                             "narrow_monitor_shows_work", "monitor_marks_unanswered_status_stale",
                                             "quit_during_slow_status_exits", "monitor_reports_daemon_not_running",
                                             "ctrl_c_exits")),
}
