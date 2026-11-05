#!/usr/bin/env python3
"""Unit regressions for the Windows console controller.

These tests exercise only pure controller logic and the explicit non-Windows
refusal. They never call a WinAPI entry point and never claim a Windows Ctrl-C
result: the real console event is proven only by the native Windows Actions
job.

    python3 tools/test_m6_windows_console.py
"""

import contextlib
import importlib.util
import io
import json
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
MODULE = ROOT / "tools" / "m6_windows_console.py"


def load():
    spec = importlib.util.spec_from_file_location("m6_windows_console", MODULE)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


CONTROLLER = load()


def test_non_windows_execute_refuses_with_fixed_facts():
    facts = CONTROLLER.execute(
        ["--mode", "resident", "--native", "/missing/xeHentai.exe", "--root", "/missing"],
        platform="posix",
    )
    assert facts["ok"] is False, facts
    assert facts["failure"] == "UNSUPPORTED_PLATFORM", facts
    assert facts["mode"] == "resident"
    assert facts["signal_sent"] is False
    assert facts["natural_exit"] is False
    assert facts["exit_code"] is None
    assert facts["forced_kill"] is False
    # The fixed report is the whole language: no raw output, path or environment.
    assert set(facts) == set(CONTROLLER.FACT_KEYS), sorted(facts)
    json.dumps(facts)  # every value is JSON-serializable (no bytes/Path)


def test_parse_args_binds_the_mode_and_owned_paths_only():
    cfg = CONTROLLER.parse_args(
        ["--mode", "prompt", "--native", "C:\\case\\xeHentai.exe", "--root", "C:\\case"]
    )
    assert cfg["mode"] == "prompt"
    assert cfg["native"] == Path("C:\\case\\xeHentai.exe")
    assert cfg["root"] == Path("C:\\case")
    invalid = [
        ["--mode", "bogus", "--native", "n", "--root", "r"],
        ["--mode", "resident", "--root", "r"],
        ["--mode", "resident", "--native", "n"],
        [],
    ]
    for argv in invalid:
        with contextlib.redirect_stderr(io.StringIO()):
            try:
                CONTROLLER.parse_args(argv)
            except SystemExit as exit:
                assert exit.code != 0
            else:
                raise AssertionError(f"accepted invalid argv: {argv}")


def test_console_clients_require_the_exact_owned_set():
    good = CONTROLLER.console_clients([10, 20], [20, 10])
    assert good["match"] is True and good["count"] == 2, good
    assert good["unexpected"] == [] and good["missing"] == []
    extra = CONTROLLER.console_clients([10, 20], [20, 10, 99])
    assert extra["match"] is False, extra
    assert extra["unexpected"] == [99], extra
    missing = CONTROLLER.console_clients([10, 20], [10])
    assert missing["match"] is False, missing
    assert missing["missing"] == [20], missing
    empty = CONTROLLER.console_clients([10, 20], [])
    assert empty["match"] is False and empty["count"] == 0, empty


def test_child_environment_redirects_owned_roots_and_drops_test_switches():
    root = Path("/case/root")
    base = {
        "SystemRoot": "C:\\Windows",
        "PATH": "C:\\Windows\\system32",
        "XEH_EMIT_ROOT": "/emit",
        "XEH_EVIDENCE_DIR": "/evidence",
        "XEH_CAPTURE_OUT": "/capture",
        "RUST_TEST_THREADS": "4",
        "XEH_M6_PYTHON": "C:\\Python\\python.exe",
        "LC_MESSAGES": "fr_FR.UTF-8",
        "HOME": "/inherited",
        "USERPROFILE": "C:\\Users\\inherited",
    }
    env = CONTROLLER.child_environment(base, root)
    assert env["SystemRoot"] == "C:\\Windows"
    assert env["PATH"] == "C:\\Windows\\system32"
    for name in (
        "XEH_EMIT_ROOT",
        "XEH_EVIDENCE_DIR",
        "XEH_CAPTURE_OUT",
        "RUST_TEST_THREADS",
        "XEH_M6_PYTHON",
        "LC_MESSAGES",
    ):
        assert name not in env, name
    assert env["HOME"] == str(root / "home")
    assert env["USERPROFILE"] == str(root / "home")
    assert env["TMPDIR"] == str(root / "tmp")
    assert env["TEMP"] == str(root / "tmp")
    assert env["TMP"] == str(root / "tmp")
    assert env["XDG_CACHE_HOME"] == str(root / "cache")
    assert env["LANG"] == "C" and env["LC_ALL"] == "C"
    assert base["HOME"] == "/inherited"  # the caller's mapping is not mutated


