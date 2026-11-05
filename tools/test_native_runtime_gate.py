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
import contextlib
import os
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
ZIP_FALLBACK_TEST = "rpc::server::tests::gallery_reads_fall_back_to_sibling_zip"
ARCHIVE_TEST = "rpc::server::tests::archive_builds_on_demand_and_reuses"
ZIP_THUMBNAIL_TEST = (
    "rpc::server::tests::gallery_zip_thumbnail_serves_resized_entry"
)
PRIVATE_HARDLINK_TEST = (
    "rpc::server::tests::native_gallery_private_hardlink_alias_is_denied"
)
ROOT_ALIAS_TEST = "rpc::server::tests::native_gallery_root_alias_escape_is_denied"
RSS_TEST = (
    "tests::native_rss_update::"
    "native_signed_rss_update_uses_the_same_host_and_preserves_non_rss_files"
)
RSS_HOLD_TEST = (
    "tests::native_rss_update::native_held_gallery_times_out_then_auto_exits"
)

# The five gallery/archive consumers appended after the two original cases.
# Order is part of the gate: macOS runs these seven, Windows these seven plus
# the ignored long-running control.
EXTRA_CASES = [
    ("native-gallery-zip-fallback", "NATIVE_GALLERY_ZIP_FALLBACK_TEST", ZIP_FALLBACK_TEST),
    ("native-gallery-archive", "NATIVE_GALLERY_ARCHIVE_TEST", ARCHIVE_TEST),
    ("native-gallery-zip-thumbnail", "NATIVE_GALLERY_ZIP_THUMBNAIL_TEST", ZIP_THUMBNAIL_TEST),
    (
        "native-gallery-private-hardlink",
        "NATIVE_GALLERY_PRIVATE_HARDLINK_TEST",
        PRIVATE_HARDLINK_TEST,
    ),
    ("native-gallery-root-alias", "NATIVE_GALLERY_ROOT_ALIAS_TEST", ROOT_ALIAS_TEST),
]


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


GATE_ENV = {
    "GITHUB_WORKSPACE": "/gate/workspace",
    "RUNNER_TEMP": "/gate/runner-temp",
    "CARGO_TARGET_DIR": "/gate/target",
    "NATIVE_GALLERY_READ_TEST": GALLERY_TEST,
    "NATIVE_RSS_UPDATE_TEST": RSS_TEST,
    "NATIVE_RSS_HOLD_CONTROL_TEST": RSS_HOLD_TEST,
}
GATE_ENV.update({env: test for _, env, test in EXTRA_CASES})


@contextlib.contextmanager
def gate_environ():
    saved = {name: os.environ.get(name) for name in GATE_ENV}
    os.environ.update(GATE_ENV)
    try:
        yield
    finally:
        for name, value in saved.items():
            if value is None:
                os.environ.pop(name, None)
            else:
                os.environ[name] = value


def gate_module():
    """Execute the REAL gate block and return what it built.

    Only the environment is controlled; the block's own code decides the case
    list. This host is not Windows, so the executed list is the seven base
    cases and the Windows-only append is asserted from the branch source.
    """
    namespace = {"__name__": "native_gate_under_test"}
    with gate_environ():
        exec(compile(gate_block(), str(WORKFLOW), "exec"), namespace)  # noqa: S102
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
    for label, env, test in EXTRA_CASES:
        assert f'"{label}"' in block, label
        assert env in block, env
        assert f"{env}: {test}" in text, label


def test_workflow_keeps_both_native_matrix_hosts():
    text = workflow_text()
    assert "macos-15-intel" in text
    assert "windows-2022" in text
    assert "native-gallery-read:" in text


def test_cargo_argv_places_ignored_after_the_libtest_separator():
    cargo_argv = gate_functions("cargo_argv")["cargo_argv"]
    argv = cargo_argv(RSS_HOLD_TEST, "/runner/temp/cargo-target", True)
    assert argv[:2] == ["cargo", "test"]
    assert argv.index("--exact") > argv.index("--")
    assert argv[argv.index("--exact") + 1] == RSS_HOLD_TEST
    assert argv.index("--ignored") > argv.index("--exact")
    assert argv.count("--ignored") == 1
    assert argv[-1] == "--nocapture"
    assert argv[argv.index("--target-dir") + 1] == "/runner/temp/cargo-target"


