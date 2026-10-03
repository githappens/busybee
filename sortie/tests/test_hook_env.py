"""The lab hooks run with the environment Sortie gives hooks, not the launcher's.

Sortie runs each hook as `sh -c BODY` with a POSIX allowlist plus every
`SORTIE_*` variable of its own environment; the agent command inherits the
whole environment. These tests run the real hook bodies from LAB_WORKFLOW.md
against a stub trusted controller, with that filtering, so a hook that relies
on an unprefixed launcher setting fails here rather than on the live tracker.
The last test checks the filtering itself against the pinned Sortie binary
when it is on PATH (the `agent` development shell).
"""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import time
import unittest

SORTIE = Path(__file__).resolve().parents[1]
WORKFLOW = SORTIE / "LAB_WORKFLOW.md"
# Sortie 1.24's documented POSIX allowlist for hook subprocesses.
ALLOWLIST = ("PATH", "HOME", "SHELL", "TMPDIR", "USER", "LOGNAME", "TERM", "LANG", "LC_ALL",
             "SSH_AUTH_SOCK", "XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS")
# Set by the shell itself, not passed by Sortie.
SHELL_OWN = ("PWD", "SHLVL", "_", "OLDPWD")


def block(text, name):
    """The body of a `name: |` block scalar in the workflow's front matter."""
    lines = text.split("\n---\n", 1)[0].splitlines()
    for i, line in enumerate(lines):
        m = re.fullmatch(r"(\s*)" + re.escape(name) + r": \|", line)
        if m:
            indent, body = len(m.group(1)), []
            for nxt in lines[i + 1:]:
                if nxt.strip() and len(nxt) - len(nxt.lstrip()) <= indent:
                    break
                body.append(nxt)
            width = min(len(x) - len(x.lstrip()) for x in body if x.strip())
            return "\n".join(x[width:] for x in body).strip() + "\n"
    raise AssertionError(f"{name} is not a block in {WORKFLOW.name}")


def hook_env(env):
    """What Sortie passes a hook from its own environment."""
    return {k: v for k, v in env.items() if k in ALLOWLIST or k.startswith("SORTIE_")}


STUB = '''#!/usr/bin/env python3
import json, os, sys
from pathlib import Path
log = Path(__file__).resolve().parents[{up}] / "calls.jsonl"
with log.open("a") as f:
    f.write(json.dumps({{"argv": sys.argv[1:], "env": dict(os.environ)}}) + "\\n")
if sys.argv[1:2] == ["profile"]:
    print("infrastructure")
'''


