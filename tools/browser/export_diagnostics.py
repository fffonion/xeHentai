#!/usr/bin/env python3
"""Diagnostic projection of the browser report for public CI artifacts.

The runner-local original (``browser-report.json``) embeds the synthetic
test credential: ``/jsonrpc`` trace entries carry the full ``post`` string
(``params`` hold ``token:<secret>``) and the full response ``body``, and
request URLs may carry query/userinfo. The original ``request.json`` holds
the secret directly. Neither file may be uploaded.

This stdlib-only exporter reads ONLY the report path given on the command
line and writes an explicitly named public projection. The projection keeps:

* report-level ``ok`` (strict boolean, original value) and ``check_count`` /
  ``trace_count`` (integers, equal to the projected array lengths; when the
  original ``meta`` carries counts they must match or projection fails);
* checks: ordered ``index``, identity ``check`` and strict boolean ``ok``
  only (no ``detail``); every entry and order is preserved, including false;
* trace: ordered ``index``, fixed route class derived from the URL path only
  (``jsonrpc`` / ``img`` / ``zip`` / ``ui`` / ``static`` / ``other``), HTTP
  ``method`` / ``status`` / ``content_type`` / ``content_length``, ``source``
  only when it is the known constant ``download-url``, and for ``jsonrpc``
  entries a parsed ``rpc`` object holding ONLY the numeric ``id`` and a
  ``method`` from the fixed allowlist below -- or an explicit ``parse``
  status enum when the post string is absent/malformed/unexpected.

Everything else (raw URLs/query/userinfo/path components, post strings,
params, bodies, check details, click geometry, freeform meta) is dropped.

Exit codes: 0 projected (a missing report yields a small ``report-absent``
status receipt with no checks/trace, never fabricated data); 1 the present
report is malformed (a fixed safe error object is written, no raw values
dumped); 2 usage error.
"""

import argparse
import json
import re
import sys
from pathlib import Path
from urllib.parse import urlsplit

PROJECTION_NAME = "browser-report-public"

# Fixed allowlist of JSON-RPC methods the existing browser stack emits.
# xeH.listTasks / xeH.getConfig are called directly by
# tools/browser/run_browser_proof.py::rpc; the remaining six are emitted by
# the frozen UI through the same driver run and appear in the accepted
# historical report fixture. Anything else is projected as
# {"parse": "unknown-method"}.
RPC_METHOD_ALLOWLIST = frozenset({
    "xeH.listTasks",
    "xeH.getConfig",
    "xeH.getInfo",
    "xeH.updateConfig",
    "xeH.pauseTask",
    "xeH.resumeTask",
    "xeH.getImage",
    "xeH.delTask",
})

KNOWN_SOURCE = "download-url"

ROUTES = ("jsonrpc", "img", "zip", "ui", "static", "other")

_DIGITS = re.compile(r"[0-9]{1,20}")
_MAX_POST_CHARS = 4096


class ProjectionError(Exception):
    """The present report cannot be faithfully projected (fixed message)."""


def classify_route(url):
    """Fixed route class from the URL path only; never retains the URL."""
    try:
        path = urlsplit(url).path or ""
    except ValueError:
        raise ProjectionError("trace entry url is not parseable")
    if "/jsonrpc" in path:
        return "jsonrpc"
    if "/img/" in path:
        return "img"
    if "/zip/" in path:
        return "zip"
    if "/ui/" in path:
        return "ui"
    if "/static/" in path:
        return "static"
    return "other"


def project_rpc(post):
    """Project one /jsonrpc post string to id+allowlisted method or status."""
    if not isinstance(post, str) or not post:
        return {"parse": "absent"}
    if len(post) > _MAX_POST_CHARS:
        return {"parse": "malformed"}
    try:
        payload = json.loads(post)
    except (json.JSONDecodeError, ValueError):
        return {"parse": "malformed"}
    if not isinstance(payload, dict):
        return {"parse": "non-object"}
    method = payload.get("method")
    if not isinstance(method, str) or method not in RPC_METHOD_ALLOWLIST:
        return {"parse": "unknown-method"}
    ident = payload.get("id")
    if type(ident) is bool:
        return {"parse": "non-numeric-id"}
    if type(ident) is int:
        return {"id": ident, "method": method}
    if isinstance(ident, str) and _DIGITS.fullmatch(ident):
        return {"id": int(ident), "method": method}
    return {"parse": "non-numeric-id"}


def project_check(index, item):
    if not isinstance(item, dict):
        raise ProjectionError(f"check entry {index} is not an object")
    name = item.get("check")
    if not isinstance(name, str) or not name.strip():
        raise ProjectionError(f"check entry {index} has no check identity")
    verdict = item.get("ok")
    if type(verdict) is not bool:
        raise ProjectionError(f"check entry {index} verdict is not a strict boolean")
    return {"index": index, "check": name, "ok": verdict}


