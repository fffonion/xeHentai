#!/usr/bin/env python3
"""Independent verifier for the frozen 49-check xeHentai WebUI browser report.

Recovered from the accepted retained browser3c report. It does not import or
trust run_browser_proof.py: it re-checks the exact 49 check identities, their
order, strict boolean verdicts, per-check detail types, the real request trace
and the measured gallery click geometry.

Usage:
  verify_webui_49.py <browser-report.json> [--expected expected-49.json]
                     [--port N]

Exit codes: 0 accepted, 1 rejected, 2 usage/parse error.
"""
import argparse
import json
import sys
from pathlib import Path

JSON_TYPES = {
    "string": str,
    "integer": int,
    "array": list,
    "object": dict,
    "boolean": bool,
}


def json_type_ok(value, type_name):
    if type_name == "integer":
        return type(value) is int
    if type_name == "boolean":
        return type(value) is bool
    return isinstance(value, JSON_TYPES[type_name])


def _is_number(value):
    return type(value) in (int, float)


def verify(report, expected, port=None):
    errors = []

    if not isinstance(report, dict):
        return ["report is not a JSON object"]

    for key in ("ok", "checks", "trace", "click_proof", "meta"):
        if key not in report:
            errors.append(f"missing top-level key {key!r}")
    if errors:
        return errors

    if report["ok"] is not True:
        errors.append(f"report ok is not the boolean true: {report['ok']!r}")

    checks = report["checks"]
    names = []
    if not isinstance(checks, list):
        errors.append("checks is not an array")
        checks = []
    for index, item in enumerate(checks):
        if not isinstance(item, dict):
            errors.append(f"check[{index}] is not an object")
            continue
        for key in ("check", "ok", "detail"):
            if key not in item:
                errors.append(f"check[{index}] missing {key!r}")
        name = item.get("check")
        if not isinstance(name, str) or not name.strip():
            errors.append(f"check[{index}] has an invalid check id: {name!r}")
            continue
        names.append(name)
        if type(item.get("ok")) is not bool:
            errors.append(f"{name}: ok is not a strict boolean: {item.get('ok')!r}")
        elif item["ok"] is not True:
            errors.append(f"{name}: ok is false")
        if "detail" in item:
            want = expected.get("detail_types", {}).get(name)
            if want and not json_type_ok(item["detail"], want):
                errors.append(f"{name}: detail is not {want}: {type(item['detail']).__name__}")

    expected_names = expected["checks"]
    if len(names) != expected["check_count"]:
        errors.append(f"check count {len(names)} != {expected['check_count']}")
    seen = set()
    duplicates = []
    for name in names:
        if name in seen and name not in duplicates:
            duplicates.append(name)
        seen.add(name)
    if duplicates:
        errors.append(f"duplicate check ids: {duplicates}")
    missing = [name for name in expected_names if name not in seen]
    extra = [name for name in names if name not in set(expected_names)]
    if missing:
        errors.append(f"missing check ids: {missing}")
    if extra:
        errors.append(f"unexpected check ids: {extra}")
    if names != expected_names:
        errors.append("check sequence differs from the accepted 49-check order")

    trace_errors = verify_trace(report["trace"], expected["trace"])
    errors.extend(trace_errors)

    click_errors = verify_click(report["click_proof"], checks, expected["click_proof"])
    errors.extend(click_errors)

    meta = report["meta"]
    if not isinstance(meta, dict):
        errors.append("meta is not an object")
    else:
        for key in expected["meta_required_keys"]:
            if key not in meta:
                errors.append(f"meta missing {key!r}")
        if type(meta.get("check_count")) is not int:
            errors.append("meta.check_count is not an integer")
        elif meta["check_count"] != len(checks) or meta["check_count"] != expected["check_count"]:
            errors.append(f"meta.check_count {meta['check_count']!r} inconsistent with checks")
        if type(meta.get("trace_count")) is not int:
            errors.append("meta.trace_count is not an integer")
        elif isinstance(report["trace"], list) and meta["trace_count"] != len(report["trace"]):
            errors.append(f"meta.trace_count {meta['trace_count']} != trace entries {len(report['trace'])}")
        for key in ("viewport", "screen"):
            value = meta.get(key)
            if not isinstance(value, dict) or not all(
                    type(value.get(k)) is int and value[k] > 0 for k in ("width", "height")):
                errors.append(f"meta.{key} is not a positive width/height object")
        base = meta.get("base")
        if not isinstance(base, str) or not base.startswith("http://"):
            errors.append(f"meta.base is not an http URL: {base!r}")
        elif port is not None and not base.endswith(f":{port}"):
            errors.append(f"meta.base {base!r} does not target port {port}")
        if not isinstance(meta.get("executable"), str) or not meta["executable"]:
            errors.append("meta.executable is not a non-empty string")
        if not isinstance(meta.get("generated"), str) or not meta["generated"]:
            errors.append("meta.generated is not a non-empty string")

    return errors


