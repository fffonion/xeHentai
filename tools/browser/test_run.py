#!/usr/bin/env python3
"""Ownership and argument tests for the browser proof orchestrator.

These do not build, start the backend, or launch a browser. They cover the
guards that must refuse before any child exists (fresh root, target binding,
explicit executable) plus the bounded retirement of one owned process group,
the real CLI SIGTERM path through main() and the CI pack comparator. Standard
library plus the caller's interpreter:

  python tools/browser/test_run.py -v
"""
import importlib.util
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(HERE))

import compare_webui_pack  # noqa: E402
import run  # noqa: E402


def temp_base():
    base = os.environ.get("TMPDIR") or os.environ.get("XEH_LEASE_TMP")
    if not base:
        raise unittest.SkipTest("TMPDIR/XEH_LEASE_TMP must confine test output")
    path = Path(base)
    path.mkdir(parents=True, exist_ok=True)
    return path


class GuardTests(unittest.TestCase):
    def setUp(self):
        self.base = temp_base()
        self.work = Path(tempfile.mkdtemp(prefix="browser-guard-", dir=str(self.base)))

    def tearDown(self):
        for path in sorted(self.work.rglob("*"), reverse=True):
            if path.is_file() or path.is_symlink():
                path.unlink()
            else:
                path.rmdir()
        self.work.rmdir()

    def test_stale_run_root_is_refused_before_children(self):
        stale = self.work / "run"
        stale.mkdir()
        (stale / "old-evidence.txt").write_text("keep\n", encoding="utf-8")
        code = run.main(["--run-root", str(stale), "--temp-root", str(self.work)])
        self.assertEqual(code, 2)
        self.assertTrue((stale / "old-evidence.txt").is_file())
        self.assertEqual(sorted(path.name for path in stale.iterdir()),
                         ["old-evidence.txt"])

    def test_run_root_must_stay_inside_temp_root(self):
        with self.assertRaises(run.RunError):
            run._assert_fresh(self.work / "run", self.work / "other")

    def test_target_requires_environment_binding(self):
        saved = os.environ.pop("CARGO_TARGET_DIR", None)
        try:
            with self.assertRaises(run.RunError):
                run._resolve_target(None)
        finally:
            if saved is not None:
                os.environ["CARGO_TARGET_DIR"] = saved

    def test_target_flag_must_match_environment(self):
        saved = os.environ.get("CARGO_TARGET_DIR")
        os.environ["CARGO_TARGET_DIR"] = str(self.work / "one")
        try:
            with self.assertRaises(run.RunError):
                run._resolve_target(self.work / "two")
            self.assertEqual(run._resolve_target(self.work / "one"),
                             (self.work / "one").resolve())
        finally:
            if saved is None:
                os.environ.pop("CARGO_TARGET_DIR", None)
            else:
                os.environ["CARGO_TARGET_DIR"] = saved

    def test_port_range(self):
        with self.assertRaises(run.RunError):
            run._validate_port(0)
        with self.assertRaises(run.RunError):
            run._validate_port(65536)
        run._validate_port(8010)

    def test_missing_executable_is_refused(self):
        with self.assertRaises(run.RunError):
            run._resolve_exe(str(self.work / "no-chrome"))

    def test_child_environment_is_confined(self):
        env = run._child_env(self.work / "run", self.work / "target")
        for name in run.ENV_LAYOUT:
            self.assertTrue(env[name].startswith(str(self.work / "run")), (name, env[name]))
        self.assertEqual(env["CARGO_TARGET_DIR"], str(self.work / "target"))
        self.assertNotIn("PYTHONPYCACHEPREFIX", env)


class RetirementTests(unittest.TestCase):
    def test_owned_group_is_bounded_and_retired(self):
        base = temp_base()
        work = Path(tempfile.mkdtemp(prefix="browser-retire-", dir=str(base)))
        try:
            process = subprocess.Popen(
                [sys.executable, "-c", "import time; time.sleep(60)"],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                start_new_session=True)
            started = time.monotonic()
            result = run._shutdown_and_reap(work, process, (), timeout=1.0)
            elapsed = time.monotonic() - started
            self.assertTrue(result["forced"])
            self.assertIsNotNone(result["exit"])
            self.assertFalse(result["group_alive"])
            self.assertLess(elapsed, 20)
            self.assertIsNotNone(process.poll())
        finally:
            for path in sorted(work.rglob("*"), reverse=True):
                path.unlink() if path.is_file() else path.rmdir()
            work.rmdir()


