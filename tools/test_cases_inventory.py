#!/usr/bin/env python3
"""Durable feature-inventory integrity tests for the production loader.

Under test is the production entry point ``compat.load_cases()`` with the
real ``tools/cases.json``. The catalog is a closed inventory: 378 unique
feature rows after the six known duplicate IDs were consolidated into one
row per ID, keeping BOTH original assertion legs. These tests pin the
post-consolidation counts, the literal identifiers, and the retained
wire/native references, and they exercise the loader's rejection of a
malformed or duplicate-bearing document.

Negative inputs are written as isolated JSON documents under the assigned
TEMP and bound through ``compat.CASES_FILE``; the real catalog is never
modified by a test and no HTTP API is faked. Standard library only:
``python3 tools/test_cases_inventory.py``.
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
    "/mnt/TEMP/workspace/xehentai-rss/tmp/xehentai-pyshape-20261031-f870b8ca")

# The six RPC feature IDs that had one wire-contract row and one focused
# native-test row each; one literal ID per feature is retained.
DUPLICATE_IDS = (
    "rpc.method.unknown",
    "rpc.method.private",
    "rpc.auth.missing",
    "rpc.auth.exact",
    "rpc.http.options",
    "rpc.http.unknown-get",
)
WIRE_SOURCE = "xeHentai/rpc.py do_POST/do_GET wire contract"
NATIVE_SOURCE = "xeHentai/rpc.py unit contract"
RPC_TARGET = "src/rpc/{mod,server}.rs + rss/rpc/handlers.rss"
EXPECTED_UNIQUE_ROWS = 378
EXPECTED_RPC_UNIQUE = 107
EXPECTED_IMPLEMENTED = 328
EXPECTED_DELTA = 14
EXPECTED_UNIMPLEMENTED = 36
CLI_UNIMPLEMENTED = 30
UPDATE_UNIMPLEMENTED = 6


def wire_assertion(case_id):
    return f"{case_id} matches the reference transcript"


def native_assertion(case_id):
    return f"{case_id} passes the focused native test"


def _scratch_base():
    base = Path(os.environ.get("TMPDIR", str(ASSIGNED_TMP / "a4-work")))
    base.mkdir(parents=True, exist_ok=True)
    return base


def _bind_catalog(document):
    """Write an isolated catalog document and bind production CASES_FILE."""
    root = Path(tempfile.mkdtemp(prefix="cases-inventory-",
                                 dir=str(_scratch_base())))
    path = root / "cases.json"
    path.write_text(json.dumps(document), encoding="utf-8")
    previous = compat.CASES_FILE
    compat.CASES_FILE = path
    return previous, root


def _load_error(document):
    """Load `document` through production load_cases; return the raised error."""
    previous, root = _bind_catalog(document)
    try:
        compat.load_cases()
    except compat.CompatError as error:
        return error
    finally:
        compat.CASES_FILE = previous
        shutil.rmtree(root, ignore_errors=True)
    raise AssertionError("load_cases accepted an invalid inventory document")


def _valid_inventory():
    return {
        "version": 1,
        "note": "isolated",
        "cases": [
            {"id": "alpha.case", "suite": "config", "status": "implemented",
             "source": "a", "target": "b", "inputs": [], "assertions": ["x"],
             "delta": None},
            {"id": "beta.case", "suite": "rpc", "status": "delta",
             "source": "c", "target": "d", "inputs": ["i"],
             "assertions": ["y"], "delta": "approved"},
        ],
    }


def _require_error(document, label):
    error = _load_error(document)
    assert isinstance(error, compat.CompatError), (label, error)
    return error


def test_current_catalog_ids_are_unique_and_strict():
    """The real catalog must load with no duplicate and only non-empty str IDs."""
    document = compat.load_cases()
    cases = document["cases"]
    assert isinstance(cases, list), type(cases)
    ids = [row["id"] for row in cases]
    for row in cases:
        assert isinstance(row, dict), row
        assert isinstance(row["id"], str) and row["id"], row
    duplicates = sorted({case_id for case_id in ids if ids.count(case_id) > 1})
    assert not duplicates, f"duplicate feature IDs remain: {duplicates}"
    assert len(cases) == EXPECTED_UNIQUE_ROWS, len(cases)
    assert len(set(ids)) == EXPECTED_UNIQUE_ROWS, len(set(ids))
    rpc_ids = [row["id"] for row in cases if row["suite"] == "rpc"]
    assert len(rpc_ids) == EXPECTED_RPC_UNIQUE, len(rpc_ids)
    assert len(set(rpc_ids)) == EXPECTED_RPC_UNIQUE, len(set(rpc_ids))


def test_six_merged_ids_keep_both_original_legs():
    """Each merged ID is one row retaining the wire and native literals."""
    document = compat.load_cases()
    rows = {row["id"]: [] for row in document["cases"]}
    for row in document["cases"]:
        if row["id"] in rows:
            rows[row["id"]].append(row)
    for case_id in DUPLICATE_IDS:
        matched = rows[case_id]
        assert len(matched) == 1, (case_id, len(matched))
        row = matched[0]
        assert row["suite"] == "rpc", row
        assert row["status"] == "implemented", row
        assert row["delta"] is None, row
        assert row["target"] == RPC_TARGET, row
        assert row["inputs"] == [], row
        assert isinstance(row["source"], str), row
        assert WIRE_SOURCE in row["source"], row["source"]
        assert NATIVE_SOURCE in row["source"], row["source"]
        assert row["source"] == f"{WIRE_SOURCE}; {NATIVE_SOURCE}", row["source"]
        expected = [wire_assertion(case_id), native_assertion(case_id)]
        assert row["assertions"] == expected, row["assertions"]


def test_unimplemented_cli_and_update_rows_unchanged():
    """CLI 30 and update 6 stay unimplemented with null delta."""
    document = compat.load_cases()
    for suite, expected in (("cli", CLI_UNIMPLEMENTED),
                            ("update", UPDATE_UNIMPLEMENTED)):
        rows = [row for row in document["cases"] if row["suite"] == suite]
        assert len(rows) == expected, (suite, len(rows))
        assert len({row["id"] for row in rows}) == expected, suite
        for row in rows:
            assert row["status"] == "unimplemented", row
            assert row["delta"] is None, row
    statuses = {}
    for row in document["cases"]:
        statuses[row["status"]] = statuses.get(row["status"], 0) + 1
    assert statuses.get("implemented") == EXPECTED_IMPLEMENTED, statuses
    assert statuses.get("delta") == EXPECTED_DELTA, statuses
    assert statuses.get("unimplemented") == EXPECTED_UNIMPLEMENTED, statuses


def test_duplicate_id_is_rejected_before_any_case_map():
    document = _valid_inventory()
    document["cases"].append(dict(document["cases"][0]))
    error = _require_error(document, "duplicate")
    message = str(error).lower()
    assert "duplicate" in message, message
    assert "alpha.case" in str(error), message


def test_missing_cases_collection_is_rejected():
    document = _valid_inventory()
    document.pop("cases")
    _require_error(document, "missing cases")


def test_non_list_cases_collection_is_rejected():
    document = _valid_inventory()
    document["cases"] = {"alpha.case": document["cases"][0]}
    _require_error(document, "cases mapping")


def test_non_object_document_is_rejected():
    _require_error([_valid_inventory()["cases"][0]], "top-level list")


def test_non_object_row_is_rejected():
    document = _valid_inventory()
    document["cases"].append("alpha.case")
    _require_error(document, "row string")


def test_missing_id_is_rejected():
    document = _valid_inventory()
    del document["cases"][1]["id"]
    _require_error(document, "missing id")


def test_empty_id_is_rejected():
    document = _valid_inventory()
    document["cases"][1]["id"] = ""
    _require_error(document, "empty id")


def test_non_string_id_is_rejected():
    for value in (7, None, ["alpha"], {"id": "alpha"}, True):
        document = _valid_inventory()
        document["cases"][1]["id"] = value
        _require_error(document, f"non-string id {value!r}")


def test_distinct_inventory_loads_without_changes():
    document = _valid_inventory()
    loaded = _load_document(document)
    assert loaded == document, loaded


def _load_document(document):
    previous, root = _bind_catalog(document)
    try:
        return compat.load_cases()
    finally:
        compat.CASES_FILE = previous
        shutil.rmtree(root, ignore_errors=True)


TESTS = [
    test_current_catalog_ids_are_unique_and_strict,
    test_six_merged_ids_keep_both_original_legs,
    test_unimplemented_cli_and_update_rows_unchanged,
    test_duplicate_id_is_rejected_before_any_case_map,
    test_missing_cases_collection_is_rejected,
    test_non_list_cases_collection_is_rejected,
    test_non_object_document_is_rejected,
    test_non_object_row_is_rejected,
    test_missing_id_is_rejected,
    test_empty_id_is_rejected,
    test_non_string_id_is_rejected,
    test_distinct_inventory_loads_without_changes,
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
