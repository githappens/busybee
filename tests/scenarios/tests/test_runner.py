"""The scenario runner without a worker: metadata, preflight, fixtures and
result classification, against substituted tools, users and processes."""
from pathlib import Path
import json
import os
import stat
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import procedures
import runner

SCENARIOS = Path(__file__).resolve().parents[1]


def meta(**over):
    base = {"schema": runner.META_SCHEMA, "id": "umask-startup", "summary": "s", "platforms": ["linux"],
            "modes": ["cold", "prepared"], "required_modes": ["cold"], "deadline_s": 60,
            "procedure": "task_permissions", "assertions": list(procedures.PROCEDURES["task_permissions"].assertions),
            "tools": {"busybee": "workspace", "bzbd": "workspace", "pueued": ">=4.0"}}
    return {**base, **over}


class FakeProbe:
    """Tool queries answered from a table, as a guest with these tools would."""

    def __init__(self, bin_dir):
        self.bin_dir = Path(bin_dir)
        self.os, self.uid = "linux", 0
        self.tools = {}
        self.versions = {"pueued": "pueued 4.0.4", "busybee": "bzb 0.1.7"}
        self.describe_out = "0.1.0-7-gabcdef1"
        on_path = self.bin_dir / "path"
        on_path.mkdir()
        for path in (self.bin_dir / "busybee", self.bin_dir / "bzbd", on_path / "pueued", on_path / "setpriv"):
            path.write_text(f"#!/bin/sh\necho {path.name}\n")
            path.chmod(0o755)
            if path.parent == on_path:
                self.tools[path.name] = str(path)

    def platform(self):
        return self.os

    def euid(self):
        return self.uid

    def which(self, name):
        return self.tools.get(name)

    def version(self, path):
        return self.versions.get(Path(path).name)

    def describe(self, checkout):
        return self.describe_out


def tester():
    return runner.User("tester", os.getuid(), os.getgid(), [])


class MetaTests(unittest.TestCase):
    def test_shipped_scenarios_are_valid(self):
        shipped = sorted(p.stem for p in SCENARIOS.glob("*.toml"))
        self.assertIn("umask-startup", shipped)
        self.assertIn("wedged-task", shipped)
        for name in shipped:
            with self.subTest(name=name):
                loaded = runner.load_meta(name, SCENARIOS)
                self.assertEqual(loaded["id"], name)

    def test_metadata_declares_every_contract_field(self):
        self.assertEqual(runner.meta_errors(meta(), "umask-startup"), [])
        broken = meta()
        del broken["deadline_s"]
        self.assertIn("missing field 'deadline_s'", runner.meta_errors(broken, "umask-startup"))
        cases = {
            "id must match the file name": meta(id="other"),
            "platforms must be a non-empty subset of linux, macos": meta(platforms=["windows"]),
            "modes must be a non-empty subset of cold, prepared": meta(modes=[]),
            "required_modes must be a subset of modes": meta(modes=["prepared"], required_modes=["cold"]),
            "deadline_s must be an integer within 1..3600": meta(deadline_s=0),
            "unknown procedure 'nope'": meta(procedure="nope"),
            "assertions must be exactly what procedure task_permissions checks": meta(assertions=["task_runs"]),
            "tools must map a name to 'workspace' or '>=MAJOR.MINOR'": meta(tools={"pueued": "4"}),
            "only busybee, bzbd are workspace tools": meta(tools={"pueued": "workspace"}),
            "affected_revision must be a full commit id": meta(affected_revision="main"),
        }
        for message, value in cases.items():
            with self.subTest(message=message):
                self.assertIn(message, runner.meta_errors(value, "umask-startup"))

    def test_unknown_scenario_is_refused(self):
        with self.assertRaises(runner.MetaError):
            runner.load_meta("../etc/passwd", SCENARIOS)
        with self.assertRaises(runner.MetaError):
            runner.load_meta("no-such-scenario", SCENARIOS)


class PreflightTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.bin = Path(tmp.name)
        self.probe = FakeProbe(self.bin)

    def codes(self):
        facts, findings = runner.preflight(meta(), self.bin, "/checkout", self.probe)
        return facts, [f["code"] for f in findings]

    def test_matching_tools_pass_and_are_recorded(self):
        facts, codes = self.codes()
        self.assertEqual(codes, [])
        self.assertEqual(facts["expected_version"], "0.1.7")
        busybee = facts["tools"]["busybee"]
        self.assertEqual((busybee["version"], busybee["requirement"]), ("0.1.7", "workspace"))
        self.assertRegex(busybee["sha256"], r"^[0-9a-f]{64}$")
        self.assertEqual(facts["tools"]["pueued"]["version"], "4.0.4")
        self.assertRegex(facts["tools"]["bzbd"]["sha256"], r"^[0-9a-f]{64}$")

    def test_missing_tool_is_environment_failure(self):
        (self.bin / "bzbd").unlink()
        del self.probe.tools["pueued"]
        _, codes = self.codes()
        self.assertEqual(codes.count("tool_missing"), 2)

    def test_downgraded_tools_fail_preflight(self):
        self.probe.versions["busybee"] = "bzb 0.1.6"
        self.probe.versions["pueued"] = "pueued 3.4.1"
        _, codes = self.codes()
        self.assertEqual(sorted(codes), ["tool_version_mismatch", "tool_version_too_old"])

    def test_unreadable_version_is_not_a_pass(self):
        self.probe.versions["busybee"] = "busybee 0.1.7"  # not what the tool reports
        self.probe.describe_out = None
        _, codes = self.codes()
        self.assertIn("tool_version_unknown", codes)
        self.assertIn("revision_version_unknown", codes)

    def test_platform_and_privilege_are_checked(self):
        self.probe.os, self.probe.uid = "freebsd", 1000
        facts, findings = runner.preflight(meta(), self.bin, "/checkout", self.probe)
        self.assertEqual(sorted(f["code"] for f in findings),
                         ["platform_not_applicable", "platform_unsupported", "runner_not_root"])
        # Linux drops privileges with setpriv, so it must be there.
        self.probe.os, self.probe.uid = "linux", 0
        del self.probe.tools["setpriv"]
        _, findings = runner.preflight(meta(), self.bin, "/checkout", self.probe)
        self.assertEqual([f["code"] for f in findings], ["tool_missing"])

    def test_macos_runs_scenarios_without_setpriv(self):
        # macOS has no setpriv; the runner drops privileges itself there.
        self.probe.os = "macos"
        del self.probe.tools["setpriv"]
        _, findings = runner.preflight(meta(platforms=["linux", "macos"]), self.bin, "/checkout", self.probe)
        self.assertEqual(findings, [])
        _, findings = runner.preflight(meta(), self.bin, "/checkout", self.probe)
        self.assertEqual([f["code"] for f in findings], ["platform_not_applicable"])

    def test_expected_version_follows_the_build_script(self):
        self.assertEqual(runner.version_from_describe("0.1.0-5-gabcdef1"), "0.1.5")
        self.assertEqual(runner.version_from_describe("v1.4.7-3-gabcdef1"), "1.4.10")
        self.assertIsNone(runner.version_from_describe("release-5-gabcdef1"))
        self.assertTrue(runner.terminal.at_least("4.0.4", "4.0"))
        self.assertFalse(runner.terminal.at_least("3.9.9", "4.0"))


class UserTests(unittest.TestCase):
    def test_the_scenario_user_is_dropped_to_without_setpriv_on_macos(self):
        linux, mac = runner.scenario_user("linux"), runner.scenario_user("macos")
        self.assertEqual(linux.prefix[0], "setpriv")
        self.assertEqual(mac.prefix[:2], [sys.executable, "-c"])
        self.assertEqual(mac.prefix[3:], [str(mac.uid), str(mac.gid)])
        # The shim clears supplementary groups, then the group, then the user, and execs.
        code = mac.prefix[2]
        self.assertLess(code.index("setgroups"), code.index("setgid"))
        self.assertLess(code.index("setgid"), code.index("setuid"))
        self.assertIn("execvp", code)


