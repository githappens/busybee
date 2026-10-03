"""Prepare a real disposable checkout without network or account credentials."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "prepare-workspace.sh"
# An operator's SSH host alias for a deploy key. The test's git configuration
# rewrites it to a local repository, so no SSH connection is made.
SSH_URL = "git@deploy-alias:owner/repo.git"


class Fixture:
    """A source repository, a trusted snapshot, a `gh` double and a clone of
    the source in which the script runs."""

    def __init__(self, test, root, agent="codex", origin=None):
        self.test, self.root = test, root
        self.config = root / "gitconfig"
        self.config.write_text('[credential]\n\thelper = "!printf \'username=wrong\\npassword=fixture\\n\'"\n')
        self.trusted = root / "trusted"
        (self.trusted / "sortie").mkdir(parents=True)
        (self.trusted / "sortie/lab.py").write_text("# Eligibility is tested in test_lab.py.\n")
        for name in ("machine-safety-hook.sh", "claude-settings.json", "isolated.sh"):
            (self.trusted / "sortie" / name).write_text("trusted guard\n")
        tools = root / "bin"
        tools.mkdir()
        gh = tools / "gh"
        gh.write_text("#!/bin/sh\n[ \"$1 $2\" = 'auth git-credential' ] || exit 9\n"
                      "cat >/dev/null\nprintf 'username=task-account\\npassword=fixture\\n'\n")
        gh.chmod(0o755)
        self.env = dict(os.environ, GIT_CONFIG_GLOBAL=str(self.config), GIT_CONFIG_NOSYSTEM="1",
                        GIT_TERMINAL_PROMPT="0", PATH=str(tools) + os.pathsep + os.environ["PATH"],
                        BUSYBEE_SORTIE_TRUSTED=str(self.trusted), SORTIE_ISSUE_IDENTIFIER="72",
                        BUSYBEE_SORTIE_AGENT_KIND=agent)
        self.env.pop("BUSYBEE_SORTIE_WORKSPACES", None)
        self.source, self.workspace = root / "source", root / "workspace"
        self.run(["git", "init", "-q", "-b", "main", str(self.source)])
        self.run(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test",
                  "commit", "-q", "--allow-empty", "-m", "baseline"], self.source)
        if origin:
            with self.config.open("a") as f:
                f.write(f'[url "{self.source}"]\n\tinsteadOf = {origin}\n')
        self.run(["git", "clone", "-q", origin or str(self.source), str(self.workspace)])

    def run(self, argv, cwd=None, input=None):
        result = subprocess.run(argv, cwd=cwd or self.root, env=self.env, input=input,
                                text=True, capture_output=True, timeout=10)
        self.test.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout.strip()

    def prepare(self):
        result = subprocess.run(["bash", str(SCRIPT)], cwd=self.workspace, env=self.env,
                                text=True, capture_output=True, timeout=10)
        return result

    def git(self, *args):
        return self.run(["git", *args], self.workspace)


class PrepareWorkspaceTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.root = Path(directory.name).resolve()

    def test_continuation_preserves_work_and_uses_task_credentials(self):
        f = Fixture(self, self.root)
        (f.workspace / "sortie").mkdir()
        self.assertEqual(f.prepare().returncode, 0)
        self.assertEqual(f.git("branch", "--show-current"), "sortie-lab/72")
        f.git("-c", "user.name=Fixture", "-c", "user.email=fixture@example.test",
              "commit", "-q", "--allow-empty", "-m", "checkpoint")
        head = f.git("rev-parse", "HEAD")
        (f.workspace / "unfinished.txt").write_text("keep this work\n")
        self.assertEqual(f.prepare().returncode, 0)
        self.assertEqual(f.git("rev-parse", "HEAD"), head)
        self.assertEqual((f.workspace / "unfinished.txt").read_text(), "keep this work\n")
        credentials = f.run(["git", "credential", "fill"], f.workspace, "protocol=https\nhost=example.test\n\n")
        self.assertIn("username=task-account", credentials)
        self.assertNotIn("username=wrong", credentials)
        self.assertIn("username=wrong", f.config.read_text())

    def test_claude_guard_comes_from_trusted_snapshot_not_candidate(self):
        f = Fixture(self, self.root, agent="claude-code")
        (f.workspace / "sortie").mkdir()
        for name in ("machine-safety-hook.sh", "claude-settings.json", "isolated.sh"):
            (f.workspace / "sortie" / name).write_text("candidate guard must not execute\n")
        self.assertEqual(f.prepare().returncode, 0)
        for path in (".claude/hooks/machine-safety-hook.sh", ".claude/settings.json", ".claude/isolated.sh"):
            self.assertEqual((f.workspace / path).read_text(), "trusted guard\n")
        (f.trusted / "sortie/machine-safety-hook.sh").unlink()
        failed = f.prepare()
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("machine-safety-hook.sh", failed.stderr)

    def test_an_ssh_origin_keeps_its_transport_and_never_prompts(self):
        f = Fixture(self, self.root, origin=SSH_URL)
        self.assertEqual(f.prepare().returncode, 0, "the SSH origin must fetch through its own transport")
        self.assertEqual(f.git("branch", "--show-current"), "sortie-lab/72")
        self.assertEqual(f.git("config", "remote.origin.url"), SSH_URL)
        # No gh HTTPS helper is forced onto an SSH checkout ...
        helpers = subprocess.run(["git", "config", "--local", "--get-all", "credential.helper"], cwd=f.workspace,
                                 env=f.env, text=True, capture_output=True).stdout
        self.assertNotIn("gh auth", helpers)
        # ... and a key that would ask for a passphrase or a touch fails instead of waiting.
        self.assertIn("BatchMode=yes", f.git("config", "--local", "core.sshCommand"))

    def test_a_shared_workspace_root_names_each_checkout_after_the_repository(self):
        shared = self.root / "shared"
        shared.mkdir()
        f = Fixture(self, self.root)
        f.env["BUSYBEE_SORTIE_WORKSPACES"] = str(shared)
        self.assertEqual(f.prepare().returncode, 0)
        link = shared / "busybee-72"
        self.assertTrue(link.is_symlink())
        self.assertEqual(link.resolve(), f.workspace.resolve())
        # Idempotent across continuations, and a stale link is repointed.
        link.unlink()
        link.symlink_to(self.root / "gone")
        self.assertEqual(f.prepare().returncode, 0)
        self.assertEqual(link.resolve(), f.workspace.resolve())

    def test_a_shared_workspace_root_never_replaces_an_operator_directory(self):
        shared = self.root / "shared"
        (shared / "busybee-72").mkdir(parents=True)
        (shared / "busybee-72" / "mine").write_text("operator's own checkout\n")
        f = Fixture(self, self.root)
        f.env["BUSYBEE_SORTIE_WORKSPACES"] = str(shared)
        result = f.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((shared / "busybee-72").is_symlink())
        self.assertEqual((shared / "busybee-72" / "mine").read_text(), "operator's own checkout\n")
        self.assertIn("busybee-72", result.stderr)
        self.assertIn("not a link", result.stderr)


if __name__ == "__main__":
    unittest.main()
