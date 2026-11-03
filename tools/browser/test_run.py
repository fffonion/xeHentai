#!/usr/bin/env python3
"""Ownership and argument tests for the browser proof orchestrator.

These do not build, start the backend, or launch a browser. They cover the
guards that must refuse before any child exists (fresh root, target binding,
explicit executable) plus the bounded retirement of one owned process group
and the CI pack comparator. Standard library plus the caller's interpreter:

  python tools/browser/test_run.py -v
"""
import importlib.util
import os
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
