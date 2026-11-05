#!/usr/bin/env python3
"""Isolated Windows console controller for the M6 native Ctrl-C cases.

This script is test orchestration only. It is never imported by the product,
never used as an application loader or bootstrap, and has no dependency beyond
the Python standard library.

The controller is launched by one Windows-only Rust exact case with
CREATE_NEW_CONSOLE, so it owns a fresh console. It clears its own inherited
Ctrl-C-ignore attribute, starts the real ``xeHentai`` executable in *another*
new console with owned cwd/environment/pipes, waits for a real readiness signal
(the product's RPC ``getInfo`` answer, or the actual interactive login prompt
bytes), then attaches to the native console and broadcasts exactly one
CTRL_C_EVENT. It never signals the test runner console: the attached client set
must be exactly {controller, native child} or the run fails before signaling.

It prints exactly one fixed JSON object of typed facts (booleans, integer
counts, closed failure labels and numeric WinAPI error codes). It never prints
raw native output, environment or paths. Exit status 0 means every fact holds;
anything else is a failure that the Rust case must not launder.

On a non-Windows host ``execute`` refuses with ``UNSUPPORTED_PLATFORM``; the
real console event is only proven by the native Windows Actions job.
"""

import argparse
import json
import os
import socket
import subprocess
import sys
import threading
import time
from pathlib import Path

CTRL_C_EVENT = 0
CREATE_NEW_CONSOLE = 0x0000_0010

PROMPT_TEXT = b"login to exhentai (y/n)? > "
READY_TIMEOUT = 90.0
EXIT_TIMEOUT = 60.0
TAIL_LIMIT = 65536
LOG_READ_LIMIT = 4_000_000

REMOVE_ENV = (
    "XEH_EMIT_ROOT",
    "XEH_EVIDENCE_DIR",
    "XEH_CAPTURE_OUT",
    "RUST_TEST_THREADS",
    "XEH_BUILD_ID",
    "XEH_M6_PYTHON",
    "LC_MESSAGES",
)

FACT_KEYS = (
    "mode",
    "ok",
    "failure",
    "failure_op",
    "win_error_code",
    "signal_sent",
    "natural_exit",
    "forced_kill",
    "exit_code",
    "console_client_count",
    "console_clients_match",
    "unexpected_console_client",
    "prompt_observed",
    "started_logged",
    "cleanup_logged",
    "session_nw",
    "session_tasks_empty",
    "stdout_bytes",
    "stderr_bytes",
)

# Closed set of setup/control failure labels. They survive conclusion; derived
# fact failures (SIGNAL_NOT_SENT, ...) are recomputed from the facts.
STAGE_LABELS = frozenset(
    {
        "UNSUPPORTED_PLATFORM",
        "NATIVE_MISSING",
        "ROOT_MISSING",
        "RSS_FIXTURE_MISSING",
        "CLEAR_CTRL_HANDLER",
        "CONFIG_FIXTURE",
        "START_NATIVE",
        "EXITED_BEFORE_READY",
        "READY_TIMEOUT",
        "FREE_CONSOLE",
        "ATTACH_CONSOLE",
        "IGNORE_CTRL_HANDLER",
        "GET_CONSOLE_PROCESS_LIST",
        "UNEXPECTED_CONSOLE_CLIENT",
        "GENERATE_CTRL_C",
        "EXIT_TIMEOUT",
        "HARNESS_IO",
    }
)


class Stage(Exception):
    """A fixed failure stage; the label is part of the closed report."""

    def __init__(self, label, op=None, code=None):
        super().__init__(label)
        self.label = label
        self.op = op
        self.code = code


def parse_args(argv):
    parser = argparse.ArgumentParser(
        prog="m6_windows_console.py",
        description="Isolated Windows console Ctrl-C controller for M6 tests.",
    )
    parser.add_argument("--mode", required=True, choices=("resident", "prompt"))
    parser.add_argument("--native", required=True)
    parser.add_argument("--root", required=True)
    namespace = parser.parse_args(argv)
    return {
        "mode": namespace.mode,
        "native": Path(namespace.native),
        "root": Path(namespace.root),
    }


