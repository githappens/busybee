from pathlib import Path
import json
import subprocess
import sys
import tempfile
import tomllib
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import contracts
import parallels
import vmctl

LICENSE_KEY = "ABCDEF-123456-SECRET-KEY"
HOME = "/home/someone-private"

VALID_CONFIG = """\
schema = 1
state_dir = "build/vm"
clone_strategy = "linked"

[deadlines]
command = 600
scenario = 1800
run = 7200
cleanup = 300

[budget]
cpus = 4
memory_mib = 8192
storage_gib = 64

[templates.linux]
manifest = "templates/linux/manifest.json"
"""


def manifest(**overrides):
    value = {
        "schema": contracts.TEMPLATE_SCHEMA, "name": "linux", "os": "linux", "arch": "arm64",
        "vm_id": "{11111111-2222-3333-4444-555555555555}",
        "snapshot_id": "{66666666-7777-8888-9999-000000000000}",
        "provisioning_revision": "0123456789abcdef0123456789abcdef01234567",
        "lock_hashes": {"infra/vm/flake.lock": "sha256:00"}, "tools": {"nix": "2.30"},
        "parallels_version": "27.0.0", "clone_modes": ["linked", "full"],
        "validated_at": "2026-10-01T00:00:00Z",
    }
    value.update(overrides)
    return value


class RecordingRunner:
    """Answers the read-only Parallels queries and records every argv."""

    def __init__(self, vms=(), snapshots=None, server=None, fail=False):
        self.calls = []
        self.vms = list(vms)
        self.snapshots = snapshots or {}
        self.server = server or {"Version": "Desktop 27.0.0-58628", "License": {"state": "valid", "key": LICENSE_KEY},
                                 "Signed In": "yes", "VM home": f"{HOME}/Parallels", "Hardware Id": "HW-SECRET"}
        self.fail = fail

    def __call__(self, argv):
        self.calls.append(list(argv))
        if self.fail:
            raise parallels.ParallelsError("dispatcher unavailable")
        tool, rest = Path(argv[0]).name, argv[1:]
        if tool == "prlctl" and rest == ["--version"]:
            return "prlctl version 27.0.0 (58628)\n"
        if tool == "prlsrvctl" and rest == ["info", "--json"]:
            return json.dumps(self.server)
        if tool == "prlctl" and rest == ["list", "--all", "--json"]:
            return json.dumps([{"uuid": vm, "status": "stopped", "name": "x"} for vm in self.vms])
        if tool == "prlctl" and rest[:1] == ["snapshot-list"]:
            return json.dumps(self.snapshots.get(rest[1], {}))
        raise AssertionError(f"unexpected Parallels call {argv}")


class FakeHost:
    def __init__(self, tools=("prlctl", "prlsrvctl")):
        self.tools = set(tools)

    def which(self, name):
        return f"{HOME}/bin/{name}" if name in self.tools else None

    def executable(self, path):
        return Path(path).name in self.tools

    def cpus(self):
        return 12

    def memory_mib(self):
        return 32768

    def free_storage_gib(self, path):
        return 500

    def os_name(self):
        return "Darwin"

    def arch(self):
        return "arm64"


class Repo:
    def __init__(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        (self.root / "build" / "vm").mkdir(parents=True)

    def write_config(self, text=VALID_CONFIG):
        path = self.root / "build" / "vm" / "local.toml"
        path.write_text(text)
        return path

    def write_manifest(self, value):
        path = self.root / "build" / "vm" / "templates" / "linux" / "manifest.json"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(value))

    def tree(self):
        return {str(p.relative_to(self.root)): (p.stat().st_mtime_ns if p.is_file() else None)
                for p in sorted(self.root.rglob("*"))}

    def doctor(self, runner=None, host=None, config=None):
        return vmctl.doctor(self.root, config or self.root / "build" / "vm" / "local.toml",
                            host or FakeHost(), runner or RecordingRunner())


def codes(result, severity="error"):
    return {f["code"] for f in result["findings"] if f["severity"] == severity}