def test_the_two_original_cases_are_unchanged_and_never_ignored():
    module = gate_module()
    cases = module["CASES"]
    labels = [case[0] for case in cases]
    names = [case[1] for case in cases]
    flags = [case[2] for case in cases]
    # The original two keep their exact position, name and non-ignored flag.
    assert labels[:2] == ["native-gallery-read", "native-rss-update"]
    assert names[:2] == [GALLERY_TEST, RSS_TEST]
    assert flags[:2] == [False, False]
    argv = module["cargo_argv"](RSS_TEST, "/runner/temp/cargo-target")
    assert "--ignored" not in argv
    assert argv[-3:] == ["--exact", RSS_TEST, "--nocapture"]
    # The five gallery/archive consumers follow them, in order, never ignored.
    assert labels[2:] == [label for label, _, _ in EXTRA_CASES]
    assert names[2:] == [test for _, _, test in EXTRA_CASES]
    assert flags[2:] == [False] * len(EXTRA_CASES)


def test_seven_base_cases_bind_the_five_gallery_archive_consumers():
    module = gate_module()
    cases = module["CASES"]
    assert len(cases) == 7, cases
    assert [case[2] for case in cases] == [False] * 7
    text = workflow_text()
    block = gate_block()
    for label, env, test in EXTRA_CASES:
        # Each case owns an independent safe OUTDIR and exact argv.
        assert "${{ runner.temp }}/" + label in text, label
        argv = module["cargo_argv"](test, "/runner/temp/cargo-target")
        assert argv[argv.index("--exact") + 1] == test, label
        assert "--ignored" not in argv, label
        assert argv[argv.index("--target-dir") + 1] == "/runner/temp/cargo-target"
        assert env in block, env
    # Windows is the only host that adds the ignored long-running control.
    assert len(cases) + 1 == 8


def test_the_ignored_control_is_appended_only_in_the_windows_branch():
    tree = ast.parse(gate_block())
    branches = [
        node
        for node in tree.body
        if isinstance(node, ast.If) and "os.name" in ast.unparse(node.test)
    ]
    assert len(branches) == 1, [
        ast.unparse(node.test) for node in tree.body if isinstance(node, ast.If)
    ]
    assert ast.unparse(branches[0].test) in ('os.name == "nt"', "os.name == 'nt'")
    body = "\n".join(ast.unparse(node) for node in branches[0].body)
    assert "native-rss-hold-control" in body
    assert "NATIVE_RSS_HOLD_CONTROL_TEST" in body
    assert "True" in body
    for node in tree.body:
        if isinstance(node, ast.If) and node is not branches[0]:
            assert "CASES.append" not in ast.unparse(node)
    text = workflow_text()
    assert f"NATIVE_RSS_HOLD_CONTROL_TEST: {RSS_HOLD_TEST}" in text
    assert "${{ runner.temp }}/native-rss-hold-control" in text


MIGRATION_BEGIN = "# --- migration-chain-gate: begin ---"
MIGRATION_END = "# --- migration-chain-gate: end ---"
MIGRATION_CASE = (
    "tests::native_rss_update::migration_chain::"
    "native_legacy_session_to_archived_ui_and_rss_b_same_host"
)


def migration_block():
    text = workflow_text()
    start = text.index(MIGRATION_BEGIN) + len(MIGRATION_BEGIN)
    stop = text.index(MIGRATION_END, start)
    return textwrap.dedent(text[start:stop])


def migration_functions(*names):
    block = migration_block()
    tree = ast.parse(block, filename=str(WORKFLOW))
    found = {
        node.name: ast.get_source_segment(block, node)
        for node in tree.body
        if isinstance(node, ast.FunctionDef) and node.name in names
    }
    missing = [name for name in names if name not in found]
    assert not missing, f"migration gate block is missing {missing}"
    source = "import re\n\n\n" + "\n\n\n".join(found[name] for name in names)
    namespace = {}
    exec(compile(source, str(WORKFLOW), "exec"), namespace)  # noqa: S102 - own workflow source
    return namespace


