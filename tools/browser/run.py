#!/usr/bin/env python3
"""Run the exact 49-check WebUI proof against the real Rust fixture backend.

One bounded orchestration:

  prepare fresh fixtures -> `tests::browser_fixture_server` -> readiness and
  HTTP health -> original-UI Playwright driver -> independent 49 verifier ->
  shutdown sentinel and bounded backend retirement.

The backend is the repository's real ignored integration test (real RSS
dispatcher, App, RSS runtime and frozen UI pack) on a loopback listener; it
seeds fixture tasks through the existing compat seam and runs no downloader
task loop. Generated reports, screenshots and process logs stay under the
explicit fresh run root.

The target comes only from CARGO_TARGET_DIR or an explicitly matching
--target-dir. The Chromium executable is explicit (--exe) or read from
XEH_CHROMIUM, otherwise resolved from the installed Playwright package.
Nothing is downloaded.

Usage:
  python tools/browser/run.py --run-root /fresh/run --worktree . \
      --temp-root /owned/temp --port 8010 --exe /path/to/chromium

Exit codes: 0 accepted, 1 verification failure, 2 usage/operational error.
"""
import argparse
import json
import os
import signal
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

TOOLS = Path(__file__).resolve().parents[1]
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))
import compat  # noqa: E402

HERE = Path(__file__).resolve().parent
SECRET = "s3cret"
TEST_NAME = "tests::browser_fixture_server"
BROWSER_MARKERS = ("chrome-headless-shell", "chrome", "chromium")
ENV_LAYOUT = {
    "HOME": "home",
    "TMPDIR": "tmp",
    "XDG_CONFIG_HOME": "xdg/config",
    "XDG_CACHE_HOME": "xdg/cache",
    "XDG_DATA_HOME": "xdg/data",
    "XDG_RUNTIME_DIR": "xdg/runtime",
    "PIP_CACHE_DIR": "pip",
    "UV_CACHE_DIR": "uv",
}


class RunError(RuntimeError):
    """A bounded operational error that never counts as a verification pass."""


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description="Run the 49-check WebUI proof.")
    parser.add_argument("--run-root", required=True, type=Path,
                        help="fresh exclusive run directory (must not exist)")
    parser.add_argument("--temp-root", type=Path, default=None,
                        help="owned TEMP group containing the run root (default: its parent)")
    parser.add_argument("--worktree", type=Path, default=HERE.parents[1],
                        help="candidate source tree providing ui/ and rss/")
    parser.add_argument("--port", type=int, default=8010,
                        help="real backend listener port (default 8010)")
    parser.add_argument("--exe", default=None,
                        help="explicit Chromium executable (default: XEH_CHROMIUM or Playwright)")
    parser.add_argument("--python", default=os.environ.get("XEH_PYTHON") or sys.executable,
                        help="interpreter with Pillow and Playwright (default: XEH_PYTHON or this one)")
    parser.add_argument("--target-dir", default=None, type=Path,
                        help="Cargo target; must equal CARGO_TARGET_DIR when both are set")
    parser.add_argument("--startup-timeout", type=float, default=1800.0,
                        help="bounded seconds to wait for build, bind and readiness")
    parser.add_argument("--driver-timeout", type=float, default=900.0,
                        help="bounded seconds for the 49-check browser driver")
    parser.add_argument("--shutdown-timeout", type=float, default=120.0,
                        help="bounded seconds for the backend to exit on the sentinel")
    return parser.parse_args(argv)


def _resolve_target(explicit):
    env_target = os.environ.get("CARGO_TARGET_DIR")
    if explicit is None:
        if not env_target:
            raise RunError("CARGO_TARGET_DIR must be set to the assigned target")
        return Path(env_target).resolve()
    resolved = Path(explicit).resolve()
    if env_target is not None and Path(env_target).resolve() != resolved:
        raise RunError(
            f"--target-dir {resolved} does not match CARGO_TARGET_DIR "
            f"{Path(env_target).resolve()}")
    return resolved


def _resolve_exe(explicit):
    candidate = explicit or os.environ.get("XEH_CHROMIUM")
    if candidate is None:
        try:
            from playwright.sync_api import sync_playwright
        except ImportError as error:
            raise RunError(
                "no --exe/XEH_CHROMIUM and Playwright is not importable") from error
        with sync_playwright() as playwright:
            candidate = playwright.chromium.executable_path
    path = Path(candidate).resolve()
    if not path.is_file():
        raise RunError(
            f"Chromium executable missing: {path} "
            "(install Playwright Chromium or pass --exe)")
    return path


