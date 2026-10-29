# SPDX-License-Identifier: GPL-3.0-or-later
"""Process isolation for the xeHentai compatibility oracle.

The entry script lives in the fixture root so the original FILEPATH
assignment resolves inside that root. This file does not patch upstream
sources.
"""

import atexit
import json
import os
import socket
import subprocess
import sys
import uuid

import process_log

OPENS = []
SPAWNS = []
FORBIDDEN = [item for item in os.environ.get("XEH_FORBIDDEN_ROOTS", "").split(":") if item]
PROCESS_ID = os.environ.get("XEH_PROCESS_ID", "root")
PARENT_ID = os.environ.get("XEH_PARENT_ID")
_GUID_SEQ = {"n": 0}


def install_deterministic_ids():
    """Replace uuid4 with a per-process counter before upstream import.

    Task ids stay concrete golden values. They are not stripped as volatile
    fields; a change in call count or order changes the id and must fail compare.
    """

    def uuid4():
        _GUID_SEQ["n"] += 1
        return uuid.UUID(int=_GUID_SEQ["n"] << 96)

    uuid.uuid4 = uuid4


def _record_open(path):
    if isinstance(path, bytes):
        path = path.decode("utf-8", "replace")
    if isinstance(path, str):
        OPENS.append(path)


def _audit_hook(event, args):
    if event == "open" and args:
        _record_open(args[0])
    elif event == "subprocess.Popen":
        _record_spawn(args)


def _spawn_field(env, key):
    if not isinstance(env, dict):
        return None
    value = env.get(key)
    if isinstance(value, str) and value:
        return value
    return None


def _record_spawn(args):
    """Record one Popen attempt from the passed env. Never store args or the env."""
    env = args[3] if len(args) > 3 else None
    SPAWNS.append({
        "child_id": _spawn_field(env, "XEH_PROCESS_ID"),
        "parent_id": _spawn_field(env, "XEH_PARENT_ID"),
        "audit": _spawn_field(env, "XEH_AUDIT_PATH"),
        "output": _spawn_field(env, "XEH_OUT"),
        "run_id": _spawn_field(env, "XEH_RUN_ID"),
    })


def _install_socket_guard():
    allowed = {"127.0.0.1", "::1", "localhost"}
    real_connect = socket.socket.connect

    def connect(self, address):
        host = address[0] if isinstance(address, tuple) and address else address
        if host not in allowed:
            raise ConnectionRefusedError("blocked non-local socket %r" % (address,))
        return real_connect(self, address)

    socket.socket.connect = connect
    real_create = socket.create_connection

    def create_connection(address, *args, **kwargs):
        host = address[0] if isinstance(address, tuple) and address else address
        if host not in allowed:
            raise ConnectionRefusedError("blocked non-local socket %r" % (address,))
        return real_create(address, *args, **kwargs)

    socket.create_connection = create_connection


def _under_forbidden(path):
    for root in FORBIDDEN:
        if path == root or path.startswith(root + os.sep):
            return True
    return False


def _write_json(path, data):
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    with open(path, "w") as handle:
        json.dump(data, handle)


def _valid_audit(process_id, parent_id, opens=None):
    run_id = os.environ.get("XEH_RUN_ID")
    if not run_id:
        raise SystemExit("capture run_id is missing")
    return {
        "process_id": process_id,
        "parent_id": parent_id,
        "pid": os.getpid(),
        "opens": [] if opens is None else opens,
        "home": os.environ.get("HOME"),
        "cwd": os.getcwd(),
        "filepath": None,
        "env": process_log.observed_env(),
        "run_id": run_id,
        "spawns": [],
    }


def _audit_body(payload, shape):
    if shape == "array":
        return []
    if shape == "empty-object":
        return {}
    if shape == "missing-opens":
        body = dict(payload)
        body.pop("opens", None)
        return body
    return payload