class Lab:
    """A source repository, a trusted snapshot holding the real hook scripts
    and stub controllers, and a launcher-shaped environment."""

    def __init__(self, root):
        self.root = root
        self.home = root / "home"
        self.home.mkdir()
        self.source = root / "source"
        git = ["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test"]
        subprocess.run(["git", "init", "-q", "-b", "main", str(self.source)], check=True)
        subprocess.run(git + ["-C", str(self.source), "commit", "-q", "--allow-empty", "-m", "base"], check=True)
        self.trusted = root / "trusted"
        (self.trusted / "sortie").mkdir(parents=True)
        (self.trusted / "scripts/vm").mkdir(parents=True)
        for name in ("hook-env.sh", "prepare-workspace.sh"):
            shutil.copy(SORTIE / name, self.trusted / "sortie" / name)
        for name in ("machine-safety-hook.sh", "claude-settings.json", "isolated.sh"):
            (self.trusted / "sortie" / name).write_text("trusted guard\n")
        for rel, up in (("sortie/lab.py", 1), ("sortie/review-triage.py", 1), ("scripts/vm/vmctl.py", 2)):
            (self.trusted / rel).write_text(STUB.format(up=up))
        self.lab_root = root / "checkout"
        self.lab_root.mkdir()
        self.workspaces = root / "shared"
        (self.workspaces / ".sortie/busybee").mkdir(parents=True)
        self.workspace = self.workspaces / ".sortie/busybee/72"
        self.workspace.mkdir()

    def launcher_env(self):
        """The environment launch.sh hands Sortie: its own names, and the
        SORTIE_BUSYBEE_* copies hooks can see."""
        env = dict(os.environ, HOME=str(self.home), GIT_CONFIG_NOSYSTEM="1", GITHUB_TOKEN="fixture-token")
        settings = {"TRUSTED": str(self.trusted), "LAB_ROOT": str(self.lab_root),
                    "CLONE_URL": str(self.source), "AGENT_KIND": "claude-code",
                    "WORKSPACES": str(self.workspaces), "GITHUB_TOKEN": "fixture-token"}
        env.update(BUSYBEE_SORTIE_TRUSTED=settings["TRUSTED"], BUSYBEE_LAB_ROOT=settings["LAB_ROOT"],
                   BUSYBEE_SORTIE_CLONE_URL=settings["CLONE_URL"],
                   BUSYBEE_SORTIE_AGENT_KIND=settings["AGENT_KIND"],
                   BUSYBEE_SORTIE_WORKSPACES=settings["WORKSPACES"])
        env.update({f"SORTIE_BUSYBEE_{k}": v for k, v in settings.items()})
        return env

    def injected(self):
        return {"SORTIE_ISSUE_ID": "4072", "SORTIE_ISSUE_IDENTIFIER": "72",
                "SORTIE_WORKSPACE": str(self.workspace), "SORTIE_ATTEMPT": "0",
                "SORTIE_REACTION_RESULT": str(self.root / "reaction.json")}

    def hook(self, name, env):
        body = block(WORKFLOW.read_text(), name)
        return subprocess.run(["sh", "-c", body], cwd=self.workspace, env={**hook_env(env), **self.injected()},
                              text=True, capture_output=True, timeout=60)

    def calls(self):
        log = self.trusted / "calls.jsonl"
        return [json.loads(x) for x in log.read_text().splitlines()] if log.exists() else []