def _validate_port(port):
    if not 1 <= port <= 65535:
        raise RunError(f"port out of range: {port}")


def _assert_fresh(run_root, temp_root):
    run_root = Path(run_root).resolve()
    temp_root = Path(temp_root).resolve()
    if run_root == temp_root or temp_root not in run_root.parents:
        raise RunError(f"run root {run_root} is not inside TEMP root {temp_root}")
    if run_root.exists():
        raise RunError(f"refusing to reuse existing run root: {run_root}")
    return run_root, temp_root


def _child_env(run_root, target):
    env = os.environ.copy()
    env.pop("PYTHONPYCACHEPREFIX", None)
    env.update({
        name: str(run_root / relative) for name, relative in ENV_LAYOUT.items()
    })
    env.update({
        "PYTHONDONTWRITEBYTECODE": "1",
        "CARGO_TARGET_DIR": str(target),
        "XEH_BROWSER_REQUEST": str(run_root / "request.json"),
    })
    return env


def _create_env_dirs(run_root):
    for relative in ENV_LAYOUT.values():
        (run_root / relative).mkdir(parents=True, exist_ok=True)
    os.chmod(run_root / ENV_LAYOUT["XDG_RUNTIME_DIR"], 0o700)


def _git_state(worktree):
    head = subprocess.run(["git", "-C", str(worktree), "rev-parse", "HEAD"],
                          capture_output=True, text=True)
    status = subprocess.run(["git", "-C", str(worktree), "status", "--porcelain"],
                            capture_output=True, text=True)
    if head.returncode != 0 or status.returncode != 0:
        raise RunError(f"cannot read Git state of {worktree}: {head.stderr}{status.stderr}")
    return head.stdout.strip(), bool(status.stdout.strip())


def _tail(path, limit=4000):
    try:
        return path.read_text(encoding="utf-8", errors="replace")[-limit:]
    except OSError:
        return ""


def _start_backend(argv, cwd, env, stdout_path, stderr_path):
    stdout = open(stdout_path, "w", encoding="utf-8")
    stderr = open(stderr_path, "w", encoding="utf-8")
    process = subprocess.Popen(
        argv, cwd=str(cwd), env=env, stdout=stdout, stderr=stderr,
        text=True, start_new_session=(os.name == "posix"))
    return process, stdout, stderr


def _wait_ready(run_root, process, port, timeout, stderr_path):
    ready_path = run_root / "ready.json"
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RunError(
                f"backend exited before readiness (exit {process.returncode}): "
                f"{_tail(stderr_path)}")
        if ready_path.is_file():
            try:
                ready = json.loads(ready_path.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError):
                time.sleep(0.2)
                continue
            if ready.get("port") != port:
                raise RunError(f"ready port {ready.get('port')!r} != listener port {port}")
            return ready
        time.sleep(0.25)
    raise RunError(f"backend did not become ready within {timeout:.0f}s")


def _jsonrpc_status(base, token):
    body = json.dumps({
        "method": "xeH.listTasks", "id": 1,
        "params": [["token:" + token, "!finished"], {}],
    }).encode()
    request = urllib.request.Request(
        base + "/jsonrpc", data=body,
        headers={"Content-Type": "application/json-rpc"})
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            return response.status
    except urllib.error.HTTPError as error:
        return error.code


def _wait_health(base, timeout=60.0):
    deadline = time.monotonic() + timeout
    last = None
    while time.monotonic() < deadline:
        try:
            with urllib.request.urlopen(base + "/ui/", timeout=5) as response:
                ui = response.status
            good = _jsonrpc_status(base, SECRET)
            wrong = _jsonrpc_status(base, "wrong")
        except (urllib.error.URLError, OSError) as error:
            last = str(error)
            time.sleep(0.25)
            continue
        if ui == 200 and good == 200 and wrong == 403:
            return {"ui": ui, "jsonrpc_ok": good, "jsonrpc_wrong": wrong}
        last = f"ui={ui} jsonrpc_ok={good} jsonrpc_wrong={wrong}"
        time.sleep(0.25)
    raise RunError(f"backend health never matched (last: {last})")