class ProcessScanTests(unittest.TestCase):
    def test_macos_processes_are_found_by_marker_and_owner(self):
        root = "/private/tmp/bzs-abc"
        names = ("  101   -2 bzbd\n  102   -2 pueued\n  103  501 bzbd\n  104    0 sleep\n  105    0 sh\n"
                 "  106   -2 python3.13\n")
        environ = (f"  101 /tmp/bzs-abc/bin/bzbd HOME=/x {runner.MARKER}={root}\n"
                   "  102 pueued -d -c x PATH=/bin\n"
                   f"  104 sleep 86400 A=1 {runner.MARKER}={root} B=2\n"
                   f"  105 sh -c x {runner.MARKER}={root}-other\n"
                   "  106 python3.13 x\n")
        found = runner.parse_ps(names, environ, root, 4294967294)
        self.assertEqual([(p["pid"], p["comm"]) for p in found], [(101, "bzbd"), (102, "pueued"), (104, "sleep")])
        self.assertTrue(all(p["umask"] is None for p in found))


class FakeProcesses:
    def __init__(self):
        self.listed = []
        self.signalled = []

    def __call__(self, root, uid):
        return list(self.listed)

    def kill(self, pid, sig):
        self.signalled.append((pid, sig))
        self.listed = [p for p in self.listed if p["pid"] != pid]


class FixtureTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.tmp = Path(tmp.name)
        self.bin = self.tmp / "bin"
        self.bin.mkdir()
        FakeProbe(self.bin)
        self.procs = FakeProcesses()

    def fixture(self, mode="cold", base=None, os_name="linux"):
        fx = runner.Fixture(mode, os_name, self.bin, ["busybee", "bzbd"], tester(), base or self.tmp,
                            procs=self.procs, kill=self.procs.kill)
        self.addCleanup(fx.remove)
        return fx

    def test_cold_fixture_starts_no_daemons(self):
        fx = self.fixture()
        fx.write()
        written = sorted(str(p.relative_to(fx.root)) for p in fx.root.rglob("*") if not p.is_dir())
        self.assertEqual(written, ["bin/busybee", "bin/bzbd", "config/busybee.toml", "config/pueue.yml"])
        (fx.root / "work" / "made-later").write_text("x")
        self.assertEqual(fx.describe()["written"], written)
        self.assertFalse(fx.state.exists())
        self.assertFalse(fx.pueue.exists())
        checks = fx.cold_checks()
        self.assertEqual([(c["name"], c["status"]) for c in checks], [("cold_fixture_starts_no_daemons", "passed")])
        # Copies of the workspace build, byte for byte, beside each other as busybee expects.
        self.assertEqual((fx.bin / "bzbd").read_bytes(), (self.bin / "bzbd").read_bytes())

    def test_cold_fixture_refuses_a_running_daemon(self):
        fx = self.fixture()
        fx.write()
        self.procs.listed = [{"pid": 41, "comm": "pueued", "uid": os.getuid(), "umask": "0022"}]
        checks = fx.cold_checks()
        self.assertEqual(checks[0]["status"], "failed")
        self.assertIn("pueued", checks[0]["detail"])
        fx.state.mkdir()
        self.procs.listed = []
        self.assertEqual(fx.cold_checks()[0]["status"], "failed")

    def test_routing_is_private_and_pueue_config_is_a_file(self):
        fx = self.fixture()
        fx.write()
        env = fx.env()
        self.assertEqual(env["BUSYBEE_STATE_DIR"], str(fx.root / "state"))
        self.assertEqual(env["BUSYBEE_CONFIG"], str(fx.root / "config" / "busybee.toml"))
        self.assertTrue(Path(env["PUEUE_CONFIG_PATH"]).is_file())
        self.assertEqual(env["HOME"], str(fx.root / "home"))
        self.assertEqual(env[runner.MARKER], str(fx.root))
        yaml = Path(env["PUEUE_CONFIG_PATH"]).read_text()
        # pueued creates its pueue_directory but not a separate runtime_directory.
        self.assertIn(f"pueue_directory: {fx.pueue}\n", yaml)
        self.assertIn(f"runtime_directory: {fx.pueue}\n", yaml)
        self.assertIn(f"unix_socket_path: {fx.pueue / 'pueue.sock'}\n", yaml)
        self.assertNotIn("XDG_RUNTIME_DIR", env)

    def test_socket_paths_respect_the_platform_limit(self):
        deep = self.tmp / ("d" * 70)
        deep.mkdir()
        fx = self.fixture(base=deep)
        with self.assertRaises(runner.HarnessError) as raised:
            fx.write()
        self.assertEqual(raised.exception.code, "socket_path_too_long")
        self.assertEqual(runner.SOCKET_LIMIT, {"linux": 108, "macos": 104})

    def test_runs_are_bounded_and_kill_their_process_group(self):
        fx = self.fixture()
        fx.write()
        done = fx.run("quick", ["sh", "-c", "umask; echo out; echo err >&2; exit 3"], runner.monotonic() + 30)
        self.assertEqual(done["exit_code"], 3)
        self.assertEqual(done["stdout"], "0022\nout\n")
        self.assertEqual(done["stderr"], "err\n")
        started = runner.monotonic()
        with self.assertRaises(runner.Deadline):
            fx.run("wedged", ["sh", "-c", "sleep 30 & sleep 30"], runner.monotonic() + 1)
        self.assertLess(runner.monotonic() - started, 10)

    def test_cleanup_stops_scenario_processes_and_reports_survivors(self):
        fx = self.fixture()
        fx.write()
        self.procs.listed = [{"pid": 41, "comm": "bzbd", "uid": 1, "umask": "0177"}]
        cleanup = fx.cleanup(5)
        self.assertEqual([p["pid"] for p in cleanup["stopped"]], [41])
        self.assertEqual(cleanup["remaining"], [])
        self.assertFalse(fx.root.exists())

        stubborn = self.fixture()
        stubborn.write()
        self.procs.listed = [{"pid": 42, "comm": "pueued", "uid": 1, "umask": "0022"}]
        self.procs.kill = lambda pid, sig: self.procs.signalled.append((pid, sig))
        stubborn.kill = self.procs.kill
        cleanup = stubborn.cleanup(1)
        self.assertEqual([p["pid"] for p in cleanup["remaining"]], [42])

    @unittest.skipIf(os.geteuid() == 0, "root removes a directory whatever its mode")
    def test_a_root_that_cannot_be_removed_is_reported(self):
        fx = self.fixture()
        fx.write()
        locked = fx.root / "work" / "locked"
        locked.mkdir()
        (locked / "f").write_text("x")
        locked.chmod(0o500)
        self.addCleanup(locked.chmod, 0o700)  # runs before the fixture's own removal
        cleanup = fx.cleanup(1)
        self.assertFalse(cleanup["removed"])
        self.assertIn("Permission denied", cleanup["remove_error"])
        self.assertTrue(fx.root.exists())
        status, findings = runner.classify([], False, [{"name": "a", "status": "passed"}], ["a"], cleanup)
        self.assertEqual(status, "environment_failure")
        self.assertIn("cleanup_incomplete", [f["code"] for f in findings])

    def test_diagnostics_record_modes_and_config(self):
        fx = self.fixture()
        fx.write()
        fx.state.mkdir(mode=0o700)
        (fx.state / "bzbd.log").write_text("x" * (runner.LOG_TAIL + 10))
        found = fx.diagnostics()
        self.assertEqual(found["modes"]["state"], {"type": "dir", "mode": "0700"})
        self.assertIn("pool_size", found["files"]["config/busybee.toml"])
        self.assertEqual(len(found["files"]["state/bzbd.log"]), runner.LOG_TAIL)


