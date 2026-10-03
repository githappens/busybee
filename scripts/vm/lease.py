"""The macOS slot: one long-lived guest, leased to one run at a time.

See docs/design/agent-lab.md §macOS workers. Exclusion is an flock on
`slot.lock` in the slot's state directory. `worker create macos` takes it,
then hands its open descriptor to the run's supervisor, which holds it for
the lease's lifetime: whenever that process ends, however it ends, the kernel
frees the slot. Waiters take numbered tickets and are served in arrival order;
a ticket whose process is gone is dropped. The lease is independent of
busybee, which is the software under test.
"""
import fcntl
import json
import os
from pathlib import Path
import time

POLL_S = 1


class Waited(Exception):
    """The slot did not come free before the waiter's deadline."""


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


class Slot:
    def __init__(self, state, alive=alive, clock=time.monotonic, sleep=time.sleep):
        self.dir = Path(state) / "slots" / "macos"
        self.alive, self.clock, self.sleep = alive, clock, sleep

    @property
    def record_path(self):
        """The slot guest's record: its VM, the baseline it was cloned from and its reset snapshot."""
        return self.dir / "slot.json"

    def _edit(self, change):
        self.dir.mkdir(parents=True, exist_ok=True)
        with open(self.dir / "queue.lock", "a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            path = self.dir / "queue.json"
            queue = json.loads(path.read_text()) if path.is_file() else {"next": 0, "waiting": []}
            queue["waiting"] = [w for w in queue["waiting"] if self.alive(w["pid"])]
            answer = change(queue)
            tmp = path.with_suffix(f".{os.getpid()}.tmp")
            tmp.write_text(json.dumps(queue))
            os.replace(tmp, path)
            return answer

    def join(self, run_id):
        def take(queue):
            ticket = queue["next"]
            queue["next"] += 1
            queue["waiting"].append({"ticket": ticket, "pid": os.getpid(), "run_id": run_id})
            return ticket
        return self._edit(take)

    def ahead(self, ticket):
        return self._edit(lambda queue: sum(1 for w in queue["waiting"] if w["ticket"] < ticket))

    def leave(self, ticket):
        self._edit(lambda queue: queue.update(waiting=[w for w in queue["waiting"] if w["ticket"] != ticket]))

    def try_hold(self):
        """The slot's lock as an open file, or None while another process holds it."""
        self.dir.mkdir(parents=True, exist_ok=True)
        f = open(self.dir / "slot.lock", "a")
        try:
            fcntl.flock(f, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            f.close()
            return None
        return f

    def acquire(self, run_id, seconds, report, blocked):
        """Wait in line for the slot; returns its held lock file. `report(ahead,
        why)` hears each change of position. `blocked()` names a reason the free
        lock is still not usable (a previous holder's evidence is not collected),
        or returns None."""
        until = self.clock() + seconds
        ticket = self.join(run_id)
        said = None
        try:
            while True:
                ahead, why = self.ahead(ticket), None
                if ahead == 0:
                    held = self.try_hold()
                    if held is not None:
                        why = blocked()
                        if why is None:
                            return held
                        held.close()
                    else:
                        why = "a lease holds the slot"
                    ahead = 1
                if (ahead, why) != said:
                    report(ahead, why)
                    said = (ahead, why)
                if self.clock() >= until:
                    raise Waited(f"the macOS slot was not free within {seconds:.0f}s: "
                                 f"{why or f'{ahead} waiter(s) ahead'}")
                self.sleep(POLL_S)
        finally:
            self.leave(ticket)
