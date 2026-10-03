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

[worker]
cpus = 2
memory_mib = 4096
storage_gib = 32
artifact_mib = 1024

[templates.linux]
manifest = "templates/linux/manifest.json"
"""


def manifest(**overrides):
    value = {
        "schema": contracts.TEMPLATE_SCHEMA, "name": "linux", "candidate": "r-20261001T000000Z-abcdef",
        "os": "linux", "arch": "arm64",
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
            # Real `prlctl list` prints bare UUIDs; manifests carry braces.
            return json.dumps([{"uuid": vm.strip("{}"), "status": "stopped", "name": "x"} for vm in self.vms])
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
            "worker over budget": VALID_CONFIG.replace("memory_mib = 4096", "memory_mib = 16384"),
            "missing worker allocation": VALID_CONFIG.replace("[worker]\ncpus = 2\nmemory_mib = 4096\n"
                                                              "storage_gib = 32\nartifact_mib = 1024\n", ""),
            "missing artifact budget": VALID_CONFIG.replace("artifact_mib = 1024\n", ""),
            "tiny artifact budget": VALID_CONFIG.replace("artifact_mib = 1024", "artifact_mib = 1"),
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

    def test_macos_needs_full_clones_explicitly(self):
        # Linked macOS clones boot to a black screen; the config must say full
        # for macOS rather than the controller switching on its own.
        macos = '\n[templates.macos]\nmanifest = "templates/macos/manifest.json"\n'
        inherited = self.errors(VALID_CONFIG + macos)
        self.assertIn("clone_mode_unsupported", [code for code, _ in inherited])
        full = macos + 'clone_strategy = "full"\nsource = "my-prepared-mac"\nuser = "lab"\n' \
                       'bootstrap_key = "keys/bootstrap"\n'
        self.assertEqual(self.errors(VALID_CONFIG + full), [])
        for name, text in {"linked macos": full.replace('"full"', '"linked"'),
                           "macos source on linux": VALID_CONFIG.replace(
                               '[templates.linux]\n', '[templates.linux]\nsource = "x"\n'),
                           "escaping key": full.replace('"keys/bootstrap"', '"../../../.ssh/id"'),
                           "absolute key": full.replace('"keys/bootstrap"', '"/etc/key"'),
                           "numeric user": full.replace('"lab"', '42')}.items():
            with self.subTest(name):
                self.assertTrue(self.errors(VALID_CONFIG + text if "linux" not in name else text), name)


class ManifestTests(unittest.TestCase):
    def test_manifest_schema(self):
        self.assertEqual(contracts.manifest_errors(manifest()), [])
        for broken in (manifest(schema="other"), manifest(os="windows"), manifest(clone_modes=["magic"]),
                       manifest(clone_modes=[]), {k: v for k, v in manifest().items() if k != "snapshot_id"},
                       manifest(extra=1), manifest(os="macos", clone_modes=["linked", "full"])):
            self.assertTrue(contracts.manifest_errors(broken), broken)
        self.assertEqual(contracts.manifest_errors(manifest(os="macos", clone_modes=["full"])), [])


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
                  "candidate": "r-20261001T000000Z-abcdef", "baseline_vm_id": "{22222222-2222-3333-4444-555555555555}",
                  "reset_snapshot_id": "{77777777-7777-8888-9999-000000000000}", "status": "ready",
                  "allocation": {"cpus": 2, "memory_mib": 4096, "storage_gib": 32, "artifact_mib": 1024},
                  "source": {"revision": "0" * 40, "patch_sha256": None},
                  "created_at": "2026-10-01T00:00:00Z", "deadline": "2026-10-01T02:00:00Z"}
        self.assertEqual(contracts.worker_errors(record), [])
        self.assertTrue(contracts.worker_errors({**record, "worker": "someone-elses-vm"}))
        self.assertTrue(contracts.worker_errors({**record, "deadline": "2026-09-30T00:00:00Z"}))
        self.assertTrue(contracts.worker_errors({**record, "reset_snapshot_id": "latest"}))
        self.assertTrue(contracts.worker_errors({**record, "source": {"revision": "main"}}))
        # A macOS worker is a lease on the slot guest, not a per-run clone.
        leased = {**record, "template": "macos", "clone_strategy": "full", "worker": "busybee-lab-macos-slot"}
        self.assertEqual(contracts.worker_errors(leased), [])
        self.assertTrue(contracts.worker_errors({**leased, "worker": "someone-elses-mac"}))

    def test_result_states_are_the_documented_set(self):
        self.assertEqual(set(contracts.RESULT_STATES), {
            "success", "product_failure", "environment_failure", "timeout", "cancelled",
            "incomplete_collection", "unsupported"})


class CliTests(unittest.TestCase):
    SCRIPT = Path(__file__).resolve().parents[1] / "vmctl.py"

    def run_cli(self, *args):
        return subprocess.run([sys.executable, str(self.SCRIPT), *args], capture_output=True, text=True)

    def test_terminal_operations_are_implemented(self):
        # They parse and reach the controller: without a config they fail as an
        # environment failure, never as an unimplemented operation.
        run = "r-20261001T000000Z-abcdef"
        with tempfile.TemporaryDirectory() as tmp:
            config = ["--config", str(Path(tmp) / "absent.toml")]
            for argv in (["terminal", "open", run, *config, "--", "true"],
                         ["terminal", "send", run, "0001", "--text", "q", *config],
                         ["terminal", "resize", run, "0001", "--cols", "80", "--rows", "24", *config],
                         ["terminal", "capture", run, "0001", *config]):
                with self.subTest(argv[:2]):
                    out = self.run_cli("--json", *argv)
                    self.assertEqual(out.returncode, vmctl.EXIT_FAILED, out.stderr)
                    result = json.loads(out.stdout)
                    self.assertEqual(result["operation"], " ".join(argv[:2]))
                    self.assertEqual(result["status"], "environment_failure")
                    self.assertEqual({f["code"] for f in result["findings"]}, {"config_missing"})

    def test_verify_is_implemented(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = self.run_cli("--json", "verify", "--revision", "HEAD", "--config", str(Path(tmp) / "absent.toml"))
            self.assertEqual(out.returncode, vmctl.EXIT_FAILED, out.stderr)
            result = json.loads(out.stdout)
            self.assertEqual((result["operation"], result["status"]), ("verify", "environment_failure"))
            self.assertEqual({f["code"] for f in result["findings"]}, {"config_missing"})

    def test_gate_is_implemented(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = self.run_cli("--json", "gate", "--issue", "69", "--revision", "HEAD",
                               "--config", str(Path(tmp) / "absent.toml"))
            self.assertEqual(out.returncode, vmctl.EXIT_FAILED, out.stderr)
            result = json.loads(out.stdout)
            self.assertEqual((result["operation"], result["status"]), ("gate", "environment_failure"))
            self.assertEqual({f["code"] for f in result["findings"]}, {"config_missing"})
        self.assertIn("gate", vmctl.CAPABILITIES)

    def test_template_operations_need_a_valid_config(self):
        with tempfile.TemporaryDirectory() as tmp:
            out = self.run_cli("--json", "template", "build", "linux", "--arch", "aarch64",
                               "--config", str(Path(tmp) / "absent.toml"))
        self.assertEqual(out.returncode, vmctl.EXIT_FAILED, out.stderr)
        result = json.loads(out.stdout)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("config_missing", {f["code"] for f in result["findings"]})

    def test_worker_operations_need_a_valid_config(self):
        run_id = contracts.new_run_id()
        with tempfile.TemporaryDirectory() as tmp:
            absent = str(Path(tmp) / "absent.toml")
            for argv in (["worker", "create", "linux", "--revision", "HEAD"], ["worker", "reset", run_id],
                         ["worker", "destroy", run_id], ["exec", run_id, "--cwd", "/", "--timeout", "5"],
                         ["inspect", run_id], ["signal", run_id, "TERM", "42"], ["console", "capture", run_id],
                         ["collect", run_id], ["template", "prune", "linux"], ["status"], ["status", run_id],
                         ["status", run_id, "0001"], ["wait", run_id, "0001"], ["read", run_id, "0001", "stdout"],
                         ["export", run_id], ["exec", run_id, "--cwd", "/", "--detach"]):
                with self.subTest(argv):
                    out = self.run_cli("--json", *argv, "--config", absent)
                    self.assertEqual(out.returncode, vmctl.EXIT_FAILED, out.stderr)
                    self.assertIn("config_missing", {f["code"] for f in json.loads(out.stdout)["findings"]})

    def test_an_unknown_operation_is_a_usage_error(self):
        self.assertEqual(self.run_cli("frobnicate").returncode, 2)


if __name__ == "__main__":
    unittest.main()