def _planted_record(out, audit_body, record_id, parent, audit_path=None):
    if audit_path is None:
        child_dir = os.path.join(out, "processes", "planted-child")
        audit = os.path.join(child_dir, "audit.json")
        stdout = os.path.join(child_dir, "stdout.txt")
        stderr = os.path.join(child_dir, "stderr.txt")
    else:
        audit = audit_path
        parent_dir = os.path.dirname(audit_path)
        stdout = os.path.join(parent_dir, "stdout.txt")
        stderr = os.path.join(parent_dir, "stderr.txt")
    _write_json(audit, audit_body)
    os.makedirs(os.path.dirname(stdout), exist_ok=True)
    open(stdout, "wb").close()
    open(stderr, "wb").close()
    return {
        "id": record_id,
        "parent": parent,
        "audit": audit,
        "stdout": stdout,
        "stderr": stderr,
        "exit": 0,
    }


_PROCESS_SET_INJECTION = {"done": False}


def _real_process_audits(out):
    root = os.path.join(out, "processes")
    found = []
    if not os.path.isdir(root):
        return found
    for name in os.listdir(root):
        if name == "root":
            continue
        audit = os.path.join(root, name, "audit.json")
        if os.path.isfile(audit) and ("cli-" in name or "locale" in name):
            found.append(audit)
    return found


def _load_child_list(path):
    if not os.path.isfile(path):
        return {"processes": []}
    with open(path) as handle:
        return json.load(handle)


def _write_child_list(path, current, processes):
    body = {"processes": processes}
    run_id = current.get("run_id") if isinstance(current, dict) else None
    if not run_id:
        run_id = os.environ.get("XEH_RUN_ID")
    if run_id:
        body["run_id"] = run_id
    _write_json(path, body)


def apply_process_set_injection(shape, out, path, fixture):
    """Mutate capture files after real CLI/locale children exist.

    Unset in production captures. This does not alter the fixed upstream tree.
    """
    if shape not in (
        "all-empty",
        "all-drop-one",
        "all-bogus-parent",
        "all-duplicate-id",
        "all-extra-audit",
        "all-stale-run",
        "all-foreign-audit",
    ):
        return False
    if _PROCESS_SET_INJECTION["done"]:
        return True
    real = _real_process_audits(out)
    if not real:
        return True
    _PROCESS_SET_INJECTION["done"] = True
    current = _load_child_list(path)
    processes = list(current.get("processes") or [])
    if shape == "all-empty":
        _write_child_list(path, current, [])
        return True
    if shape == "all-drop-one":
        kept = []
        dropped = False
        for item in processes:
            ident = item.get("id") if isinstance(item, dict) else ""
            audit = item.get("audit") if isinstance(item, dict) else ""
            if (
                not dropped
                and isinstance(ident, str)
                and isinstance(audit, str)
                and os.path.isfile(audit)
                and ("cli-" in ident or "locale" in ident)
            ):
                dropped = True
                continue
            kept.append(item)
        _write_child_list(path, current, kept)
        return True
    if shape == "all-bogus-parent":
        for item in processes:
            audit = item.get("audit") if isinstance(item, dict) else ""
            if isinstance(audit, str) and os.path.isfile(audit):
                item["parent"] = "missing-parent"
                with open(audit) as handle:
                    body = json.load(handle)
                body["parent_id"] = "missing-parent"
                _write_json(audit, body)
                break
        _write_child_list(path, current, processes)
        return True
    if shape == "all-duplicate-id":
        if processes:
            processes.append(dict(processes[0]))
        _write_child_list(path, current, processes)
        return True
    if shape == "all-extra-audit":
        extra = os.path.join(out, "processes", "stale-child", "audit.json")
        _write_json(extra, _valid_audit("stale-child", "root"))
        return True
    if shape == "all-stale-run":
        stale = "stale-run-id"
        _write_json(path, {"processes": processes, "run_id": stale})
        audit_root = os.path.join(out, "processes")
        for dirpath, _dirnames, filenames in os.walk(audit_root):
            if "audit.json" not in filenames:
                continue
            audit_path = os.path.join(dirpath, "audit.json")
            with open(audit_path) as handle:
                body = json.load(handle)
            body["run_id"] = stale
            for spawn in body.get("spawns") or []:
                if isinstance(spawn, dict):
                    spawn["run_id"] = stale
            _write_json(audit_path, body)
        return True
    if shape == "all-foreign-audit":
        foreign = os.path.join(fixture, "other-run", "audit.json")
        for item in processes:
            audit = item.get("audit") if isinstance(item, dict) else ""
            if isinstance(audit, str) and os.path.isfile(audit):
                with open(audit) as handle:
                    body = json.load(handle)
                _write_json(foreign, body)
                item["audit"] = foreign
                break
        _write_child_list(path, current, processes)
        return True
    return False


