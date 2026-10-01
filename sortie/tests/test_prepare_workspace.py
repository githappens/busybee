"""Prepare a real disposable checkout without network or account credentials."""
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / "prepare-workspace.sh"


class PrepareWorkspaceTests(unittest.TestCase):
    def test_lab_continuation_preserves_work_and_uses_task_credentials(self):
        self.check_continuation("lab", "sortie-lab/72")

    def test_product_continuation_preserves_work_and_uses_task_credentials(self):
        self.check_continuation("product", "sortie/72")

    def test_claude_guard_comes_from_trusted_snapshot_not_candidate(self):
        self.check_continuation("product", "sortie/72", "claude-code")

    def check_continuation(self, profile, branch, agent="codex"):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config = root / "gitconfig"
            config.write_text('[credential]\n\thelper = "!printf \'username=wrong\\npassword=fixture\\n\'"\n')
            trusted = root / "trusted"
            (trusted / "sortie").mkdir(parents=True)
            (trusted / "sortie/lab.py").write_text("# Eligibility is tested in test_lab.py.\n")
            for name in ("machine-safety-hook.sh", "claude-settings.json", "isolated.sh"):
                (trusted / "sortie" / name).write_text("trusted guard\n")
            tools = root / "bin"
            tools.mkdir()
            gh = tools / "gh"
            gh.write_text("#!/bin/sh\n[ \"$1 $2\" = 'auth git-credential' ] || exit 9\n"
                          "cat >/dev/null\nprintf 'username=task-account\\npassword=fixture\\n'\n")
            gh.chmod(0o755)
            env = dict(os.environ, GIT_CONFIG_GLOBAL=str(config), GIT_CONFIG_NOSYSTEM="1",
                       GIT_TERMINAL_PROMPT="0", PATH=str(tools) + os.pathsep + os.environ["PATH"],
                       BUSYBEE_SORTIE_TRUSTED=str(trusted), SORTIE_ISSUE_IDENTIFIER="72",
                       BUSYBEE_SORTIE_AGENT_KIND=agent, BUSYBEE_SORTIE_PROFILE=profile)

            def run(argv, cwd=root, input=None):
                result = subprocess.run(argv, cwd=cwd, env=env, input=input,
                                        text=True, capture_output=True, timeout=10)
                self.assertEqual(result.returncode, 0, result.stderr)
                return result.stdout.strip()

            source, workspace = root / "source", root / "workspace"
            run(["git", "init", "-q", "-b", "main", str(source)])
            run(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test",
                 "commit", "-q", "--allow-empty", "-m", "baseline"], source)
            run(["git", "clone", "-q", str(source), str(workspace)])
            (workspace / "sortie").mkdir()
            for name in ("machine-safety-hook.sh", "claude-settings.json", "isolated.sh"):
                (workspace / "sortie" / name).write_text("candidate guard must not execute\n")
            run(["bash", str(SCRIPT)], workspace)
            self.assertEqual(run(["git", "branch", "--show-current"], workspace), branch)
            run(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.test",
                 "commit", "-q", "--allow-empty", "-m", "checkpoint"], workspace)
            head = run(["git", "rev-parse", "HEAD"], workspace)
            (workspace / "unfinished.txt").write_text("keep this work\n")
            run(["bash", str(SCRIPT)], workspace)
            self.assertEqual(run(["git", "rev-parse", "HEAD"], workspace), head)
            self.assertEqual((workspace / "unfinished.txt").read_text(), "keep this work\n")
            credentials = run(["git", "credential", "fill"], workspace,
                              "protocol=https\nhost=example.test\n\n")
            self.assertIn("username=task-account", credentials)
            self.assertNotIn("username=wrong", credentials)
            self.assertIn("username=wrong", config.read_text())
            if agent == "claude-code":
                for path in (".claude/hooks/machine-safety-hook.sh", ".claude/settings.json", ".claude/isolated.sh"):
                    self.assertEqual((workspace / path).read_text(), "trusted guard\n")
                (trusted / "sortie/machine-safety-hook.sh").unlink()
                failed = subprocess.run(["bash", str(SCRIPT)], cwd=workspace, env=env,
                                        text=True, capture_output=True, timeout=10)
                self.assertNotEqual(failed.returncode, 0)
                self.assertIn("machine-safety-hook.sh", failed.stderr)


if __name__ == "__main__":
    unittest.main()
