#!/usr/bin/env python3
"""Durable gallery proofs for the A4 closeout.

Live-Python-App deepcopy isolation characterization: build a live App from
the existing pinned upstream and the current gallery fixture builders in a
fresh owned sibling TEMP directory, capture the actual production
``_gallery_poststate``, freeze its independent JSON representation, mutate
live Task nested meta/config/maps/queues and the App registry/cookies, then
verify the OLD snapshot byte representation is unchanged while a fresh
capture reflects the real mutation.

This characterizes the already-implemented deepcopy (not invented RED):
it passes because ``_gallery_task_obs``/``_gallery_poststate`` deep-freeze.
Runnable without extra dependencies: ``python3 tools/test_compat_gallery.py``.
"""

import copy
import io
import json
import os
import shutil
import sys
import tempfile
import traceback
import zipfile
from pathlib import Path

TOOLS = Path(__file__).resolve().parent
if str(TOOLS) not in sys.path:
    sys.path.insert(0, str(TOOLS))

import compat  # noqa: E402

ASSIGNED_TMP = Path(
    "/mnt/TEMP/workspace/xehentai-rss/tmp/xehentai-pyshape-20261031-f870b8ca")
DEFAULT_UPSTREAM = ASSIGNED_TMP / "upstream"


def _base_tmp():
    base = Path(os.environ.get("TMPDIR", str(ASSIGNED_TMP / "a4-work")))
    base.mkdir(parents=True, exist_ok=True)
    return base


def _upstream():
    path = Path(os.environ.get("XEH_UPSTREAM", str(DEFAULT_UPSTREAM)))
    if not (path / "xeHentai" / "config.py").is_file():
        raise AssertionError(f"invalid upstream tree: {path}")
    return path


def _build_live_gallery_app(work):
    """Live App from pinned upstream + current gallery fixture builders."""
    upstream = _upstream()
    core, task_module, const_module, defaults = compat._load_upstream(upstream)
    request = compat.gallery_request()
    app = compat._rpc_build_app(
        core, task_module, const_module, defaults, work, request["tasks"])
    # Same nontrivial reverse-order cookie fixture the oracle uses.
    app.set_cookie("zebra=3; apple=1; mango=2")
    return app


def _freeze(value):
    return json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True)


def test_gallery_poststate_snapshot_is_deepcopy_isolated():
    work = Path(tempfile.mkdtemp(prefix="gallery-deepcopy-",
                                 dir=str(_base_tmp())))
    try:
        app = _build_live_gallery_app(work)
        snapshot = compat._gallery_poststate(app, work)
        assert snapshot["tasks"], "expected gallery tasks in snapshot"
        frozen = _freeze(snapshot)
        first_guid = snapshot["tasks"][0][0]
        live = app._all_tasks[first_guid]
        had_gid = "gid" in live.__dict__
        old_gid = live.__dict__.get("gid")
        old_sethash = live.__dict__.get("sethash")

        # Mutate the LIVE objects only: nested meta/config/maps, registry
        # object state, app queue and cookies.
        live.meta["title"] = "MUTATED-TITLE"
        live.meta["finished"] = 999
        live.config["download_thread_cnt"] = 99
        live.renamed_map[9876] = "mutated.jpg"
        live.state = 99
        if had_gid:
            live.gid = "MUTATED-GID"
            live.sethash = "MUTATED-SETHASH"
        app.tasks.queue.append("MUTATED-QUEUE-GUID")
        app.set_cookie("mutated_cookie=1")

        # OLD snapshot byte representation is unchanged.
        assert _freeze(snapshot) == frozen, \
            "live mutation aliased the frozen poststate snapshot"

        # Fresh capture reflects the real mutation.
        fresh = compat._gallery_poststate(app, work)
        fresh_frozen = _freeze(fresh)
        assert fresh_frozen != frozen, \
            "fresh capture did not reflect the live mutation"
        fresh_record = dict(
            (guid, record) for guid, record in fresh["tasks"])[first_guid]
        assert fresh_record["meta"]["title"] == "MUTATED-TITLE", fresh_record["meta"]
        assert fresh_record["meta"]["finished"] == 999, fresh_record["meta"]
        assert fresh_record["config"]["download_thread_cnt"] == 99, \
            fresh_record["config"]
        assert [9876, "mutated.jpg"] in fresh_record["renamed"], \
            fresh_record["renamed"]
        assert fresh_record["state"] == 99, fresh_record["state"]
        if had_gid:
            assert old_gid != "MUTATED-GID" and old_sethash != "MUTATED-SETHASH"
            assert fresh_record["gid"] == "MUTATED-GID", fresh_record.get("gid")
            assert fresh_record["sethash"] == "MUTATED-SETHASH", \
                fresh_record.get("sethash")
            old_record = dict(
                (guid, record) for guid, record in snapshot["tasks"])[first_guid]
            assert old_record["gid"] == old_gid, old_record.get("gid")
            assert old_record["sethash"] == old_sethash, old_record.get("sethash")
        assert "MUTATED-QUEUE-GUID" in fresh["queue"], fresh["queue"]
        assert ["mutated_cookie", "1"] in fresh["cookies"], fresh["cookies"]
        # Disk dimension untouched by pure object mutation.
        assert fresh["disk"] == snapshot["disk"], "disk must be unaffected"
    finally:
        shutil.rmtree(work, ignore_errors=True)


