#!/usr/bin/env python3
"""Parser regressions for the native consumer CI gate.

The gate that decides PASS/ZERO_MATCH_REJECTED/INFRA_* lives inline in
``.github/workflows/ci.yml``. This file extracts the REAL ``classify``,
``cargo_argv`` and ``_text`` definitions from the workflow's marker block and
exercises them with controlled libtest strings. It never starts Cargo and never
claims a platform result: the fixture strings are unit data, and the real
native verdicts come only from the workflow run itself.

    python3 tools/test_native_runtime_gate.py
"""

import ast
import sys
import textwrap
import traceback
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
BEGIN = "# --- native-gallery-read-gate: begin ---"
END = "# --- native-gallery-read-gate: end ---"

GALLERY_TEST = (
    "rpc::server::tests::native_gallery_read_serves_an_ordinary_file_verbatim"
)
RSS_TEST = (
    "tests::native_rss_update::"
    "native_signed_rss_update_uses_the_same_host_and_preserves_non_rss_files"
)


def workflow_text():
    return WORKFLOW.read_text(encoding="utf-8")


def gate_block():
    text = workflow_text()
    start = text.index(BEGIN) + len(BEGIN)
    stop = text.index(END, start)
    return textwrap.dedent(text[start:stop])


def gate_functions(*names):
    """Compile only the named function definitions from the real gate block."""
    block = gate_block()
    tree = ast.parse(block, filename=str(WORKFLOW))
    found = {}
    for node in tree.body:
        if isinstance(node, ast.FunctionDef) and node.name in names:
            found[node.name] = ast.get_source_segment(block, node)
    missing = [name for name in names if name not in found]
    assert not missing, f"gate block is missing {missing}"
    source = "import re\n\n\n" + "\n\n\n".join(found[name] for name in names)
    namespace = {}
    exec(compile(source, str(WORKFLOW), "exec"), namespace)  # noqa: S102 - own workflow source
    return namespace


def pass_output(name):
    return (
        "running 1 test\n"
        f"test {name} ... ok\n"
        "\n"
        "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; "
        "163 filtered out; finished in 0.01s\n"
    )


def zero_output():
    return (
        "running 0 tests\n"
        "\n"
        "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; "
        "164 filtered out; finished in 0.00s\n"
    )


def test_libtest_pass_is_the_only_pass_verdict():
    classify = gate_functions("classify")["classify"]
    result = classify(pass_output(RSS_TEST), 0, RSS_TEST)
    assert result["verdict"] == "PASS", result
    assert result["executed"] == 1
    assert result["exact_test_name_observed"] is True


def test_zero_match_is_rejected_even_with_exit_zero():
    classify = gate_functions("classify")["classify"]
    result = classify(zero_output(), 0, RSS_TEST)
    assert result["verdict"] == "ZERO_MATCH_REJECTED", result
    assert result["executed"] == 0


def test_compile_failure_is_infrastructure_not_behaviour():
    classify = gate_functions("classify")["classify"]
    result = classify(zero_output(), 101, RSS_TEST)
    assert result["verdict"] == "INFRA_SETUP_OR_COMPILE", result


def test_failed_exact_case_is_behavior_red():
    classify = gate_functions("classify")["classify"]
    stdout = (
        "running 1 test\n"
        f"test {RSS_TEST} ... FAILED\n"
        "\n"
        "failures:\n"
        f"    {RSS_TEST}\n"
        "\n"
        "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; "
        "163 filtered out; finished in 0.02s\n"
    )
    result = classify(stdout, 101, RSS_TEST)
    assert result["verdict"] == "BEHAVIOR_RED", result


def test_inexact_match_is_not_a_pass():
    classify = gate_functions("classify")["classify"]
    other = pass_output("tests::native_rss_update::some_other_case")
    result = classify(other, 0, RSS_TEST)
    assert result["verdict"] == "INVALID_LIBTEST_SUMMARY", result
    assert result["exact_test_name_observed"] is False


def test_cargo_argv_binds_target_jobs_locked_and_exact_name():
    cargo_argv = gate_functions("cargo_argv")["cargo_argv"]
    argv = cargo_argv(RSS_TEST, "/runner/temp/cargo-target")
    assert argv[:2] == ["cargo", "test"]
    assert "--locked" in argv
    assert argv[argv.index("--jobs") + 1] == "2"
    assert argv[argv.index("--target-dir") + 1] == "/runner/temp/cargo-target"
    assert argv[argv.index("--exact") + 1] == RSS_TEST
    assert argv.index("--exact") > argv.index("--")
    assert argv.count("--target-dir") == 1


def test_timeout_text_is_decoded_with_replacement():
    text_of = gate_functions("_text")["_text"]
    assert text_of(None) == ""
    assert text_of("already text") == "already text"
    decoded = text_of(b"partial \xff output")
    assert isinstance(decoded, str)
    assert decoded == "partial \ufffd output"


def test_gate_requires_both_cases_and_the_exact_rss_name():
    block = gate_block()
    text = workflow_text()
    assert '"native-gallery-read"' in block
    assert '"native-rss-update"' in block
    assert "NATIVE_GALLERY_READ_TEST" in block
    assert "NATIVE_RSS_UPDATE_TEST" in block
    assert f"NATIVE_GALLERY_READ_TEST: {GALLERY_TEST}" in text
    assert f"NATIVE_RSS_UPDATE_TEST: {RSS_TEST}" in text


def test_workflow_keeps_both_native_matrix_hosts():
    text = workflow_text()
    assert "macos-15-intel" in text
    assert "windows-2022" in text
    assert "native-gallery-read:" in text


TESTS = [
    test_libtest_pass_is_the_only_pass_verdict,
    test_zero_match_is_rejected_even_with_exit_zero,
    test_compile_failure_is_infrastructure_not_behaviour,
    test_failed_exact_case_is_behavior_red,
    test_inexact_match_is_not_a_pass,
    test_cargo_argv_binds_target_jobs_locked_and_exact_name,
    test_timeout_text_is_decoded_with_replacement,
    test_gate_requires_both_cases_and_the_exact_rss_name,
    test_workflow_keeps_both_native_matrix_hosts,
]


def main():
    failures = 0
    for test in TESTS:
        try:
            test()
        except Exception:  # noqa: BLE001 - report, keep running
            failures += 1
            print(f"FAIL {test.__name__}")
            traceback.print_exc()
        else:
            print(f"ok {test.__name__}")
    print(f"{len(TESTS) - failures}/{len(TESTS)} passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