def _shutdown_and_reap(run_root, process, streams, timeout, force=False):
    """Ask the owned backend to exit, then confirm direct exit and group gone.

    `force` retires the whole owned group without relying on the shutdown
    sentinel or on a second polite wait; it is used only while the CLI is
    already unwinding. The streams are closed only after the direct exit and
    the whole group are confirmed, so an unconfirmed retirement keeps custody.
    """
    forced = force
    retired = True
    if not forced:
        try:
            (run_root / "shutdown").write_text("shutdown\n", encoding="utf-8")
        except OSError:
            # A failed sentinel write must not be able to block forced
            # retirement of an owned group.
            forced = True
    code = None
    if not forced:
        try:
            code = process.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            forced = True
        else:
            if os.name == "posix" and compat._process_group_alive(process.pid):
                forced = True
    if forced:
        retired, _, _ = compat._retire_and_drain(process)
        code = process.returncode
    alive = compat._process_group_alive(process.pid) if os.name == "posix" else False
    confirmed = retired and process.poll() is not None and not alive
    if confirmed:
        for stream in streams:
            try:
                stream.close()
            except OSError:
                pass
    return {"exit": code, "forced": forced, "group_alive": alive,
            "confirmed": confirmed}


def _remaining_browsers(run_root, exe):
    """Find only browser processes started for this run root's executable."""
    found = []
    proc_root = Path("/proc")
    if not proc_root.is_dir():
        return found
    own = {os.getpid(), os.getppid()}
    exe_text = str(exe)
    profile = f"--user-data-dir={run_root}"
    for entry in proc_root.iterdir():
        if not entry.name.isdigit() or int(entry.name) in own:
            continue
        try:
            raw = (entry / "cmdline").read_bytes()
        except OSError:
            continue
        command = raw.replace(b"\0", b" ").decode("utf-8", "replace")
        if exe_text and command.startswith(exe_text):
            found.append({"pid": int(entry.name), "cmdline": command[:300]})
        elif profile in command and any(marker in command for marker in BROWSER_MARKERS):
            found.append({"pid": int(entry.name), "cmdline": command[:300]})
    return found