STUB_BACKEND_SOURCE = '''import json, os, signal, socket, sys, time

signal.signal(signal.SIGTERM, signal.SIG_IGN)
listener = socket.socket()
listener.bind(("127.0.0.1", 0))
listener.listen(8)
with open(sys.argv[1], "w", encoding="utf-8") as handle:
    json.dump({"pid": os.getpid(), "port": listener.getsockname()[1]}, handle)
time.sleep(300)
'''

CONTROLLER_SOURCE = '''import json, os, signal, sys
from pathlib import Path

config = json.loads(Path(sys.argv[1]).read_text(encoding="utf-8"))
sys.path.insert(0, config["browser"])
import run

marker_calls = []


def previous(signum, frame):
    marker_calls.append(signum)


signal.signal(signal.SIGTERM, previous)


def seed_steps(args, state):
    run_root = Path(args.run_root)
    run_root.mkdir(parents=True, exist_ok=True)
    state["prepared"] = True
    process, stdout, stderr = run._start_backend(
        [sys.executable, config["stub"], config["ready"]],
        run_root, os.environ.copy(),
        run_root / "backend.stdout.log", run_root / "backend.stderr.log")
    state["backend"] = {"argv": ["stub-backend"], "process": process,
                        "streams": (stdout, stderr)}


run._steps = seed_steps
result = {"propagated": None, "handler_restored": None, "returncode": None,
          "error": None, "marker_calls": marker_calls}
try:
    result["returncode"] = run.main([
        "--run-root", config["run_root"], "--temp-root", config["temp_root"],
        "--worktree", config["worktree"], "--exe", sys.executable,
        "--python", sys.executable, "--port", str(config["port"]),
        "--shutdown-timeout", "1"])
except BaseException as error:
    result["propagated"] = (error.code if isinstance(error, SystemExit)
                            else type(error).__name__)
    result["error"] = repr(error)
result["handler_restored"] = signal.getsignal(signal.SIGTERM) is previous
result["marker_calls"] = marker_calls
Path(config["result"]).write_text(json.dumps(result), encoding="utf-8")
print(json.dumps(result))
'''