class DoctorTests(unittest.TestCase):
    def setUp(self):
        self.repo = Repo()
        self.addCleanup(self.repo.tmp.cleanup)

    def test_doctor_reports_missing_prerequisites(self):
        # Missing tools.
        self.repo.write_config()
        result = self.repo.doctor(host=FakeHost(tools=()))
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("tool_missing", codes(result))

        # Invalid config.
        self.repo.write_config("schema = 1\nstate_dir = 3\n")
        result = self.repo.doctor()
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("config_invalid", codes(result))

        # Missing config file is its own finding, not an invalid one.
        result = self.repo.doctor(config=self.repo.root / "build" / "vm" / "absent.toml")
        self.assertIn("config_missing", codes(result))

        # Missing baseline: tools and config are fine, no template yet.
        self.repo.write_config()
        result = self.repo.doctor()
        self.assertEqual(codes(result), {"baseline_missing"})
        self.assertEqual(result["status"], "environment_failure")

        # Unsupported clone mode, from the config value itself...
        self.repo.write_config(VALID_CONFIG.replace('"linked"', '"snapshot"'))
        result = self.repo.doctor()
        self.assertIn("clone_mode_unsupported", codes(result))
        self.assertNotIn("config_invalid", codes(result))

        # ...and from a baseline that never proved the configured mode.
        self.repo.write_config()
        self.repo.write_manifest(manifest(clone_modes=["full"]))
        vm, snap = manifest()["vm_id"], manifest()["snapshot_id"]
        runner = RecordingRunner(vms=[vm], snapshots={vm: {snap: {"name": "baseline"}}})
        result = self.repo.doctor(runner=runner)
        self.assertEqual(codes(result), {"clone_mode_unsupported"})
        self.assertEqual(result["data"]["templates"]["linux"]["state"], "ineligible")

        # An unreachable Parallels service is not "everything is missing".
        result = self.repo.doctor(runner=RecordingRunner(fail=True))
        self.assertIn("parallels_unavailable", codes(result))

    def test_a_ready_host_and_baseline_pass(self):
        self.repo.write_config()
        self.repo.write_manifest(manifest())
        vm, snap = manifest()["vm_id"], manifest()["snapshot_id"]
        runner = RecordingRunner(vms=[vm], snapshots={vm: {snap: {"name": "baseline"}}})
        result = self.repo.doctor(runner=runner)
        self.assertEqual(result["status"], "success", result["findings"])
        self.assertEqual(result["schema"], contracts.RESULT_SCHEMA)
        self.assertEqual(result["data"]["parallels"]["version"], "27.0.0")
        self.assertEqual(result["data"]["templates"]["linux"]["state"], "ready")

    def test_a_baseline_whose_snapshot_is_gone_is_missing(self):
        self.repo.write_config()
        self.repo.write_manifest(manifest())
        vm = manifest()["vm_id"]
        result = self.repo.doctor(runner=RecordingRunner(vms=[vm], snapshots={vm: {}}))
        self.assertIn("baseline_missing", codes(result))

    def test_a_budget_larger_than_the_host_fails(self):
        self.repo.write_config(VALID_CONFIG.replace("cpus = 4", "cpus = 64"))
        self.assertIn("budget_exceeds_host", codes(self.repo.doctor()))

    def test_unlicensed_or_signed_out_parallels_fails(self):
        self.repo.write_config()
        server = {"Version": "Desktop 27.0.0-1", "License": {"state": "expired"}, "Signed In": "no"}
        self.assertIn("parallels_not_authorized", codes(self.repo.doctor(runner=RecordingRunner(server=server))))

    def test_doctor_is_read_only(self):
        self.repo.write_config()
        self.repo.write_manifest(manifest())
        vm, snap = manifest()["vm_id"], manifest()["snapshot_id"]
        runner = RecordingRunner(vms=[vm], snapshots={vm: {snap: {}}})
        before = self.repo.tree()
        self.repo.doctor(runner=runner)
        self.assertEqual(self.repo.tree(), before, "doctor changed files")
        self.assertTrue(runner.calls)
        for argv in runner.calls:
            self.assertTrue(parallels.is_read_only(argv), argv)

    def test_the_adapter_refuses_anything_but_a_query(self):
        adapter = parallels.Parallels("prlctl", "prlsrvctl", RecordingRunner())
        for argv in (["prlctl", "start", "x"], ["prlctl", "clone", "x", "--name", "y"],
                     ["prlctl", "snapshot", "x"], ["prlctl", "snapshot-switch", "x", "-i", "y"],
                     ["prlsrvctl", "set", "--verbose-log", "on"], ["prlctl", "list", "--all", "--json", "--info"]):
            self.assertFalse(parallels.is_read_only(argv), argv)
            with self.assertRaises(parallels.ParallelsError):
                adapter.query(argv[1:], tool=argv[0])

    def test_human_and_json_output_carry_no_private_values(self):
        self.repo.write_config()
        result = self.repo.doctor()
        for text in (json.dumps(result), vmctl.summary(result)):
            self.assertNotIn(LICENSE_KEY, text)
            self.assertNotIn("HW-SECRET", text)
            self.assertNotIn(HOME, text)
            self.assertNotIn(str(self.repo.root), text)