def _receipt(args, state):
    run_root = Path(args.run_root).resolve()
    report_path = run_root / "browser-report.json"
    report = {}
    if report_path.is_file():
        try:
            report = json.loads(report_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            report = {}
    checks = report.get("checks") or []
    names = [entry.get("check") for entry in checks if isinstance(entry, dict)]
    backend = state.get("backend") or {}
    return {
        "tool": "tools/browser/run.py",
        "started": state.get("started"),
        "finished": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "worktree": str(Path(args.worktree).resolve()),
        "head_before": state.get("head_before"),
        "head_after": state.get("head_after"),
        "dirty_before": state.get("dirty_before"),
        "dirty_after": state.get("dirty_after"),
        "target_dir": state.get("target_dir"),
        "port": args.port,
        "run_root": str(run_root),
        "python": state.get("python"),
        "chromium": state.get("chromium"),
        "backend": {
            # Only the recorded scalars are copied: an unconfirmed retirement
            # keeps live process/stream custody in state, and that custody must
            # never leak into the JSON receipt or masquerade as a success.
            "argv": backend.get("argv"),
            "exit": backend.get("exit"),
            "forced_retirement": backend.get("forced"),
            "group_alive": backend.get("group_alive"),
            "reaped": backend.get("reaped") is True,
            "passed": backend.get("passed"),
            "failed": backend.get("failed"),
            "status": backend.get("status"),
            "test": TEST_NAME,
            "stdout_log": str(run_root / "backend.stdout.log"),
            "stderr_log": str(run_root / "backend.stderr.log"),
        },
        "driver": state.get("driver") or {},
        "verifier": state.get("verifier") or {},
        "report": str(report_path),
        "checks": {
            "count": len(checks),
            "all_true": report.get("ok") is True and len(checks) == 49
            and all(entry.get("ok") is True for entry in checks),
            "names": names,
        },
        "residual_browser_processes": state.get("residual_browsers") or [],
        "failures": state.get("failures") or [],
        "ok": state.get("ok") is True and not (state.get("failures") or []),
    }


def _write_receipt(args, state):
    if not state.get("prepared"):
        return
    run_root = Path(args.run_root).resolve()
    if not run_root.is_dir():
        return
    if state.get("head_after") is None:
        try:
            state["head_after"], state["dirty_after"] = _git_state(args.worktree)
        except RunError as error:
            state.setdefault("failures", []).append(str(error))
    (run_root / "receipt.json").write_text(
        json.dumps(_receipt(args, state), indent=1), encoding="utf-8")


def _cleanup(args, state, force=False):
    """Retire the one owned backend; keep custody until the exit is confirmed.

    `force` is the unwind path's bounded retirement and is not a public mode.
    Marking the backend reaped and dropping its process/stream references
    happens only after the direct child has exited AND the whole owned group is
    gone; otherwise the custody stays and the receipt records the failure.
    """
    backend = state.get("backend")
    if not backend or state.get("reaped"):
        return
    run_root = Path(args.run_root).resolve()
    result = _shutdown_and_reap(
        run_root, backend["process"], backend.get("streams") or (),
        args.shutdown_timeout, force=force)
    for name in ("exit", "forced", "group_alive"):
        backend[name] = result[name]
    backend["reaped"] = result["confirmed"]
    state["reaped"] = result["confirmed"]
    if result["confirmed"]:
        backend.pop("process", None)
        backend.pop("streams", None)
        return
    state.setdefault("failures", []).append(
        "owned backend retirement was not confirmed; process custody retained")


def _evaluate(args, state):
    failures = state.setdefault("failures", [])
    run_root = Path(args.run_root).resolve()
    report_path = run_root / "browser-report.json"
    backend_stdout = run_root / "backend.stdout.log"
    backend = state.get("backend") or {}
    driver = state.get("driver") or {}
    verifier = state.get("verifier") or {}

    results, summary = compat.parse_test_results(
        backend_stdout.read_text(encoding="utf-8", errors="replace")
        if backend_stdout.exists() else "")
    backend["status"] = summary["status"] if summary else None
    backend["passed"] = summary["passed"] if summary else None
    backend["failed"] = summary["failed"] if summary else None
    if backend.get("exit") != 0:
        failures.append(f"backend exit {backend.get('exit')}")
    if not summary or summary["status"] != "ok" or summary["passed"] != 1 \
            or summary["failed"] != 0:
        failures.append(f"backend summary is not exactly ok/1 passed: {summary}")
    if results.get(TEST_NAME) != "ok":
        failures.append(f"backend test row is not ok: {results.get(TEST_NAME)!r}")
    if backend.get("group_alive"):
        failures.append("backend process group is still alive after shutdown")
    if backend.get("forced"):
        failures.append("backend needed forced retirement instead of the sentinel")

    if driver.get("exit") != 0:
        failures.append(f"browser driver exit {driver.get('exit')}")
    if verifier.get("exit") != 0:
        failures.append(f"independent verifier exit {verifier.get('exit')}")
    if not report_path.is_file():
        failures.append(f"missing browser report: {report_path}")

    report = {}
    if report_path.is_file():
        try:
            report = json.loads(report_path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            failures.append(f"unreadable browser report: {error}")
    checks = report.get("checks") or []
    if len(checks) != 49 or report.get("ok") is not True \
            or not all(entry.get("ok") is True for entry in checks):
        failures.append("browser report is not 49 true checks")

    try:
        state["head_after"], state["dirty_after"] = _git_state(args.worktree)
    except RunError as error:
        failures.append(str(error))
    if state.get("head_after") != state.get("head_before"):
        failures.append("source HEAD changed during the run")
    if state.get("dirty_after"):
        failures.append("source worktree is dirty after the run")

    state["residual_browsers"] = _remaining_browsers(
        run_root, state.get("chromium") or "")
    if state["residual_browsers"]:
        failures.append(
            f"{len(state['residual_browsers'])} browser process(es) left under the run root")
    report_env = os.environ.get("XEH_BROWSER_REPORT")
    if report_env and Path(report_env).resolve() != report_path.resolve():
        failures.append(
            f"XEH_BROWSER_REPORT {report_env} is not {report_path}")

    state["ok"] = not failures
    if failures:
        for failure in failures:
            print("FAIL", failure, file=sys.stderr)
        return 1
    print(f"BROWSER49 OK {len(checks)} checks; backend 1 passed; verifier exit {verifier.get('exit')}")
    return 0


def _steps(args, state):
    run_root = Path(args.run_root).resolve()
    worktree = Path(args.worktree).resolve()
    python = state["python"]
    target = state["target_dir"]
    report_path = run_root / "browser-report.json"
    backend_stdout = run_root / "backend.stdout.log"
    backend_stderr = run_root / "backend.stderr.log"

    print(f"prepare {run_root}")
    prepared = compat.run_command(
        [python, str(HERE / "prepare.py"),
         "--run-root", str(run_root), "--worktree", str(worktree),
         "--temp-root", str(Path(args.temp_root).resolve()),
         "--port", str(args.port)],
        worktree, state["env"],
        run_root.parent / f"{run_root.name}.prepare.log", timeout_s=120)
    if prepared.returncode != 0:
        raise RunError(
            f"prepare failed (exit {prepared.returncode}): {prepared.stderr[-2000:]}")
    state["prepared"] = True
    _create_env_dirs(run_root)

    backend_argv = [
        os.environ.get("CARGO", "cargo"), "test", "--locked", "--offline",
        "--jobs", "2", "--target-dir", str(target),
        "--bin", "xeHentai", TEST_NAME,
        "--", "--exact", "--ignored", "--nocapture",
    ]
    print("start backend", " ".join(backend_argv))
    process, stdout, stderr = _start_backend(
        backend_argv, worktree, state["env"], backend_stdout, backend_stderr)
    state["backend"] = {
        "argv": backend_argv, "process": process, "streams": (stdout, stderr),
    }
    ready = _wait_ready(run_root, process, args.port,
                        args.startup_timeout, backend_stderr)
    state["ready"] = ready
    state["health"] = _wait_health(f"http://127.0.0.1:{args.port}")
    print(f"ready port={ready['port']} health={state['health']}")

    driver_argv = [
        python, str(HERE / "run_browser_proof.py"),
        "--run-root", str(run_root), "--port", str(args.port),
        "--exe", str(state["chromium"]),
    ]
    print("run browser driver")
    driver = compat.run_command(
        driver_argv, worktree, state["env"],
        run_root / "driver.log", timeout_s=args.driver_timeout)
    state["driver"] = {"argv": driver_argv, "exit": driver.returncode,
                       "log": str(run_root / "driver.log")}

    verifier_argv = [
        python, str(HERE / "verify_webui_49.py"), str(report_path),
        "--port", str(args.port),
    ]
    print("run independent verifier")
    verifier = compat.run_command(
        verifier_argv, worktree, state["env"],
        run_root / "verifier.log", timeout_s=120)
    state["verifier"] = {"argv": verifier_argv, "exit": verifier.returncode,
                         "log": str(run_root / "verifier.log")}


def _execute(args, state):
    try:
        _steps(args, state)
    except compat.CommandTimeout as error:
        state.setdefault("failures", []).append(str(error))
    except BaseException as error:
        state["error"] = repr(error)
        state.setdefault("failures", []).append(f"unexpected error: {error!r}")
        raise
    _cleanup(args, state)
    return _evaluate(args, state)


def main(argv=None):
    args = parse_args(argv)
    try:
        _validate_port(args.port)
        temp_root = args.temp_root or Path(args.run_root).resolve().parent
        run_root, temp_root = _assert_fresh(args.run_root, temp_root)
        target = _resolve_target(args.target_dir)
        chromium = _resolve_exe(args.exe)
    except RunError as error:
        print(f"browser-run error: {error}", file=sys.stderr)
        return 2
    # Keep the venv path as given: resolving it would follow the venv
    # interpreter symlink to the base Python and lose site-packages.
    python = Path(os.path.abspath(args.python))
    if not python.is_file():
        print(f"browser-run error: python missing: {python}", file=sys.stderr)
        return 2
    args.run_root = run_root
    args.temp_root = temp_root
    args.worktree = Path(args.worktree).resolve()
    state = {
        "started": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
        "target_dir": str(target),
        "python": str(python),
        "chromium": str(chromium),
        "env": _child_env(run_root, target),
        "failures": [],
    }
    try:
        state["head_before"], state["dirty_before"] = _git_state(args.worktree)
    except RunError as error:
        print(f"browser-run error: {error}", file=sys.stderr)
        return 2

    previous_term = compat._install_sigterm_unwind()
    try:
        try:
            code = _execute(args, state)
        except BaseException as error:
            # This CLI boundary is the single owner of cancellation cleanup:
            # any step, polite-shutdown or evaluation exception arrives here
            # with the owned backend still retained. Further signals are
            # ignored only for this bounded retirement (the accepted compat
            # helpers), the receipt records the truthful state, and the exact
            # previous handlers are restored before the ORIGINAL exception is
            # re-raised bare. Cleanup or logging failures are recorded and
            # never replace that first exception.
            saved = compat._suspend_cancellation_signals()
            try:
                try:
                    _cleanup(args, state, force=True)
                except BaseException as cleanup_error:
                    state.setdefault("failures", []).append(
                        f"backend retirement failed: {cleanup_error!r}")
                try:
                    _write_receipt(args, state)
                except BaseException as receipt_error:
                    print(f"receipt write failed: {receipt_error!r}",
                          file=sys.stderr)
            finally:
                compat._restore_cancellation_signals(saved)
            if isinstance(error, RunError):
                print(f"browser-run error: {error}", file=sys.stderr)
                return 2
            raise
        try:
            _write_receipt(args, state)
        except BaseException as receipt_error:
            print(f"receipt write failed: {receipt_error!r}", file=sys.stderr)
        return code
    finally:
        compat._restore_signal_handler(signal.SIGTERM, previous_term)


if __name__ == "__main__":
    sys.exit(main())
