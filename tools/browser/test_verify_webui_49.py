#!/usr/bin/env python3
"""Mutation tests for verify_webui_49.py.

The positive control is the accepted retained browser3c report (historical
evidence, not new proof). Every mutation below must be rejected: the verifier
must not pass on count alone. Run with:

  <venv>/bin/python test_verify_webui_49.py -v
"""
import copy
import json
import os
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import verify_webui_49 as verifier  # noqa: E402

RETAINED = Path(os.environ.get(
    "XEH_RETAINED_REPORT",
    str(HERE / "fixtures" / "browser-report-accepted-historical-3c28436.json")))
EXPECTED = json.loads((HERE / "expected-49.json").read_text(encoding="utf-8"))
BASE = json.loads(RETAINED.read_text(encoding="utf-8"))


def index_of(name):
    for i, item in enumerate(BASE["checks"]):
        if item["check"] == name:
            return i
    raise KeyError(name)


class VerifierTests(unittest.TestCase):
    def assertRejected(self, report, needle=None, port=None):
        errors = verifier.verify(report, EXPECTED, port=port)
        self.assertTrue(errors, "mutation was accepted")
        if needle:
            self.assertTrue(any(needle in e for e in errors),
                            f"no error contained {needle!r}: {errors}")

    def test_00_historical_report_accepted(self):
        self.assertEqual(verifier.verify(BASE, EXPECTED, port=8010), [])

    def test_01_missing_check(self):
        report = copy.deepcopy(BASE)
        report["checks"].pop(index_of("search.tag"))
        report["meta"]["check_count"] = len(report["checks"])
        self.assertRejected(report, "missing check ids")

    def test_02_extra_check(self):
        report = copy.deepcopy(BASE)
        report["checks"].append({"check": "extra.invented", "ok": True, "detail": ""})
        self.assertRejected(report, "unexpected check ids")

    def test_03_duplicate_check(self):
        report = copy.deepcopy(BASE)
        report["checks"].insert(6, copy.deepcopy(report["checks"][index_of("tasks.row.alpha")]))
        self.assertRejected(report, "duplicate check ids")

    def test_04_ok_false(self):
        report = copy.deepcopy(BASE)
        report["checks"][index_of("filter.reset")]["ok"] = False
        report["ok"] = True
        self.assertRejected(report, "ok is false")

    def test_05_ok_wrong_type_int(self):
        report = copy.deepcopy(BASE)
        report["checks"][index_of("tasks.rows")]["ok"] = 1
        self.assertRejected(report, "not a strict boolean")

    def test_06_ok_wrong_type_string(self):
        report = copy.deepcopy(BASE)
        report["checks"][index_of("locale.zh-CN.host")]["ok"] = "true"
        self.assertRejected(report, "not a strict boolean")

    def test_07_report_ok_false(self):
        report = copy.deepcopy(BASE)
        report["ok"] = False
        self.assertRejected(report, "report ok is not the boolean true")

    def test_08_missing_detail_key(self):
        report = copy.deepcopy(BASE)
        del report["checks"][index_of("zip.filename")]["detail"]
        self.assertRejected(report, "missing 'detail'")

    def test_09_detail_wrong_type(self):
        report = copy.deepcopy(BASE)
        report["checks"][index_of("tasks.rows")]["detail"] = "3"
        self.assertRejected(report, "detail is not integer")

    def test_10_trace_missing_img(self):
        report = copy.deepcopy(BASE)
        report["trace"] = [e for e in report["trace"] if "/img/" not in e["url"]]
        report["meta"]["trace_count"] = len(report["trace"])
        self.assertRejected(report, "never requested /img/")

    def test_11_trace_missing_403(self):
        report = copy.deepcopy(BASE)
        report["trace"] = [e for e in report["trace"]
                           if not ("/jsonrpc" in e["url"] and e["status"] == 403)]
        report["meta"]["trace_count"] = len(report["trace"])
        self.assertRejected(report, "status 403")

    def test_12_trace_entry_missing_status(self):
        report = copy.deepcopy(BASE)
        del report["trace"][0]["status"]
        self.assertRejected(report, "missing 'status'")

    def test_13_trace_too_short(self):
        report = copy.deepcopy(BASE)
        report["trace"] = report["trace"][:5]
        report["meta"]["trace_count"] = 5
        self.assertRejected(report, "fewer than")

    def test_14_trace_method_missing(self):
        report = copy.deepcopy(BASE)
        del report["trace"][0]["method"]
        self.assertRejected(report, "method is not a non-empty string")

    def test_15_click_dialog_still_open(self):
        report = copy.deepcopy(BASE)
        report["click_proof"]["after"]["dialog_visible"] = True
        self.assertRejected(report, "after.dialog_visible is not false")

    def test_16_click_detail_after_visible(self):
        report = copy.deepcopy(BASE)
        detail = report["checks"][index_of("gallery.click-last-closes")]["detail"]
        detail["after_visible"] = True
        self.assertRejected(report, "after_visible contradicts measured after")

    def test_17_click_detail_screen_mismatch(self):
        report = copy.deepcopy(BASE)
        detail = report["checks"][index_of("gallery.click-last-closes")]["detail"]
        detail["screen"] = 999
        self.assertRejected(report, "screen != before.screen_width")

    def test_18_click_proof_missing_before(self):
        report = copy.deepcopy(BASE)
        del report["click_proof"]["before"]
        self.assertRejected(report, "click_proof.before is not an object")

    def test_19_meta_check_count(self):
        report = copy.deepcopy(BASE)
        report["meta"]["check_count"] = 50
        self.assertRejected(report, "meta.check_count")

    def test_20_meta_trace_count(self):
        report = copy.deepcopy(BASE)
        report["meta"]["trace_count"] = 57
        self.assertRejected(report, "meta.trace_count")

    def test_21_check_order_changed(self):
        report = copy.deepcopy(BASE)
        report["checks"][0], report["checks"][1] = report["checks"][1], report["checks"][0]
        self.assertRejected(report, "sequence differs")

    def test_22_checks_not_array(self):
        report = copy.deepcopy(BASE)
        report["checks"] = {"connect.dialog": True}
        self.assertRejected(report, "checks is not an array")

    def test_23_check_not_object(self):
        report = copy.deepcopy(BASE)
        report["checks"][3] = "connect.connected"
        self.assertRejected(report, "is not an object")

    def test_24_detail_type_array(self):
        report = copy.deepcopy(BASE)
        report["checks"][index_of("config.basic.dir")]["detail"] = "downloads"
        self.assertRejected(report, "detail is not array")

    def test_25_base_port_mismatch(self):
        self.assertRejected(BASE, "does not target port", port=9999)

    def test_26_click_zone_invalid(self):
        report = copy.deepcopy(BASE)
        report["click_proof"]["before"]["click_zone"] = "middle"
        self.assertRejected(report, "click_zone is not next/prev")

    def test_27_report_not_object(self):
        self.assertRejected([], "not a JSON object")

    def test_28_missing_top_level_trace(self):
        report = copy.deepcopy(BASE)
        del report["trace"]
        self.assertRejected(report, "missing top-level key 'trace'")


if __name__ == "__main__":
    unittest.main(verbosity=2)