class ConfigTests(unittest.TestCase):
    def setUp(self):
        self.repo = Repo()
        self.addCleanup(self.repo.tmp.cleanup)

    def errors(self, text):
        return contracts.config_errors(tomllib.loads(text), self.repo.root)

    def test_the_example_config_is_valid(self):
        example = Path(__file__).resolve().parents[3] / "infra" / "vm" / "local.example.toml"
        self.assertEqual(self.errors(example.read_text()), [])

    def test_local_config_rejects_invalid_budgets(self):
        cases = {
            "zero deadline": VALID_CONFIG.replace("command = 600", "command = 0"),
            "negative deadline": VALID_CONFIG.replace("run = 7200", "run = -1"),
            "unbounded deadline": VALID_CONFIG.replace("run = 7200", "run = 10000000"),
            "float deadline": VALID_CONFIG.replace("command = 600", "command = 600.5"),
            "infinite deadline": VALID_CONFIG.replace("run = 7200", "run = inf"),
            "command outlives scenario": VALID_CONFIG.replace("command = 600", "command = 3600"),
            "scenario outlives run": VALID_CONFIG.replace("scenario = 1800", "scenario = 9000"),
            "missing deadline": VALID_CONFIG.replace("cleanup = 300\n", ""),
            "zero cpus": VALID_CONFIG.replace("cpus = 4", "cpus = 0"),
            "absurd memory": VALID_CONFIG.replace("memory_mib = 8192", "memory_mib = 99999999999"),
            "tiny storage": VALID_CONFIG.replace("storage_gib = 64", "storage_gib = 1"),
            "string budget": VALID_CONFIG.replace("cpus = 4", 'cpus = "4"'),
            "boolean budget": VALID_CONFIG.replace("cpus = 4", "cpus = true"),
            "unknown key": VALID_CONFIG.replace("[budget]", "[budget]\ngpus = 1"),
            "wrong schema": VALID_CONFIG.replace("schema = 1", "schema = 2"),
            "state outside build/vm": VALID_CONFIG.replace('"build/vm"', '"build/other"'),
            "absolute state": VALID_CONFIG.replace('"build/vm"', '"/tmp/vm"'),
            "escaping state": VALID_CONFIG.replace('"build/vm"', '"build/vm/../../src"'),
            "escaping manifest": VALID_CONFIG.replace('"templates/linux/manifest.json"', '"../../../etc/x.json"'),
        }
        for name, text in cases.items():
            with self.subTest(name):
                self.assertTrue(self.errors(text), f"{name} was accepted")

    def test_a_symlinked_state_dir_cannot_escape(self):
        outside = tempfile.TemporaryDirectory()
        self.addCleanup(outside.cleanup)
        (self.repo.root / "build" / "vm").rmdir()
        (self.repo.root / "build" / "vm").symlink_to(outside.name)
        self.assertTrue(self.errors(VALID_CONFIG))

    def test_a_nested_state_dir_is_allowed(self):
        self.assertEqual(self.errors(VALID_CONFIG.replace('"build/vm"', '"build/vm/state"')), [])