class ClassifyTests(unittest.TestCase):
    declared = ["a", "b"]

    def classify(self, harness=(), timed_out=False, assertions=(), remaining=()):
        # The shape Fixture.cleanup returns: a survivor keeps the root.
        return runner.classify(list(harness), timed_out, list(assertions), self.declared,
                               {"remaining": list(remaining), "removed": not remaining,
                                "remove_error": "scenario processes are still running" if remaining else None})

    def passed(self, *names):
        return [{"name": n, "status": "passed"} for n in names]

    def test_all_passed_is_success(self):
        self.assertEqual(self.classify(assertions=self.passed("a", "b"))[0], "success")

    def test_a_failed_assertion_is_a_product_failure_and_stays_one(self):
        failed = [{"name": "a", "status": "failed"}, {"name": "b", "status": "not_reached"}]
        self.assertEqual(self.classify(assertions=failed)[0], "product_failure")
        status, findings = self.classify(assertions=failed, remaining=[{"pid": 9}])
        self.assertEqual(status, "product_failure")
        self.assertIn("cleanup_incomplete", [f["code"] for f in findings])

    def test_a_failed_assertion_outranks_a_later_fault(self):
        failed = [{"name": "a", "status": "failed"}, {"name": "b", "status": "not_reached"}]
        self.assertEqual(self.classify(timed_out=True, assertions=failed)[0], "product_failure")
        crash = [{"code": "runner_error", "message": "m"}]
        self.assertEqual(self.classify(harness=crash, assertions=failed)[0], "product_failure")

    def test_harness_faults_and_timeouts_are_never_success(self):
        harness = [{"code": "tool_missing", "message": "m"}]
        self.assertEqual(self.classify(harness=harness)[0], "environment_failure")
        self.assertEqual(self.classify(timed_out=True, assertions=self.passed("a"))[0], "timeout")
        self.assertEqual(self.classify(assertions=self.passed("a", "b"), remaining=[{"pid": 9}])[0],
                         "environment_failure")

    def test_no_fixture_means_nothing_to_remove(self):
        status, findings = runner.classify([{"code": "tool_missing", "message": "m"}], False, [], self.declared,
                                           runner.NO_CLEANUP)
        self.assertNotIn("cleanup_incomplete", [f["code"] for f in findings])
        self.assertIsNone(runner.NO_CLEANUP["removed"])

    def test_an_unevaluated_assertion_is_not_a_pass(self):
        status, findings = self.classify(assertions=self.passed("a"))
        self.assertEqual(status, "environment_failure")
        self.assertIn("assertion_not_evaluated", [f["code"] for f in findings])


class FakeFixture:
    """Stands in for Fixture in run_scenario: records what was asked of it."""

    def __init__(self, test, mode, wedge=False, cold_ok=True):
        self.test, self.mode, self.wedge, self.cold_ok = test, mode, wedge, cold_ok
        self.calls = []
        self.root = Path("/tmp/bzs-test")
        self.terminals, self.terminal_dir = {}, None

    def close_terminals(self, seconds):
        self.calls.append("close_terminals")
        return {name: {"forced": [], "remaining": []} for name in self.terminals}

    def write(self):
        self.calls.append("write")

    def cold_checks(self):
        self.calls.append("cold_checks")
        return [{"name": "cold_fixture_starts_no_daemons", "status": "passed" if self.cold_ok else "failed",
                 "detail": ""}]

    def prepare(self, deadline):
        self.calls.append("prepare")
        return [{"name": "pueued", "pid": 1, "umask": "0022"}]

    def describe(self):
        return {"root": str(self.root), "mode": self.mode}

    def diagnostics(self):
        self.calls.append("diagnostics")
        return {"modes": {}, "files": {"state/bzbd.log": "log"}, "processes": []}

    def cleanup(self, seconds):
        self.calls.append("cleanup")
        return {"stopped": [], "remaining": [], "removed": True, "elapsed_s": 0.0}