class HookEnvironment(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.lab = Lab(Path(tmp.name).resolve())

    def run_ok(self, name, env):
        done = self.lab.hook(name, env)
        self.assertEqual(done.returncode, 0, f"{name}: {done.stderr}{done.stdout}")
        return done

    def test_every_hook_works_with_the_filtered_environment(self):
        lab, env = self.lab, self.lab.launcher_env()
        self.run_ok("after_create", env)
        self.assertTrue((lab.workspace / ".git").is_dir())
        self.run_ok("before_run", env)
        self.assertEqual((lab.workspace / ".sortie/trusted-controller").read_text().strip(), str(lab.trusted))
        self.assertTrue((lab.workspace / ".claude/settings.json").is_file(), "claude-code guard not installed")
        self.assertEqual((lab.workspaces / "busybee-72").resolve(), lab.workspace.resolve())
        self.run_ok("after_run", env)
        self.run_ok("script", env)  # the bot_review triage script
        calls = lab.calls()
        self.assertEqual([c["argv"][:1] for c in calls if c["argv"][:1] != ["--root"]],
                         [["check"], ["profile"], []])
        vm = [c["argv"] for c in calls if c["argv"][:1] == ["--root"]]
        self.assertEqual([a[:4] for a in vm], [["--root", str(lab.lab_root), "session", "start"],
                                               ["--root", str(lab.lab_root), "session", "end"]])
        self.assertIn("--profile", vm[0])
        self.assertEqual(vm[0][vm[0].index("--profile") + 1], "infrastructure")
        for call in calls:
            self.assertEqual(call["env"].get("BUSYBEE_LAB_ROOT"), str(lab.lab_root), call["argv"])
            self.assertEqual(call["env"].get("GITHUB_TOKEN"), "fixture-token", call["argv"])

    def test_a_hook_without_launcher_settings_fails_loudly(self):
        env = {k: v for k, v in self.lab.launcher_env().items() if not k.startswith("SORTIE_BUSYBEE_")}
        for name in ("after_create", "before_run", "after_run", "script"):
            done = self.lab.hook(name, env)
            self.assertNotEqual(done.returncode, 0, name)
            self.assertIn("SORTIE_BUSYBEE_", done.stderr, name)
            self.assertIn("run-lab.sh", done.stderr, name)
            self.assertNotIn("repository ''", done.stderr, name)
        self.assertEqual(self.lab.calls(), [])

    def test_each_missing_setting_is_named(self):
        for name in ("LAB_ROOT", "CLONE_URL", "AGENT_KIND", "GITHUB_TOKEN"):
            env = self.lab.launcher_env()
            del env[f"SORTIE_BUSYBEE_{name}"]
            done = self.lab.hook("after_create", env)
            self.assertNotEqual(done.returncode, 0, name)
            self.assertIn(f"SORTIE_BUSYBEE_{name}", done.stderr, name)
            self.assertFalse((self.lab.workspace / ".git").exists(), name)

    def test_the_launcher_exports_every_setting_the_hooks_require(self):
        names = set(re.findall(r"SORTIE_BUSYBEE_[A-Z_]+", (SORTIE / "hook-env.sh").read_text()))
        self.assertTrue(names)
        launcher = (SORTIE / "launch.sh").read_text()
        for name in sorted(names):
            self.assertRegex(launcher, rf"export [^\n]*\b{name}=", name)


@unittest.skipUnless(shutil.which("sortie"), "the pinned sortie binary is not on PATH (nix develop .#agent)")
class PinnedSortie(unittest.TestCase):
    """The filtering the tests above assume is the pinned binary's: one real
    dispatch from a file tracker runs the real hooks and a stub agent."""

    def test_real_dispatch_runs_every_hook_and_the_agent_keeps_the_environment(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = Path(tmp.name).resolve()
        lab = Lab(root)
        lab.workspace.rmdir()
        text = WORKFLOW.read_text()
        agent = root / "agent.sh"
        agent.write_text(f"#!/bin/sh\nenv > {root}/agent.env\nexit 1\n")
        agent.chmod(0o755)
        issues = root / "issues.json"
        issues.write_text(json.dumps([{"id": "4072", "identifier": "72", "title": "t", "description": "d",
                                       "state": "To Do", "labels": []}]))

        def indent(name):
            return "\n".join("    " + x for x in block(text, name).splitlines())

        (root / "WORKFLOW.md").write_text(f"""---
tracker:
  kind: file
  active_states: [To Do]
  terminal_states: [Done]
file:
  path: {issues}
workspace:
  root: {lab.workspaces}/.sortie/busybee
db_path: {root}/sortie.db
polling:
  interval_ms: 1000
hooks:
  after_create: |
{indent("after_create")}
  before_run: |
{indent("before_run")}
  after_run: |
    env > {root}/hook.env
    set -e
{indent("after_run")}
    touch {root}/after_run.done
agent:
  kind: claude-code
  command: {agent}
  max_turns: 1
  max_sessions: 1
claude-code:
  permission_mode: bypassPermissions
---
prompt
""")
        env = dict(lab.launcher_env(), PROBE_UNPREFIXED="kept-for-agent")
        log = (root / "sortie.log").open("w")
        proc = subprocess.Popen(["sortie", "--port", "0", str(root / "WORKFLOW.md")], cwd=root, env=env,
                                stdout=log, stderr=subprocess.STDOUT)
        try:
            deadline = time.monotonic() + 60
            while (not (root / "after_run.done").exists() and time.monotonic() < deadline
                   and "hook failed" not in (root / "sortie.log").read_text()):
                time.sleep(0.2)
        finally:
            proc.terminate()
            proc.wait(timeout=30)
            log.close()
        output = "\n".join(x for x in (root / "sortie.log").read_text().splitlines()
                           if "level=WARN" in x or "level=ERROR" in x)
        self.assertTrue((root / "after_run.done").exists(), output)
        self.assertNotIn("hook failed", output)
        hook = dict(x.split("=", 1) for x in (root / "hook.env").read_text().splitlines() if "=" in x)
        stray = sorted(k for k in hook if k not in ALLOWLIST + SHELL_OWN and not k.startswith("SORTIE_"))
        self.assertEqual(stray, [], "the pinned Sortie passes hooks more than the tests assume")
        self.assertNotIn("PROBE_UNPREFIXED", hook)
        agent_env = (root / "agent.env").read_text()
        self.assertIn("PROBE_UNPREFIXED=kept-for-agent", agent_env)
        self.assertEqual([c["argv"][2:4] for c in lab.calls() if c["argv"][:1] == ["--root"]],
                         [["session", "start"], ["session", "end"]])


if __name__ == "__main__":
    unittest.main()
