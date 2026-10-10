"""`vmctl terminal open|send|resize|capture`: a real PTY in a worker, for an agent to operate.

See docs/design/agent-lab.md §Make UI behavior observable. `open` stages the
controller's terminal host (tests/scenarios/terminal.py) the way scenarios are
staged and starts it as a detached exec, so the terminal lives exactly as long
as that exec: the supervisor's command and run deadlines bound it, and
`signal`, reset and destroy end it. The host keeps one zellij session whose
pane runs the command under `script`. `send` and `resize` act on that pane;
`capture` records zellij's screen once it has settled, fetches the recording,
and replays and renders it here (screen.py).

Each handle is `runs/<run>/terminal/<NNNN>/`: `handle.json` (the controller's
record), and from the guest `state.json`, `recording/{output,input,timing}`
and `captures/NNNN.json`, beside which the host writes `NNNN.cells.json` and
the rendered `NNNN.png`. Rendered terminal images live only under `terminal/`;
Parallels screenshots of the VM's display stay under `console/`.
"""
from datetime import datetime, timezone
import json
import re
import secrets
import shlex
import shutil
import socket
import sys

import contracts
import scenario
import screen
import template
import worker

GUEST_ROOT = "/var/tmp/busybee-terminal"
# The holder starts inside the worker checkout's development shell.
OPEN_S = 120
HANDLE = re.compile(r"^\d{4}$")


def _host_module(repo):
    scenario._runner(repo)  # puts the controller's tests/scenarios on the path
    import terminal
    return terminal


def _stamp():
    return datetime.now(timezone.utc).strftime(contracts.TIMESTAMP)


def _handle_dir(workers, run_id, handle):
    if not HANDLE.match(str(handle)):
        raise worker.Refused("terminal_invalid", f"{handle!r} is not a terminal handle")
    hdir = worker.run_dir(workers.state, run_id) / "terminal" / handle
    if not (hdir / "handle.json").is_file():
        raise worker.Refused("terminal_invalid", f"run {run_id} has no terminal {handle}")
    return hdir


def _ready(workers, run_id):
    _, record = workers._owned(run_id)
    if record["status"] != "ready":
        raise worker.Refused("worker_not_ready", f"worker {run_id} is {record['status']}")
    return record


def _guest_state(g, workers, gdir):
    status, out, _ = g.run(f"cat {shlex.quote(gdir)}/state.json", workers._bound("command"), check=False)
    try:
        return json.loads(out) if status == 0 else None
    except ValueError:
        return None  # being rewritten


