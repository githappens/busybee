"""Which Parallels VMs the controller owns.

A VM is claimed here before it is created and released after it is deleted,
so an interrupted controller can always find what it left behind. Every
mutating Parallels call checks this registry; nothing else is ever touched.
"""
import json
import os
from pathlib import Path

SCHEMA = "busybee.vm.registry/v1"
PREFIX = "busybee-lab-"
ROLES = ("candidate", "validation")


class Registry:
    def __init__(self, state):
        self.path = Path(state) / "registry.json"

    def _load(self):
        if not self.path.exists():
            return {"schema": SCHEMA, "vms": {}}
        data = json.loads(self.path.read_text())
        if data.get("schema") != SCHEMA:
            raise ValueError(f"{self.path.name}: schema is not {SCHEMA}")
        return data

    def _save(self, data):
        self.path.parent.mkdir(parents=True, exist_ok=True)
        tmp = self.path.with_suffix(".tmp")
        tmp.write_text(json.dumps(data, indent=2) + "\n")
        os.replace(tmp, self.path)

    def get(self, name):
        return self._load()["vms"].get(name)

    def all(self):
        return self._load()["vms"]

    def owns(self, name):
        return name in self._load()["vms"]

    def claim(self, name, role, template, run_id, deadline):
        if not name.startswith(PREFIX) or role not in ROLES:
            raise ValueError(f"refusing to claim {name!r} as {role!r}")
        data = self._load()
        if name in data["vms"]:
            raise ValueError(f"{name} is already claimed")
        data["vms"][name] = {"role": role, "template": template, "run_id": run_id, "vm_id": None,
                             "deadline": deadline}
        self._save(data)

    def bind(self, name, vm_id):
        data = self._load()
        data["vms"][name]["vm_id"] = vm_id
        self._save(data)

    def release(self, name):
        data = self._load()
        data["vms"].pop(name)
        self._save(data)
