"""The publishable export of a run's evidence.

See docs/design/agent-lab.md §Evidence required for a verified fix. Raw evidence
stays under the run directory. `export` writes a separate copy for publishing:
environment values outside ENV_ALLOWLIST, credentials, machine paths, the
user and host names, IP and MAC addresses, and VM, snapshot and run identities
are replaced by labelled placeholders. Exit codes, timings, argv and logs keep
their meaning. Binary artifacts cannot be scanned, so they are listed by
digest instead of copied: commit bundles and the Parallels screenshots under
`console/`.

Terminal images are different: they are rendered from text. A terminal's
recording and zellij's screens are redacted with placeholders of the same
length, so the timing log's byte counts and the screen layout still hold, and
its cells and images are rendered again from that redacted copy (screen.py);
the private images are never copied. Text is scanned as written, so a value a
terminal wrapped across lines is matched only as far as each line holds it.
"""
import getpass
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import socket

import contracts
import screen

PUBLIC_SCHEMA = "busybee.vm.public/v1"
# Environment values that describe a run without identifying the host or
# carrying a credential. Every other value is replaced.
ENV_ALLOWLIST = ("RUST_LOG", "RUST_BACKTRACE", "NO_COLOR", "CARGO_TERM_COLOR", "TERM", "COLUMNS", "LINES", "LANG",
                 "LC_ALL", "TZ")
# Text evidence, published after redaction. Bundles and images are withheld.
TEXT = ("command.json", "result.json", "stdout", "stderr", "collected.json", "source.json", "worktree.diff",
        "events.jsonl", "source.patch")
# Shorter values are too likely to occur by chance to be redacted from logs;
# the structured env record redacts them all the same.
MIN_SECRET = 4

PATTERNS = (
    ("private-key", re.compile(rb"-----BEGIN [A-Z ]*PRIVATE KEY-----.*?-----END [A-Z ]*PRIVATE KEY-----", re.S)),
    ("token", re.compile(rb"\b(?:gh[pousr]_[A-Za-z0-9]{20,}|github_pat_[A-Za-z0-9_]{20,}|AKIA[0-9A-Z]{16}"
                         rb"|xox[abprs]-[A-Za-z0-9-]{10,}|sk-[A-Za-z0-9_-]{20,})")),
    ("vm-id", re.compile(rb"\{?\b[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}\b\}?")),
    ("mac", re.compile(rb"\b[0-9A-Fa-f]{2}(?::[0-9A-Fa-f]{2}){5}\b|\b001[Cc]42[0-9A-Fa-f]{6}\b")),
    ("ip", re.compile(rb"\b(?:(?:25[0-5]|2[0-4]\d|1?\d?\d)\.){3}(?:25[0-5]|2[0-4]\d|1?\d?\d)\b")),
    ("home", re.compile(rb"/(?:Users|home)/[^/\s\"']+")),
)


class Redactor:
    def __init__(self, literals):
        # Longest first, so a path is replaced whole before its parts.
        self.literals = sorted(((v.encode(), label) for v, label in literals.items() if len(v) >= MIN_SECRET),
                               key=lambda item: -len(item[0]))
        self.counts = {}

    def _count(self, label, n):
        if n:
            self.counts[label] = self.counts.get(label, 0) + n

    def bytes(self, data, keep_length=False):
        """`data` with every secret replaced by `<label>`, or with `keep_length`
        by a placeholder of the secret's own length, so offsets into it hold."""
        def placeholder(found, label):
            if not keep_length:
                return f"<{label}>".encode()
            tag = f"<{label}>".encode()
            return tag + b"*" * (len(found) - len(tag)) if len(tag) <= len(found) else b"*" * len(found)
        for value, label in self.literals:
            self._count(label, data.count(value))
            data = data.replace(value, placeholder(value, label))
        for label, pattern in PATTERNS:
            data, n = pattern.subn(lambda m, label=label: placeholder(m.group(0), label), data)
            self._count(label, n)
        return data

    def json(self, value, keep_length=()):
        """A JSON document with every env value outside the allowlist replaced,
        then scanned as text; string values of the `keep_length` keys keep
        their length."""
        def walk(node):
            if isinstance(node, dict):
                return {k: ({n: v if n in ENV_ALLOWLIST else f"<redacted:{n}>" for n, v in node[k].items()}
                            if k == "env" and isinstance(node[k], dict) else
                            self.bytes(node[k].encode(), keep_length=True).decode()
                            if k in keep_length and isinstance(node[k], str) else walk(node[k])) for k in node}
            if isinstance(node, list):
                return [walk(v) for v in node]
            return node
        return self.bytes((json.dumps(walk(json.loads(value)), indent=2) + "\n").encode())


def _secrets(rdir):
    """Every env value an exec or a terminal was given outside the allowlist,
    labelled by name. A terminal's also travel in its holder's argv."""
    found = {}
    for path in [*rdir.glob("exec/*/command.json"), *rdir.glob("terminal/*/handle.json")]:
        for name, value in json.loads(path.read_text()).get("env", {}).items():
            if name not in ENV_ALLOWLIST:
                found[value] = f"redacted:{name}"
    return found


