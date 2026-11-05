#!/usr/bin/env python3
"""Mutation tests for the M8 migration chain report verifier.

The positive control is a synthetic *contract* report: these fixtures are unit
data, not product evidence. Every mutation must be rejected through the same
``verify_report`` entry point the CI gate uses. Run with:

  <venv>/bin/python tools/browser/test_migration_probe.py -v
"""

import copy
import json
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import migration_probe as verifier  # noqa: E402


def check_set(names):
    return [{"name": name, "ok": True, "detail": ""} for name in names]


HEAD = "b" * 40
WORKTREE = "/owned/worktree"
ARCHIVE_ENTRIES = [
    ".xehdone",
    "unique-1.jpg",
    "unique-2.jpg",
    "unique-3.jpg",
    "unique-4.jpg",
]


def sample(root):
    probe = Path(root) / "probe-report.json"
    probe.write_text(
        json.dumps(
            {
                "schema": verifier.PROBE_SCHEMA,
                "case": "m8-migration-ui-probe",
                "base_origin": "http://127.0.0.1:1",
                "port": 1,
                "guid": "m8chain1",
                "checks": check_set(verifier.UI_CHECKS),
                "screenshot": str(Path(root) / "probe.png"),
            }
        ),
        encoding="utf-8",
    )
    return {
        "schema": verifier.REPORT_SCHEMA,
        "case": verifier.CASE,
        "run_root": str(Path(root)),
        "source": {"head": HEAD, "dirty": False, "worktree": WORKTREE},
        "host": {"exe": "xeHentai", "same_exe_path": True},
        "rss": {
            "files": [
                "rss/filters/match.rss",
                "rss/rpc/handlers.rss",
                "rss/updater/github.rss",
                "rss/updater/updater.rss",
            ],
            "b_marker": verifier.B_MARKER,
            "applied_update_id": verifier.UPDATE_ID,
            "applied_v": 2,
        },
        "archive": {
            "zip_name": "M8 Migration Chain.zip",
            "entries": [
                ".xehdone",
                "unique-1.jpg",
                "unique-2.jpg",
                "unique-3.jpg",
                "unique-4.jpg",
            ],
            "compression": "stored",
            "four_images_byte_equal": True,
            "loose_dir_removed": True,
        },
        "resources": {
            "ui_assets": 7,
            "cache_fresh": True,
            "config_rss": "unchanged",
            "session_in_window": "unchanged",
            "exe_unchanged": True,
        },
        "ui_probe": {"checks": check_set(verifier.UI_CHECKS), "report": str(probe)},
        "fixture": {
            "python_source": verifier.PYTHON_REVISION,
            "present": [1, 4],
            "missing": [2, 3],
            "image_requests": 2,
            "old_image_requests": 0,
            "barrier_distinct": 2,
            "cookie_sent": True,
        },
        "negative": {
            "attempted": True,
            "single_worker": True,
            "barrier_overlap": False,
            "barrier_timed_out": True,
            "clean_exit": True,
        },
        "checks": check_set(verifier.REQUIRED_CHECKS),
        "cleanup": {"install_retained": False, "temp_root": str(Path(root))},
    }


class ReportVerifierTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = Path(self.directory.name)
        self.base = sample(self.root)

    def tearDown(self):
        self.directory.cleanup()

    def assertRejected(
        self, report, needle=None, root=None, clean=False, head=HEAD, worktree=WORKTREE
    ):
        with self.assertRaises(verifier.VerificationError) as caught:
            verifier.verify_report(report, root or self.root, clean, head, worktree)
        if needle:
            self.assertIn(needle, str(caught.exception))

    def test_positive_contract_report_is_accepted(self):
        verifier.verify_report(self.base, self.root, False, HEAD, WORKTREE)

    def test_a_report_from_another_clean_commit_is_rejected(self):
        stale = copy.deepcopy(self.base)
        stale["source"]["head"] = "c" * 40
        self.assertRejected(stale, "does not match the checkout")

    def test_a_report_from_another_worktree_is_rejected(self):
        wrong = copy.deepcopy(self.base)
        wrong["source"]["worktree"] = str(self.root / "another-worktree")
        self.assertRejected(wrong, "worktree")

    def test_the_binding_requires_both_trusted_values(self):
        with self.assertRaises(verifier.VerificationError) as caught:
            verifier.verify_report(self.base, self.root, False, None, WORKTREE)
        self.assertIn("expected head and worktree", str(caught.exception))
        with self.assertRaises(verifier.VerificationError) as caught:
            verifier.verify_report(self.base, self.root, False, HEAD, None)
        self.assertIn("expected head and worktree", str(caught.exception))

    def test_a_malformed_expected_head_is_rejected_not_repaired(self):
        for bad in (HEAD.upper(), " " + HEAD, HEAD[:-1], HEAD + "\n"):
            with self.assertRaises(verifier.VerificationError) as caught:
                verifier.verify_report(self.base, self.root, False, bad, WORKTREE)
            self.assertIn("expected head format", str(caught.exception))

    def test_the_binding_requires_a_clean_source_without_the_flag(self):
        dirty = copy.deepcopy(self.base)
        dirty["source"]["dirty"] = True
        self.assertRejected(dirty, "dirty")

    def test_clean_requirement_rejects_a_dirty_report(self):
        dirty = copy.deepcopy(self.base)
        dirty["source"]["dirty"] = True
        self.assertRejected(dirty, "dirty", clean=True)

    def test_missing_check_is_rejected(self):
        report = copy.deepcopy(self.base)
        report["checks"].pop(3)
        self.assertRejected(report, "names must be exactly")

    def test_false_check_is_rejected(self):
        report = copy.deepcopy(self.base)
        report["checks"][5]["ok"] = False
        self.assertRejected(report, "is not ok")

    def test_wrong_check_type_is_rejected(self):
        report = copy.deepcopy(self.base)
        report["checks"][1]["ok"] = 1
        self.assertRejected(report, "is not ok")
        report = copy.deepcopy(self.base)
        report["fixture"]["image_requests"] = "2"
        self.assertRejected(report, "image request accounting")

    def test_extra_check_is_rejected(self):
        report = copy.deepcopy(self.base)
        report["checks"].append({"name": "extra.case", "ok": True, "detail": ""})
        self.assertRejected(report, "names must be exactly")

    def test_extra_top_level_key_is_rejected(self):
        report = copy.deepcopy(self.base)
        report["unexpected"] = True
        self.assertRejected(report, "keys mismatch")

    def test_missing_b_wire_marker_is_rejected(self):
        report = copy.deepcopy(self.base)
        report["rss"]["b_marker"] = ""
        self.assertRejected(report, "B wire marker")

    def test_missing_archive_entry_and_compression_are_rejected(self):
        report = copy.deepcopy(self.base)
        report["archive"]["entries"].remove("unique-3.jpg")
        self.assertRejected(report, "archive entry set")
        report = copy.deepcopy(self.base)
        report["archive"]["compression"] = "deflated"
        self.assertRejected(report, "compression")

    def test_the_archive_entry_set_is_exactly_the_marker_and_four_images(self):
        report = copy.deepcopy(self.base)
        report["archive"]["entries"].remove(".xehdone")
        self.assertRejected(report, "archive entry set")
        report = copy.deepcopy(self.base)
        report["archive"]["entries"].insert(1, "notes.txt")
        self.assertRejected(report, "archive entry set")
        report = copy.deepcopy(self.base)
        report["archive"]["entries"].insert(1, "unique-2.jpg")
        self.assertRejected(report, "archive entry set")
        report = copy.deepcopy(self.base)
        report["archive"]["entries"] = ["unique-1.jpg", ".xehdone"] + ARCHIVE_ENTRIES[2:]
        self.assertRejected(report, "archive entry set")
        report = copy.deepcopy(self.base)
        report["archive"]["entries"].append("unique-5.jpg")
        self.assertRejected(report, "archive entry set")
        self.assertEqual(verifier.ARCHIVE_ENTRIES, ARCHIVE_ENTRIES)

    def test_stale_run_root_and_missing_probe_report_are_rejected(self):
        report = copy.deepcopy(self.base)
        report["run_root"] = str(self.root / "another-run")
        self.assertRejected(report, "stale run root")
        report = copy.deepcopy(self.base)
        report["ui_probe"]["report"] = str(self.root / "missing.json")
        self.assertRejected(report, "probe report is missing")

    def test_missing_case_identity_and_ui_check_set_are_rejected(self):
        report = copy.deepcopy(self.base)
        report["case"] = "tests::native_rss_update::some_other_case"
        self.assertRejected(report, "case identity")
        report = copy.deepcopy(self.base)
        report["ui_probe"]["checks"].pop(0)
        self.assertRejected(report, "names must be exactly")


if __name__ == "__main__":
    unittest.main(verbosity=2)
