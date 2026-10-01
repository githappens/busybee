"""Exercise the actual legacy Actions step with failed and successful routing."""
import os
from pathlib import Path
import subprocess
import tempfile
import textwrap
import unittest


class WorkflowRoutingTests(unittest.TestCase):
    def run_pick(self, route, status):
        workflow = (Path(__file__).resolve().parents[2] /
                    ".github/workflows/codex-gate.yml").read_text()
        step = workflow.split("      - name: Find PRs with unjudged Codex activity", 1)[1]
        block = step.split("        run: |\n", 1)[1]
        lines = []
        for line in block.splitlines():
            if line.strip() and not line.startswith("          "):
                break
            lines.append(line)
        self.assertTrue(lines, "The real Actions routing step must be exercised")
        stubs = r'''
gh() {
  case "$*" in
    'pr list '*) printf '82\n' ;;
    *'/pulls/82/files '*) printf 'legacy\n' >> "$CALL_LOG"; printf 'sortie/example.sh\n' ;;
    *'/issues/82/labels '*) cat >/dev/null ;;
    *) echo "Unexpected gh call: $*" >&2; return 99 ;;
  esac
}
python3() { printf '%s\n' "$ROUTE_OUTPUT"; return "$ROUTE_STATUS"; }
jq() { cat; }
'''
        with tempfile.TemporaryDirectory() as directory:
            calls = Path(directory) / "calls"
            env = dict(os.environ, ONLY_PR="", REPO="example/repo", ROUTE_OUTPUT=route,
                       ROUTE_STATUS=str(status), CALL_LOG=str(calls),
                       GITHUB_OUTPUT=str(Path(directory) / "output"))
            result = subprocess.run(["bash", "-c", stubs + textwrap.dedent("\n".join(lines))],
                                    env=env, capture_output=True, text=True, timeout=10)
            return result, calls.read_text() if calls.exists() else ""

    def test_failed_routing_never_reaches_legacy_judgement(self):
        result, calls = self.run_pick("", 1)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, "")

    def test_lab_pr_is_skipped_by_legacy_gate(self):
        result, calls = self.run_pick("lab", 0)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, "")

    def test_legacy_pr_retains_existing_policy(self):
        result, calls = self.run_pick("legacy", 0)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(calls, "legacy\n")


if __name__ == "__main__":
    unittest.main()
