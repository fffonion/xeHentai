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
import sys

OPENS = []
FORBIDDEN = [item for item in os.environ.get("XEH_FORBIDDEN_ROOTS", "").split(":") if item]


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
    out = os.environ.get("XEH_OUT")
    if not out:
        return
    payload = {
        "opens": OPENS,
        "home": os.environ.get("HOME"),
        "cwd": os.getcwd(),
        "filepath": getattr(sys.modules.get("xeHentai.const"), "FILEPATH", None),
    }
    with open(os.path.join(out, "audit.json"), "w") as handle:
        json.dump(payload, handle)


def install_guards():
    sys.addaudithook(_audit_hook)
    _install_socket_guard()
    atexit.register(write_audit)
    if os.environ.get("XEH_ISOLATION_BREAK") == "1" and FORBIDDEN:
        sentinel = os.path.join(FORBIDDEN[0], "SECRET_SENTINEL")
        handle = open(sentinel, "rb")
        handle.close()


def isolation_failed():
    fixture = os.environ.get("XEH_FIXTURE_ROOT", "")
    const = sys.modules.get("xeHentai.const")
    filepath = getattr(const, "FILEPATH", "")
    if fixture and not (filepath == fixture or str(filepath).startswith(fixture + os.sep)):
        return "FILEPATH escaped the fixture: %s" % filepath
    for path in OPENS:
        if _under_forbidden(path):
            return path
    return None


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
    install_guards()
    if mode == "suite" and suite == "isolation":
        import xeHentai.core  # noqa: F401
        reason = isolation_failed()
        write_audit()
        if reason:
            sys.stderr.write("isolation breach: %s\n" % reason)
            raise SystemExit(3)
        sys.stdout.write("isolation-ok\n")
        raise SystemExit(0)
    if mode == "suite":
        import harness
        harness.run_suite(suite)
        reason = isolation_failed()
        write_audit()
        if reason:
            sys.stderr.write("isolation breach: %s\n" % reason)
            raise SystemExit(3)
        return
    import harness
    harness.run_mode(mode, rest)
    reason = isolation_failed()
    write_audit()
    if reason:
        sys.stderr.write("isolation breach: %s\n" % reason)
        raise SystemExit(3)


if __name__ == "__main__":
    main(sys.argv[1:])