def project_trace_entry(index, entry):
    if not isinstance(entry, dict):
        raise ProjectionError(f"trace entry {index} is not an object")
    url = entry.get("url")
    if not isinstance(url, str) or not url:
        raise ProjectionError(f"trace entry {index} has no url string")
    route = classify_route(url)
    method = entry.get("method")
    if method is not None and (not isinstance(method, str) or not method):
        raise ProjectionError(f"trace entry {index} method is not usable")
    status = entry.get("status")
    if type(status) is not int:
        raise ProjectionError(f"trace entry {index} status is not an integer")
    content_type = entry.get("content_type")
    if content_type is not None and not isinstance(content_type, str):
        raise ProjectionError(f"trace entry {index} content_type is not usable")
    content_length = entry.get("content_length")
    if content_length is not None and not isinstance(content_length, str) \
            and type(content_length) is not int:
        raise ProjectionError(f"trace entry {index} content_length is not usable")
    projected = {
        "index": index,
        "route": route,
        "method": method,
        "status": status,
        "content_type": content_type,
        "content_length": content_length,
    }
    if entry.get("source") == KNOWN_SOURCE:
        projected["source"] = KNOWN_SOURCE
    if route == "jsonrpc":
        projected["rpc"] = project_rpc(entry.get("post"))
    return projected


def project_report(report):
    """Project the parsed original report; raise ProjectionError if unusable."""
    if not isinstance(report, dict):
        raise ProjectionError("report is not a JSON object")
    for key in ("ok", "checks", "trace"):
        if key not in report:
            raise ProjectionError(f"report is missing top-level key {key}")
    if type(report["ok"]) is not bool:
        raise ProjectionError("report ok is not a strict boolean")
    checks = report["checks"]
    trace = report["trace"]
    if not isinstance(checks, list):
        raise ProjectionError("report checks is not an array")
    if not isinstance(trace, list):
        raise ProjectionError("report trace is not an array")
    meta = report.get("meta")
    if isinstance(meta, dict):
        for key in ("check_count", "trace_count"):
            if key in meta and type(meta[key]) is not int:
                raise ProjectionError(f"report meta {key} is not an integer")
        if "check_count" in meta and meta["check_count"] != len(checks):
            raise ProjectionError("report meta check_count contradicts checks")
        if "trace_count" in meta and meta["trace_count"] != len(trace):
            raise ProjectionError("report meta trace_count contradicts trace")
    return {
        "projection": PROJECTION_NAME,
        "projection_note": (
            "Diagnostic projection derived from the runner-local original "
            "browser report; it is not the original verifier input. Omitted: "
            "raw URLs/query/userinfo/path components, request post strings, "
            "params, response bodies, check details, click geometry and "
            "freeform meta."
        ),
        "ok": report["ok"],
        "check_count": len(checks),
        "trace_count": len(trace),
        "checks": [project_check(i, item) for i, item in enumerate(checks)],
        "trace": [project_trace_entry(i, entry) for i, entry in enumerate(trace)],
    }


def absent_receipt():
    return {
        "projection": PROJECTION_NAME,
        "projection_note": (
            "No browser report existed when the projection ran (an earlier "
            "gate failed); no checks or trace are fabricated here."
        ),
        "status": "report-absent",
        "check_count": 0,
        "trace_count": 0,
    }


def error_receipt(message):
    return {
        "projection": PROJECTION_NAME,
        "status": "report-malformed",
        "error": message,
    }


def write_json(path, payload):
    path = Path(path)
    if path.parent != Path("."):
        path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=1, sort_keys=True) + "\n",
                    encoding="utf-8")


def main(argv=None):
    parser = argparse.ArgumentParser(description=(__doc__ or "").splitlines()[0])
    parser.add_argument("report", type=Path,
                        help="explicit original browser-report.json path (read-only)")
    parser.add_argument("--out", required=True, type=Path,
                        help="explicit public projection path to write")
    args = parser.parse_args(argv)
    if not args.report.is_file():
        write_json(args.out, absent_receipt())
        print(f"DIAGNOSTICS ABSENT {args.report} -> {args.out}")
        return 0
    try:
        report = json.loads(args.report.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError, ValueError):
        write_json(args.out, error_receipt("report is not parseable JSON"))
        print(f"DIAGNOSTICS REJECTED {args.report}", file=sys.stderr)
        return 1
    try:
        projected = project_report(report)
    except ProjectionError as exc:
        write_json(args.out, error_receipt(str(exc)))
        print(f"DIAGNOSTICS REJECTED {args.report}: {exc}", file=sys.stderr)
        return 1
    before = args.report.read_bytes()
    write_json(args.out, projected)
    if args.report.read_bytes() != before:
        print("DIAGNOSTICS REJECTED original report changed during export",
              file=sys.stderr)
        return 1
    print(f"DIAGNOSTICS OK {len(projected['checks'])} checks, "
          f"{len(projected['trace'])} trace entries -> {args.out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
