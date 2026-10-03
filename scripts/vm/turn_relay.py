#!/usr/bin/env python3
"""One agent turn's streams, relayed so that the turn ends with its runner.

Staged into the worker for each turn (scripts/vm/session.py). The runner is
the guest administrator: a daemon it starts inherits its stdin and stdout. Were
those the SSH channel, the channel, and so the turn, would stay open as long
as the daemon lives. Here the runner gets pipes to this relay instead; when it
exits, whatever it wrote is drained and the relay exits with its status,
closing the channel while its daemons keep running in the guest. The status
is also written to `status` beside the relay, so the controller can tell the
runner's exit from one of ssh or the shell around it.

  relay ARGV...
"""
import os
import selectors
import subprocess
import sys
import threading
import time

# How long output still trickling in after the runner exits is relayed.
DRAIN_S = 1.0


def write_all(fd, data):
    while data:
        data = data[os.write(fd, data):]


def feed(stdin):
    """stdin to the runner, in a thread: it may be a file or /dev/null, which
    no selector accepts."""
    try:
        for data in iter(lambda: os.read(0, 65536), b""):
            write_all(stdin.fileno(), data)
    except (BrokenPipeError, OSError):
        pass
    finally:
        try:
            stdin.close()
        except OSError:
            pass


def main(argv):
    if not argv:
        print("relay: usage: relay ARGV...", file=sys.stderr)
        return 2
    try:
        proc = subprocess.Popen(argv, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    except OSError as err:
        print(f"relay: {argv[0]}: {err.strerror}", file=sys.stderr)
        with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "status"), "w") as f:
            f.write("127\n")
        return 127
    threading.Thread(target=feed, args=(proc.stdin,), daemon=True).start()
    sel = selectors.DefaultSelector()
    sel.register(proc.stdout.fileno(), selectors.EVENT_READ, 1)
    sel.register(proc.stderr.fileno(), selectors.EVENT_READ, 2)
    ended = None
    while sel.get_map():
        events = sel.select(timeout=0.2)
        for key, _ in events:
            data = os.read(key.fd, 65536)
            if not data:
                sel.unregister(key.fd)
                continue
            write_all(key.data, data)
        if ended is None and proc.poll() is not None:
            ended = time.monotonic()
        if ended is not None and (not events or time.monotonic() - ended > DRAIN_S):
            break  # the runner is gone; a daemon it left may hold its pipes open
    code = proc.wait()
    code = code if code >= 0 else 128 - code
    with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "status"), "w") as f:
        f.write(f"{code}\n")
    return code


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
