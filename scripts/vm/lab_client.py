#!/usr/bin/env python3
"""`lab`: an issue agent's controller, from inside its worker.

Staged into the worker for each agent turn (docs/design/agent-lab.md §Agent
sessions). Every command is one request to the session broker on the host,
through the socket in $BUSYBEE_LAB_SOCKET; the broker serves it for this
worker only. Prints the JSON result; exits 0 on success, 1 otherwise, 2 on
usage errors. Standard library only.

  lab inspect | console | collect | checkpoint | reset | fetch | handoff
  lab signal SIGNAL PID
  lab exec [--cwd D] [--env N=V] [--timeout S] [--detach] -- ARGV...
  lab status EXEC | wait EXEC [--timeout S] | read EXEC stdout|stderr [--offset N]
  lab terminal open [--cols C --rows R] [--cwd D] [--env N=V] [--timeout S] -- ARGV...
  lab terminal send H (--text T | --key K... | --bytes HEX)
  lab terminal resize H --cols C --rows R
  lab terminal capture H [--expect TEXT] [--timeout S]
  lab scenario ID --mode cold|prepared [--bin-dir D]
  lab push [--force-with-lease]
  lab pr status | view | ready | create --title T --body-file F | comment --body-file F
  lab request JSON          (a raw request)

`--run RUN` and `--target TARGET` address a request explicitly; the broker
refuses any run but this worker's and any target but the worker.
"""
import argparse
import json
import os
import socket
import sys


def send(path, payload):
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.connect(path)
        s.sendall((json.dumps(payload) + "\n").encode())
        data = b""
        while not data.endswith(b"\n"):
            chunk = s.recv(65536)
            if not chunk:
                break
            data += chunk
    return json.loads(data)


def env_map(items):
    bad = [i for i in items if "=" not in i]
    if bad:
        raise SystemExit(f"lab: --env takes NAME=VALUE, not {', '.join(bad)}")
    return dict(i.partition("=")[::2] for i in items)


def parser():
    root = argparse.ArgumentParser(prog="lab", description=__doc__,
                                   formatter_class=argparse.RawDescriptionHelpFormatter)
    root.add_argument("--run", help="address a run explicitly (only this worker's is served)")
    root.add_argument("--target", help="address a target explicitly (only 'worker' is served)")
    ops = root.add_subparsers(dest="op", required=True)
    for name in ("inspect", "collect", "checkpoint", "reset", "fetch", "handoff"):
        ops.add_parser(name)
    ops.add_parser("console")
    sig = ops.add_parser("signal")
    sig.add_argument("signal")
    sig.add_argument("pid")
    run = ops.add_parser("exec")
    run.add_argument("--cwd")
    run.add_argument("--env", action="append", default=[])
    run.add_argument("--timeout", type=int)
    run.add_argument("--detach", action="store_true")
    ops.add_parser("status").add_argument("exec")
    wait = ops.add_parser("wait")
    wait.add_argument("exec")
    wait.add_argument("--timeout", type=int)
    read = ops.add_parser("read")
    read.add_argument("exec")
    read.add_argument("stream", choices=("stdout", "stderr"))
    read.add_argument("--offset", type=int, default=0)
    term = ops.add_parser("terminal").add_subparsers(dest="action", required=True)
    topen = term.add_parser("open")
    topen.add_argument("--cols", type=int, default=120)
    topen.add_argument("--rows", type=int, default=40)
    topen.add_argument("--cwd")
    topen.add_argument("--env", action="append", default=[])
    topen.add_argument("--timeout", type=int)
    tsend = term.add_parser("send")
    tsend.add_argument("handle")
    what = tsend.add_mutually_exclusive_group(required=True)
    what.add_argument("--text")
    what.add_argument("--key", action="append")
    what.add_argument("--bytes")
    tresize = term.add_parser("resize")
    tresize.add_argument("handle")
    tresize.add_argument("--cols", type=int, required=True)
    tresize.add_argument("--rows", type=int, required=True)
    tcap = term.add_parser("capture")
    tcap.add_argument("handle")
    tcap.add_argument("--expect")
    tcap.add_argument("--timeout", type=int, default=10)
    scen = ops.add_parser("scenario")
    scen.add_argument("scenario")
    scen.add_argument("--mode", required=True, choices=("cold", "prepared"))
    scen.add_argument("--bin-dir", default="build/debug")
    ops.add_parser("push").add_argument("--force-with-lease", action="store_true")
    pr = ops.add_parser("pr").add_subparsers(dest="action", required=True)
    for name in ("status", "view", "ready"):
        pr.add_parser(name)
    create = pr.add_parser("create")
    create.add_argument("--title", required=True)
    create.add_argument("--body-file", required=True)
    pr.add_parser("comment").add_argument("--body-file", required=True)
    ops.add_parser("request").add_argument("json")
    return root


def build(args, command):
    """The request for parsed arguments; `command` is the argv after `--`."""
    if args.op == "request":
        return json.loads(args.json)
    a = {}
    op = args.op
    if op == "console":
        op = "console-capture"
    elif op == "signal":
        a = {"signal": args.signal, "pid": args.pid}
    elif op == "exec":
        a = {"argv": command, "cwd": args.cwd, "env": env_map(args.env), "timeout": args.timeout,
             "detach": args.detach}
    elif op in ("status", "wait", "read"):
        a = {"exec": args.exec}
        if op == "wait":
            a["timeout"] = args.timeout
        if op == "read":
            a.update(stream=args.stream, offset=args.offset)
    elif op == "terminal":
        op = f"terminal-{args.action}"
        if args.action == "open":
            a = {"argv": command, "cols": args.cols, "rows": args.rows, "cwd": args.cwd, "env": env_map(args.env),
                 "timeout": args.timeout}
        elif args.action == "send":
            a = {"handle": args.handle, "text": args.text, "keys": args.key, "bytes": args.bytes}
        elif args.action == "resize":
            a = {"handle": args.handle, "cols": args.cols, "rows": args.rows}
        else:
            a = {"handle": args.handle, "expect": args.expect, "timeout": args.timeout}
    elif op == "scenario":
        a = {"scenario": args.scenario, "mode": args.mode, "bin_dir": args.bin_dir}
    elif op == "push":
        a = {"force_with_lease": args.force_with_lease}
    elif op == "pr":
        op = f"pr-{args.action}"
        if args.action == "create":
            a = {"title": args.title, "body": open(args.body_file).read()}
        elif args.action == "comment":
            a = {"body": open(args.body_file).read()}
    request = {"op": op, "args": a}
    if args.run is not None:
        request["run"] = args.run
    if args.target is not None:
        request["target"] = args.target
    return request


def main(argv=None):
    argv = sys.argv[1:] if argv is None else argv
    split = argv.index("--") if "--" in argv else len(argv)
    args = parser().parse_args(argv[:split])
    path = os.environ.get("BUSYBEE_LAB_SOCKET")
    if not path:
        print("lab: BUSYBEE_LAB_SOCKET is not set; lab runs inside an agent turn", file=sys.stderr)
        return 2
    reply = send(path, build(args, argv[split + 1:]))
    print(json.dumps(reply, indent=2))
    return 0 if reply.get("status") == "success" else 1


if __name__ == "__main__":
    sys.exit(main())
