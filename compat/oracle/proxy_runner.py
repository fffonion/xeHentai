#!/usr/bin/env python3
"""Task8 proxy oracle: real fixed-source Pool/Core/Glype against synthetic local fixtures.

One case per process. The case file is synthetic; the upstream package is the
fixed 43fc55c tree on PYTHONPATH. Pool clocks are explicit values from the case.
The Glype server is local and its request transcript is normalized only by
replacing its own ephemeral base URL with the literal ``<glype>`` marker.
"""

import json
import os
import socket
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import threading

FORBIDDEN_HOSTS = {"127.0.0.1", "::1", "localhost"}


def install_socket_guard():
    real_connect = socket.socket.connect
    real_create = socket.create_connection

    def connect(self, address):
        host = address[0] if isinstance(address, tuple) and address else address
        if host not in FORBIDDEN_HOSTS:
            raise ConnectionRefusedError("blocked non-local socket %r" % (address,))
        return real_connect(self, address)

    def create_connection(address, *args, **kwargs):
        host = address[0] if isinstance(address, tuple) and address else address
        if host not in FORBIDDEN_HOSTS:
            raise ConnectionRefusedError("blocked non-local socket %r" % (address,))
        return real_create(address, *args, **kwargs)

    socket.socket.connect = connect
    socket.create_connection = create_connection


install_socket_guard()

XEH_CASE = os.environ["XEH_CASE"]
XEH_RESULT = os.environ["XEH_RESULT"]
with open(XEH_CASE) as handle:
    CASE = json.load(handle)

sys.path.insert(0, os.environ["XEH_UPSTREAM"])
import requests
import xeHentai.const as const
import xeHentai.proxy as proxy
import xeHentai.util as util

const.FILEPATH = os.environ.get("XEH_FIXTURE_ROOT", os.getcwd())


def pool_state(pool):
    return {
        "order": list(pool.proxies.keys()),
        "counts": {key: [value[1], value[2]] for key, value in pool.proxies.items()},
        "disabled": dict(pool.disabled),
    }


def selected_address(not_good):
    try:
        cells = dict(zip(not_good.__code__.co_freevars, not_good.__closure__ or ()))
        return cells["addr"].cell_contents
    except Exception:
        return None


def run_pool(case):
    pool = proxy.Pool()
    steps = case["request"].get("steps", [])
    output = []
    real_time = proxy.time.time
    try:
        for step in steps:
            op = step.get("op", "")
            clock = float(step.get("now", 0.0))
            proxy.time.time = lambda clock=clock: clock
            entry = {
                "op": op,
                "ok": True,
                "error": "",
                "address": None,
                "proxy": None,
            }
            try:
                if op == "proxy_add":
                    pool.add_proxy(step["address"])
                elif op == "proxy_select":
                    _, not_good = pool.proxied_request(None)
                    entry["address"] = selected_address(not_good)
                elif op == "proxy_not_good":
                    pool.not_good(step["address"])(
                        step.get("weight", 5), step.get("expire", 0.0)
                    )
                elif op == "proxy_trace":
                    run_trace(pool, step)
                elif op == "proxy_has":
                    pool.has_available_proxies()
                else:
                    entry["ok"] = False
                    entry["error"] = "UnknownOp"
            except Exception as exc:
                entry["ok"] = False
                entry["error"] = type(exc).__name__
            entry["proxy"] = pool_state(pool)
            output.append(entry)
    finally:
        proxy.time.time = real_time
    return {"observable": {"steps": output, "seen": None}}


def exception_for(name, detail):
    from requests import exceptions

    classes = {
        "ConnectTimeout": exceptions.ConnectTimeout,
        "ConnectionError": exceptions.ConnectionError,
        "ProxyError": exceptions.ProxyError,
        "InvalidSchema": exceptions.InvalidSchema,
        "ProxySchemeUnknown": proxy.ProxySchemeUnknown,
    }
    cls = classes.get(name, exceptions.ConnectionError)
    if name == "ConnectionError" and "BadStatusLine" in detail:
        return exceptions.ConnectionError("BadStatusLine: " + detail)
    if name == "ProxyError":
        return exceptions.ProxyError(detail)
    return cls(detail)


def run_trace(pool, step):
    addr = step["address"]
    weight = step.get("weight", 1)
    outcome = step.get("outcome", "success")
    check_present = step.get("check_present", False)
    check_ok = step.get("check_ok", True)
    exceptions = []

    if outcome == "success":
        value = step.get("return", "value")
        check = (lambda item: check_ok) if check_present else None
        decorator = pool.trace_proxy(addr, weight=weight, check_func=check, exceptions=exceptions)
        wrapped = decorator(lambda: value)
        wrapped()
    else:
        detail = step.get("detail", "boom")
        name = step.get("exception_class", "ConnectionError")
        error = exception_for(name, detail)

        @pool.trace_proxy(addr, weight=weight, exceptions=exceptions)
        def wrapped():
            raise error

        try:
            wrapped()
        except Exception:
            pass