def facts_template(mode):
    facts = {
        "mode": mode,
        "ok": False,
        "failure": None,
        "failure_op": None,
        "win_error_code": None,
        "signal_sent": False,
        "natural_exit": False,
        "forced_kill": False,
        "exit_code": None,
        "console_client_count": 0,
        "console_clients_match": False,
        "unexpected_console_client": False,
        "prompt_observed": False,
        "started_logged": False,
        "cleanup_logged": False,
        "session_nw": None,
        "session_tasks_empty": False,
        "stdout_bytes": 0,
        "stderr_bytes": 0,
    }
    assert tuple(facts) == FACT_KEYS
    return facts


def console_clients(expected, observed):
    """Pure exact-set check for the console the controller just attached to."""
    expected_set = {int(pid) for pid in expected}
    observed_list = sorted(int(pid) for pid in observed)
    return {
        "count": len(observed_list),
        "match": observed_list == sorted(expected_set),
        "unexpected": sorted(set(observed_list) - expected_set),
        "missing": sorted(expected_set - set(observed_list)),
    }


def child_environment(base, root):
    """The owned child environment: platform runtime kept, test switches gone."""
    env = {name: value for name, value in base.items() if name not in REMOVE_ENV}
    home = str(Path(root) / "home")
    tmp = str(Path(root) / "tmp")
    cache = str(Path(root) / "cache")
    env.update(
        {
            "HOME": home,
            "USERPROFILE": home,
            "TMPDIR": tmp,
            "TEMP": tmp,
            "TMP": tmp,
            "XDG_CACHE_HOME": cache,
            "LANG": "C",
            "LC_ALL": "C",
        }
    )
    return env


def prompt_seen(tail):
    """Prompt bytes may arrive without a trailing newline; never wait for one."""
    return PROMPT_TEXT in bytes(tail)


def choose_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def write_rpc_config(root, port):
    (Path(root) / "config.py").write_text(
        'rpc_interface = "127.0.0.1"\n'
        f"rpc_port = {port}\n"
        "rpc_open_browser = False\n"
        'auto_update = "off"\n',
        encoding="utf-8",
    )


def rpc_get_info(port, timeout=2.0):
    """A real loopback JSON-RPC getInfo request; no proxy/environment involved."""
    body = b'{"method":"xeH.getInfo","id":"m6","params":[[],{}]}'
    head = (
        b"POST /jsonrpc HTTP/1.0\r\nHost: x\r\nContent-Length: "
        + str(len(body)).encode()
        + b"\r\n\r\n"
    )
    try:
        with socket.create_connection(("127.0.0.1", int(port)), timeout=timeout) as conn:
            conn.settimeout(timeout)
            conn.sendall(head + body)
            data = b""
            while True:
                chunk = conn.recv(8192)
                if not chunk:
                    break
                data += chunk
                if len(data) > 262144:
                    break
                if b'"version"' in data and data.rstrip().endswith(b"}"):
                    break
    except OSError:
        return False
    status = data.split(b"\r\n", 1)[0] if data else b""
    return status.startswith(b"HTTP/1.") and b" 200" in status and b'"version"' in data


class Drain:
    """Continuous pipe drain with a bounded in-memory tail."""

    def __init__(self, pipe):
        self.pipe = pipe
        self.total = 0
        self.tail = b""
        self._lock = threading.Lock()
        self._thread = threading.Thread(target=self._run, daemon=True)

    def start(self):
        self._thread.start()

    def _run(self):
        while True:
            try:
                chunk = self.pipe.read1(65536)
            except (OSError, ValueError):
                break
            if not chunk:
                break
            with self._lock:
                self.total += len(chunk)
                self.tail = (self.tail + chunk)[-TAIL_LIMIT:]

    def snapshot(self):
        with self._lock:
            return self.tail

    def join(self, timeout):
        self._thread.join(timeout)


def read_run_facts(root, facts):
    root = Path(root)
    try:
        log = (root / "eh.log").read_text(encoding="utf-8", errors="replace")
    except OSError:
        log = ""
    log = log[:LOG_READ_LIMIT]
    facts["started_logged"] = "started." in log
    facts["cleanup_logged"] = "cleaning up..." in log
    try:
        session = json.loads((root / "h.json").read_text(encoding="utf-8"))
    except (OSError, ValueError):
        session = None
    if isinstance(session, dict):
        cookies = session.get("cookies")
        if isinstance(cookies, dict) and isinstance(cookies.get("nw"), str):
            facts["session_nw"] = cookies["nw"]
        facts["session_tasks_empty"] = session.get("tasks") == {}