def verify_trace(trace, rules):
    errors = []
    if not isinstance(trace, list):
        return ["trace is not an array"]
    if len(trace) < rules["min_entries"]:
        errors.append(f"trace has {len(trace)} entries, fewer than {rules['min_entries']}")
    for index, entry in enumerate(trace):
        if not isinstance(entry, dict):
            errors.append(f"trace[{index}] is not an object")
            continue
        for key in rules["entry_required_keys"]:
            if key not in entry:
                errors.append(f"trace[{index}] missing {key!r}")
        if not isinstance(entry.get("url"), str) or not entry["url"].startswith("http"):
            errors.append(f"trace[{index}] url is not an http URL")
        if type(entry.get("status")) is not int:
            errors.append(f"trace[{index}] status is not an integer")
        if entry.get("source") != rules.get("method_required_unless_source"):
            if not isinstance(entry.get("method"), str) or not entry["method"]:
                errors.append(f"trace[{index}] method is not a non-empty string")
        if entry.get("content_type") is not None and not isinstance(entry["content_type"], str):
            errors.append(f"trace[{index}] content_type is not a string or null")
        length = entry.get("content_length")
        if length is not None and not isinstance(length, str) and type(length) is not int:
            errors.append(f"trace[{index}] content_length is not a string, integer or null")
    urls = [entry.get("url", "") for entry in trace if isinstance(entry, dict)]
    for path in rules["required_paths"]:
        if not any(path in url for url in urls):
            errors.append(f"trace never requested {path}")
    for need in rules["require_status"]:
        path, status = need["path"], need["status"]
        hit = [entry for entry in trace
               if isinstance(entry, dict) and path in str(entry.get("url", ""))
               and entry.get("status") == status]
        if not hit:
            errors.append(f"trace has no {path} response with status {status}")
    return errors


def verify_click(click_proof, checks, rules):
    errors = []
    if not isinstance(click_proof, dict):
        return ["click_proof is not an object"]
    before = click_proof.get("before")
    after = click_proof.get("after")
    if not isinstance(before, dict):
        errors.append("click_proof.before is not an object")
    else:
        for key in rules["before_required_keys"]:
            if key not in before:
                errors.append(f"click_proof.before missing {key!r}")
        for key in ("screen_width", "screen_height", "inner_width", "inner_height"):
            if not _is_number(before.get(key)) or before.get(key) <= 0:
                errors.append(f"click_proof.before.{key} is not a positive number")
        if not isinstance(before.get("img_src"), str) or not before["img_src"].startswith("http"):
            errors.append("click_proof.before.img_src is not an http URL")
        rect = before.get("img_rect")
        if not isinstance(rect, dict) or not all(_is_number(rect.get(k)) for k in ("x", "y", "width", "height")):
            errors.append("click_proof.before.img_rect is not a geometric rect")
        elif rect["width"] <= 1 or rect["height"] <= 1:
            errors.append("click_proof.before.img_rect is not a real rendered image box")
        if type(before.get("dialog_visible")) is not bool:
            errors.append("click_proof.before.dialog_visible is not a boolean")
        if not isinstance(before.get("element_from_point"), dict):
            errors.append("click_proof.before.element_from_point is not an object")
        xy = before.get("click_xy")
        if not (isinstance(xy, list) and len(xy) == 2 and all(_is_number(v) for v in xy)):
            errors.append("click_proof.before.click_xy is not a two-number point")
        if before.get("click_zone") not in ("next", "prev"):
            errors.append("click_proof.before.click_zone is not next/prev")
    if not isinstance(after, dict):
        errors.append("click_proof.after is not an object")
    else:
        for key in rules["after_required_keys"]:
            if key not in after:
                errors.append(f"click_proof.after missing {key!r}")
        if type(after.get("dialog_visible")) is not bool:
            errors.append("click_proof.after.dialog_visible is not a boolean")
        elif after["dialog_visible"] is not False:
            errors.append("click_proof.after.dialog_visible is not false")

    detail = None
    for item in checks:
        if isinstance(item, dict) and item.get("check") == rules["check"]:
            detail = item.get("detail")
    if not isinstance(detail, dict):
        errors.append(f"{rules['check']}: detail is not the click evidence object")
        return errors
    for key in rules["detail_required_keys"]:
        if key not in detail:
            errors.append(f"{rules['check']}: detail missing {key!r}")
    if isinstance(before, dict) and isinstance(after, dict):
        if detail.get("screen") != before.get("screen_width"):
            errors.append(f"{rules['check']}: detail screen != before.screen_width")
        xy = before.get("click_xy")
        if isinstance(xy, list) and len(xy) == 2 and detail.get("x") != xy[0]:
            errors.append(f"{rules['check']}: detail x != before.click_xy[0]")
        if detail.get("zone") != before.get("click_zone"):
            errors.append(f"{rules['check']}: detail zone != before.click_zone")
        if detail.get("before_visible") is not before.get("dialog_visible"):
            errors.append(f"{rules['check']}: detail before_visible contradicts measured before")
        if detail.get("after_visible") is not after.get("dialog_visible"):
            errors.append(f"{rules['check']}: detail after_visible contradicts measured after")
        if detail.get("before_visible") is not True or detail.get("after_visible") is not False:
            errors.append(f"{rules['check']}: click did not close the reader (before true -> after false)")
    return errors


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("report", type=Path)
    parser.add_argument("--expected", type=Path,
                        default=Path(__file__).with_name("expected-49.json"))
    parser.add_argument("--port", type=int, default=None)
    args = parser.parse_args(argv)
    try:
        report = json.loads(args.report.read_text(encoding="utf-8"))
        expected = json.loads(args.expected.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as exc:
        print(f"WEBUI49 ERROR {exc}")
        return 2

    errors = verify(report, expected, port=args.port)
    if errors:
        print(f"WEBUI49 REJECT {len(errors)} problem(s) in {args.report}")
        for error in errors:
            print(" -", error)
        return 1
    print(f"WEBUI49 ACCEPT {expected['check_count']} checks, "
          f"{len(report['trace'])} trace entries, click evidence consistent: {args.report}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