def _archive_response(comment):
    data = io.BytesIO()
    with zipfile.ZipFile(data, "w") as archive:
        archive.writestr("sample.jpg", b"image")
        archive.comment = comment.encode("utf-8")
    return {
        "id": "rpc.http.zip-valid",
        "status": "HTTP/1.0 200 OK",
        "headers": [("content-type", "application/zip")],
        "body": data.getvalue().decode("latin-1"),
        "trailing_newline": False,
    }


def test_rpc_archive_comment_uses_displayed_version():
    suffix = "\nTitle:Gallery\nOriginal URL:https://example.test/gallery"
    oracle = {"cases": [_archive_response("xeHentai Archiver v2.023" + suffix)]}
    candidate = {"cases": [_archive_response("xeHentai Archiver v3.0.0-dev" + suffix)]}
    cases = {}
    assert compat._compare_rpc_group(cases, oracle, candidate) == []
    assert cases["rpc.http.zip-valid"] is True


def test_rpc_archive_comment_rejects_different_gallery():
    suffix = "\nTitle:Gallery\nOriginal URL:https://example.test/gallery"
    oracle = {"cases": [_archive_response("xeHentai Archiver v2.023" + suffix)]}
    candidate = {"cases": [_archive_response(
        "xeHentai Archiver v3.0.0-dev" + suffix.replace("Gallery", "Different"))]}
    cases = {}
    assert compat._compare_rpc_group(cases, oracle, candidate) == [
        "rpc.http.zip-valid: zip comment mismatch"]
    assert cases["rpc.http.zip-valid"] is False


def test_gallery_archive_comment_uses_displayed_version():
    suffix = "\nTitle:Gallery\nOriginal URL:https://example.test/gallery"
    assert compat._archive_comment_matches(
        "xeHentai Archiver v2.023" + suffix,
        "xeHentai Archiver v3.0.0-dev" + suffix)
    assert not compat._archive_comment_matches(
        "xeHentai Archiver v2.023" + suffix,
        "xeHentai Archiver v3.0.0-dev" + suffix.replace("Gallery", "Different"))


def test_files_archive_comments_keep_metadata_comparison():
    suffix = "\nTitle:Gallery\nOriginal URL:https://example.test/gallery"
    old_comment = "xeHentai Archiver v2.023" + suffix
    new_comment = "xeHentai Archiver v3.0.0-dev" + suffix
    tree = [{"path": "downloads/archive.zip", "type": "file",
             "size": 200 + len(old_comment.encode("utf-8")), "text": None}]
    oracle = {
        "archive": {"first": {"archive": {
            "comment": old_comment,
            "entries": ["sample.jpg"]}, "tree": copy.deepcopy(tree)},
            "tree": copy.deepcopy(tree)},
        "zip64": {"comment": old_comment, "entry_count": 1},
    }
    candidate = copy.deepcopy(oracle)
    candidate["archive"]["first"]["archive"]["comment"] = new_comment
    candidate["zip64"]["comment"] = new_comment
    for entries in (candidate["archive"]["first"]["tree"],
                    candidate["archive"]["tree"]):
        entries[0]["size"] = 200 + len(new_comment.encode("utf-8"))
    expected = compat._files_expected_archive_comments(oracle, candidate)
    assert expected == candidate
    assert oracle["zip64"]["comment"] == old_comment
    altered = copy.deepcopy(candidate)
    altered["zip64"]["comment"] = altered["zip64"]["comment"].replace(
        "Title:Gallery", "Title:Different")
    assert compat._files_expected_archive_comments(oracle, altered) != altered
    altered = copy.deepcopy(candidate)
    altered["archive"]["tree"][0]["size"] += 1
    assert compat._files_expected_archive_comments(oracle, altered) != altered


TESTS = [
    test_gallery_poststate_snapshot_is_deepcopy_isolated,
    test_rpc_archive_comment_uses_displayed_version,
    test_rpc_archive_comment_rejects_different_gallery,
    test_gallery_archive_comment_uses_displayed_version,
    test_files_archive_comments_keep_metadata_comparison,
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