def open_terminal(workers, run_id, argv, cols, rows, cwd, env, timeout):
    op = "terminal open"
    host = _host_module(workers.repo)
    if not argv:
        raise worker.Refused("argv_empty", "terminal open needs a program to run")
    for key, value in (("cols", cols), ("rows", rows)):
        low, high = host.SIZE_BOUNDS[key]
        if not low <= value <= high:
            raise worker.Refused("size_invalid", f"{key} {value} is outside {low}..{high}")
    bad = [k for k in env if not worker.ENV_NAME.match(k)]
    if bad:
        raise worker.Refused("env_invalid", f"not environment variable names: {', '.join(map(repr, bad))}")
    record = _ready(workers, run_id)
    if record["template"] != "linux":
        # terminal.py needs util-linux script's advanced timing and /proc; a
        # macOS guest's terminal access is checked as SSH PTY transport only.
        raise worker.Refused("platform_unsupported", f"the terminal driver runs on linux workers, not "
                             f"{record['template']}")
    stage, digest = scenario.stage_runner(workers, run_id, record)
    hdir = worker._next_dir(worker.run_dir(workers.state, run_id) / "terminal")
    handle = hdir.name
    gdir = f"{GUEST_ROOT}/{run_id}/{handle}"
    session = f"bzt-{handle}-{secrets.token_hex(3)}"
    hold = ["nix", "develop", "-c", "python3", f"{stage}/terminal.py", "hold", "--dir", gdir, "--session", session,
            "--cols", str(cols), "--rows", str(rows), "--cwd", cwd, *(f"--env={k}={v}" for k, v in env.items()),
            "--", *argv]
    meta = {"handle": handle, "run_id": run_id, "guest_dir": gdir, "stage": stage, "runner_sha256": digest,
            "session": session, "argv": argv, "cwd": cwd, "env": env, "cols": cols, "rows": rows,
            "opened_at": _stamp(), "exec": None}
    template._write_json(hdir / "handle.json", meta)
    try:
        started = workers.exec(run_id, hold, worker.CHECKOUT, {}, timeout, detach=True)
    except worker.Refused:
        shutil.rmtree(hdir)  # nothing was queued: no terminal to keep a record of
        raise
    if started["status"] != "success":
        shutil.rmtree(hdir)
        return contracts.result(op, started["status"], f"terminal {handle} did not start", started["findings"])
    meta["exec"] = started["data"]["exec"]
    template._write_json(hdir / "handle.json", meta)
    g = workers._guest(record)
    until = workers.clock() + OPEN_S
    while True:
        state = _guest_state(g, workers, gdir)
        if state and state.get("status") == "ready":
            meta.update({k: state[k] for k in ("python", "pane", "pts", "client_pid", "holder_pid", "zellij")})
            template._write_json(hdir / "handle.json", meta)
            return contracts.result(op, "success", f"terminal {handle} is {cols}x{rows}; send, resize and "
                                    "capture take it", data=_summary(workers, run_id, meta))
        if state and state.get("status") in ("failed", "closed"):
            return contracts.result(op, "environment_failure", f"terminal {handle} could not start", [
                contracts.finding(state.get("code", "terminal_failed"), state.get("message", "the holder closed"))],
                {"handle": handle, "exec": meta["exec"]})
        if workers._handle(run_id, meta["exec"])["state"] == "finished":
            done = workers.wait(run_id, meta["exec"])
            tail = (workers.state / done["data"]["stderr"]).read_bytes()[-1000:].decode(errors="replace") \
                if "stderr" in done["data"] else ""
            return contracts.result(op, "environment_failure", f"terminal {handle} exited before it was ready", [
                contracts.finding("terminal_unavailable", f"the holder ended {done['status']}: {tail.strip()}")],
                {"handle": handle, "exec": meta["exec"]})
        if workers.clock() >= until:
            return contracts.result(op, "timeout", f"terminal {handle} was not ready", [
                contracts.finding("terminal_not_ready", f"not ready within {OPEN_S}s; its exec "
                                  f"{meta['exec']} still bounds it")], {"handle": handle, "exec": meta["exec"]})
        workers.sleep(worker.POLL_S)


def _summary(workers, run_id, meta):
    return {"run_id": run_id, "handle": meta["handle"], "exec": meta["exec"], "session": meta["session"],
            "pane": meta.get("pane"), "cols": meta["cols"], "rows": meta["rows"], "zellij": meta.get("zellij"),
            "holder_pid": meta.get("holder_pid"),
            "path": workers._rel(worker.run_dir(workers.state, run_id) / "terminal" / meta["handle"])}


def _op(workers, run_id, handle, op, args):
    """Run one terminal.py operation in the guest; returns (handle record,
    parsed reply, guest connection)."""
    hdir = _handle_dir(workers, run_id, handle)
    meta = json.loads((hdir / "handle.json").read_text())
    if "python" not in meta:
        raise worker.Refused("terminal_not_ready", f"terminal {handle} never became ready")
    record = _ready(workers, run_id)
    workers._window()
    g = workers._guest(record)
    command = shlex.join([meta["python"], f"{meta['stage']}/terminal.py", op, "--dir", meta["guest_dir"], *args])
    status, out, err = g.run(command, workers._bound("command"), check=False)
    try:
        reply = json.loads(out.strip().splitlines()[-1])
    except (ValueError, IndexError):
        reply = {"error": "terminal_unavailable", "message": f"terminal.py {op} exited {status}: {err.strip()[-500:]}"}
    return meta, reply, g


def _failed(op, handle, reply):
    return contracts.result(op, "environment_failure", f"terminal {handle}: {reply['error']}",
                            [contracts.finding(reply["error"], reply.get("message", ""))], {"handle": handle})


def send(workers, run_id, handle, text=None, keys=(), data=None):
    op = "terminal send"
    args = ["--text", text] if text is not None else [a for k in keys for a in ("--key", k)] if keys \
        else ["--bytes", data]
    _, reply, _ = _op(workers, run_id, handle, "send", args)
    if "error" in reply:
        return _failed(op, handle, reply)
    return contracts.result(op, "success", f"sent to terminal {handle}", data={"run_id": run_id, "handle": handle})


