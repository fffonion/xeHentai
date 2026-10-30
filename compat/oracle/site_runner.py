#!/usr/bin/env python3
"""Run frozen xeHentai site filters against synthetic response fixtures only."""

import importlib
import inspect
import json
import os
from pathlib import Path
import socket
import sys
from types import SimpleNamespace

from requests.structures import CaseInsensitiveDict
from requests.exceptions import ConnectionError


def deny_network(*_args, **_kwargs):
    raise RuntimeError("site oracle network access is disabled")


def json_value(value):
    if inspect.isgenerator(value) or inspect.isgeneratorfunction(value):
        return "stream"
    if isinstance(value, bytes):
        return list(value)
    if isinstance(value, tuple):
        return [json_value(item) for item in value]
    if isinstance(value, list):
        return [json_value(item) for item in value]
    if isinstance(value, dict):
        return {str(key): json_value(item) for key, item in value.items()}
    if isinstance(value, str):
        codepoints = [ord(char) for char in value]
        if any(0xD800 <= value <= 0xDFFF for value in codepoints):
            return {"$python_string_codepoints": codepoints}
    return value


def response_for(request):
    return SimpleNamespace(
        status_code=request["status_code"],
        url=request.get("requested_url", ""),
        _real_url=request.get("real_url", ""),
        headers=CaseInsensitiveDict(
            zip(request.get("header_names", []), request.get("header_values", []))
        ),
        text=request.get("text", ""),
        content_length=request.get("content_length"),
    )


def run_case(case, filters, util, aborted_type):
    request = case["request"]
    operation = request["operation"]
    response = response_for(request)
    callbacks = []
    stream = []
    stream_state = {"order": 1, "bytes_read": 0, "current": None}
    succeeded = request.get("suc_returns", [])
    failed = request.get("fail_returns", [])
    callback_counts = {"suc": 0, "fail": 0}
    generator_holder: dict[str, object] = {"value": None}

    def make_callback(kind, returns):
        def callback(value):
            callback_order = len(callbacks)
            index = callback_counts[kind]
            callback_counts[kind] += 1
            returned = returns[index] if index < len(returns) else None
            callback_event = {
                "kind": kind,
                "value": json_value(value),
                "returned": returned,
            }
            if operation == "download":
                callback_event["order"] = callback_order
            callbacks.append(callback_event)
            if kind == "suc" and isinstance(value, tuple) and value and inspect.isgeneratorfunction(value[0]):
                generator_holder["value"] = value[0]()
            return returned
        return callback

    def append_stream(kind, data=None, callback_return=None):
        event = {"kind": kind, "order": stream_state["order"]}
        stream_state["order"] += 1
        if kind == "complete":
            event["bytes_read"] = stream_state["bytes_read"]
        else:
            event["bytes"] = list(data or [])
            event["callback_return"] = callback_return
            event["bytes_read"] = stream_state["bytes_read"]
        stream.append(event)

    def iter_content(_chunk_size):
        for event in request.get("stream_events", []):
            kind = event["kind"]
            if kind == "connection-error":
                append_stream("connection-error", [], event.get("callback_return", ""))
                raise ConnectionError("synthetic iterator failure")
            if kind not in ("chunk", "callback-connection-error"):
                raise ValueError("unsupported synthetic stream event")
            stream_state["current"] = event
            yield bytes(event.get("bytes", []))
            stream_state["current"] = None

    def iter_content_callback(chunk):
        current = stream_state["current"] or {}
        stream_state["bytes_read"] += len(chunk)
        callback_return = current.get("callback_return", "")
        append_stream("callback", chunk, callback_return)
        if current.get("kind") == "callback-connection-error":
            append_stream("connection-error", [], callback_return)
            raise ConnectionError("synthetic callback failure")
        return callback_return

    if operation == "download" and request.get("stream_callback_present", False):
        response.iter_content = iter_content
        response.iter_content_cb = iter_content_callback

    suc = make_callback("suc", succeeded)
    fail = make_callback("fail", failed)
    outer_return = None
    exception = None

    try:
        if operation == "metadata":
            outer_return = filters.flt_metadata(response, suc, fail)
        elif operation == "pages":
            outer_return = filters.flt_pageurl(response, suc, fail)
        elif operation == "mpv":
            outer_return = filters.flt_pageurl_mpv(response, suc, fail)
        elif operation == "login":
            outer_return = filters.login_exhentai(response, suc, fail)
        elif operation == "image":
            outer_return = filters.flt_imgurl_wrapper(request.get("original_image", False))(
                response, suc, fail
            )
        elif operation == "download":
            outer_return = filters.download_file_wrapper(request.get("dirpath", ""))(
                response, suc, fail
            )
        elif operation == "reset_quota":
            outer_return = filters.reset_quota(response, suc, fail)
        elif operation == "support.escape":
            outer_return = util.htmlescape(request.get("text", ""))
        elif operation == "support.time":
            outer_return = util.parse_human_time(request.get("text", ""))
        else:
            raise ValueError("unknown site operation")
    except Exception as error:  # Preserve the source exception class, not its message.
        exception = type(error).__name__

    generator = generator_holder["value"]
    if inspect.isgenerator(generator):
        try:
            for chunk in generator:
                append_stream("yield", chunk, "")
            append_stream("complete")
        except Exception as error:  # The generator runs after its success callback.
            exception = type(error).__name__
            if isinstance(error, aborted_type):
                exception = "DownloadAbortedException"

    debug_path = Path("debug.htm")
    debug = list(debug_path.read_bytes()) if debug_path.is_file() else None
    return {
        "id": case["id"],
        "observable": {
            "callbacks": callbacks,
            "return": json_value(outer_return),
            "exception": exception,
            "debug": debug,
            "stream": stream,
        },
    }


def main():
    fixture_root = Path(os.environ["XEH_FIXTURE_ROOT"]).resolve()
    upstream = Path(os.environ["XEH_UPSTREAM"]).resolve()
    cases_path = Path(os.environ["XEH_CASES"]).resolve()
    result_path = Path(os.environ["XEH_RESULT"]).resolve()
    if Path.cwd().resolve() != fixture_root:
        raise SystemExit("site oracle cwd does not match its owned fixture root")
    if not (upstream / "xeHentai" / "filters.py").is_file():
        raise SystemExit("fixed upstream filters.py is absent")

    socket.socket.connect = deny_network
    socket.create_connection = deny_network
    sys.path.insert(0, str(upstream))
    filters = importlib.import_module("xeHentai.filters")
    util = importlib.import_module("xeHentai.util")
    aborted_type = importlib.import_module("xeHentai.const").DownloadAbortedException

    with cases_path.open(encoding="utf-8") as handle:
        cases = json.load(handle)["cases"]
    results = []
    for index, case in enumerate(cases):
        case_root = fixture_root / ("case-%03d" % index)
        case_root.mkdir()
        os.chdir(case_root)
        results.append(run_case(case, filters, util, aborted_type))
    result_path.write_text(
        json.dumps({"cases": results}, ensure_ascii=True, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print("site-oracle cases=%d" % len(results))


if __name__ == "__main__":
    main()
