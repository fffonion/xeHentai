#!/usr/bin/env python3
"""Behavioural contract tests for the B6 update comparison pipeline.

These exercise the PRODUCTION compat.py registration, comparator, native
intersection guard, self-check and global verifier with the real catalog and
real inventory documents. They never contact a network, never start Cargo and
never write outside the assigned TEMP: the live oracle/probe/native execution
is the two-round suite proof, not this file.

Fixture observations come from the pipeline's own registered expectation
builder so that each test mutates ONE registered aspect and checks that the
intended guard detects it (not an unrelated default failure). The independent
expectation data itself lives in tools/compat.py and is checked against the
real producers in the final rounds.

    python3 tools/test_update_compat.py
"""

import json
import os
import shutil
import sys
import tempfile
import traceback
from pathlib import Path

TOOLS = Path(__file__).resolve().parent
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import compat  # noqa: E402

ASSIGNED_TMP = Path(
    "/mnt/TEMP/workspace/xehentai-rss/tmp/xehentai-pyshape-resume-20261102-89633df7")
ORIGIN = "http://127.0.0.1:43123"


def _required(name):
    value = getattr(compat, name, None)
    assert value is not None, f"compat.{name} is not registered"
    return value


def _case_table():
    table = _required("UPDATE_CASES")
    assert isinstance(table, (list, tuple)), table
    assert len(table) >= 11, len(table)
    ids = []
    for entry in table:
        assert isinstance(entry, dict), entry
        assert isinstance(entry["case"], str) and entry["case"], entry
        assert entry["id"] in compat.UPDATE_CASE_IDS, entry
        assert isinstance(entry["sections"], tuple), entry
        ids.append(entry["case"])
    assert len(set(ids)) == len(ids), ids
    return table


def _observation(case, side, origin=ORIGIN):
    build = _required("update_expected_observation")
    value = build(case, side, origin)
    assert isinstance(value, dict), value
    assert value["side"] == side, value
    assert isinstance(value["sections"], dict), value
    return value


def _fixtures(origin=ORIGIN):
    table = _case_table()
    oracle = {entry["case"]: _observation(entry["case"], "python", origin)
              for entry in table}
    candidate = {entry["case"]: _observation(entry["case"], "native", origin)
                 for entry in table}
    return oracle, candidate


def _run_compare(oracle, candidate):
    cases = {}
    diffs = compat.compare_update(cases, oracle, candidate, ORIGIN)
    assert isinstance(diffs, list), diffs
    return cases, diffs


def _assert_mutation_detected(case, side, mutate, owner, needle=None):
    oracle, candidate = _fixtures()
    target = oracle if side == "python" else candidate
    mutate(target[case]["sections"])
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs, f"{case}/{side} mutation produced no diff"
    assert cases.get(owner) is False, (
        f"{case}/{side} mutation did not fail {owner}: {cases}")
    assert any(owner in text or case in text for text in diffs), diffs
    if needle is not None:
        assert any(needle in text for text in diffs), (needle, diffs)


def test_update_is_registered_everywhere_required():
    assert "update" in compat.ALL_SUITES, compat.ALL_SUITES
    assert "update" in compat.NATIVE_COMMANDS, sorted(compat.NATIVE_COMMANDS)
    assert "update" in compat.NATIVE_CASE_MAP, sorted(compat.NATIVE_CASE_MAP)
    assert "update" in compat.ORACLE_SUITES, compat.ORACLE_SUITES
    assert set(compat.UPDATE_CASE_IDS) == {
        "update.channel", "update.commit", "update.decision",
        "update.package", "update.apply",
    }, compat.UPDATE_CASE_IDS
    table = _case_table()
    owners = {entry["id"] for entry in table}
    assert owners == set(compat.UPDATE_CASE_IDS), owners


def test_update_expectations_compare_clean_on_both_sides():
    oracle, candidate = _fixtures()
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs == [], diffs
    assert set(cases) == set(compat.UPDATE_CASE_IDS), cases
    for case_id in compat.UPDATE_CASE_IDS:
        assert cases[case_id] is True, (case_id, cases)