def apply_child_list_shape():
    """Test injection. Unset in production captures."""
    if PROCESS_ID != "root":
        return
    shape = os.environ.get("XEH_CHILD_LIST_SHAPE")
    if not shape:
        return
    out = os.environ.get("XEH_OUT")
    fixture = os.environ.get("XEH_FIXTURE_ROOT")
    if not out or not fixture:
        return
    path = os.path.join(out, "expected-children.json")
    if apply_process_set_injection(shape, out, path, fixture):
        return
    if shape == "empty-object":
        _write_json(path, {})
        return
    if shape == "processes-null":
        _write_json(path, {"processes": None})
        return
    if shape == "bad-item":
        audit = os.path.join(out, "processes", "planted-child", "audit.json")
        _write_json(audit, _valid_audit("planted-child", "root"))
        _write_json(path, {"processes": [{"audit": audit, "exit": 0}]})
        return
    forbidden = (FORBIDDEN[0] + "/SECRET_SENTINEL") if FORBIDDEN else "/home/wow/SECRET_SENTINEL"
    if shape == "child-empty-object":
        record = _planted_record(out, {}, "planted-child", "root")
    elif shape == "child-opens-string":
        record = _planted_record(
            out, dict(_valid_audit("planted-child", "root"), opens="not-a-list"),
            "planted-child", "root",
        )
    elif shape == "child-opens-mixed":
        record = _planted_record(
            out, dict(_valid_audit("planted-child", "root"), opens=[1, forbidden]),
            "planted-child", "root",
        )
    elif shape == "identity-mismatch":
        record = _planted_record(
            out, _valid_audit("planted-child", "root"), "not-the-child", "root",
        )
    elif shape == "parent-mismatch":
        record = _planted_record(
            out, _valid_audit("planted-child", "root"), "planted-child", "not-root",
        )
    elif shape == "foreign-audit":
        foreign = os.path.join(fixture, "other-run", "audit.json")
        record = _planted_record(
            out, _valid_audit("foreign-child", "root"), "foreign-child", "root",
            audit_path=foreign,
        )
    else:
        return
    _write_child_list(path, _load_child_list(path), [record])


def write_audit():
    path = process_log.audit_path()
    if not path or os.environ.get("XEH_SUPPRESS_AUDIT") == "1":
        return
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    if os.environ.get("XEH_MALFORM_AUDIT") == "1":
        with open(path, "w") as handle:
            handle.write("not-json")
        return
    run_id = os.environ.get("XEH_RUN_ID")
    if not run_id:
        raise SystemExit("capture run_id is missing")
    payload = {
        "process_id": PROCESS_ID,
        "parent_id": PARENT_ID,
        "pid": os.getpid(),
        "opens": OPENS,
        "home": os.environ.get("HOME"),
        "cwd": os.getcwd(),
        "filepath": getattr(sys.modules.get("xeHentai.const"), "FILEPATH", None),
        "env": process_log.observed_env(),
        "run_id": run_id,
        "spawns": SPAWNS,
    }
    shape = os.environ.get("XEH_AUDIT_SHAPE") if PROCESS_ID == "root" else None
    with open(path, "w") as handle:
        json.dump(_audit_body(payload, shape), handle)
    if PROCESS_ID == "root":
        apply_child_list_shape()


def install_guards():
    sys.addaudithook(_audit_hook)
    _install_socket_guard()
    atexit.register(write_audit)


