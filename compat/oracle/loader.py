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
    payload = {
        "process_id": PROCESS_ID,
        "parent_id": PARENT_ID,
        "pid": os.getpid(),
        "opens": OPENS,
        "home": os.environ.get("HOME"),
        "cwd": os.getcwd(),
        "filepath": getattr(sys.modules.get("xeHentai.const"), "FILEPATH", None),
        "env": process_log.observed_env(),
    }
    with open(path, "w") as handle:
        json.dump(payload, handle)


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
