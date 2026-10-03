"""macOS baselines: a candidate prepared from an operator's macOS VM, validated on a clone.

See docs/design/agent-lab.md §Build and validate templates and §macOS workers.
macOS cannot be installed unattended the way the NixOS template is, so its
one-time, partly interactive setup (the OS, its account with passwordless
sudo, Nix with an unencrypted store, SSH) is an operator's prepared VM, named
as `[templates.macos] source`. `template build macos` full-clones it into an
owned candidate and provisions that from infra/vm/macos without a person:
the pinned Command Line Tools, a neutral host name, the run's own SSH host
key and access key in place of the source's, warmed dependencies and GNU
timeout from the development shell. The source is read, never changed.

macOS guests have no Parallels guest exec, so SSH is the only control
channel; they ignore an ACPI stop request (a shutdown dialog appears instead)
and are halted from inside; they boot only from full clones; and Apple allows
two of them running per host, a limit Parallels reports only in the VM's log.
"""
import json
import shlex
import subprocess
import time

import contracts
import guest
import parallels
import registry
import template

CAPABILITIES = ("host_isolation", "boot", "nix_store", "dev_tools", "command_access", "file_roundtrip",
                "terminal_transport", "console_capture", "task_state", "shutdown", "snapshot_reset")
INFRA = template.INFRA / "macos"
# Tools every routine run relies on, and what their absence means.
DEV_TOOLS = (("xcode-select -p", "the Command Line Tools are not installed"),
             ("xcrun --find clang", "the Command Line Tools have no clang"),
             ("git --version", "git is only Apple's install shim"),
             ("timeout --version", "GNU timeout, which bounds every exec, is not on PATH"))
NIX_READY = "mount | grep -q ' on /nix ' && nix --version"


NIX_POLL_S = 2


def checkout(user):
    return f"/Users/{user}/busybee"


def wait_for_nix(g, until, bound, sleep=time.sleep):
    """Whether the Nix store came up before `until` (monotonic). SSH answers
    before determinate-nixd has mounted /nix, so command access alone is not
    readiness."""
    while True:
        if g.run(NIX_READY, bound(), check=False)[0] == 0:
            return True
        if time.monotonic() >= until:
            return False
        sleep(NIX_POLL_S)