def test_update_documented_channel_and_format_deltas_are_explicit():
    """The registered per-side expectations keep the real deltas, not blanks."""
    table = {entry["case"]: entry for entry in _case_table()}
    stable = table["channel.stable"]
    python = _observation("channel.stable", "python")["sections"]
    native = _observation("channel.stable", "native")["sections"]
    assert python["source"]["channel_url"].endswith("commits?sha=master")
    assert native["source"]["channel_url"].endswith(
        "/repos/fffonion/xeHentai/commits?sha=rss")
    assert "/archive/" in python["source"]["download_link"]
    assert native["source"]["download_link"].endswith("/xeHentai-rss.zip")
    assert native["source"]["descriptor_link"] == (
        native["source"]["download_link"] + ".manifest.json")
    assert python["source"]["archive_prefix"].endswith("/xeHentai")
    assert native["source"]["archive_prefix"].endswith("/")
    assert stable["id"] == "update.channel"
    package = _observation("package.envelope", "native")["sections"]["package"]
    assert package["member_names"] == ["info.json", "info.sig", "payload.zip"]
    assert package["compress_types"] == [0, 0, 0], package
    source_package = _observation("package.envelope", "python")["sections"]["package"]
    assert source_package["member_names"][0] == "info.json"
    assert set(source_package["compress_types"]) == {0}, source_package
    assert source_package["member_names"] != package["member_names"]


def test_update_first_versus_second_commit_is_detected():
    _assert_mutation_detected(
        "commit.first", "python",
        lambda section: section["commit"].__setitem__("update_id", "second-entry"),
        "update.commit", "commit.update_id")
    _assert_mutation_detected(
        "commit.first", "native",
        lambda section: section["commit"].__setitem__("message", "first entry"),
        "update.commit", "commit.message")


def test_update_channel_and_business_url_are_detected():
    _assert_mutation_detected(
        "channel.stable", "python",
        lambda section: section["source"].__setitem__(
            "channel_url",
            "https://api.github.com/repos/fffonion/xeHentai/commits?sha=dev"),
        "update.channel", "source.channel_url")
    _assert_mutation_detected(
        "channel.stable", "native",
        lambda section: section["feed"].__setitem__(
            "targets", ["/repos/fffonion/xeHentai/commits?sha=dev"]),
        "update.channel", "feed.targets")
    _assert_mutation_detected(
        "channel.beta", "native",
        lambda section: section["source"].__setitem__("channel", "rss"),
        "update.channel", "source.channel")
    _assert_mutation_detected(
        "channel.beta-missing", "native",
        lambda section: section["feed"].__setitem__("request_count", 1),
        "update.channel", "feed.request_count")


def test_update_typed_metadata_and_wrong_types_are_rejected():
    _assert_mutation_detected(
        "commit.first", "python",
        lambda section: section["commit"].__setitem__("update_id_type", 1),
        "update.commit")
    # Bool-as-int and a truthy string where an int is required, on BOTH sides.
    oracle, candidate = _fixtures()
    for side in (oracle, candidate):
        side["decision.off"]["sections"]["feed"]["request_count"] = True
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs, "bool-as-int passed the schema on both sides"
    assert cases.get("update.decision") is False, cases
    oracle, candidate = _fixtures()
    for side in (oracle, candidate):
        side["decision.same"]["sections"]["decision"]["pending_before"] = "yes"
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs, "truthy wrong-type passed the schema on both sides"
    assert cases.get("update.decision") is False, cases


def test_update_both_sided_missing_fields_fail():
    oracle, candidate = _fixtures()
    for side in (oracle, candidate):
        del side["package.envelope"]["sections"]["package"]["member_names"]
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs, "a both-sided omission passed"
    assert cases.get("update.package") is False, cases
    # Unknown variation: the same extra field on both sides must fail.
    oracle, candidate = _fixtures()
    for side in (oracle, candidate):
        side["apply.valid"]["sections"]["apply"]["unknown_field"] = 1
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs, "an unknown both-sided variation passed"
    assert cases.get("update.apply") is False, cases
    # A missing case (missing round evidence for one ID) must fail.
    oracle, candidate = _fixtures()
    del candidate["apply.reject"]
    cases, diffs = _run_compare(oracle, candidate)
    assert diffs, "a missing case passed"
    assert cases.get("update.apply") is False, cases