class ManifestTests(unittest.TestCase):
    def test_manifest_schema(self):
        self.assertEqual(contracts.manifest_errors(manifest()), [])
        for broken in (manifest(schema="other"), manifest(os="windows"), manifest(clone_modes=["magic"]),
                       manifest(clone_modes=[]), {k: v for k, v in manifest().items() if k != "snapshot_id"},
                       manifest(extra=1)):
            self.assertTrue(contracts.manifest_errors(broken), broken)


class ContractTests(unittest.TestCase):
    def test_run_and_worker_ids(self):
        run_id = contracts.new_run_id()
        self.assertTrue(contracts.valid_run_id(run_id), run_id)
        self.assertEqual(contracts.worker_name(run_id), f"busybee-lab-{run_id}")
        for bad in ("", "r-1", "../x", "r-20261001T000000Z-xyz123"):
            self.assertFalse(contracts.valid_run_id(bad), bad)

    def test_worker_ownership_record(self):
        run_id = contracts.new_run_id()
        record = {"schema": contracts.WORKER_SCHEMA, "run_id": run_id, "worker": contracts.worker_name(run_id),
                  "vm_id": "{11111111-2222-3333-4444-555555555555}", "template": "linux",
                  "snapshot_id": "{66666666-7777-8888-9999-000000000000}", "clone_strategy": "linked",
                  "created_at": "2026-10-01T00:00:00Z", "deadline": "2026-10-01T02:00:00Z"}
        self.assertEqual(contracts.worker_errors(record), [])
        self.assertTrue(contracts.worker_errors({**record, "worker": "someone-elses-vm"}))
        self.assertTrue(contracts.worker_errors({**record, "deadline": "2026-09-30T00:00:00Z"}))

    def test_result_states_are_the_documented_set(self):
        self.assertEqual(set(contracts.RESULT_STATES), {
            "success", "product_failure", "environment_failure", "timeout", "cancelled",
            "incomplete_collection", "unsupported"})


class CliTests(unittest.TestCase):
    SCRIPT = Path(__file__).resolve().parents[1] / "vmctl.py"

    def run_cli(self, *args):
        return subprocess.run([sys.executable, str(self.SCRIPT), *args], capture_output=True, text=True)

    def test_controller_reports_unimplemented_operations(self):
        for argv in (["template", "build", "linux"], ["template", "validate", "linux"],
                     ["template", "promote", "linux"], ["worker", "create", "linux"],
                     ["worker", "reset", "w"], ["worker", "destroy", "w"], ["exec", "w", "--", "true"],
                     ["terminal", "open", "w"], ["terminal", "send", "w", "q"], ["terminal", "resize", "w"],
                     ["terminal", "capture", "w"], ["inspect", "w"], ["signal", "w", "TERM", "1"],
                     ["console", "capture", "w"], ["collect", "w"]):
            with self.subTest(argv):
                out = self.run_cli("--json", *argv)
                self.assertEqual(out.returncode, vmctl.EXIT_UNSUPPORTED, out.stderr)
                result = json.loads(out.stdout)
                self.assertEqual(result["status"], "unsupported")
                self.assertEqual(result["operation"], " ".join(argv[:2] if argv[0] in vmctl.GROUPS else argv[:1]))
                self.assertNotEqual(result["status"], "success")
                human = self.run_cli(*argv)
                self.assertEqual(human.returncode, vmctl.EXIT_UNSUPPORTED)
                self.assertIn("unsupported", human.stdout)

    def test_an_unknown_operation_is_a_usage_error(self):
        self.assertEqual(self.run_cli("frobnicate").returncode, 2)


if __name__ == "__main__":
    unittest.main()