class MacLab(template.Lab):
    OS = "macos"
    CAPABILITIES = CAPABILITIES
    TTY = "/dev/ttys"

    def _user(self, cdir):
        return json.loads((cdir / "candidate.json").read_text())["guest_user"]

    # Hooks

    def _boot(self, vm):
        self.prl.start_reporting(vm)

    def _access(self, ip, cdir, known_hosts):
        known_hosts.write_text(f"{ip} {' '.join((cdir / 'host.pub').read_text().split()[:2])}\n")
        return self.open_guest(ip, cdir / "access", known_hosts, user=self._user(cdir), posix=True)

    sleep = staticmethod(time.sleep)

    def _prerequisites(self, g, passed):
        if not wait_for_nix(g, self._until(self.config["deadlines"]["command"]), lambda: self._bound("command"),
                            self.sleep):
            raise RuntimeError("the Nix store is not mounted at /nix, or nix does not run: an encrypted store "
                               "waits for a password at boot")
        passed("nix_store", nix=g.run("nix --version", self._bound("command"))[1].strip())
        missing = [why for command, why in DEV_TOOLS if g.run(command, self._bound("command"), check=False)[0] != 0]
        if missing:
            raise RuntimeError("missing prerequisites: " + "; ".join(missing))
        passed("dev_tools", **{command.split()[0]: "present" for command, _ in DEV_TOOLS})

    def _identity(self, g):
        return {"macos": g.run("sw_vers -productVersion", self._bound("command"))[1].strip()}

    def _halt(self, vm, g):
        # The connection drops as the guest goes down; its status is no answer.
        g.run("sudo -n shutdown -h now", self._bound("command"), check=False)
        self._wait_state(vm, "stopped", self.config["deadlines"]["command"])

    # Building a candidate

    def _settings(self):
        entry = self.config.get("templates", {}).get("macos", {})
        missing = [k for k in ("source", "user", "bootstrap_key") if not entry.get(k)]
        return entry, missing

    def build(self, name, arch):
        op = "template build"
        entry, missing = self._settings()
        if missing:
            return contracts.result(op, "environment_failure", "the macOS source is not configured", [
                contracts.finding("config_invalid", f"[templates.macos] needs {', '.join(missing)} to build from "
                                  "an operator-prepared macOS VM")])
        pin = json.loads((self.repo / INFRA / "baseline.json").read_text())
        if arch != pin["arch"]:
            return contracts.result(op, "unsupported", f"no {name} baseline for {arch}", [
                contracts.finding("arch_unsupported", f"{name} baselines are pinned for {pin['arch']} only")])
        source = entry["source"]
        listed = {vm["name"]: vm["status"] for vm in json.loads(self.prl.query(["list", "--all", "--json"]))}
        if source not in listed:
            return contracts.result(op, "environment_failure", "no such source VM", [
                contracts.finding("source_missing", "Parallels lists no VM by the configured source name")])
        if listed[source] != "stopped":
            return contracts.result(op, "environment_failure", "the source VM is running", [
                contracts.finding("source_running", f"the source is {listed[source]}; a clone needs it stopped")])

        run_id = contracts.new_run_id()
        vm = f"{registry.PREFIX}tpl-{name}-{run_id}"
        cdir = template.candidate_dir(self.state, name, run_id)
        cdir.mkdir(parents=True)
        record = {"run_id": run_id, "vm": vm, "status": "building", "arch": arch, "guest_user": entry["user"]}
        template._write_json(cdir / "candidate.json", record)
        expires = self._start()
        try:
            for key in ("access", "host"):
                subprocess.run(["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", f"busybee-lab-{key}",
                                "-f", str(cdir / key)], check=True)
            self.reg.claim(vm, "candidate", name, run_id, expires)
            workers = self.state / "workers"
            workers.mkdir(parents=True, exist_ok=True)
            self.prl.clone_source(source, vm, workers)
            info = self.prl.info(vm)
            self.reg.bind(vm, info["vm_id"])
            self.prl.isolate(vm, info["devices"], template.HOST_DEVICES)
            budget = self.config["budget"]
            self.prl.allocate(vm, budget["cpus"], budget["memory_mib"])
            self._boot(vm)

            ip = self.lease(info["mac"], self._until(self.config["deadlines"]["command"]))
            bootstrap = self.open_guest(ip, self.state / entry["bootstrap_key"], cdir / "source_known_hosts",
                                        accept_new=True, user=entry["user"], posix=True)
            bootstrap.wait(self._until(self.config["deadlines"]["command"]))
            self._provision(bootstrap, cdir, pin)

            g = self._access(ip, cdir, cdir / "known_hosts")
            g.wait(self._until(self.config["deadlines"]["command"]))
            tools = self._warm_and_clean(g, entry["user"])
            tools["command_line_tools"] = pin["command_line_tools"]
            problems = template.task_state_problems(self._facts(g), (cdir / "access.pub").read_text())
            if problems:
                raise RuntimeError("baseline would carry task state: " + "; ".join(problems))
            guest_arch = g.run("uname -m", self._bound("command"))[1].strip()
            self._halt(vm, g)
            snapshot = self.prl.snapshot(vm, f"baseline {run_id}")
        except Exception as err:  # every failure ends in owned cleanup and a recorded result
            notes = self._dispose(vm, cdir / "failure-console.png")
            record.update(status="failed", reason=str(err), cleanup=notes)
            template._write_json(cdir / "candidate.json", record)
            status = "timeout" if isinstance(err, template.DeadlineExceeded) else "environment_failure"
            return contracts.result(op, status, f"candidate {run_id} failed", [
                contracts.finding(getattr(err, "code", "template_build_failed"), str(err)),
                *(contracts.finding("cleanup_incomplete", n) for n in notes)], {"candidate": run_id})

        revision, hashes = self._provenance(pin)
        record.update(status="built", manifest={
            "schema": contracts.TEMPLATE_SCHEMA, "name": name, "candidate": run_id, "os": "macos",
            "arch": guest_arch, "vm_id": info["vm_id"], "snapshot_id": snapshot,
            "provisioning_revision": revision, "lock_hashes": hashes, "tools": tools,
            "parallels_version": self.prl.query(["--version"]).split()[2], "clone_modes": [], "validated_at": None})
        template._write_json(cdir / "candidate.json", record)
        return contracts.result(op, "success", f"candidate {run_id} built; validate it next",
                                data={"candidate": run_id, "vm": vm, "snapshot_id": snapshot,
                                      "provenance": {"revision": revision, "lock_hashes": hashes, "tools": tools}})

    def _provenance(self, pin):
        revision, hashes = super()._provenance({"sha256": None})
        hashes.pop("installer")
        hashes["command_line_tools"] = pin["command_line_tools"]
        return revision, hashes

    def _provision(self, g, cdir, pin):
        """The pinned tools and the run's keys, through the operator's bootstrap key."""
        stage = "/private/tmp/busybee-lab-provision"
        g.run(f"rm -rf {stage} && mkdir -m 700 {stage} && cat > {stage}/authorized", self._bound("command"),
              stdin=(cdir / "access.pub").read_bytes())
        g.run(f"umask 077 && cat > {stage}/hostkey", self._bound("command"), stdin=(cdir / "host").read_bytes())
        args = " ".join(map(shlex.quote, (pin["command_line_tools"], pin["hostname"], f"{stage}/authorized",
                                          f"{stage}/hostkey")))
        _, out, err = g.run(f"bash -s -- {args}", self._bound("scenario"),
                            stdin=(self.repo / INFRA / "provision.sh").read_bytes())
        (cdir / "provision.log").write_text(out + err)

    def _warm_and_clean(self, g, user):
        co = checkout(user)
        source = subprocess.run(["git", "-C", str(self.repo), "archive", "HEAD"], capture_output=True,
                                check=True).stdout
        g.run(f"rm -rf {co} && mkdir -p {co} && tar -C {co} -xf -", self._bound("command"), stdin=source)
        # The dev shell and the crate sources; neither busybee nor pueued starts.
        g.run(f"cd {co} && nix develop -c cargo fetch", self._bound("scenario"))
        # Every exec is bounded by GNU timeout: take the dev shell's coreutils,
        # so its version is pinned by the repository's flake.lock.
        found = g.run(f"cd {co} && nix develop -c sh -c 'command -v timeout'", self._bound("command"))[1]
        coreutils = found.strip().splitlines()[-1].rsplit("/bin/", 1)[0]
        if not coreutils.startswith("/nix/store/"):
            raise RuntimeError(f"the dev shell's timeout is not from the Nix store: {found.strip()!r}")
        g.run(f"nix profile add {shlex.quote(coreutils)}", self._bound("command"))
        tools = {}
        for tool, command in (("macos", "sw_vers -productVersion"), ("nix", "nix --version"),
                              ("kernel", "uname -r"), ("timeout", "timeout --version | head -n 1"),
                              ("rustc", f"cd {co} && nix develop -c rustc --version")):
            tools[tool] = g.run(command, self._bound("command"))[1].strip().splitlines()[-1]
        g.run("bash -s", self._bound("command"), stdin=(self.repo / INFRA / "clean.sh").read_bytes())
        return tools
