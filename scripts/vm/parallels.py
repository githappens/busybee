"""The Parallels boundary.

Everything the controller asks Parallels goes through `Parallels.query`, and
this revision only knows read-only queries: anything else raises before a
process is started. Lifecycle operations arrive with their own issues, each
added to the adapter explicitly. Tests substitute the runner, so contract and
failure paths run without Parallels installed.
"""
from pathlib import Path
import subprocess

# The complete set of commands preflight may run. A prefix match is not
# enough: `prlctl list --all --json --info` would be a different query.
READ_ONLY = (
    ("prlctl", ("--version",)),
    ("prlsrvctl", ("info", "--json")),
    ("prlctl", ("list", "--all", "--json")),
)
SNAPSHOT_LIST = ("prlctl", "snapshot-list")

QUERY_TIMEOUT_S = 30


class ParallelsError(RuntimeError):
    pass


def is_read_only(argv):
    tool, rest = Path(argv[0]).name, tuple(argv[1:])
    if (tool, rest) in READ_ONLY:
        return True
    # `prlctl snapshot-list <vm> --json` and nothing else.
    return (tool, rest[:1]) == (SNAPSHOT_LIST[0], SNAPSHOT_LIST[1:]) and len(rest) == 3 and rest[2] == "--json"


def run(argv):
    try:
        done = subprocess.run(argv, capture_output=True, text=True, timeout=QUERY_TIMEOUT_S)
    except (OSError, subprocess.TimeoutExpired) as err:
        raise ParallelsError(f"{Path(argv[0]).name} {argv[1]}: {err}") from err
    if done.returncode != 0:
        detail = (done.stderr or done.stdout).strip().splitlines()
        raise ParallelsError(f"{Path(argv[0]).name} {argv[1]} exited {done.returncode}: "
                             f"{detail[-1] if detail else 'no output'}")
    return done.stdout


class Parallels:
    def __init__(self, prlctl, prlsrvctl, runner=run):
        self.tools = {"prlctl": prlctl, "prlsrvctl": prlsrvctl}
        self.runner = runner

    def query(self, args, tool="prlctl"):
        argv = [self.tools[Path(tool).name], *args]
        if not is_read_only(argv):
            raise ParallelsError(f"refusing a non-query Parallels command: {Path(tool).name} {' '.join(args)}")
        return self.runner(argv)
