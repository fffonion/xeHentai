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


TESTS = [test_gallery_poststate_snapshot_is_deepcopy_isolated]


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
