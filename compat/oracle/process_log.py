# SPDX-License-Identifier: GPL-3.0-or-later
"""Per-process audit paths shared by the oracle parent and its children."""

import json
import os

RECEIPT_KEYS = (
    "HOME",
    "USERPROFILE",
    "LANG",
    "LC_ALL",
    "PWD",
    "CARGO_TARGET_DIR",
    "PYTHONDONTWRITEBYTECODE",
    "PYTHONNOUSERSITE",
    "XEH_FIXTURE_ROOT",
    "XEH_UPSTREAM",
    "XEH_SUITE",
    "XEH_LEASE_TMP",
)

RUNTIME_KEYS = (
    "PATH",
    "PYTHONPATH",
    "PYTHONHOME",
    "VIRTUAL_ENV",
)


def process_id():
    return os.environ.get("XEH_PROCESS_ID", "root")


def audit_path():
    explicit = os.environ.get("XEH_AUDIT_PATH")
    if explicit:
        return explicit
    out = os.environ.get("XEH_OUT")
    if not out:
        return None
    return os.path.join(out, "processes", process_id(), "audit.json")


def observed_env():
    return {key: os.environ.get(key) for key in RECEIPT_KEYS if key in os.environ}


def child_env(process_id_value, audit, out_path):
    env = {}
    for key in RECEIPT_KEYS + RUNTIME_KEYS:
        if key in os.environ:
            env[key] = os.environ[key]
    for key in (
        "XEH_FORBIDDEN_ROOTS",
        "XEH_FIXTURE_ROOT",
        "XEH_UPSTREAM",
        "XEH_WORKSPACE",
        "XEH_SUITE",
        "XEH_LEASE_TMP",
    ):
        if key in os.environ:
            env[key] = os.environ[key]
    env["XEH_PROCESS_ID"] = process_id_value
    env["XEH_PARENT_ID"] = process_id()
    env["XEH_AUDIT_PATH"] = audit
    env["XEH_OUT"] = out_path
    env["COLUMNS"] = "4096"
    return env


def ensure_expected():
    if process_id() != "root":
        return
    out = os.environ.get("XEH_OUT")
    if not out:
        return
    path = os.path.join(out, "expected-children.json")
    if os.path.isfile(path):
        return
    with open(path, "w") as handle:
        json.dump({"processes": []}, handle)


def add_child(entry):
    if process_id() != "root":
        return
    out = os.environ["XEH_OUT"]
    path = os.path.join(out, "expected-children.json")
    current = {"processes": []}
    if os.path.isfile(path):
        with open(path) as handle:
            current = json.load(handle)
    current.setdefault("processes", []).append(entry)
    with open(path, "w") as handle:
        json.dump(current, handle)


def write_required(path, data):
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    try:
        with open(path, "wb") as handle:
            handle.write(data)
    except OSError as exc:
        raise SystemExit("cannot write oracle output %s: %s" % (path, exc))