def test_update_decision_package_and_apply_mutations_are_detected():
    _assert_mutation_detected(
        "decision.off", "native",
        lambda section: section["decision"].__setitem__("outcome", "pending"),
        "update.decision", "decision.outcome")
    _assert_mutation_detected(
        "decision.off", "python",
        lambda section: section["decision"]["events"][1].__setitem__(
            "level", "INFO "),
        "update.decision", "decision.events")
    _assert_mutation_detected(
        "decision.download", "native",
        lambda section: section["decision"].__setitem__(
            "pending_after", False),
        "update.decision", "pending_after")
    _assert_mutation_detected(
        "package.envelope", "native",
        lambda section: section["package"].__setitem__(
            "member_names", ["info.json", "payload.zip"]),
        "update.package", "member_names")
    _assert_mutation_detected(
        "package.envelope", "python",
        lambda section: section["package"].__setitem__(
            "compress_types", [8]),
        "update.package", "compress_types")
    _assert_mutation_detected(
        "apply.valid", "native",
        lambda section: section["apply"].__setitem__("session_equal", False),
        "update.apply", "session_equal")
    _assert_mutation_detected(
        "apply.valid", "native",
        lambda section: section["apply"].__setitem__("live_changed", False),
        "update.apply", "live_changed")
    _assert_mutation_detected(
        "apply.reject", "native",
        lambda section: section["apply"].__setitem__("pending_after", True),
        "update.apply", "pending_after")
    _assert_mutation_detected(
        "cli.off", "python",
        lambda section: section["feed"].__setitem__("request_count", 1),
        "update.decision", "request_count")


def _native_names():
    mapping = _required("update_required_native_names")()
    assert isinstance(mapping, dict), mapping
    assert set(mapping) == set(compat.UPDATE_CASE_IDS), mapping
    for case_id, names in mapping.items():
        assert names, f"{case_id} has no mapped native test"
        for name in names:
            assert isinstance(name, str) and name, (case_id, name)
    return mapping


def _status_fixture(*, omit=(), non_ok=(), extra=()):
    results = {}
    for names in _native_names().values():
        for name in names:
            results[name] = "ok"
    for name in omit:
        results.pop(name, None)
    for name in non_ok:
        assert name in results, name
        results[name] = "FAILED"
    for name in extra:
        results[name] = "ok"
    return results


def _native_run(results):
    return {
        "results": results,
        "summary": {"status": "ok", "passed": len(results),
                    "failed": 0, "ignored": 0},
        "argv": ["cargo", "test"],
        "stdout": "",
    }


def _all_true():
    return {case_id: True for case_id in compat.UPDATE_CASE_IDS}


def test_update_native_intersection_requires_every_mapped_name_same_round():
    mapping = _native_names()
    cases = _all_true()
    diffs = compat.validate_update_native_proof(
        [_native_run(_status_fixture())], cases)
    assert diffs == [], diffs
    assert all(cases[case_id] is True for case_id in compat.UPDATE_CASE_IDS)

    victim_case = next(iter(mapping))
    victim = mapping[victim_case][0]
    cases = _all_true()
    diffs = compat.validate_update_native_proof(
        [_native_run(_status_fixture(omit=(victim,)))], cases)
    assert cases[victim_case] is False, cases
    assert any(victim in text for text in diffs), diffs

    cases = _all_true()
    diffs = compat.validate_update_native_proof(
        [_native_run(_status_fixture(non_ok=(victim,)))], cases)
    assert cases[victim_case] is False, cases
    assert any(victim in text for text in diffs), diffs

    # A native-ok set can never resurrect a failed wire/comparator leg.
    cases = _all_true()
    cases[victim_case] = False
    diffs = compat.validate_update_native_proof(
        [_native_run(_status_fixture())], cases)
    assert cases[victim_case] is False, cases
    assert any(victim_case in text for text in diffs), diffs

    # Missing runs / extra names / zero tests are rejected.
    cases = _all_true()
    assert compat.validate_update_native_proof([], cases), "empty native runs"
    cases = _all_true()
    diffs = compat.validate_update_native_proof(
        [_native_run(_status_fixture(extra=("tests::m7p::bogus",)))], cases)
    assert any("bogus" in text for text in diffs), diffs
    assert all(cases[case_id] is False for case_id in compat.UPDATE_CASE_IDS)