def wedged_procedure(fx, check, deadline):
    raise runner.Deadline("task")


class RunScenarioTests(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        self.bin = Path(tmp.name)
        self.probe = FakeProbe(self.bin)
        self.fixtures = []

    def run_scenario(self, mode="cold", procedure=None, **fixture):
        def make(mode_, *args, **kwargs):
            fx = FakeFixture(self, mode_, **fixture)
            self.fixtures.append(fx)
            return fx
        procs = dict(procedures.PROCEDURES)
        if procedure:
            procs["task_permissions"] = procedures.Procedure(procedure, procs["task_permissions"].assertions)
        return runner.run_scenario(meta(), mode, self.bin, "/checkout", 5, self.probe, make, procs)

    def test_missing_tool_is_environment_failure_before_any_fixture(self):
        del self.probe.tools["pueued"]
        result = self.run_scenario()
        self.assertEqual(result["status"], "environment_failure")
        self.assertEqual(self.fixtures, [])
        self.assertTrue(all(a["status"] == "not_reached" for a in result["assertions"]))
        self.assertEqual(runner.EXIT[result["status"]], 2)

    def test_prepared_fixture_is_explicit(self):
        def passing(fx, check, deadline):
            for name in procedures.PROCEDURES["task_permissions"].assertions:
                check.record(name, True, "ok")
        result = self.run_scenario("prepared", passing)
        self.assertEqual(result["status"], "success")
        self.assertEqual((result["mode"], result["fixture"]["mode"]), ("prepared", "prepared"))
        self.assertEqual(result["required_modes"], ["cold"])
        self.assertFalse(result["satisfies_required_mode"])
        self.assertEqual(self.fixtures[0].calls[:2], ["write", "prepare"])
        self.assertEqual(result["fixture"]["daemons"][0]["umask"], "0022")

    def test_cold_fixture_starts_no_daemons(self):
        def passing(fx, check, deadline):
            for name in procedures.PROCEDURES["task_permissions"].assertions:
                check.record(name, True, "ok")
        result = self.run_scenario("cold", passing)
        self.assertNotIn("prepare", self.fixtures[0].calls)
        self.assertEqual(result["fixture"]["checks"][0]["status"], "passed")
        self.assertTrue(result["satisfies_required_mode"])
        dirty = self.run_scenario("cold", passing, cold_ok=False)
        self.assertEqual(dirty["status"], "environment_failure")
        self.assertIn("fixture_not_cold", [f["code"] for f in dirty["findings"]])

    def test_scenario_timeout_is_not_success(self):
        result = self.run_scenario("cold", wedged_procedure)
        self.assertEqual(result["status"], "timeout")
        self.assertIn("scenario_timeout", [f["code"] for f in result["findings"]])
        # Diagnostics are taken before cleanup, and both happen after a timeout.
        self.assertEqual(self.fixtures[0].calls[-2:], ["diagnostics", "cleanup"])
        self.assertEqual(result["diagnostics"]["files"]["state/bzbd.log"], "log")

    def test_a_crashing_procedure_is_an_environment_failure(self):
        def crash(fx, check, deadline):
            raise KeyError("boom")
        result = self.run_scenario("cold", crash)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("runner_error", [f["code"] for f in result["findings"]])
        self.assertEqual(self.fixtures[0].calls[-1], "cleanup")

    def test_terminals_are_closed_before_cleanup_and_reported(self):
        def drove_a_terminal(fx, check, deadline):
            fx.terminals["monitor"], fx.terminal_dir = object(), Path("/var/tmp/bzt-test")
            for name in check.declared:
                check.record(name, True, "seen")
        result = self.run_scenario("cold", drove_a_terminal)
        self.assertEqual(result["status"], "success", result["findings"])
        calls = self.fixtures[0].calls
        self.assertLess(calls.index("close_terminals"), calls.index("cleanup"))
        self.assertEqual(result["terminals"], {"dir": "/var/tmp/bzt-test", "names": ["monitor"],
                                               "closed": {"monitor": {"forced": [], "remaining": []}}})
        self.assertNotIn("terminals", self.run_scenario("cold", wedged_procedure))

    def test_a_terminal_that_cannot_be_provided_is_environment_failure(self):
        def no_zellij(fx, check, deadline):
            raise runner.terminal.TerminalError("tool_version_too_old", "zellij is 0.40.0")
        result = self.run_scenario("cold", no_zellij)
        self.assertEqual(result["status"], "environment_failure")
        self.assertIn("tool_version_too_old", {f["code"] for f in result["findings"]})

    def test_result_is_one_json_document(self):
        result = self.run_scenario("cold", wedged_procedure)
        self.assertEqual(result["schema"], runner.RESULT_SCHEMA)
        self.assertEqual(json.loads(json.dumps(result)), result)


FAKE_BUSYBEE = """#!/bin/sh
# Stands in for busybee: `status --json` reports a whole pool; otherwise it runs
# the task after `--` under the umask its daemon would hand down.
if [ "$1" = status ]; then echo '{"pool_size": 2, "free": 2, "held": 0, "leases": []}'; exit 0; fi
shift
umask %s
exec "$@"
"""


class ProcedureTests(unittest.TestCase):
    def fixture(self, task_umask):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        bin_dir = Path(tmp.name) / "bin"
        bin_dir.mkdir()
        FakeProbe(bin_dir)
        (bin_dir / "busybee").write_text(FAKE_BUSYBEE % task_umask)
        procs = FakeProcesses()
        fx = runner.Fixture("cold", "linux", bin_dir, ["busybee", "bzbd"], tester(), tmp.name, procs=procs,
                            kill=procs.kill)
        fx.write()
        self.addCleanup(fx.remove)
        return fx

    def run_procedure(self, task_umask):
        fx = self.fixture(task_umask)
        check = runner.Check(procedures.PROCEDURES["task_permissions"].assertions)
        procedures.task_permissions(fx, check, runner.monotonic() + 30)
        return {a["name"]: a["status"] for a in check.listed()}, check.observations

    def test_task_permissions_pass_under_the_callers_umask(self):
        statuses, seen = self.run_procedure("022")
        self.assertEqual(set(statuses.values()), {"passed"}, statuses)
        self.assertEqual((seen["task"]["umask"], seen["task"]["directory_mode"], seen["task"]["executable_mode"]),
                         ("0022", "0755", "0755"))

    @unittest.skipIf(os.geteuid() == 0, "root bypasses the directory permission this checks")
    def test_a_leaked_daemon_umask_fails_the_task_assertions(self):
        statuses, seen = self.run_procedure("177")
        self.assertEqual(statuses, {"task_runs": "passed", "task_directory_traversable": "failed",
                                    "task_executable_output_runs": "failed", "lease_released": "passed"})
        self.assertEqual((seen["task"]["umask"], seen["task"]["directory_mode"]), ("0177", "0600"))

    def test_probe_facts_parse(self):
        facts = procedures.parse_probe("umask=0177\nmkdir=0\nnested=1\ncp=0\nexec=126\n")
        self.assertEqual(facts, {"umask": "0177", "mkdir": "0", "nested": "1", "cp": "0", "exec": "126"})

    def test_lease_accounting(self):
        clean = {"pool_size": 2, "free": 2, "held": 0, "leases": []}
        self.assertTrue(procedures.released(clean)[0])
        self.assertFalse(procedures.released({**clean, "free": 1, "held": 1})[0])
        self.assertFalse(procedures.released(None)[0])


if __name__ == "__main__":
    unittest.main()