def conclude(facts):
    """Derive ok/failure from the recorded facts; a forced kill can never pass."""
    if facts["failure"] in STAGE_LABELS:
        facts["ok"] = False
        return facts
    facts["failure"] = None
    checks = [
        ("SIGNAL_NOT_SENT", facts["signal_sent"] is True),
        (
            "NATIVE_EXIT_NOT_CLEAN",
            facts["natural_exit"] is True
            and facts["exit_code"] == 0
            and facts["forced_kill"] is False,
        ),
        ("START_NOT_LOGGED", facts["started_logged"] is True),
        ("CLEANUP_NOT_LOGGED", facts["cleanup_logged"] is True),
        ("SESSION_INVALID", facts["session_nw"] == "1" and facts["session_tasks_empty"] is True),
        (
            "CONSOLE_CLIENTS_MISMATCH",
            facts["console_clients_match"] is True
            and facts["console_client_count"] == 2
            and facts["unexpected_console_client"] is False,
        ),
    ]
    if facts["mode"] == "prompt":
        checks.append(("PROMPT_NOT_OBSERVED", facts["prompt_observed"] is True))
    for label, passed in checks:
        if not passed:
            facts["failure"] = label
            facts["ok"] = False
            return facts
    facts["ok"] = True
    return facts


def windows_api():
    import ctypes
    from ctypes import wintypes

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.SetConsoleCtrlHandler.argtypes = [ctypes.c_void_p, wintypes.BOOL]
    kernel32.SetConsoleCtrlHandler.restype = wintypes.BOOL
    kernel32.FreeConsole.argtypes = []
    kernel32.FreeConsole.restype = wintypes.BOOL
    kernel32.AttachConsole.argtypes = [wintypes.DWORD]
    kernel32.AttachConsole.restype = wintypes.BOOL
    kernel32.GetConsoleProcessList.argtypes = [
        ctypes.POINTER(wintypes.DWORD),
        wintypes.DWORD,
    ]
    kernel32.GetConsoleProcessList.restype = wintypes.DWORD
    kernel32.GenerateConsoleCtrlEvent.argtypes = [wintypes.DWORD, wintypes.DWORD]
    kernel32.GenerateConsoleCtrlEvent.restype = wintypes.BOOL
    return {"kernel32": kernel32, "ctypes": ctypes, "wintypes": wintypes}


def _win_error(ctypes):
    return int(ctypes.get_last_error())


def _wait_ready(cfg, proc, port, out, facts):
    deadline = time.monotonic() + READY_TIMEOUT
    while time.monotonic() < deadline:
        if proc.poll() is not None:
            return "exited"
        if cfg["mode"] == "prompt":
            if prompt_seen(out.snapshot()):
                facts["prompt_observed"] = True
                return "ready"
            time.sleep(0.05)
        else:
            if rpc_get_info(port):
                return "ready"
            time.sleep(0.2)
    return "timeout"