def literals(repo, state, record):
    """The machine- and run-specific values the public export must not carry."""
    values = {str(Path(repo)): "repo", str(Path(repo).resolve()): "repo", str(Path(state)): "state",
              str(Path(state).resolve()): "state", str(Path.home()): "home",
              getpass.getuser(): "user", socket.gethostname(): "host", socket.gethostname().split(".")[0]: "host",
              record["worker"]: "vm-name", record["run_id"]: "run-id", record["candidate"]: "baseline"}
    for key in ("vm_id", "baseline_vm_id", "snapshot_id", "reset_snapshot_id"):
        values[record[key]] = "vm-id"
    # The account a macOS baseline's key logs into (NixOS guests use root).
    candidate = Path(state) / "templates" / record["template"] / "candidates" / record["candidate"] / "candidate.json"
    user = json.loads(candidate.read_text()).get("guest_user") if candidate.is_file() else None
    if user:
        values[user] = "guest-user"
    return values


def export(repo, state, record):
    """Write the run's public evidence to runs/<run>/public; returns what it wrote."""
    rdir = Path(state) / "runs" / record["run_id"]
    out = rdir / "public"
    redact = Redactor({**_secrets(rdir), **literals(repo, state, record)})
    shutil.rmtree(out, ignore_errors=True)
    files, withheld = {}, {}
    summary = {k: record[k] for k in ("template", "clone_strategy", "allocation", "source", "status", "created_at",
                                      "deadline")}
    sources = [p for p in sorted(rdir.rglob("*")) if p.is_file() and out not in p.parents
               and p.parent.name not in ("checkpoint.tmp", "checkpoint.old")]
    for path in sources:
        rel = path.relative_to(rdir)
        if rel.parts[0] == "terminal":
            clean = _terminal_file(redact, rel, path.read_bytes())
            if clean is not None:
                durable(out / rel, clean)
                files[str(rel)] = hashlib.sha256(clean).hexdigest()
            continue
        if rel.name not in TEXT:
            if rel.name == "commits.bundle" or rel.suffix == ".png":
                withheld[str(rel)] = hashlib.sha256(path.read_bytes()).hexdigest()
            continue  # otherwise controller bookkeeping: the record, locks, keys, the supervisor's own log
        data = path.read_bytes()
        clean = redact.json(data) if rel.name.endswith(".json") else redact.bytes(data)
        durable(out / rel, clean)
        files[str(rel)] = hashlib.sha256(clean).hexdigest()
    missing = []
    for hdir in sorted((out / "terminal").iterdir()) if (out / "terminal").is_dir() else ():
        if not (hdir / "recording" / "timing").is_file():
            missing.append(f"terminal/{hdir.name}: no recording was collected")
            continue
        for record in screen.render_handle(hdir):
            for name in (f"{record['capture']}.cells.json", record["image"]):
                files[str((hdir / "captures" / name).relative_to(out))] = \
                    hashlib.sha256((hdir / "captures" / name).read_bytes()).hexdigest()
    run = redact.json(json.dumps(summary).encode())
    durable(out / "run.json", run)
    files["run.json"] = hashlib.sha256(run).hexdigest()
    manifest = {"schema": PUBLIC_SCHEMA, "evidence_schema": contracts.EVIDENCE_SCHEMA, "files": files,
                "withheld": withheld, "missing": missing, "redactions": redact.counts,
                "env_allowlist": list(ENV_ALLOWLIST)}
    durable(out / "manifest.json", (json.dumps(manifest, indent=2) + "\n").encode())
    return {"path": out, "files": files, "withheld": withheld, "missing": missing, "redactions": redact.counts}


def _terminal_file(redact, rel, data):
    """A terminal file for publishing, or None for what is rendered again."""
    if rel.name.endswith(".cells.json") or rel.suffix == ".png":
        return None  # derived from the private recording: rendered again from the public one
    if rel.parent.name == "recording" and rel.name in ("output", "input"):
        return redact.bytes(data, keep_length=True)
    if rel.suffix == ".json":
        return redact.json(data, keep_length=("screen", "ansi"))
    return redact.bytes(data)


def scenario_records(rdir):
    """Every scenario run recorded in a run directory, in exec order."""
    return [json.loads(p.read_text()) for p in sorted(Path(rdir).glob("scenarios/*/result.json"))]


def coverage(records):
    """Per scenario: the latest status in each fixture mode it ran in, and
    whether the latest run in every required mode passed. A required mode with
    no run is missing; a result in another mode does not stand in for it."""
    out = {}
    for record in records:
        entry = out.setdefault(record["scenario"], {"modes": {}})
        entry["required_modes"] = record["required_modes"]
        entry["modes"][record["mode"]] = record["status"]
    for entry in out.values():
        entry["missing"] = [m for m in entry["required_modes"] if m not in entry["modes"]]
        entry["failing"] = [m for m in entry["required_modes"] if entry["modes"].get(m, "success") != "success"]
        entry["verified"] = not entry["missing"] and not entry["failing"]
    return out


def durable(path, data):
    """Write `path` and flush it to disk before returning."""
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "wb") as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())