def run_config(case):
    import xeHentai.core as core

    const.FILEPATH = os.environ.get("XEH_FIXTURE_ROOT", os.getcwd())
    existing = os.path.join(const.FILEPATH, "existing")
    os.makedirs(existing, exist_ok=True)
    obj = core.xeHentai()
    steps = case["request"].get("steps", [])
    output = []
    for step in steps:
        overlay = step.get("overlay", {})
        op = step.get("op", "update")
        entry = {"op": op, "ok": True, "error": "", "proxy": None}
        try:
            obj.update_config(proxy=overlay.get("proxy", []), dir=existing)
        except Exception as exc:
            entry["ok"] = False
            entry["error"] = type(exc).__name__
        entry["proxy"] = None if obj.proxy is None else pool_state(obj.proxy)
        output.append(entry)
    return {"observable": {"steps": output, "seen": None}}


def run_policy(case):
    request = case["request"]
    config = request.get("config", {})
    urls = request.get("urls", [])
    flags = {
        "proxy_image": bool(config.get("proxy_image", False)),
        "proxy_image_only": bool(config.get("proxy_image_only", False)),
    }
    web = util.get_proxy_policy(flags)
    all_re = util.get_proxy_policy({"proxy_image": True, "proxy_image_only": False})
    image_re = util.get_proxy_policy({"proxy_image": False, "proxy_image_only": True})
    rows = []
    for url in urls:
        rows.append(
            {
                "url": url,
                "webpage": bool(web.match(url)),
                "all": bool(all_re.match(url)),
                "image_only": bool(image_re.match(url)),
            }
        )
    return {"observable": {"urls": rows, "seen": None}}


class ScriptedHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    responses = []
    seen = []
    lock = threading.Lock()

    def log_message(self, *args):
        return

    def do_GET(self):
        length = int(self.headers.get("content-length") or 0)
        body = self.rfile.read(length) if length else b""
        headers = {}
        for key in self.headers.keys():
            lower = key.lower()
            if lower == "connection":
                continue
            headers[lower] = self.headers.get(key)
        with self.lock:
            index = len(self.seen)
            self.seen.append(
                {"method": self.command, "path": self.path, "headers": headers, "body": body.decode("latin-1")}
            )
            response = self.responses[index] if index < len(self.responses) else {
                "status": 500,
                "headers": [],
                "body": b"",
            }
        self.send_response(response["status"])
        for name, value in response["headers"]:
            self.send_header(name, value)
        payload = response["body"]
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)
        self.close_connection = True


def parse_responses(case):
    output = []
    for item in case["glype"]["responses"]:
        headers = [(name, value) for name, value in item.get("headers", [])]
        if "body_text" in item:
            body = item["body_text"].encode("utf-8")
        else:
            body = bytes.fromhex(item["body_hex"])
        output.append({"status": item.get("status", 200), "headers": headers, "body": body})
    return output


def run_glype(case):
    handler_class = type(
        "CaseScriptedHandler",
        (ScriptedHandler,),
        {"responses": parse_responses(case), "seen": [], "lock": threading.Lock()},
    )
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler_class)
    port = server.server_address[1]
    threading.Thread(target=server.serve_forever, daemon=True).start()
    base = "http://127.0.0.1:%d" % port
    address = base + case["glype"]["path"]
    session = requests.Session()
    session.headers = {item["name"]: item["value"] for item in case["glype"]["session_headers"]}
    handle = proxy.glype_proxy(address, lambda *args, **kwargs: (lambda func: func))
    function = handle(session)
    flows = []
    glyph_s = ""
    try:
        for target in case["glype"]["targets"]:
            try:
                response = function("GET", target)
            except Exception as exc:
                flows.append(
                    {"ok": False, "exception": type(exc).__name__, "detail": str(exc)}
                )
                break
            content_type = response.headers.get("content-type") or ""
            text = content_type.startswith("text")
            content = response.content.decode("utf-8") if text else ""
            flows.append(
                {
                    "ok": True,
                    "exception": "",
                    "detail": "",
                    "url": response.url,
                    "status": response.status_code,
                    "content_type": content_type,
                    "content": content,
                    "content_bytes": [] if text else list(response.content),
                    "set_cookie": response.headers.get("set-cookie") or "",
                }
            )
        defaults = handle.__defaults__ or ()
        if defaults and isinstance(defaults[0], dict):
            glyph_s = defaults[0].get("s", "")
    finally:
        server.shutdown()
        server.server_close()
    seen = []
    authority = base[len("http://"):]
    for item in handler_class.seen:
        normalized = json.loads(json.dumps(item))
        for key in ("referer", "host"):
            if key in normalized["headers"]:
                normalized["headers"][key] = (
                    normalized["headers"][key].replace(base, "<glype>").replace(authority, "<glype>")
                )
        seen.append(normalized)
    ok = all(flow["ok"] for flow in flows)
    exception = ""
    detail = ""
    if flows and not flows[-1]["ok"]:
        exception = flows[-1]["exception"]
        detail = flows[-1]["detail"]
    return {
        "observable": {
            "ok": ok,
            "exception": exception,
            "detail": detail,
            "flows": flows,
            "glyph_s": glyph_s,
            "seen": seen,
        }
    }


def main():
    kind = CASE["kind"]
    if kind == "pool":
        result = run_pool(CASE)
    elif kind == "config":
        result = run_config(CASE)
    elif kind == "policy":
        result = run_policy(CASE)
    elif kind == "glype":
        result = run_glype(CASE)
    else:
        raise SystemExit("unknown case kind: %s" % kind)
    with open(XEH_RESULT, "w") as handle:
        json.dump(result, handle)


if __name__ == "__main__":
    main()