def resize(workers, run_id, handle, cols, rows):
    op = "terminal resize"
    _, reply, _ = _op(workers, run_id, handle, "resize", ["--cols", str(cols), "--rows", str(rows)])
    if "error" in reply:
        return _failed(op, handle, reply)
    return contracts.result(op, "success", f"terminal {handle} is {cols}x{rows}", data={
        "run_id": run_id, "handle": handle, "cols": cols, "rows": rows})


def capture(workers, run_id, handle, expect=None, timeout=10):
    """Capture the screen (waiting first for `expect` when given), fetch the
    recording and render it. A terminal whose command has ended can still be
    captured: its recording is replayed to the end, and zellij's screen, gone
    with its session, is recorded as absent."""
    op = "terminal capture"
    args = ["--timeout", str(timeout)] + (["--expect", expect] if expect is not None else [])
    meta, reply, g = _op(workers, run_id, handle, "capture", args)
    if "error" in reply and reply["error"] != "terminal_closed":
        return _failed(op, handle, reply)
    hdir = _handle_dir(workers, run_id, handle)
    worker.fetch_terminal(g, workers._bound("command"), meta["guest_dir"], hdir)
    try:
        if "error" in reply:
            reply = _final_capture(workers, hdir)
        screen.render_handle(hdir)
    except (screen.RecordingError, screen.RenderError) as err:
        return contracts.result(op, "environment_failure", f"terminal {handle} could not be rendered", [
            contracts.finding("terminal_render_failed", str(err))], {"handle": handle})
    cells = json.loads((hdir / "captures" / f"{reply['capture']}.cells.json").read_text())
    state = json.loads((hdir / "state.json").read_text())
    data = {"run_id": run_id, "handle": handle, "capture": reply["capture"], "status": state.get("status"),
            "app": state.get("app"), "cols": cells["size"]["cols"], "rows": cells["size"]["rows"],
            "settled": reply.get("settled"), "agrees": cells["agrees"], "text": cells["text"],
            "zellij_text": cells["zellij_text"], "missing_glyphs": cells["render"]["missing_glyphs"],
            "image": workers._rel(hdir / "captures" / f"{reply['capture']}.png"),
            "cells": workers._rel(hdir / "captures" / f"{reply['capture']}.cells.json")}
    findings = []
    if expect is not None and not reply.get("expect_seen"):
        findings.append(contracts.finding("expect_not_seen", f"{expect!r} did not appear within {timeout}s; "
                                          "the capture shows what did"))
        return contracts.result(op, "timeout", f"terminal {handle}: {expect!r} not seen", findings, data)
    if reply.get("settled") is False:
        findings.append(contracts.finding("capture_unsettled", "output was still changing at the deadline", "warning"))
    return contracts.result(op, "success", f"terminal {handle} capture {reply['capture']}", findings, data)


def _final_capture(workers, hdir):
    """A capture of a closed terminal: the whole recording, without zellij's view."""
    recording = screen.Recording.load(hdir / "recording")
    name = _host_module(workers.repo).next_capture(hdir / "captures")
    sizes = recording.sizes()
    if not sizes:
        raise screen.RecordingError("the terminal never had a size")
    size = sizes[-1]
    capture = {"schema": "busybee.terminal.capture/v1", "capture": name, "label": "closed", "at": _stamp(),
               "output_bytes": (hdir / "recording" / "output").stat().st_size, "settled": True,
               "cols": size["cols"], "rows": size["rows"], "screen": None, "ansi": None, "pane": None}
    (hdir / "captures" / f"{name}.json").write_text(json.dumps(capture, indent=2) + "\n")
    return capture


# How long to wait for the SSH local forward to accept a connection.
FORWARD_READY_S = 10
# Seconds between forward-ready polls.
FORWARD_POLL_S = 0.2