def wait_for_path(path, timeout=60.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if path.exists():
            return True
        time.sleep(0.02)
    return False


def wait_group_gone(pgid, timeout=40.0):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        try:
            os.killpg(pgid, 0)
        except ProcessLookupError:
            return True
        except PermissionError:
            return False
        time.sleep(0.05)
    return False


class SignalCleanupTests(unittest.TestCase):
    """The CLI owns cancellation cleanup: a real SIGTERM during the polite wait."""

    def setUp(self):
        self.base = temp_base()
        self.work = Path(tempfile.mkdtemp(prefix="browser-signal-", dir=str(self.base)))
        self.stub_pid = None
        self.controller = None

    def tearDown(self):
        if self.stub_pid is not None:
            try:
                os.killpg(self.stub_pid, signal.SIGKILL)
            except (ProcessLookupError, PermissionError):
                pass
        if self.controller is not None and self.controller.poll() is None:
            self.controller.kill()
            self.controller.wait(timeout=30)

    def test_sigterm_during_polite_wait_retires_backend_and_propagates(self):
        scripts = self.work / "fixture"
        scripts.mkdir()
        (scripts / "stub_backend.py").write_text(STUB_BACKEND_SOURCE, encoding="utf-8")
        (scripts / "controller.py").write_text(CONTROLLER_SOURCE, encoding="utf-8")
        run_root = self.work / "run"
        ready = run_root / "ready.json"
        config = {
            "browser": str(HERE),
            "stub": str(scripts / "stub_backend.py"),
            "ready": str(ready),
            "run_root": str(run_root),
            "temp_root": str(self.work),
            "worktree": str(ROOT),
            "result": str(self.work / "controller-result.json"),
            "port": 8144,
        }
        config_path = self.work / "controller.json"
        config_path.write_text(json.dumps(config), encoding="utf-8")
        self.controller = subprocess.Popen(
            [sys.executable, str(scripts / "controller.py"), str(config_path)],
            stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
            cwd=str(self.work), env=os.environ.copy())
        self.assertTrue(wait_for_path(ready, timeout=60),
                        "stub backend never became ready")
        stub = json.loads(ready.read_text(encoding="utf-8"))
        self.stub_pid = stub["pid"]
        self.assertTrue(wait_for_path(run_root / "shutdown", timeout=30),
                        "production cleanup never wrote the shutdown sentinel")
        # The stub ignores the sentinel, so the CLI's polite process.wait is
        # blocked right now; interrupt THAT wait with a real SIGTERM, then send
        # a second one while the unwind is still retiring the owned group.
        self.controller.send_signal(signal.SIGTERM)
        time.sleep(0.3)
        self.controller.send_signal(signal.SIGTERM)
        try:
            stdout, stderr = self.controller.communicate(timeout=120)
        except subprocess.TimeoutExpired:
            self.fail("controller did not finish within the finite outer bound")
        result = json.loads(
            (self.work / "controller-result.json").read_text(encoding="utf-8"))
        self.assertIsNone(
            result["returncode"],
            f"cancellation was translated into an ordinary result: {result}\n{stdout}\n{stderr}")
        self.assertEqual(
            result["propagated"], 143,
            f"original SIGTERM cancellation did not propagate: {result}\n{stdout}\n{stderr}")
        self.assertTrue(result["handler_restored"],
                        f"previous SIGTERM handler was not restored: {result}")
        receipt = json.loads((run_root / "receipt.json").read_text(encoding="utf-8"))
        backend = receipt["backend"]
        self.assertIs(backend["forced_retirement"], True, receipt)
        self.assertIs(backend["group_alive"], False, receipt)
        self.assertIs(backend["reaped"], True, receipt)
        self.assertIsNotNone(backend["exit"], receipt)
        self.assertEqual(receipt["failures"], [], receipt)
        self.assertTrue(wait_group_gone(self.stub_pid),
                        "stub backend group survived the CLI unwind")
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", stub["port"]))


class PackCompareTests(unittest.TestCase):
    def test_equal_pack_is_accepted_and_mutation_rejected(self):
        base = temp_base()
        work = Path(tempfile.mkdtemp(prefix="browser-pack-", dir=str(base)))
        try:
            dist = work / "dist"
            candidate = work / "ui"
            dist.mkdir()
            candidate.mkdir()
            for index in range(7):
                name = f"static/asset-{index}.js"
                for root in (dist, candidate):
                    (root / "static").mkdir(exist_ok=True)
                    (root / name).write_bytes(f"body-{index}".encode())
                (dist / name).with_suffix(".js.map").write_bytes(b"map")
            (candidate / "PROVENANCE").write_text("pack metadata\n", encoding="utf-8")
            self.assertEqual(compare_webui_pack.compare(dist, candidate, 7), [])
            (candidate / "static/asset-3.js").write_bytes(b"mutated")
            problems = compare_webui_pack.compare(dist, candidate, 7)
            self.assertTrue(any("asset-3.js" in problem for problem in problems))
        finally:
            for path in sorted(work.rglob("*"), reverse=True):
                path.unlink() if path.is_file() else path.rmdir()
            work.rmdir()


@unittest.skipUnless(importlib.util.find_spec("PIL"), "Pillow not installed")
class PrepareTests(unittest.TestCase):
    def test_prepare_creates_fresh_fixture_and_refuses_reuse(self):
        base = temp_base()
        work = Path(tempfile.mkdtemp(prefix="browser-prepare-", dir=str(base)))
        run_root = work / "run"
        argv = [sys.executable, str(HERE / "prepare.py"),
                "--run-root", str(run_root), "--worktree", str(ROOT),
                "--temp-root", str(work), "--port", "8123"]
        try:
            first = subprocess.run(argv, capture_output=True, text=True)
            self.assertEqual(first.returncode, 0, first.stderr)
            downloads = run_root / "backend" / "downloads"
            self.assertEqual(
                sum(1 for path in downloads.rglob("*") if path.is_file()), 18)
            self.assertTrue((run_root / "request.json").is_file())
            self.assertFalse((run_root / "ready.json").exists())
            second = subprocess.run(argv, capture_output=True, text=True)
            self.assertNotEqual(second.returncode, 0)
            self.assertIn("refusing to reuse existing", second.stderr)
        finally:
            for path in sorted(work.rglob("*"), reverse=True):
                path.unlink() if path.is_file() else path.rmdir()
            work.rmdir()


if __name__ == "__main__":
    unittest.main(verbosity=2)