def isolation_failed():
    fixture = os.environ.get("XEH_FIXTURE_ROOT", "")
    const = sys.modules.get("xeHentai.const")
    filepath = getattr(const, "FILEPATH", "")
    if fixture and filepath and not (filepath == fixture or str(filepath).startswith(fixture + os.sep)):
        return "FILEPATH escaped the fixture: %s" % filepath
    for path in OPENS:
        if _under_forbidden(path):
            return path
    return None


def decoy_open():
    install_guards()
    for root in FORBIDDEN:
        sentinel = os.path.join(root, "SECRET_SENTINEL")
        try:
            handle = open(sentinel, "rb")
            handle.close()
        except OSError:
            pass
    write_audit()
    raise SystemExit(3)


def spawn_decoy():
    if os.environ.get("XEH_DECOY_CHILD") != "1" or PROCESS_ID != "root":
        return
    out = os.environ["XEH_OUT"]
    child_out = os.path.join(out, "processes", "decoy-child")
    os.makedirs(child_out, exist_ok=True)
    audit = os.path.join(child_out, "audit.json")
    env = process_log.child_env("decoy-child", audit, child_out)
    if os.environ.get("XEH_SUPPRESS_CHILD_AUDIT") == "1":
        env["XEH_SUPPRESS_AUDIT"] = "1"
    env.pop("XEH_DECOY_CHILD", None)
    proc = subprocess.Popen(
        [sys.executable, os.path.abspath(__file__), "--mode", "decoy-open"],
        cwd=os.environ.get("XEH_FIXTURE_ROOT", os.getcwd()),
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    stdout, stderr = proc.communicate()
    process_log.write_required(os.path.join(child_out, "stdout.txt"), stdout)
    process_log.write_required(os.path.join(child_out, "stderr.txt"), stderr)
    process_log.add_child(
        {
            "id": "decoy-child",
            "parent": PROCESS_ID,
            "audit": audit,
            "stdout": os.path.join(child_out, "stdout.txt"),
            "stderr": os.path.join(child_out, "stderr.txt"),
            "exit": proc.returncode,
        }
    )


def main(argv):
    mode = None
    suite = "all"
    rest = []
    index = 0
    while index < len(argv):
        item = argv[index]
        if item == "--mode" and index + 1 < len(argv):
            mode = argv[index + 1]
            index += 2
            continue
        if item == "--suite" and index + 1 < len(argv):
            suite = argv[index + 1]
            index += 2
            continue
        if item == "--":
            rest = argv[index + 1:]
            break
        rest.append(item)
        index += 1
    if mode is None:
        raise SystemExit("loader requires --mode")
    if mode == "decoy-open":
        decoy_open()
    install_guards()
    if os.environ.get("XEH_ISOLATION_BREAK") == "1" and FORBIDDEN:
        sentinel = os.path.join(FORBIDDEN[0], "SECRET_SENTINEL")
        try:
            handle = open(sentinel, "rb")
            handle.close()
        except OSError:
            pass
    if PROCESS_ID == "root":
        process_log.ensure_expected()
        spawn_decoy()
    install_deterministic_ids()
    upstream = os.environ["XEH_UPSTREAM"]
    sys.path.insert(0, upstream)
    import xeHentai.const as const

    const.FILEPATH = os.environ.get("XEH_FIXTURE_ROOT", os.getcwd())
    if mode == "suite" and suite == "isolation":
        import xeHentai.core  # noqa: F401
        reason = isolation_failed()
        write_audit()
        if reason:
            sys.stderr.write("isolation breach: %s\n" % reason)
            raise SystemExit(3)
        sys.stdout.write("isolation-ok\n")
        raise SystemExit(0)
    import harness

    if mode == "suite":
        harness.run_suite(suite)
    else:
        harness.run_mode(mode, rest)
    reason = isolation_failed()
    write_audit()
    if reason:
        sys.stderr.write("isolation breach: %s\n" % reason)
        raise SystemExit(3)


if __name__ == "__main__":
    try:
        main(sys.argv[1:])
    except SystemExit:
        raise
    except Exception:
        write_audit()
        raise