def run_windows(cfg):
    facts = facts_template(cfg["mode"])
    api = windows_api()
    kernel32 = api["kernel32"]
    ctypes = api["ctypes"]
    wintypes = api["wintypes"]
    proc = None
    stdin = None
    out = None
    err = None
    try:
        if not cfg["native"].is_file():
            raise Stage("NATIVE_MISSING")
        if not cfg["root"].is_dir():
            raise Stage("ROOT_MISSING")
        if not (cfg["root"] / "rss").is_dir():
            raise Stage("RSS_FIXTURE_MISSING")
        # The inherited ignore attribute must be gone before the native child is
        # created, otherwise the child starts deaf to Ctrl-C.
        if not kernel32.SetConsoleCtrlHandler(None, False):
            raise Stage("CLEAR_CTRL_HANDLER", "SetConsoleCtrlHandler", _win_error(ctypes))
        port = None
        if cfg["mode"] == "resident":
            port = choose_port()
            try:
                write_rpc_config(cfg["root"], port)
            except OSError:
                raise Stage("CONFIG_FIXTURE")
            argv = [str(cfg["native"]), "--auto-update", "off"]
        else:
            argv = [str(cfg["native"]), "-i", "--auto-update", "off"]
        env = child_environment(os.environ, cfg["root"])
        try:
            proc = subprocess.Popen(
                argv,
                cwd=str(cfg["root"]),
                env=env,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                creationflags=CREATE_NEW_CONSOLE,
                close_fds=True,
            )
        except OSError:
            raise Stage("START_NATIVE")
        stdin = proc.stdin
        out = Drain(proc.stdout)
        err = Drain(proc.stderr)
        out.start()
        err.start()
        readiness = _wait_ready(cfg, proc, port, out, facts)
        if proc.poll() is not None:
            raise Stage("EXITED_BEFORE_READY")
        if readiness != "ready":
            raise Stage("READY_TIMEOUT")
        # Only this controller detaches and reattaches; the Rust test runner and
        # any gateway keep their own console untouched.
        if not kernel32.FreeConsole():
            raise Stage("FREE_CONSOLE", "FreeConsole", _win_error(ctypes))
        if not kernel32.AttachConsole(proc.pid):
            raise Stage("ATTACH_CONSOLE", "AttachConsole", _win_error(ctypes))
        # AttachConsole reset this process's handlers; ignore Ctrl-C here only,
        # after the native process is already running with its own handlers.
        if not kernel32.SetConsoleCtrlHandler(None, True):
            raise Stage("IGNORE_CTRL_HANDLER", "SetConsoleCtrlHandler", _win_error(ctypes))
        buffer = (wintypes.DWORD * 64)()
        count = kernel32.GetConsoleProcessList(buffer, 64)
        if count == 0:
            raise Stage(
                "GET_CONSOLE_PROCESS_LIST", "GetConsoleProcessList", _win_error(ctypes)
            )
        observed = [int(buffer[index]) for index in range(min(count, 64))]
        clients = console_clients([os.getpid(), proc.pid], observed)
        facts["console_client_count"] = clients["count"]
        facts["console_clients_match"] = clients["match"]
        facts["unexpected_console_client"] = bool(clients["unexpected"])
        if not clients["match"]:
            # Never signal a console that contains any other process.
            raise Stage("UNEXPECTED_CONSOLE_CLIENT")
        if not kernel32.GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0):
            raise Stage(
                "GENERATE_CTRL_C", "GenerateConsoleCtrlEvent", _win_error(ctypes)
            )
        facts["signal_sent"] = True
        # Keep the stdin writer open and let the native process exit on its own.
        deadline = time.monotonic() + EXIT_TIMEOUT
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                break
            time.sleep(0.05)
        if proc.poll() is None:
            raise Stage("EXIT_TIMEOUT")
        facts["natural_exit"] = True
        facts["exit_code"] = int(proc.returncode)
        read_run_facts(cfg["root"], facts)
    except Stage as stage:
        facts["failure"] = stage.label
        facts["failure_op"] = stage.op
        facts["win_error_code"] = stage.code
        facts["ok"] = False
    except OSError:
        facts["failure"] = "HARNESS_IO"
        facts["ok"] = False
    finally:
        if proc is not None and proc.poll() is None:
            facts["forced_kill"] = True
            try:
                proc.kill()
            except OSError:
                pass
            try:
                proc.wait(timeout=30)
            except (OSError, subprocess.TimeoutExpired):
                pass
        if stdin is not None:
            try:
                stdin.close()
            except OSError:
                pass
        for drain in (out, err):
            if drain is not None:
                drain.join(20)
        if out is not None:
            facts["stdout_bytes"] = int(out.total)
        if err is not None:
            facts["stderr_bytes"] = int(err.total)
        conclude(facts)
    return facts


def execute(argv, platform=None):
    cfg = parse_args(argv)
    facts = facts_template(cfg["mode"])
    if (platform or os.name) != "nt":
        facts["failure"] = "UNSUPPORTED_PLATFORM"
        return conclude(facts)
    return run_windows(cfg)


def main(argv=None):
    facts = execute(sys.argv[1:] if argv is None else argv)
    print(json.dumps(facts, sort_keys=True))
    return 0 if facts["ok"] else 1


if __name__ == "__main__":
    sys.exit(main())