def test_update_self_check_runs_production_path_with_mutations():
    oracle, candidate = _fixtures()
    runs = [_native_run(_status_fixture())]
    bundle = {"cases": oracle, "origin": ORIGIN}
    checks = compat.self_check("update", bundle, {"cases": candidate,
                                                  "origin": ORIGIN}, runs)
    assert checks, checks
    assert all(entry["detected"] for entry in checks), checks
    names = {entry["mutation"] for entry in checks}
    for required in (
        "first-versus-second-commit", "channel-url", "mode-outcome",
        "typed-metadata", "pending-member", "application-state",
        "error-class", "both-sided-missing", "both-sided-wrong-type",
        "both-sided-truthy-type", "both-sided-falsy-type",
        "request-sequence", "session-state", "source-channel",
        "native-missing-name", "native-non-ok", "false-wire-native-ok",
        "missing-case", "unknown-variation",
    ):
        assert required in names, (required, sorted(names))


def test_global_verify_all_rejects_missing_other_suite_proofs():
    """Scoped update evidence must never be reported as global acceptance."""
    root = Path(tempfile.mkdtemp(
        prefix="b6-verify-", dir=str(_scratch_base())))
    try:
        head = compat.subprocess.run(
            ["git", "rev-parse", "HEAD"], cwd=compat.REPO,
            capture_output=True, text=True)
        manifest = {
            "tool": "tools/compat.py",
            "head": head.stdout.strip(),
            "dirty": bool(compat.subprocess.run(
                ["git", "status", "--porcelain"], cwd=compat.REPO,
                capture_output=True, text=True).stdout.strip()),
            "suites": ["update"],
            "complete": False,
            "results": [],
        }
        (root / "manifest.json").write_text(
            json.dumps(manifest), encoding="utf-8")
        for round_number in (1, 2):
            round_dir = root / "update" / f"round-{round_number}"
            round_dir.mkdir(parents=True)
            (round_dir / "comparison.json").write_text(json.dumps({
                "suite": "update", "round": round_number, "ok": True,
                "diffs": [],
                "cases": {case_id: True for case_id in compat.UPDATE_CASE_IDS},
                "native": [],
            }), encoding="utf-8")
        assert compat.verify_all(root) == 1, (
            "scoped update evidence was accepted as global completion")
    finally:
        shutil.rmtree(root, ignore_errors=True)


def _scratch_base():
    base = Path(os.environ.get("TMPDIR", str(ASSIGNED_TMP / "b6-work")))
    base.mkdir(parents=True, exist_ok=True)
    return base


TESTS = [
    test_update_is_registered_everywhere_required,
    test_update_expectations_compare_clean_on_both_sides,
    test_update_documented_channel_and_format_deltas_are_explicit,
    test_update_first_versus_second_commit_is_detected,
    test_update_channel_and_business_url_are_detected,
    test_update_typed_metadata_and_wrong_types_are_rejected,
    test_update_both_sided_missing_fields_fail,
    test_update_decision_package_and_apply_mutations_are_detected,
    test_update_native_intersection_requires_every_mapped_name_same_round,
    test_update_self_check_runs_production_path_with_mutations,
    test_global_verify_all_rejects_missing_other_suite_proofs,
]


def main():
    failures = 0
    for test in TESTS:
        try:
            test()
        except Exception:  # noqa: BLE001 - report and keep running
            failures += 1
            print(f"FAIL {test.__name__}")
            traceback.print_exc()
        else:
            print(f"ok {test.__name__}")
    print(f"{len(TESTS) - failures}/{len(TESTS)} passed")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