def test_migration_gate_rejects_zero_match_and_accepts_only_exact_pass():
    classify = migration_functions("classify_chain")["classify_chain"]
    passed = classify(pass_output(MIGRATION_CASE), 0, MIGRATION_CASE)
    assert passed["verdict"] == "PASS", passed
    assert passed["executed"] == 1
    assert passed["exact_test_name_observed"] is True
    zero = classify(zero_output(), 0, MIGRATION_CASE)
    assert zero["verdict"] == "ZERO_MATCH_REJECTED", zero
    assert zero["executed"] == 0
    compile_error = classify(zero_output(), 101, MIGRATION_CASE)
    assert compile_error["verdict"] == "INFRA_SETUP_OR_COMPILE", compile_error
    other = classify(pass_output("tests::native_rss_update::some_other_case"), 0, MIGRATION_CASE)
    assert other["verdict"] == "INVALID_LIBTEST_SUMMARY", other
    failed = classify(
        "running 1 test\n"
        f"test {MIGRATION_CASE} ... FAILED\n\n"
        "test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; "
        "415 filtered out; finished in 0.02s\n",
        101,
        MIGRATION_CASE,
    )
    assert failed["verdict"] == "BEHAVIOR_RED", failed


def test_migration_gate_argv_is_ignored_exact_and_target_bound():
    argv = migration_functions("cargo_argv_chain")["cargo_argv_chain"](
        MIGRATION_CASE, "/runner/temp/cargo-target"
    )
    assert argv[:2] == ["cargo", "test"]
    assert argv[argv.index("--exact") + 1] == MIGRATION_CASE
    assert argv.index("--exact") > argv.index("--")
    assert argv.index("--ignored") > argv.index("--exact")
    assert argv[-1] == "--nocapture"
    assert argv[argv.index("--target-dir") + 1] == "/runner/temp/cargo-target"
    assert argv.count("--target-dir") == 1


def test_migration_gate_verifies_a_fresh_report_and_binds_the_run_root():
    block = migration_block()
    assert "--verify-report" in block
    assert "--expected-root" in block
    assert "--require-clean" in block
    assert "REPORT_MISSING" in block
    assert 'RUN_ROOT / "migration-report.json"' in block
    assert "migration-run" in block
    assert "migration-gate" in block
    text = workflow_text()
    assert "Migration chain (real host, original UI, ignored exact)" in text
    assert "${{ runner.temp }}/migration-run/migration-report.json" in text
    assert "${{ runner.temp }}/migration-gate/gate.json" in text


def test_migration_gate_binds_the_actual_checkout_not_the_report():
    block = migration_block()
    # Trusted identity comes from real Git reads with checked exit status.
    assert '"--expected-head"' in block
    assert '"--expected-worktree"' in block
    assert "returncode != 0" in block
    assert '"rev-parse", "HEAD"' in block
    assert '"--show-toplevel"' in block
    assert '"status", "--porcelain", "--untracked-files=all"' in block
    assert "GITHUB_SHA" in block
    assert "SOURCE_CHANGED" in block
    # A missing report is still rejected through the bound verifier call.
    assert '"REPORT_MISSING"' in block


def test_migration_gate_verifier_argv_carries_the_trusted_identity():
    namespace = migration_functions("verifier_entry")
    calls = []

    class Completed:
        returncode = 0
        stdout = "migration report: ACCEPTED\n"
        stderr = ""

    namespace["run"] = lambda argv, timeout: (calls.append(argv), Completed())[1]
    namespace["REPORT"] = ROOT / "missing-migration-report.json"
    namespace["RUN_ROOT"] = ROOT
    namespace["VERIFIER"] = ROOT / "tools" / "browser" / "migration_probe.py"
    namespace["PYTHON"] = "python3"
    entry = namespace["verifier_entry"]({"head": "a" * 40, "worktree": "/bound/worktree"})
    argv = calls[0]
    assert argv[argv.index("--expected-head") + 1] == "a" * 40
    assert argv[argv.index("--expected-worktree") + 1] == "/bound/worktree"
    assert argv[argv.index("--expected-root") + 1] == str(ROOT)
    # A zero exit from a run without a report is still a refusal.
    assert entry["report_present"] is False
    assert entry["returncode"] == 0
    assert entry["verdict"] == "REPORT_MISSING"


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
    test_cargo_argv_places_ignored_after_the_libtest_separator,
    test_the_two_original_cases_are_unchanged_and_never_ignored,
    test_seven_base_cases_bind_the_five_gallery_archive_consumers,
    test_the_ignored_control_is_appended_only_in_the_windows_branch,
    test_migration_gate_rejects_zero_match_and_accepts_only_exact_pass,
    test_migration_gate_argv_is_ignored_exact_and_target_bound,
    test_migration_gate_verifies_a_fresh_report_and_binds_the_run_root,
    test_migration_gate_binds_the_actual_checkout_not_the_report,
    test_migration_gate_verifier_argv_carries_the_trusted_identity,
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