def test_prompt_is_detected_from_bytes_without_a_trailing_newline():
    assert CONTROLLER.prompt_seen(b"xeHentai started." + CONTROLLER.PROMPT_TEXT)
    assert CONTROLLER.prompt_seen(CONTROLLER.PROMPT_TEXT)  # no newline required
    assert not CONTROLLER.prompt_seen(b"login to exhentai (y/n)")
    assert not CONTROLLER.prompt_seen(b"")


def test_facts_template_and_conclusion_are_fixed_and_strict():
    facts = CONTROLLER.facts_template("resident")
    assert set(facts) == set(CONTROLLER.FACT_KEYS), sorted(facts)
    assert facts["ok"] is False and facts["failure"] is None
    good = dict(facts)
    good.update(
        signal_sent=True,
        natural_exit=True,
        exit_code=0,
        console_client_count=2,
        console_clients_match=True,
        started_logged=True,
        cleanup_logged=True,
        session_nw="1",
        session_tasks_empty=True,
    )
    CONTROLLER.conclude(good)
    assert good["ok"] is True, good
    assert good["failure"] is None, good
    for name, value in [
        ("forced_kill", True),
        ("signal_sent", False),
        ("natural_exit", False),
        ("exit_code", 1),
        ("exit_code", None),
        ("started_logged", False),
        ("cleanup_logged", False),
        ("session_nw", None),
        ("session_nw", "2"),
        ("session_tasks_empty", False),
        ("console_clients_match", False),
        ("console_client_count", 3),
    ]:
        mutated = dict(good)
        mutated[name] = value
        CONTROLLER.conclude(mutated)
        assert mutated["ok"] is False, (name, value, mutated)
        assert mutated["failure"], (name, value)
    prompt = CONTROLLER.facts_template("prompt")
    prompt.update(
        signal_sent=True,
        natural_exit=True,
        exit_code=0,
        console_client_count=2,
        console_clients_match=True,
        started_logged=True,
        cleanup_logged=True,
        session_nw="1",
        session_tasks_empty=True,
        prompt_observed=False,
    )
    CONTROLLER.conclude(prompt)
    assert prompt["ok"] is False and prompt["failure"] == "PROMPT_NOT_OBSERVED", prompt
    prompt["prompt_observed"] = True
    CONTROLLER.conclude(prompt)
    assert prompt["ok"] is True, prompt


def test_main_on_this_host_prints_one_fixed_json_and_fails():
    buffer = io.StringIO()
    with contextlib.redirect_stdout(buffer):
        code = CONTROLLER.main(
            ["--mode", "resident", "--native", "n", "--root", "r"]
        )
    assert code == 1, code
    text = buffer.getvalue()
    assert text.count("\n") == 1, repr(text)
    facts = json.loads(text)
    assert facts["ok"] is False and facts["failure"] == "UNSUPPORTED_PLATFORM", facts
    assert set(facts) == set(CONTROLLER.FACT_KEYS), sorted(facts)


TESTS = [
    test_non_windows_execute_refuses_with_fixed_facts,
    test_parse_args_binds_the_mode_and_owned_paths_only,
    test_console_clients_require_the_exact_owned_set,
    test_child_environment_redirects_owned_roots_and_drops_test_switches,
    test_prompt_is_detected_from_bytes_without_a_trailing_newline,
    test_facts_template_and_conclusion_are_fixed_and_strict,
    test_main_on_this_host_prints_one_fixed_json_and_fails,
]


def main():
    failures = 0
    for test in TESTS:
        try:
            test()
        except Exception as error:  # noqa: BLE001 - report and keep running
            failures += 1
            print(f"FAIL {test.__name__}: {error!r}")
        else:
            print(f"ok {test.__name__}")
    print(f"{len(TESTS) - failures}/{len(TESTS)} passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