def _free_host_port():
    """A free loopback port on the host, released just before being returned."""
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def watch(workers, run_id, handle, json_output=False):
    """Start a read-only live view of an open terminal handle.

    Starts the zellij web server in the guest (127.0.0.1 only), creates a
    read-only token, and opens an SSH local forward from the host's loopback to
    the guest's web server.  Prints the access URL, then blocks until the
    handle's exec ends, the worker is no longer ready, or the run deadline
    passes.  Revokes the token and kills the forward on exit.

    Returns a contracts result on setup failure, or None (already printed) on a
    normal watch cycle that ended cleanly or was interrupted.
    """
    op = "terminal watch"
    hdir = _handle_dir(workers, run_id, handle)
    meta = json.loads((hdir / "handle.json").read_text())
    if "python" not in meta:
        raise worker.Refused("terminal_not_ready", f"terminal {handle} never became ready")
    record = _ready(workers, run_id)
    workers._window()
    g = workers._guest(record)

    # Step 1: Start web server in guest and create a read-only token.
    start_cmd = shlex.join([meta["python"], f"{meta['stage']}/terminal.py",
                            "watch_start", "--dir", meta["guest_dir"]])
    status, out, err = g.run(start_cmd, workers._bound("command"), check=False)
    try:
        reply = json.loads(out.strip().splitlines()[-1])
    except (ValueError, IndexError):
        reply = {"error": "terminal_unavailable",
                 "message": f"terminal.py watch_start exited {status}: {err.strip()[-500:]}"}
    if "error" in reply:
        return _failed(op, handle, reply)

    guest_port = reply["port"]
    token = reply["token"]
    token_name = reply["token_name"]

    # Step 2: Open an SSH local forward on the host's loopback.
    host_port = _free_host_port()
    fwd = g.local_forward(host_port, guest_port)

    # Wait for the forward to accept connections.
    deadline = workers.clock() + FORWARD_READY_S
    while workers.clock() < deadline:
        if fwd.poll() is not None:
            err_tail = fwd.stderr.read().decode(errors="replace")[-500:]
            return contracts.result(op, "environment_failure",
                                    f"SSH forward for terminal {handle} failed",
                                    [contracts.finding("forward_failed", err_tail)],
                                    {"handle": handle})
        try:
            socket.create_connection(("127.0.0.1", host_port), timeout=1).close()
            break
        except OSError:
            workers.sleep(FORWARD_POLL_S)
    else:
        fwd.kill()
        fwd.wait()
        return contracts.result(op, "timeout",
                                f"SSH forward for terminal {handle} did not come up in {FORWARD_READY_S}s",
                                [contracts.finding("forward_timeout",
                                                   "the local forward was not ready in time")],
                                {"handle": handle})

    # Step 3: Print the access URL immediately.
    url = f"http://127.0.0.1:{host_port}/?token={token}&session={meta['session']}"
    result = contracts.result(op, "success",
                              f"terminal {handle}: watching at http://127.0.0.1:{host_port}/",
                              data={"url": url, "host_port": host_port, "guest_port": guest_port,
                                    "token_name": token_name, "session": meta["session"],
                                    "handle": handle, "run_id": run_id})
    print(json.dumps(result, indent=2) if json_output else
          f"{result['operation']}: {result['status']} ({result['summary']})\n  url: {url}")
    sys.stdout.flush()

    # Step 4: Block until the handle, worker or deadline ends.
    run_dl = worker.run_deadline(record)
    ended_by = "unknown"
    try:
        while True:
            if workers.clock() >= run_dl:
                ended_by = "run_deadline"
                break
            try:
                _, record_now = workers._owned(run_id)
                if record_now["status"] != "ready":
                    ended_by = "worker_not_ready"
                    break
            except worker.Refused:
                ended_by = "worker_gone"
                break
            if workers._handle(run_id, meta["exec"])["state"] == "finished":
                ended_by = "handle_closed"
                break
            if fwd.poll() is not None:
                ended_by = "forward_died"
                break
            workers.sleep(worker.POLL_S)
    except KeyboardInterrupt:
        ended_by = "cancelled"

    # Step 5: Clean up — revoke the token and kill the forward.
    fwd.kill()
    fwd.wait()
    try:
        stop_cmd = shlex.join([meta["python"], f"{meta['stage']}/terminal.py",
                               "watch_stop", "--dir", meta["guest_dir"],
                               "--token-name", token_name])
        g.run(stop_cmd, workers._bound("command"), check=False)
    except Exception:  # best-effort; guest may be gone
        pass

    return None  # output was already printed; caller should exit 0
