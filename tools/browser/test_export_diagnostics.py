#!/usr/bin/env python3
"""Focused tests for tools/browser/export_diagnostics.py.

Every fixture in this file is a synthetic exporter-test value (names are
prefixed ``exporter.synthetic.`` and secrets use the ``exporter-sentinel-``
marker). They exercise the real exporter entry through its actual CLI and
are not application results. Run with:

  python tools/browser/test_export_diagnostics.py -v
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
EXPORTER = HERE / "export_diagnostics.py"
RETAINED = HERE / "fixtures" / "browser-report-accepted-historical-3c28436.json"

SYN_SECRET = "exporter-sentinel-secret-7qZ2"
SYN_TOKEN = "token:" + SYN_SECRET
SYN_USER = "exporter-sentinel-user"
SYN_IMG_CAP = "8c5218ee"
SYN_ZIP_CAP = "e89ffe21"


def temp_base():
    base = os.environ.get("TMPDIR") or os.environ.get("XEH_LEASE_TMP")
    if not base:
        raise unittest.SkipTest("TMPDIR/XEH_LEASE_TMP must confine test output")
    path = Path(base)
    path.mkdir(parents=True, exist_ok=True)
    return path


def synthetic_report():
    """Small synthetic report exercising every projection branch."""
    return {
        "ok": True,
        "checks": [
            {"check": "exporter.synthetic.connect", "ok": True,
             "detail": "detail-text-connect " + SYN_SECRET},
            {"check": "exporter.synthetic.rows", "ok": False,
             "detail": {"count": 3, "leak": SYN_TOKEN}},
            {"check": "exporter.synthetic.config", "ok": True, "detail": [1, 2]},
        ],
        "trace": [
            {"url": "http://127.0.0.1:8010/jsonrpc?w=300&x=1",
             "method": "POST", "status": 200,
             "content_type": "application/json-rpc", "content_length": "120",
             "post": json.dumps({"method": "xeH.listTasks", "id": 7,
                                 "params": [[SYN_TOKEN, "finished"], {}]}),
             "body": '{"result":[{"config":{"dir":"/tmp/' + SYN_SECRET + '"}}]}',
             "unknown-key-trace": {"nested": SYN_SECRET}},
            {"url": f"http://{SYN_USER}:{SYN_SECRET}@127.0.0.1:8010/jsonrpc",
             "method": "POST", "status": 200,
             "content_type": "application/json-rpc", "content_length": "99",
             "post": json.dumps({"method": "xeH.updateConfig", "id": "4242",
                                 "params": [[SYN_TOKEN], {"x": 1}]}),
             "body": "response-body-" + SYN_SECRET},
            {"url": f"http://127.0.0.1:8010/img/{SYN_IMG_CAP}/beta0002/1?w=300&_=0",
             "method": "GET", "status": 200,
             "content_type": "image/jpeg", "content_length": "1450"},
            {"url": f"http://127.0.0.1:8010/zip/{SYN_ZIP_CAP}/beta0002/beta.zip",
             "method": "GET", "status": 200,
             "content_type": "application/zip", "content_length": 1450},
            {"url": "http://127.0.0.1:8010/ui/?q=" + SYN_SECRET,
             "method": "GET", "status": 200,
             "content_type": "text/html", "content_length": "516"},
            {"url": "http://127.0.0.1:8010/static/css/app.css",
             "method": "GET", "status": 200,
             "content_type": "text/css", "content_length": "10"},
            {"url": "http://127.0.0.1:8010/favicon.ico",
             "method": "GET", "status": 404,
             "content_type": None, "content_length": None},
            {"source": "download-url",
             "url": f"http://127.0.0.1:8010/zip/{SYN_ZIP_CAP}/beta0002/beta.zip",
             "status": 200, "content_type": "application/zip",
             "content_length": "1450",
             "browser_download": "beta.zip",
             "method": None},
            {"url": "http://127.0.0.1:8010/jsonrpc",
             "method": "POST", "status": 500,
             "content_type": "application/json-rpc", "content_length": "40",
             "post": '{"method":"xeH.listTasks","id":9,"params":[[TRUNCATED',
             "body": "truncated-" + SYN_SECRET},
            {"url": "http://127.0.0.1:8010/jsonrpc",
             "method": "POST", "status": 200,
             "content_type": "application/json-rpc", "content_length": "40",
             "post": json.dumps({"method": "xeH.invented", "id": 3, "params": []}),
             "body": "{}"},
            {"url": "http://127.0.0.1:8010/jsonrpc",
             "method": "POST", "status": 200,
             "content_type": "application/json-rpc", "content_length": "40",
             "post": json.dumps({"method": "xeH.getInfo", "id": SYN_TOKEN}),
             "body": "{}"},
            {"url": "http://127.0.0.1:8010/jsonrpc",
             "method": "POST", "status": 403,
             "content_type": "application/json-rpc", "content_length": "40",
             "body": "{}"},
        ],
        "click_proof": {"before": {"img_src": "http://" + SYN_SECRET}},
        "meta": {"check_count": 3, "trace_count": 12,
                 "base": "http://127.0.0.1:8010",
                 "secret_note": SYN_TOKEN},
        "unknown-key-top": SYN_SECRET,
    }


FORBIDDEN_LABELS = (
    ("synthetic secret value", SYN_SECRET),
    ("token prefix", "token:"),
    ("query string", "?w=300"),
    ("userinfo marker", SYN_USER),
    ("img capability path", SYN_IMG_CAP),
    ("zip capability path", SYN_ZIP_CAP),
    ("check detail text", "detail-text-"),
    ("unknown trace key", "unknown-key-trace"),
    ("unknown top key", "unknown-key-top"),
    ("raw url scheme", "http://"),
    ("response body key", '"body":'),
    ("request params key", '"params":'),
    ("check detail key", '"detail":'),
    ("click proof key", '"click_proof":'),
    ("secret note key", '"secret_note":'),
)


class ExporterTests(unittest.TestCase):
    def setUp(self):
        self.base = temp_base()
        self.work = Path(tempfile.mkdtemp(prefix="browser-export-",
                                           dir=str(self.base)))

    def tearDown(self):
        for path in sorted(self.work.rglob("*"), reverse=True):
            if path.is_file() or path.is_symlink():
                path.unlink()
            else:
                path.rmdir()
        self.work.rmdir()

    def run_exporter(self, report_path, out_name="public.json"):
        out = self.work / out_name
        proc = subprocess.run(
            [sys.executable, str(EXPORTER), str(report_path),
             "--out", str(out)],
            capture_output=True, text=True, timeout=60)
        return proc, out

    def write_report(self, name, payload, raw=None):
        path = self.work / name
        if raw is None:
            raw = json.dumps(payload).encode("utf-8")
        path.write_bytes(raw)
        return path

    def assert_scrubbed(self, text):
        hits = [label for label, needle in FORBIDDEN_LABELS if needle in text]
        self.assertEqual(hits, [])

    def test_checks_identity_order_and_false_verdicts(self):
        report = self.write_report("report.json", synthetic_report())
        proc, out = self.run_exporter(report)
        self.assertEqual(proc.returncode, 0)
        projected = json.loads(out.read_text(encoding="utf-8"))
        self.assertEqual([c["check"] for c in projected["checks"]],
                         ["exporter.synthetic.connect",
                          "exporter.synthetic.rows",
                          "exporter.synthetic.config"])
        self.assertEqual([c["ok"] for c in projected["checks"]],
                         [True, False, True])
        self.assertEqual([c["index"] for c in projected["checks"]], [0, 1, 2])
        for entry in projected["checks"]:
            self.assertEqual(sorted(entry), ["check", "index", "ok"])
            self.assertIs(type(entry["ok"]), bool)
        self.assertIs(type(projected["ok"]), bool)
        self.assertEqual(projected["check_count"], 3)
        self.assertEqual(projected["trace_count"], 12)

    def test_trace_routes_methods_status_content(self):
        report = self.write_report("report.json", synthetic_report())
        proc, out = self.run_exporter(report)
        self.assertEqual(proc.returncode, 0)
        projected = json.loads(out.read_text(encoding="utf-8"))
        trace = projected["trace"]
        self.assertEqual([e["route"] for e in trace],
                         ["jsonrpc", "jsonrpc", "img", "zip", "ui", "static",
                          "other", "zip", "jsonrpc", "jsonrpc", "jsonrpc",
                          "jsonrpc"])
        self.assertEqual([e["index"] for e in trace], list(range(12)))
        self.assertEqual(trace[2]["method"], "GET")
        self.assertEqual(trace[2]["status"], 200)
        self.assertEqual(trace[2]["content_type"], "image/jpeg")
        self.assertEqual(trace[2]["content_length"], "1450")
        self.assertEqual(trace[3]["content_length"], 1450)
        self.assertIsNone(trace[6]["content_type"])
        self.assertEqual(trace[7]["source"], "download-url")

    def test_rpc_numeric_ids_and_allowlisted_method(self):
        report = self.write_report("report.json", synthetic_report())
        proc, out = self.run_exporter(report)
        self.assertEqual(proc.returncode, 0)
        trace = json.loads(out.read_text(encoding="utf-8"))["trace"]
        self.assertEqual(trace[0]["rpc"], {"id": 7, "method": "xeH.listTasks"})
        self.assertEqual(trace[1]["rpc"], {"id": 4242, "method": "xeH.updateConfig"})
        self.assertNotIn("rpc", trace[2])

    def test_sentinels_absent_from_output(self):
        report = self.write_report("report.json", synthetic_report())
        proc, out = self.run_exporter(report)
        self.assertEqual(proc.returncode, 0)
        self.assert_scrubbed(out.read_text(encoding="utf-8"))

    def test_malformed_and_unknown_posts_marked(self):
        report = self.write_report("report.json", synthetic_report())
        proc, out = self.run_exporter(report)
        self.assertEqual(proc.returncode, 0)
        trace = json.loads(out.read_text(encoding="utf-8"))["trace"]
        self.assertEqual(trace[8]["rpc"], {"parse": "malformed"})
        self.assertEqual(trace[9]["rpc"], {"parse": "unknown-method"})
        self.assertEqual(trace[10]["rpc"], {"parse": "non-numeric-id"})
        self.assertEqual(trace[11]["rpc"], {"parse": "absent"})

    def test_report_absent_yields_status_only(self):
        proc, out = self.run_exporter(self.work / "missing-report.json")
        self.assertEqual(proc.returncode, 0)
        payload = json.loads(out.read_text(encoding="utf-8"))
        self.assertEqual(payload["status"], "report-absent")
        self.assertNotIn("checks", payload)
        self.assertNotIn("trace", payload)
        self.assertEqual(payload["check_count"], 0)
        self.assertEqual(payload["trace_count"], 0)

    def test_malformed_present_fails_with_fixed_error(self):
        marker = "MALFORMED-MARKER-exporter-5k1"
        cases = [
            ("not-json", b"{oops " + marker.encode()),
            ("missing-keys", {"ok": True, marker: 1}),
            ("ok-not-bool", {"ok": 1, "checks": [], "trace": []}),
            ("verdict-not-bool",
             {"ok": True, "checks": [{"check": "c", "ok": "true"}], "trace": []}),
            ("status-not-int",
             {"ok": True, "checks": [],
              "trace": [{"url": "http://x/jsonrpc", "method": "POST",
                         "status": "200", "content_type": None,
                         "content_length": None}]}),
            ("count-mismatch",
             {"ok": True, "checks": [], "trace": [],
              "meta": {"check_count": 9, "trace_count": 0}}),
        ]
        for name, payload in cases:
            with self.subTest(case=name):
                raw = payload if isinstance(payload, bytes) \
                    else json.dumps(payload).encode("utf-8")
                report = self.write_report(f"bad-{name}.json", None, raw=raw)
                before = report.read_bytes()
                proc, out = self.run_exporter(report, out_name=f"{name}.json")
                self.assertEqual(proc.returncode, 1)
                self.assertEqual(report.read_bytes(), before)
                payload_out = json.loads(out.read_text(encoding="utf-8"))
                self.assertEqual(payload_out["status"], "report-malformed")
                self.assertIsInstance(payload_out["error"], str)
                self.assertNotIn(marker, out.read_text(encoding="utf-8"))

    def test_original_bytes_unchanged(self):
        report = self.write_report("report.json", synthetic_report())
        before = report.read_bytes()
        proc, _ = self.run_exporter(report)
        self.assertEqual(proc.returncode, 0)
        self.assertEqual(report.read_bytes(), before)

    def test_historical_fixture_projects_faithfully(self):
        if not RETAINED.is_file():
            raise unittest.SkipTest("retained historical fixture is absent")
        original = json.loads(RETAINED.read_text(encoding="utf-8"))
        report = self.write_report("historical.json", original)
        proc, out = self.run_exporter(report)
        self.assertEqual(proc.returncode, 0)
        projected = json.loads(out.read_text(encoding="utf-8"))
        self.assertEqual([c["check"] for c in projected["checks"]],
                         [c["check"] for c in original["checks"]])
        self.assertEqual([c["ok"] for c in projected["checks"]],
                         [c["ok"] for c in original["checks"]])
        self.assertEqual(len(projected["trace"]), len(original["trace"]))
        for entry in projected["trace"]:
            if entry["route"] == "jsonrpc" and entry["rpc"].get("method"):
                self.assertIs(type(entry["rpc"]["id"]), int)
        text = out.read_text(encoding="utf-8")
        hits = []
        if "s3cret" in text:
            hits.append("synthetic secret value")
        if "token:" in text:
            hits.append("token prefix")
        if "http://" in text:
            hits.append("raw url scheme")
        self.assertEqual(hits, [])

    def test_ci_upload_excludes_raw_request_and_report(self):
        ci = (ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8")
        lines = ci.splitlines()
        anchor = next(i for i, line in enumerate(lines)
                      if "Upload Linux diagnostics" in line)
        block = "\n".join(lines[anchor:anchor + 40])
        self.assertIn("browser-report-public.json", block)
        self.assertNotIn("browser-run/request.json", block)
        self.assertNotIn("browser-run/browser-report.json", block)
        projector = next(i for i, line in enumerate(lines)
                         if "Project browser diagnostics" in line)
        self.assertLess(projector, anchor)
        step_block = "\n".join(lines[projector:projector + 10])
        self.assertIn("export_diagnostics.py", step_block)
        self.assertIn("always()", step_block)

    def extract_project_diagnostics_script(self):
        ci = (ROOT / ".github" / "workflows" / "ci.yml").read_text(
            encoding="utf-8")
        lines = ci.splitlines()
        anchor = next(i for i, line in enumerate(lines)
                      if "Project browser diagnostics" in line)
        run_idx = next(i for i in range(anchor, min(anchor + 10, len(lines)))
                      if lines[i].strip() == "run: |")
        run_indent = len(lines[run_idx]) - len(lines[run_idx].lstrip())
        block_indent = run_indent + 2
        prefix = " " * block_indent
        script_lines = []
        for line in lines[run_idx + 1:]:
            if line.strip() == "":
                script_lines.append("")
                continue
            indent = len(line) - len(line.lstrip())
            if indent < block_indent:
                break
            script_lines.append(line[len(prefix):] if line.startswith(prefix)
                                else line.lstrip())
        script = "\n".join(script_lines).rstrip("\n") + "\n"
        self.assertIn("export_diagnostics.py", script)
        return script

    def run_project_diagnostics_script(self, script, runner_temp):
        script_path = self.work / "workflow-projection.sh"
        script_path.write_text(script, encoding="utf-8")
        env = dict(os.environ)
        env["RUNNER_TEMP"] = str(runner_temp)
        # Reproduce the real runner precondition: the step is ``if: always()``,
        # so it still runs when the "Install Python and browser test
        # dependencies" step was skipped and $RUNNER_TEMP/venv never exists.
        # The interpreter named by XEH_PYTHON is therefore deliberately absent
        # and must stay absent -- the block itself must not need it.
        env["XEH_PYTHON"] = str(runner_temp / "venv" / "bin" / "python")
        self.assertFalse(Path(env["XEH_PYTHON"]).exists(),
                         "the fake venv interpreter must never be created")
        return subprocess.run(
            ["bash", "-euo", "pipefail", str(script_path)],
            capture_output=True, text=True, timeout=60,
            cwd=str(ROOT), env=env)

    def test_project_diagnostics_block_runs_without_install_venv(self):
        """The always-step projects through the runner's PATH python3."""
        python3 = shutil.which("python3")
        self.assertIsNotNone(python3, "runner PATH must provide python3")
        version = subprocess.run([python3, "--version"],
                                 capture_output=True, text=True, timeout=60)
        self.assertEqual(version.returncode, 0, version.stderr)
        self.assertRegex(version.stdout or version.stderr, r"^Python 3\.\d+")
        script = self.extract_project_diagnostics_script()
        runner_temp = self.work / "runner-no-venv"
        (runner_temp / "browser-run").mkdir(parents=True, exist_ok=True)
        self.assertFalse((runner_temp / "venv").exists())
        proc = self.run_project_diagnostics_script(script, runner_temp)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        out = runner_temp / "ci-diagnostics" / "browser-report-public.json"
        payload = json.loads(out.read_text(encoding="utf-8"))
        self.assertEqual(payload["status"], "report-absent")
        self.assertEqual(payload["check_count"], 0)
        self.assertEqual(payload["trace_count"], 0)

    def test_project_browser_diagnostics_workflow_argv_executes(self):
        script = self.extract_project_diagnostics_script()
        with self.subTest(case="valid"):
            runner_temp = self.work / "runner-valid"
            report_path = runner_temp / "browser-run" / "browser-report.json"
            report_path.parent.mkdir(parents=True, exist_ok=True)
            before = json.dumps(synthetic_report()).encode("utf-8")
            report_path.write_bytes(before)
            proc = self.run_project_diagnostics_script(script, runner_temp)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            out = runner_temp / "ci-diagnostics" / "browser-report-public.json"
            projected = json.loads(out.read_text(encoding="utf-8"))
            self.assertEqual([c["check"] for c in projected["checks"]],
                             ["exporter.synthetic.connect",
                              "exporter.synthetic.rows",
                              "exporter.synthetic.config"])
            self.assertEqual(report_path.read_bytes(), before)
            self.assert_scrubbed(out.read_text(encoding="utf-8"))
        with self.subTest(case="absent"):
            runner_temp = self.work / "runner-absent"
            (runner_temp / "browser-run").mkdir(parents=True, exist_ok=True)
            proc = self.run_project_diagnostics_script(script, runner_temp)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            out = runner_temp / "ci-diagnostics" / "browser-report-public.json"
            payload = json.loads(out.read_text(encoding="utf-8"))
            self.assertEqual(payload["status"], "report-absent")
            self.assertNotIn("checks", payload)
            self.assertNotIn("trace", payload)
            self.assertEqual(payload["check_count"], 0)
            self.assertEqual(payload["trace_count"], 0)
        with self.subTest(case="malformed"):
            runner_temp = self.work / "runner-malformed"
            report_path = runner_temp / "browser-run" / "browser-report.json"
            report_path.parent.mkdir(parents=True, exist_ok=True)
            marker = "MALFORMED-MARKER-workflow-9t2"
            before = b"{oops " + marker.encode()
            report_path.write_bytes(before)
            proc = self.run_project_diagnostics_script(script, runner_temp)
            self.assertEqual(proc.returncode, 1, proc.stderr)
            out = runner_temp / "ci-diagnostics" / "browser-report-public.json"
            payload = json.loads(out.read_text(encoding="utf-8"))
            self.assertEqual(payload["status"], "report-malformed")
            self.assertIsInstance(payload["error"], str)
            self.assertNotIn(marker, out.read_text(encoding="utf-8"))
            self.assertEqual(report_path.read_bytes(), before)


if __name__ == "__main__":
    unittest.main()
