# SPDX-License-Identifier: GPL-3.0-or-later
"""Run pinned xeHentai sources and record observed behavior.

Synthetic hosts and cookies are labeled as such. This module does not
contact accounts, proxies, or gallery sites.
"""

import ast
import copy
import json
import os
import socket
import subprocess
import sys
import threading
import time
import traceback
import zipfile

import contract
import process_log
import re

UPSTREAM = "fffonion/xeHentai@43fc55c0c662f195d048becf6276a26ce06b09d9"
TOKEN = "oracle-token"
AUTH = "token:" + TOKEN
SYNTHETIC_URL = "https://e-hentai.org/g/1/aaaaaaaaaa/"
STABLE_DOWNLOAD_DIR = "synthetic-downloads"
CHILD_SEQ = {"n": 0}
BOUND_PORTS = []
HTTP_DATE = re.compile(
    r"^(?:Mon|Tue|Wed|Thu|Fri|Sat|Sun), [0-9]{2} "
    r"(?:Jan|Feb|Mar|Apr|May|Jun|Jul|Aug|Sep|Oct|Nov|Dec) "
    r"[0-9]{4} [0-9]{2}:[0-9]{2}:[0-9]{2} GMT$"
)


def fixture_root():
    return os.environ["XEH_FIXTURE_ROOT"]


def upstream_root():
    return os.environ["XEH_UPSTREAM"]


def out_dir():
    return os.environ["XEH_OUT"]


def source_file(*parts):
    return os.path.join(upstream_root(), *parts)


def assign_names(path):
    tree = ast.parse(open(path, "rb").read())
    names = []
    for node in tree.body:
        if isinstance(node, ast.Assign):
            for target in node.targets:
                if isinstance(target, ast.Name):
                    names.append(target.id)
    return names


def cli_option_spellings(path):
    tree = ast.parse(open(path, "rb").read())
    found = []
    for node in ast.walk(tree):
        if not isinstance(node, ast.Call):
            continue
        func = node.func
        if not isinstance(func, ast.Attribute) or func.attr != "add_argument":
            continue
        for arg in node.args:
            if isinstance(arg, ast.Constant) and isinstance(arg.value, str) and arg.value.startswith("-"):
                found.append(arg.value)
    return found


def mark_executed(cases, option, observed):
    feature_id = "cli.option.%s" % option
    if feature_id not in cases:
        return
    payload = dict(observed)
    payload["ast_declared"] = True
    payload["executed"] = True
    cases[feature_id] = feature(
        feature_id,
        "xeHentai/cli.py",
        {"option": option},
        payload,
        "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "oracle_characterized",
    )


def note_port(port):
    BOUND_PORTS.append(int(port))


def feature(feature_id, source, payload, observed, target, tests, status):
    return {
        "id": feature_id,
        "source": source,
        "input": payload,
        "expected": {
            "measured": status != "unverified",
            "implementation": "pending",
            "observed": observed,
        },
        "target": target,
        "tests": tests,
        "status": status,
    }


def safe(value):
    try:
        json.dumps(value)
        return value
    except TypeError:
        return repr(value)


def run_case(cases, feature_id, source, payload, fn, target):
    try:
        observed = fn()
        status = "oracle_characterized"
    except Exception:
        observed = {"exception": traceback.format_exc()}
        status = "oracle_failed"
    cases[feature_id] = feature(
        feature_id,
        source,
        payload,
        safe(observed),
        target,
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        status,
    )
    return observed if status == "oracle_characterized" else None


def raw_exchange(port, payload, timeout=5):
    sock = socket.create_connection(("127.0.0.1", port), timeout=timeout)
    sock.settimeout(timeout)
    try:
        sock.sendall(payload)
        try:
            sock.shutdown(socket.SHUT_WR)
        except OSError:
            pass
        chunks = []
        while True:
            try:
                data = sock.recv(65536)
            except socket.timeout:
                break
            if not data:
                break
            chunks.append(data)
        return b"".join(chunks), None
    except Exception:
        return b"".join(chunks) if "chunks" in locals() else b"", traceback.format_exc()
    finally:
        sock.close()


def split_http(blob):
    head, sep, body = blob.partition(b"\r\n\r\n")
    if not sep:
        return {"raw": blob.decode("utf-8", "replace")}
    lines = head.decode("utf-8", "replace").split("\r\n")
    headers = {}
    for line in lines[1:]:
        if ":" in line:
            key, value = line.split(":", 1)
            headers[key.strip().lower()] = value.strip()
    return {
        "status_line": lines[0] if lines else "",
        "headers": headers,
        "body": body.decode("utf-8", "replace"),
        "body_bytes": list(body[:80]),
        "extra_newline": body.endswith(b"\n"),
        "content_length_header": headers.get("content-length"),
        "body_length": len(body),
    }


def rpc_post(port, payload):
    body = json.dumps(payload).encode("utf-8")
    request = (
        b"POST /jsonrpc HTTP/1.0\r\n"
        b"Host: 127.0.0.1\r\n"
        b"Content-Type: application/json\r\n"
        b"Content-Length: " + str(len(body)).encode("ascii") + b"\r\n"
        b"Connection: close\r\n\r\n" + body
    )
    blob, error = raw_exchange(port, request)
    parsed = split_http(blob)
    parsed["client_error"] = error
    if parsed.get("body"):
        try:
            parsed["json"] = json.loads(parsed["body"].strip() or "null")
        except ValueError as exc:
            parsed["json_error"] = str(exc)
    return parsed


def start_rpc(xe):
    from xeHentai.rpc import RPCServer
    stop = {"value": False}
    server = RPCServer(
        xe,
        ("127.0.0.1", 0),
        secret=TOKEN,
        open_browser=False,
        logger=xe.logger,
        exit_check=lambda _name: stop["value"],
    )
    server.start()
    deadline = time.time() + 5
    while server.server is None and time.time() < deadline:
        time.sleep(0.05)
    if server.server is None:
        raise RuntimeError("RPC server did not bind")
    note_port(server.server.server_address[1])
    return server, stop, server.server.server_address[1]


def local_server(handler):
    from http.server import HTTPServer
    httpd = HTTPServer(("127.0.0.1", 0), handler)
    note_port(httpd.server_address[1])
    thread = threading.Thread(target=httpd.serve_forever, daemon=True)
    thread.start()
    return httpd, httpd.server_address[1]


class Response(object):
    def __init__(self, status_code=200, text="", headers=None, url="", real_url=""):
        self.status_code = status_code
        self.text = text
        self.headers = headers or {}
        self.url = url
        self._real_url = real_url or url


def catalog_cases(cases, golden):
    config_names = assign_names(source_file("xeHentai", "config.py"))
    import xeHentai.config as template
    values = {}
    for name in config_names:
        value = getattr(template, name)
        values[name] = value
        cases["config.key.%s" % name] = feature(
            "config.key.%s" % name,
            "xeHentai/config.py",
            {"key": name},
            {"default": safe(value)},
            "pending",
            ["cargo run -p xeh-xtask -- oracle capture --suite all"],
            "oracle_characterized",
        )
    golden["config"] = {"keys": values, "count": len(config_names)}

    import xeHentai.const as const
    states = {}
    errors = {}
    for name in assign_names(source_file("xeHentai", "const.py")):
        if name.startswith("TASK_STATE_"):
            states[name] = getattr(const, name)
            cases["task.state.%s" % name.lower().replace("task_state_", "")] = feature(
                "task.state.%s" % name[len("TASK_STATE_"):].lower(),
                "xeHentai/const.py",
                {"name": name},
                {"value": getattr(const, name)},
                "pending",
                ["cargo run -p xeh-xtask -- oracle capture --suite all"],
                "oracle_characterized",
            )
        if name.startswith("ERR_"):
            errors[name] = getattr(const, name)
            cases["error.%s" % name] = feature(
                "error.%s" % name,
                "xeHentai/const.py",
                {"name": name},
                {"value": getattr(const, name)},
                "pending",
                ["cargo run -p xeh-xtask -- oracle capture --suite all"],
                "oracle_characterized",
            )
    golden["states"] = states
    golden["errors"] = errors

    options = cli_option_spellings(source_file("xeHentai", "cli.py"))
    for option in options:
        cases["cli.option.%s" % option] = feature(
            "cli.option.%s" % option,
            "xeHentai/cli.py",
            {"option": option},
            {"ast_declared": True, "executed": False},
            "pending",
            ["cargo run -p xeh-xtask -- oracle capture --suite all"],
            "unverified",
        )
    cases["cli.positional.url"] = feature(
        "cli.positional.url",
        "xeHentai/cli.py",
        {"option": "url"},
        {"nargs": "*"},
        "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "oracle_characterized",
    )
    golden["cli_options"] = options

    methods = [
        "xeH.updateConfig", "xeH.addTask", "xeH.delTask", "xeH.pauseTask",
        "xeH.resumeTask", "xeH.saveSession", "xeH.loadSession", "xeH.loginExhentai",
        "xeH.setCookie", "xeH.getInfo", "xeH.getConfig", "xeH.listTasks", "xeH.getImage",
    ]
    for method in methods:
        cases["rpc.method.%s" % method] = feature(
            "rpc.method.%s" % method,
            "xeHentai/rpc.py",
            {"method": method},
            {"wire_name": method},
            "pending",
            ["cargo run -p xeh-xtask -- oracle capture --suite all"],
            "unverified",
        )
    golden["rpc_methods"] = methods

    i18n_golden = {}
    for locale in ("en_us", "zh_hans", "zh_hant"):
        path = source_file("xeHentai", "i18n", locale + ".py")
        names = [name for name in assign_names(path) if name != "err_msg"]
        module = __import__("xeHentai.i18n." + locale, fromlist=[locale])
        keys = {name: safe(getattr(module, name)) for name in names}
        err = {str(key): safe(value) for key, value in module.err_msg.items()}
        i18n_golden[locale] = {"key_count": len(names), "err_count": len(err), "keys": keys, "err": err}
        for name in names:
            cases["i18n.%s.key.%s" % (locale, name)] = feature(
                "i18n.%s.key.%s" % (locale, name),
                "xeHentai/i18n/%s.py" % locale,
                {"key": name},
                {"value": keys[name]},
                "pending",
                ["cargo run -p xeh-xtask -- oracle capture --suite all"],
                "oracle_characterized",
            )
        for code, text in err.items():
            cases["i18n.%s.err.%s" % (locale, code)] = feature(
                "i18n.%s.err.%s" % (locale, code),
                "xeHentai/i18n/%s.py" % locale,
                {"code": code},
                {"message": text},
                "pending",
                ["cargo run -p xeh-xtask -- oracle capture --suite all"],
                "oracle_characterized",
            )
    golden["i18n"] = {
        locale: {"key_count": item["key_count"], "err_count": item["err_count"]}
        for locale, item in i18n_golden.items()
    }
    golden["i18n_full"] = i18n_golden


def child(mode, argv=None, stdin=None, env=None):
    CHILD_SEQ["n"] += 1
    process_id = "child-%d-%s" % (CHILD_SEQ["n"], mode)
    child_out = os.path.join(out_dir(), "processes", process_id)
    os.makedirs(child_out, exist_ok=True)
    audit = os.path.join(child_out, "audit.json")
    command = [sys.executable, os.path.join(fixture_root(), "loader.py"), "--mode", mode, "--"]
    if argv:
        command.extend(argv)
    merged = process_log.child_env(process_id, audit, child_out)
    if env:
        for key in process_log.RECEIPT_KEYS + process_log.RUNTIME_KEYS:
            if key in env:
                merged[key] = env[key]
    proc = subprocess.Popen(
        command,
        cwd=fixture_root(),
        env=merged,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    out, err = proc.communicate(stdin)
    stdout_path = os.path.join(child_out, "stdout.txt")
    stderr_path = os.path.join(child_out, "stderr.txt")
    process_log.write_required(stdout_path, out)
    process_log.write_required(stderr_path, err)
    process_log.add_child({
        "id": process_id,
        "parent": os.environ.get("XEH_PROCESS_ID", "root"),
        "audit": audit,
        "stdout": stdout_path,
        "stderr": stderr_path,
        "exit": proc.returncode,
    })
    return proc.returncode, out.decode("utf-8", "replace"), err.decode("utf-8", "replace")


def config_and_cli(cases, golden):
    from xeHentai.core import xeHentai
    from xeHentai.const import ERR_NO_ERROR
    xe = xeHentai()
    golden["runtime_config"] = {
        "cfg": {key: safe(value) for key, value in xe.cfg.items()},
        "cookies": dict(xe.cookies),
        "user_agent": xe.headers.get("User-Agent"),
        "version": xe.verstr,
        "filepath": __import__("xeHentai.const", fromlist=["FILEPATH"]).FILEPATH,
    }
    cases["behavior.sec4.config-defaults"] = feature(
        "behavior.sec4.config-defaults",
        "xeHentai/config.py",
        {"constructor": "xeHentai"},
        golden["runtime_config"],
        "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "oracle_characterized",
    )

    def missing_proxy():
        try:
            xe.update_config()
        except KeyError as exc:
            return {"exception": "KeyError", "args": list(exc.args)}
        return {"exception": None}

    observed = run_case(
        cases, "delta.update-config-proxy", "xeHentai/core.py:71",
        {"call": "update_config()"}, missing_proxy, "pending",
    )
    golden["update_config_missing_proxy"] = observed

    def merge_errors():
        xe.cfg["ignored_errors"] = [1006]
        code, _text = xe.update_config(proxy=[], ignored_errors=[1009], dir=STABLE_DOWNLOAD_DIR)
        return {"code": code, "ignored_errors": list(xe.cfg["ignored_errors"]), "no_error": code == ERR_NO_ERROR}

    golden["ignored_errors"] = run_case(
        cases, "behavior.sec4.ignored-errors-merge", "xeHentai/core.py:67",
        {"existing": [1006], "update": [1009]}, merge_errors, "pending",
    )

    config_path = os.path.join(fixture_root(), "config.py")
    with open(config_path, "w") as handle:
        handle.write("jpn_title = False\ndownload_range = [(9, 9)]\ndaemon = True\n")
    code, out, err = child("cli-parse", [])
    defaults = {"exit": code, "stdout": out, "stderr": err}
    code, out, err = child("cli-parse", ["--jpn-title", "False", "--rename-ori", "0"])
    os.remove(config_path)
    golden["cli_bool"] = {"exit": code, "stdout": out, "stderr": err, "user_config_defaults": defaults}
    cases["delta.argparse-bool"] = feature(
        "delta.argparse-bool", "xeHentai/cli.py",
        {"argv": ["--jpn-title", "False", "--rename-ori", "0"], "user_config": {"jpn_title": False}},
        golden["cli_bool"], "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "oracle_characterized" if code == 0 else "oracle_failed",
    )
    if code == 0:
        mark_executed(cases, "--jpn-title", {"argv": ["--jpn-title", "False"], "exit": code})
        mark_executed(cases, "--rename-ori", {"argv": ["--rename-ori", "0"], "exit": code})
    range_code, range_out, range_err = child("cli-parse", ["--download-range", "5-10，15"])
    golden["cli_range"] = {"exit": range_code, "stdout": range_out, "stderr": range_err}
    if range_code == 0:
        mark_executed(cases, "--download-range", {"argv": ["--download-range", "5-10，15"], "exit": range_code})
    cases["delta.download-range-default"] = feature(
        "delta.download-range-default", "xeHentai/cli.py:183",
        {"user_config_download_range": [[9, 9]], "argv": []},
        defaults,
        "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "oracle_characterized",
    )
    code, out, err = child("cli-parse", ["--download-range", "abc"])
    golden["cli_range_bad"] = {"exit": code, "stdout": out, "stderr": err}
    code, out, err = child("cli-help")
    golden["cli_help"] = {"exit": code, "stdout": out, "stderr": err}
    mark_executed(cases, "--help", {"argv": ["--help"], "exit": code})
    code, out, err = child("cli-version")
    golden["cli_version"] = {"exit": code, "stdout": out, "stderr": err}
    mark_executed(cases, "--version", {"argv": ["--version"], "exit": code})
    short_code, short_out, _short_err = child("cli-parse", ["-u", "synthetic-user"])
    long_code, long_out, _long_err = child("cli-parse", ["--username", "synthetic-user"])

    def parsed_user(text):
        line = text.strip().splitlines()[-1] if text.strip() else "{}"
        try:
            return json.loads(line).get("username")
        except ValueError:
            return None

    mark_executed(cases, "-u", {
        "argv": ["-u", "synthetic-user"],
        "exit": short_code,
        "parsed_username": parsed_user(short_out),
    })
    mark_executed(cases, "--username", {
        "argv": ["--username", "synthetic-user"],
        "exit": long_code,
        "parsed_username": parsed_user(long_out),
    })
    cases["behavior.sec4.cli-options"] = feature(
        "behavior.sec4.cli-options", "xeHentai/cli.py",
        {"commands": ["--help", "--version"]},
        {"help": golden["cli_help"], "version": golden["cli_version"], "bad_range": golden["cli_range_bad"]},
        "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "oracle_characterized",
    )
    code, out, err = child(
        "interactive",
        stdin=b"n\nhttp://synthetic.invalid/g/1/aaaaaaaaaa/\n\n\n\n\n\n\n\n",
    )
    golden["interactive"] = {"exit": code, "stdout": out, "stderr": err}
    cases["behavior.sec4.interactive"] = feature(
        "behavior.sec4.interactive", "xeHentai/cli.py:214",
        {"stdin": "n, synthetic URL, empty defaults"},
        golden["interactive"], "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "oracle_characterized" if code == 0 else "oracle_failed",
    )
    env = os.environ.copy()
    env["LANG"] = "zh_CN.UTF-8"
    env["LC_ALL"] = "zh_CN.UTF-8"
    code, out, err = child("locale", env=env)
    golden["locale_zh_cn"] = {"exit": code, "stdout": out, "stderr": err}
    cases["behavior.sec4.i18n"] = feature(
        "behavior.sec4.i18n", "xeHentai/i18n/__init__.py",
        {"LANG": "zh_CN.UTF-8"},
        {"catalog": golden["i18n"], "locale_process": golden["locale_zh_cn"]},
        "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "oracle_characterized",
    )


def session_and_files(cases, golden):
    from xeHentai.core import xeHentai
    from xeHentai.const import TASK_STATE_DOWNLOAD, TASK_STATE_FINISHED
    from xeHentai.task import Task
    from xeHentai.util.logger import Logger
    root = fixture_root()
    session_dir = os.path.join(root, "session")
    os.makedirs(session_dir)
    old = os.getcwd()
    os.chdir(session_dir)
    try:
        xe = xeHentai()
        xe.cfg["save_tasks"] = False
        xe.cfg["dir"] = session_dir
        xe.set_cookie("ipb_member_id=11; ipb_pass_hash=synthetic-hash")
        log = Logger()
        task = Task(SYNTHETIC_URL, xe.cfg, log)
        task.meta = {"total": 3, "title": "Synthetic Title", "finished": 0, "gjname": "", "gnname": "Synthetic Title"}
        task.state = TASK_STATE_DOWNLOAD
        xe._all_tasks[task.guid] = task
        code, extra = xe.save_session()
        text = open("h.json").read()
        parsed = json.loads(text)
        golden["save_tasks_false"] = {
            "code": code, "tasks": parsed.get("tasks"), "cookies": parsed.get("cookies"), "extra": extra,
        }
        cases["behavior.sec4.save-tasks-false"] = feature(
            "behavior.sec4.save-tasks-false", "xeHentai/core.py:390",
            {"save_tasks": False, "cookie": "synthetic"},
            golden["save_tasks_false"], "pending",
            ["cargo run -p xeh-xtask -- oracle capture --suite all"],
            "oracle_characterized",
        )
        task.renamed_map[1] = "a.jpg"
        task.duplicate_map[1] = set([2])
        try:
            dumped = json.dumps(task.to_dict())
            serial = {"ok": True, "text": dumped}
        except TypeError as exc:
            serial = {"ok": False, "exception": "%s: %s" % (type(exc).__name__, exc)}
        golden["map_serialization"] = serial
        cases["delta.rename-key-types"] = feature(
            "delta.rename-key-types", "xeHentai/task.py:434",
            {"renamed_map_key": 1, "duplicate_map_value": "set"},
            serial, "pending",
            ["cargo run -p xeh-xtask -- oracle capture --suite all"],
            "oracle_characterized",
        )
        os.remove("h.json")
        with open("h.json", "w") as handle:
            handle.write("{")
        xe2 = xeHentai()
        golden["corrupt_session"] = {"still": open("h.json").read(), "has_login": xe2.has_login}
        cases["behavior.sec4.corrupt-session"] = feature(
            "behavior.sec4.corrupt-session", "xeHentai/core.py:402",
            {"h.json": "{"},
            golden["corrupt_session"], "pending",
            ["cargo run -p xeh-xtask -- oracle capture --suite all"],
            "oracle_characterized",
        )
        os.remove("h.json")
        from xeHentai.const import FILEPATH
        with open(os.path.join(FILEPATH, ".ehentai.cookie"), "w") as handle:
            handle.write("11,synthetic-hash\n")
        xe3 = xeHentai()
        golden["legacy_cookie"] = {"has_login": xe3.has_login, "cookies": dict(xe3.cookies)}
        cases["behavior.sec4.legacy-cookie"] = feature(
            "behavior.sec4.legacy-cookie", "xeHentai/core.py:428",
            {"cookie_file": "11,synthetic-hash"},
            golden["legacy_cookie"], "pending",
            ["cargo run -p xeh-xtask -- oracle capture --suite all"],
            "oracle_characterized",
        )
        gallery = os.path.join(session_dir, "gallery")
        scan = Task(SYNTHETIC_URL, {"dir": session_dir, "download_range": None, "rename_ori": False}, log)
        scan.meta = {"total": 3, "title": "gallery", "finished": 0}
        scan.state = TASK_STATE_DOWNLOAD
        fpath = scan.get_fpath()
        os.makedirs(fpath)
        for fid, payload in ((1, b"abc"), (2, b""), (3, b"z")):
            with open(os.path.join(fpath, scan.get_fidpad(fid)), "wb") as handle:
                handle.write(payload)
        before = scan.state
        scan.scan_downloaded()
        golden["scan"] = {
            "state_before": before,
            "state_after": scan.state,
            "finished": scan.meta["finished"],
            "done": sorted(scan._flist_done),
            "assigned": scan.state == TASK_STATE_FINISHED,
            "zero_removed": not os.path.exists(os.path.join(fpath, scan.get_fidpad(2))),
        }
        cases["delta.scan-downloaded-comparison"] = feature(
            "delta.scan-downloaded-comparison", "xeHentai/task.py:206",
            {"files": [1, 2, 3], "zero_byte": 2},
            golden["scan"], "pending",
            ["cargo run -p xeh-xtask -- oracle capture --suite all"],
            "oracle_characterized",
        )
        unfinished = scan.get_fid_unfinished()
        golden["unfinished"] = {"ids": unfinished, "total": scan.meta["total"]}
        cases["delta.unfinished-end"] = feature(
            "delta.unfinished-end", "xeHentai/task.py:299",
            {"total": 3, "done": golden["scan"]["done"]},
            golden["unfinished"], "pending",
            ["cargo run -p xeh-xtask -- oracle capture --suite all"],
            "oracle_characterized",
        )
        marker = Task(SYNTHETIC_URL, {"dir": session_dir, "download_range": None, "rename_ori": False}, log)
        marker.meta = {"total": 2, "title": "marked", "finished": 0}
        marker.state = TASK_STATE_DOWNLOAD
        marked_path = marker.get_fpath()
        os.makedirs(marked_path)
        with open(os.path.join(marked_path, ".xehdone"), "w") as handle:
            handle.write("synthetic\n")
        marker.scan_downloaded()
        zipped = Task(SYNTHETIC_URL, {"dir": session_dir, "download_range": None, "rename_ori": False}, log)
        zipped.meta = {"total": 1, "title": "zipped", "finished": 0}
        zipped.state = TASK_STATE_DOWNLOAD
        zip_path = zipped.get_fpath()
        os.makedirs(zip_path)
        with open(zip_path + ".zip", "wb") as handle:
            handle.write(b"")
        zipped.scan_downloaded()
        archive = Task(SYNTHETIC_URL, {"dir": session_dir, "rename_ori": False}, log)
        archive.meta = {"total": 1, "title": "archive", "finished": 0}
        archive.state = TASK_STATE_DOWNLOAD
        archive_path = archive.get_fpath()
        os.makedirs(archive_path)
        with open(os.path.join(archive_path, "1.jpg"), "wb") as handle:
            handle.write(b"abc")
        produced = archive.make_archive(False)
        with zipfile.ZipFile(produced) as zf:
            info = zf.infolist()[0]
            archive_obs = {
                "path": os.path.basename(produced),
                "compress_type": info.compress_type,
                "store": info.compress_type == zipfile.ZIP_STORED,
                "comment": zf.comment.decode("utf-8", "replace"),
                "directory_remains": os.path.isdir(archive_path),
            }
        golden["recovery"] = {
            "xehdone_finished_count": marker.meta["finished"],
            "xehdone_state": marker.state,
            "zip_finished_count": zipped.meta["finished"],
            "archive": archive_obs,
        }
        cases["delta.xehdone-zip-recovery"] = feature(
            "delta.xehdone-zip-recovery", "xeHentai/task.py:176",
            {"xehdone": True, "zip_exists": True, "make_archive_remove": False},
            golden["recovery"], "pending",
            ["cargo run -p xeh-xtask -- oracle capture --suite all"],
            "oracle_characterized",
        )
        renamed = Task(SYNTHETIC_URL, {"dir": session_dir, "rename_ori": True}, log)
        renamed.meta = {"total": 1, "title": "rename", "finished": 0}
        renamed.renamed_map[1] = "already.jpg"
        rename_path = renamed.get_fpath()
        os.makedirs(rename_path)
        with open(os.path.join(rename_path, renamed.get_fidpad(1)), "wb") as handle:
            handle.write(b"abc")
        with open(os.path.join(rename_path, "already.jpg"), "wb") as handle:
            handle.write(b"old")
        try:
            renamed.rename_fname()
            rename_obs = {"exception": None, "names": sorted(os.listdir(rename_path))}
        except Exception as exc:
            rename_obs = {"exception": "%s: %s" % (type(exc).__name__, exc), "trace": traceback.format_exc()}
        golden["rename"] = rename_obs
        cases["behavior.sec4.naming"] = feature(
            "behavior.sec4.naming", "xeHentai/task.py:329",
            {"conflict": "already.jpg"},
            rename_obs, "pending",
            ["cargo run -p xeh-xtask -- oracle capture --suite all"],
            "oracle_characterized" if rename_obs.get("exception") is None else "oracle_failed",
        )
    finally:
        os.chdir(old)


def filters_and_proxy(cases, golden):
    from xeHentai import filters
    from xeHentai.proxy import Pool
    html = (
        '<h1 id="gj">合成</h1><h1 id="gn">Synthetic EN</h1>'
        'Length:</td><td class="gdt2">3 pages</td>'
        'Showing 1 - 20 of 3 images '
        "toggle_tagmenu('artist:synthetic'"
    )
    captured = {}

    def suc(meta):
        captured["meta"] = meta

    def fail(code):
        captured["fail"] = code

    try:
        filters.flt_metadata(Response(text=html), suc, fail)
        golden["metadata"] = {key: safe(value) for key, value in captured.items()}
        meta_status = "oracle_characterized" if "meta" in captured else "oracle_failed"
    except Exception:
        golden["metadata"] = {"exception": traceback.format_exc()}
        meta_status = "oracle_failed"
    cases["behavior.sec4.metadata-filters"] = feature(
        "behavior.sec4.metadata-filters", "xeHentai/filters.py:34",
        {"html": "synthetic gallery page"},
        golden["metadata"], "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "oracle_characterized" if "meta" in golden.get("metadata", {}) else "oracle_failed",
    )
    page_fail = {}
    filters.flt_pageurl(Response(text="synthetic-no-links"), lambda _v: None, lambda code: page_fail.setdefault("code", code))
    golden["pageurl_fail"] = {"code": page_fail.get("code"), "debug_written": os.path.exists("debug.htm")}
    removed = {}
    filters.flt_metadata(Response(status_code=404, text="missing"), lambda _v: None, lambda code: removed.setdefault("code", code))
    ban = {}
    filters.flt_metadata(
        Response(text="Your IP address has been temporarily banned\nThe ban expires in 1 hour"),
        lambda _v: None,
        lambda code: ban.setdefault("code", code),
    )
    quota = filters.reset_quota(Response(), lambda _v: None, lambda _v: None)
    golden["filters_fail"] = {"removed": removed, "ban_return_ignored": ban, "reset_quota": quota, "pageurl": golden["pageurl_fail"]}
    cases["behavior.sec4.error-pages"] = feature(
        "behavior.sec4.error-pages", "xeHentai/filters.py",
        {"cases": ["404", "ban text", "no page urls"]},
        golden["filters_fail"], "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "oracle_characterized",
    )
    cases["behavior.sec4.reset-quota"] = feature(
        "behavior.sec4.reset-quota", "xeHentai/filters.py",
        {"call": "reset_quota"},
        {"return": quota}, "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "oracle_characterized",
    )
    pool = Pool()
    empty = pool.has_available_proxies()
    pool.add_proxy("http://127.0.0.1:9/")
    one = pool.has_available_proxies()
    golden["proxy_flags"] = {"empty_pool": empty, "one_proxy": one}
    cases["delta.proxy-availability"] = feature(
        "delta.proxy-availability", "xeHentai/proxy.py:46",
        {"calls": ["empty", "one http proxy"]},
        golden["proxy_flags"], "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "oracle_characterized",
    )
    seen = {"requests": []}

    class ProxyHandler(socketserver_handler()):
        def do_GET(self):
            seen["requests"].append({"path": self.path, "headers": dict(self.headers)})
            body = b"synthetic-proxy-ok"
            self.send_response(200)
            self.send_header("Content-Type", "text/plain")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            return

    httpd, port = local_server(ProxyHandler)
    try:
        import requests
        live = Pool()
        live.add_proxy("http://127.0.0.1:%d/" % port)
        handle, _bad = live.proxied_request(requests.Session())
        response = handle("GET", "http://synthetic.invalid/page", timeout=3)
        golden["http_proxy"] = {"status": response.status_code, "body": response.text, "seen": seen["requests"]}
    finally:
        httpd.shutdown()
    http_status = "oracle_characterized" if golden.get("http_proxy", {}).get("status") == 200 else "oracle_failed"
    cases["behavior.sec4.proxy-schemes.http"] = feature(
        "behavior.sec4.proxy-schemes.http", "xeHentai/proxy.py:114",
        {"proxy": "local synthetic HTTP proxy"},
        golden.get("http_proxy"), "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        http_status,
    )
    cases["behavior.sec4.proxy-schemes.https"] = feature(
        "behavior.sec4.proxy-schemes.https", "xeHentai/proxy.py:114",
        {"scheme": "https"},
        {"executed": False}, "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "unverified",
    )
    cases["behavior.sec4.proxy-schemes.socks"] = feature(
        "behavior.sec4.proxy-schemes.socks", "xeHentai/proxy.py:114",
        {"scheme": "socks"},
        {"executed": False}, "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "unverified",
    )


def socketserver_handler():
    from http.server import BaseHTTPRequestHandler
    return BaseHTTPRequestHandler


def glype_case(cases, golden):
    from xeHentai.proxy import Pool
    import requests
    state = {"n": 0, "seen": []}

    class Handler(socketserver_handler()):
        def do_GET(self):
            state["seen"].append({"path": self.path, "referer": self.headers.get("Referer"), "cookie": self.headers.get("Cookie")})
            state["n"] += 1
            if "process.php" in self.path:
                self.send_response(302)
                self.send_header("Set-Cookie", "s=synthetic-session")
                self.send_header("Content-Length", "0")
                self.end_headers()
                return
            if state["n"] == 1:
                body = b'<input type="hidden" name="action" value="sslagree">'
            else:
                body = b'see /browse.php?u=http%3A%2F%2Fsynthetic.invalid%2Fpage&b=4 end'
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *_args):
            return

    httpd, port = local_server(Handler)
    try:
        pool = Pool()
        pool.add_proxy("http://127.0.0.1:%d/browse.php?u=placeholder&b=4" % port)
        session = requests.Session()
        session.headers["Cookie"] = "ipb_member_id=11"
        handle, _bad = pool.proxied_request(session)
        response = handle("GET", "http://synthetic.invalid/page", timeout=3)
        golden["glype"] = {
            "status": response.status_code,
            "text": response.text,
            "seen": state["seen"],
        }
    except Exception:
        golden["glype"] = {"exception": traceback.format_exc(), "seen": state["seen"]}
    finally:
        httpd.shutdown()
    cases["behavior.sec4.proxy-pool"] = feature(
        "behavior.sec4.proxy-pool", "xeHentai/proxy.py:127",
        {"endpoint": "local synthetic glype"},
        golden["glype"], "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        "oracle_characterized" if "exception" not in golden["glype"] else "oracle_failed",
    )


def rpc_cases(cases, golden, logs):
    from xeHentai.core import xeHentai
    from xeHentai.rpc import hash_link
    from xeHentai.task import Task
    from xeHentai.const import TASK_STATE_FINISHED
    xe = xeHentai()
    os.makedirs(STABLE_DOWNLOAD_DIR, exist_ok=True)
    xe.cfg["dir"] = STABLE_DOWNLOAD_DIR
    task = Task(SYNTHETIC_URL, xe.cfg, xe.logger)
    task.guid = "g1"
    task.meta = {"total": 3, "title": "Synthetic", "finished": 0, "gjname": "", "gnname": "Synthetic"}
    task.state = TASK_STATE_FINISHED
    folder = task.get_fpath()
    os.makedirs(folder)
    with open(os.path.join(folder, task.get_fidpad(1)), "wb") as handle:
        handle.write(b"abcde")
    xe._all_tasks[task.guid] = task
    server, stop, port = start_rpc(xe)
    try:
        info = rpc_post(port, {"jsonrpc": "2.0", "id": "info", "method": "xeH.getInfo", "params": [[AUTH], {}]})
        config = rpc_post(port, {"jsonrpc": "2.0", "id": "cfg", "method": "xeH.getConfig", "params": [[AUTH], {}]})
        zero = rpc_post(port, {"jsonrpc": "2.0", "id": 0, "method": "xeH.getInfo", "params": [[AUTH], {}]})
        embedded = rpc_post(port, {"jsonrpc": "2.0", "id": "tok", "method": "xeH.getInfo", "params": [["xxxtoken:%sTAIL" % TOKEN], {}]})
        missing = rpc_post(port, {"jsonrpc": "2.0", "id": "no", "method": "xeH.getInfo", "params": [[], {}]})
        bare = rpc_post(port, {"jsonrpc": "2.0", "id": "bare", "method": "xeH.getInfo", "params": [[TOKEN], {}]})
        bad = rpc_post(port, {"jsonrpc": "2.0", "id": "bad", "method": "xeH.noSuch", "params": [[AUTH], {}]})
        private = rpc_post(port, {"jsonrpc": "2.0", "id": "priv", "method": "xeH._get_image_path", "params": [[AUTH, "g1", "1"], {}]})
        add_bad = rpc_post(port, {"jsonrpc": "2.0", "id": "add", "method": "xeH.addTask", "params": [[AUTH, "http://synthetic.invalid/nope"], {}]})
        add_ok = rpc_post(port, {"jsonrpc": "2.0", "id": "addok", "method": "xeH.addTask", "params": [[AUTH, SYNTHETIC_URL], {}]})
        add_ex = rpc_post(port, {"jsonrpc": "2.0", "id": "ex", "method": "xeH.addTask", "params": [[AUTH, "https://exhentai.org/g/1/aaaaaaaaaa/"], {}]})
        delete = rpc_post(port, {"jsonrpc": "2.0", "id": "del", "method": "xeH.delTask", "params": [[AUTH, "missing"], {}]})
        pause = rpc_post(port, {"jsonrpc": "2.0", "id": "pause", "method": "xeH.pauseTask", "params": [[AUTH, "missing"], {}]})
        resume = rpc_post(port, {"jsonrpc": "2.0", "id": "resume", "method": "xeH.resumeTask", "params": [[AUTH, "missing"], {}]})
        listed = rpc_post(port, {"jsonrpc": "2.0", "id": "list", "method": "xeH.listTasks", "params": [[AUTH], {}]})
        level = rpc_post(port, {"jsonrpc": "2.0", "id": "level", "method": "xeH.listTasks", "params": [[AUTH, "not-a-level"], {}]})
        images = rpc_post(port, {"jsonrpc": "2.0", "id": "img", "method": "xeH.getImage", "params": [[AUTH, "g1", "1,3"], {}]})
        images_all = rpc_post(port, {"jsonrpc": "2.0", "id": "imgall", "method": "xeH.getImage", "params": [[AUTH, "g1"], {}]})
        update = rpc_post(port, {"jsonrpc": "2.0", "id": "upd", "method": "xeH.updateConfig", "params": [[AUTH], {"proxy": []}]})
        update_full = rpc_post(port, {"jsonrpc": "2.0", "id": "updf", "method": "xeH.updateConfig", "params": [[AUTH], {"proxy": [], "dir": STABLE_DOWNLOAD_DIR}]})
        cookie = rpc_post(port, {"jsonrpc": "2.0", "id": "coo", "method": "xeH.setCookie", "params": [[AUTH, "ipb_member_id=11; ipb_pass_hash=synthetic-hash"], {}]})
        saved = rpc_post(port, {"jsonrpc": "2.0", "id": "save", "method": "xeH.saveSession", "params": [[AUTH], {}]})
        loaded = rpc_post(port, {"jsonrpc": "2.0", "id": "load", "method": "xeH.loadSession", "params": [[AUTH], {}]})
        login = rpc_post(port, {"jsonrpc": "2.0", "id": "login", "method": "xeH.loginExhentai", "params": [[AUTH, "synthetic-user", "synthetic-pass"], {}]})
        malformed_body = b"{"
        malformed = raw_exchange(port, b"POST /jsonrpc HTTP/1.0\r\nHost: 127.0.0.1\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{")
        options = raw_exchange(port, b"OPTIONS /jsonrpc HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        nope = raw_exchange(port, b"GET /nope HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        get_rpc = raw_exchange(port, b"GET /jsonrpc HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        sign = hash_link(TOKEN, "g1/1")
        image = raw_exchange(port, ("GET /img/%s/g1/1/1.jpg HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n" % sign).encode("ascii"))
        bad_sign = raw_exchange(port, b"GET /img/deadbeef/g1/1/1.jpg HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        zip_sign = hash_link(TOKEN, "g1")
        archive = raw_exchange(port, ("GET /zip/%s/g1/Synthetic.zip HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n" % zip_sign).encode("ascii"))
        static = raw_exchange(port, b"GET /ui/ HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        golden["rpc"] = {
            "getInfo": info, "getConfig": config, "id0": zero, "embedded_token": embedded,
            "missing_token": missing, "bare_token": bare, "unknown_method": bad, "private_method": private,
            "add_bad": add_bad, "add_ok": add_ok, "add_exhentai": add_ex, "delete": delete,
            "pause": pause, "resume": resume, "list": listed, "bad_level": level,
            "get_image_range": images, "get_image_all": images_all,
            "update_proxy_only": update, "update_proxy_dir": update_full,
            "set_cookie": cookie, "save": saved, "load": loaded, "login_blocked": login,
            "malformed": split_http(malformed[0]), "malformed_error": malformed[1],
            "options": split_http(options[0]), "unmatched": split_http(nope[0]),
            "get_jsonrpc": split_http(get_rpc[0]), "get_jsonrpc_error": get_rpc[1],
            "image": {"status": split_http(image[0]).get("status_line"), "body_length": split_http(image[0]).get("body_length"), "error": image[1]},
            "bad_sign": split_http(bad_sign[0]).get("status_line"),
            "zip": {"status": split_http(archive[0]).get("status_line"), "body_length": split_http(archive[0]).get("body_length"), "error": archive[1]},
            "static": split_http(static[0]),
            "zip_created": os.path.exists(folder + ".zip"),
        }
    finally:
        stop["value"] = True
        try:
            raw_exchange(port, b"GET /nope HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n", timeout=1)
        except Exception:
            pass
        server.join(timeout=2)
    for method in (
        "xeH.getInfo", "xeH.getConfig", "xeH.addTask", "xeH.delTask", "xeH.pauseTask",
        "xeH.resumeTask", "xeH.listTasks", "xeH.getImage", "xeH.updateConfig",
        "xeH.setCookie", "xeH.saveSession", "xeH.loadSession", "xeH.loginExhentai",
    ):
        key = {
            "xeH.getInfo": "getInfo", "xeH.getConfig": "getConfig", "xeH.addTask": "add_bad",
            "xeH.delTask": "delete", "xeH.pauseTask": "pause", "xeH.resumeTask": "resume",
            "xeH.listTasks": "list", "xeH.getImage": "get_image_range", "xeH.updateConfig": "update_proxy_dir",
            "xeH.setCookie": "set_cookie", "xeH.saveSession": "save", "xeH.loadSession": "load",
            "xeH.loginExhentai": "login_blocked",
        }[method]
        observed = golden["rpc"][key]
        status = "oracle_characterized" if observed.get("status_line") else "oracle_failed"
        if method == "xeH.loginExhentai":
            observed = {
                "http": golden["rpc"][key],
                "note": "oracle blocked the non-local socket; original login callback was not observed",
            }
            status = "oracle_failed"
        cases["rpc.method.%s" % method] = feature(
            "rpc.method.%s" % method, "xeHentai/rpc.py",
            {"method": method, "token": "synthetic"},
            observed, "pending",
            ["cargo run -p xeh-xtask -- oracle capture --suite all"],
            status,
        )
    cases["delta.token-placement"] = feature(
        "delta.token-placement", "xeHentai/rpc.py:363",
        {"placement": "params[0][0] must contain token:<secret>"},
        {
            "prefixed": info.get("status_line"),
            "embedded": embedded.get("status_line"),
            "bare": bare.get("status_line"),
            "missing": missing.get("status_line"),
        },
        "pending", ["cargo run -p xeh-xtask -- oracle capture --suite all"], "oracle_characterized",
    )
    cases["delta.falsy-id"] = feature(
        "delta.falsy-id", "xeHentai/rpc.py:109",
        {"id": 0}, golden["rpc"]["id0"], "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"], "oracle_characterized",
    )
    cases["delta.token-regex-log"] = feature(
        "delta.token-regex-log", "xeHentai/rpc.py:61",
        {"token": "xxxtoken:<synthetic>TAIL", "log": "RPC started URL"},
        {"embedded": golden["rpc"]["embedded_token"], "log_contains_token_marker": ",token=" in logs.getvalue()},
        "pending", ["cargo run -p xeh-xtask -- oracle capture --suite all"], "oracle_characterized",
    )
    cases["delta.get-image-range"] = feature(
        "delta.get-image-range", "xeHentai/rpc.py:488",
        {"request_range": "1,3"}, golden["rpc"]["get_image_range"], "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"], "oracle_characterized",
    )
    cases["delta.get-extra-newline"] = feature(
        "delta.get-extra-newline", "xeHentai/rpc.py:319",
        {"requests": ["OPTIONS", "GET /nope", "GET /jsonrpc"]},
        {"options": golden["rpc"]["options"], "unmatched": golden["rpc"]["unmatched"], "get_jsonrpc": golden["rpc"]["get_jsonrpc"]},
        "pending", ["cargo run -p xeh-xtask -- oracle capture --suite all"], "oracle_characterized",
    )
    cases["delta.malformed-json"] = feature(
        "delta.malformed-json", "xeHentai/rpc.py:335",
        {"body": "{"}, {"parsed": golden["rpc"]["malformed"], "client_error": golden["rpc"]["malformed_error"]},
        "pending", ["cargo run -p xeh-xtask -- oracle capture --suite all"], "oracle_characterized",
    )
    cases["delta.content-length"] = feature(
        "delta.content-length", "xeHentai/rpc.py:391",
        {"malformed": "{"}, golden["rpc"]["malformed"], "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"], "oracle_characterized",
    )
    cases["behavior.sec4.gallery-routes"] = feature(
        "behavior.sec4.gallery-routes", "xeHentai/rpc.py",
        {"routes": ["/img", "/zip", "/ui/"]},
        {"image": golden["rpc"]["image"], "zip": golden["rpc"]["zip"], "static": golden["rpc"]["static"], "zip_created": golden["rpc"]["zip_created"]},
        "pending", ["cargo run -p xeh-xtask -- oracle capture --suite all"], "oracle_characterized",
    )
    cases["behavior.sec4.hash-link"] = feature(
        "behavior.sec4.hash-link", "xeHentai/rpc.py:79",
        {"uri": "g1/1"}, {"sign_length": len(sign), "bad_sign_status": golden["rpc"]["bad_sign"]},
        "pending", ["cargo run -p xeh-xtask -- oracle capture --suite all"], "oracle_characterized",
    )
    cases["behavior.sec4.rpc-wire"] = feature(
        "behavior.sec4.rpc-wire", "xeHentai/rpc.py",
        {"samples": ["id 0", "malformed", "options"]},
        {"id0": golden["rpc"]["id0"], "options": golden["rpc"]["options"]},
        "pending", ["cargo run -p xeh-xtask -- oracle capture --suite all"], "oracle_characterized",
    )
    cases["behavior.sec4.task-commands"] = feature(
        "behavior.sec4.task-commands", "xeHentai/core.py",
        {"calls": ["add", "del", "pause", "resume"]},
        {"add_bad": add_bad, "add_ok": add_ok, "add_exhentai": add_ex, "delete": delete, "pause": pause, "resume": resume},
        "pending", ["cargo run -p xeh-xtask -- oracle capture --suite all"], "oracle_characterized",
    )


def _brief(value, depth=0):
    if isinstance(value, dict):
        brief = {}
        for key in ("status_line", "exception", "exit", "ok", "error", "client_error"):
            if key in value and value[key] not in (None, ""):
                brief[key] = value[key]
        parsed = value.get("json")
        if isinstance(parsed, dict):
            brief["json"] = {"id": parsed.get("id"), "error": parsed.get("error")}
            result = parsed.get("result")
            if isinstance(result, list) and len(result) <= 6:
                brief["json"]["result"] = result
            elif "result" in parsed:
                brief["json"]["result_type"] = type(result).__name__
        stdout = value.get("stdout")
        if isinstance(stdout, str) and stdout.strip():
            brief["stdout"] = stdout.strip()[:300]
        if brief:
            return brief
        if depth >= 2:
            return {"keys": list(value)[:8]}
        compact = {}
        for key in list(value)[:8]:
            compact[key] = _brief(value[key], depth + 1)
        return compact
    if isinstance(value, list):
        return [_brief(item, depth + 1) for item in value[:6]]
    return value


def deltas_markdown(cases):
    lines = [
        "# 兼容差异",
        "",
        "本文记录固定上游 `fffonion/xeHentai` `43fc55c0c662f195d048becf6276a26ce06b09d9` 在隔离夹具中的实测结果。",
        "夹具数据均为合成值。应用重写尚未实现，下列条目不构成迁移合同。",
        "",
        "项目状态：兼容迁移中。",
        "",
    ]
    for feature_id in contract.DELTAS:
        item = cases.get(feature_id)
        lines.append("## %s" % feature_id)
        lines.append("")
        if not item or item["status"] == "unverified":
            lines.append("- 状态：unverified")
            lines.append("- 触发与输出：本次 oracle 没有执行该路径，不记录推测结果。")
            lines.append("- 拟定合同：待批准")
            lines.append("")
            continue
        lines.append("- 来源：`%s`" % item["source"])
        lines.append("- 状态：%s" % item["status"])
        lines.append("- 输入：`%s`" % json.dumps(item["input"], ensure_ascii=False)[:500])
        observed = json.dumps(_brief(item["expected"].get("observed")), ensure_ascii=False)
        lines.append("- 实测输出：`%s`" % observed)
        lines.append("- 原始记录：`compat/golden/` 中对应文件。")
        lines.append("- 拟定合同：待批准。应用侧不把该输出当作已实现行为。")
        lines.append("")
    return "\n".join(lines) + "\n"


def apply_derived_counts(cases):
    methods = [cases[key] for key in sorted(cases) if key.startswith("rpc.method.")]
    characterized = sum(1 for item in methods if item["status"] == "oracle_characterized")
    failed = sum(1 for item in methods if item["status"] == "oracle_failed")
    unverified = sum(1 for item in methods if item["status"] == "unverified")
    if methods and characterized == len(methods):
        rpc_status = "oracle_characterized"
    elif characterized == 0 and failed == 0:
        rpc_status = "unverified"
    else:
        rpc_status = "partial"
    cases["behavior.sec4.rpc-methods"] = feature(
        "behavior.sec4.rpc-methods",
        "xeHentai/rpc.py",
        {"methods_total": len(methods)},
        {
            "methods_total": len(methods),
            "methods_characterized": characterized,
            "methods_failed": failed,
            "methods_unverified": unverified,
        },
        "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        rpc_status,
    )
    schemes = {}
    for scheme in ("http", "https", "socks"):
        key = "behavior.sec4.proxy-schemes.%s" % scheme
        if key not in cases:
            cases[key] = feature(
                key,
                "xeHentai/proxy.py:114",
                {"scheme": scheme},
                {"executed": False},
                "pending",
                ["cargo run -p xeh-xtask -- oracle capture --suite all"],
                "unverified",
            )
        schemes[scheme] = cases[key]["status"]
    if all(item == "oracle_characterized" for item in schemes.values()):
        proxy_status = "oracle_characterized"
    elif all(item == "unverified" for item in schemes.values()):
        proxy_status = "unverified"
    else:
        proxy_status = "partial"
    cases["behavior.sec4.proxy-schemes"] = feature(
        "behavior.sec4.proxy-schemes",
        "xeHentai/proxy.py:114",
        {"schemes": ["http", "https", "socks"]},
        schemes,
        "pending",
        ["cargo run -p xeh-xtask -- oracle capture --suite all"],
        proxy_status,
    )


# Diagnostic and transport fields produced by this harness. Business config,
# response bodies, and unrelated date/URL keys are not in this table.
NORMALIZE_RULES = (
    ("golden", "cli_bool", "/stderr", "fixture-root"),
    ("golden", "cli_bool", "/user_config_defaults/stderr", "fixture-root"),
    ("golden", "cli_help", "/stderr", "fixture-root"),
    ("golden", "cli_help", "/stdout", "fixture-root"),
    ("golden", "cli_range", "/stderr", "fixture-root"),
    ("golden", "cli_range_bad", "/stderr", "fixture-root"),
    ("golden", "cli_version", "/stderr", "fixture-root"),
    ("golden", "interactive", "/stderr", "fixture-root"),
    ("golden", "interactive", "/stdout", "fixture-root"),
    ("golden", "locale_zh_cn", "/stderr", "fixture-root"),
    ("golden", "runtime_config", "/filepath", "fixture-root"),
    ("golden", "glype", "/seen/0/referer", "bound-port"),
    ("golden", "glype", "/seen/1/referer", "bound-port"),
    ("golden", "glype", "/seen/2/referer", "bound-port"),
    ("case", "behavior.sec4.cli-options", "/expected/observed/bad_range/stderr", "fixture-root"),
    ("case", "behavior.sec4.cli-options", "/expected/observed/help/stderr", "fixture-root"),
    ("case", "behavior.sec4.cli-options", "/expected/observed/help/stdout", "fixture-root"),
    ("case", "behavior.sec4.cli-options", "/expected/observed/version/stderr", "fixture-root"),
    ("case", "behavior.sec4.config-defaults", "/expected/observed/filepath", "fixture-root"),
    ("case", "behavior.sec4.i18n", "/expected/observed/locale_process/stderr", "fixture-root"),
    ("case", "behavior.sec4.interactive", "/expected/observed/stderr", "fixture-root"),
    ("case", "behavior.sec4.interactive", "/expected/observed/stdout", "fixture-root"),
    ("case", "delta.argparse-bool", "/expected/observed/stderr", "fixture-root"),
    ("case", "delta.argparse-bool", "/expected/observed/user_config_defaults/stderr", "fixture-root"),
    ("case", "delta.download-range-default", "/expected/observed/stderr", "fixture-root"),
    ("case", "behavior.sec4.proxy-pool", "/expected/observed/seen/0/referer", "bound-port"),
    ("case", "behavior.sec4.proxy-pool", "/expected/observed/seen/1/referer", "bound-port"),
    ("case", "behavior.sec4.proxy-pool", "/expected/observed/seen/2/referer", "bound-port"),
    ("golden", "rpc", "/add_bad/headers/date", "http-date"),
    ("golden", "rpc", "/add_exhentai/headers/date", "http-date"),
    ("golden", "rpc", "/add_ok/headers/date", "http-date"),
    ("golden", "rpc", "/bad_level/headers/date", "http-date"),
    ("golden", "rpc", "/bare_token/headers/date", "http-date"),
    ("golden", "rpc", "/delete/headers/date", "http-date"),
    ("golden", "rpc", "/embedded_token/headers/date", "http-date"),
    ("golden", "rpc", "/getConfig/headers/date", "http-date"),
    ("golden", "rpc", "/getInfo/headers/date", "http-date"),
    ("golden", "rpc", "/get_image_all/headers/date", "http-date"),
    ("golden", "rpc", "/get_image_range/headers/date", "http-date"),
    ("golden", "rpc", "/get_jsonrpc/headers/date", "http-date"),
    ("golden", "rpc", "/id0/headers/date", "http-date"),
    ("golden", "rpc", "/list/headers/date", "http-date"),
    ("golden", "rpc", "/load/headers/date", "http-date"),
    ("golden", "rpc", "/malformed/headers/date", "http-date"),
    ("golden", "rpc", "/missing_token/headers/date", "http-date"),
    ("golden", "rpc", "/options/headers/date", "http-date"),
    ("golden", "rpc", "/pause/headers/date", "http-date"),
    ("golden", "rpc", "/private_method/headers/date", "http-date"),
    ("golden", "rpc", "/resume/headers/date", "http-date"),
    ("golden", "rpc", "/save/headers/date", "http-date"),
    ("golden", "rpc", "/set_cookie/headers/date", "http-date"),
    ("golden", "rpc", "/static/headers/date", "http-date"),
    ("golden", "rpc", "/unknown_method/headers/date", "http-date"),
    ("golden", "rpc", "/unmatched/headers/date", "http-date"),
    ("golden", "rpc", "/update_proxy_dir/headers/date", "http-date"),
    ("case", "behavior.sec4.gallery-routes", "/expected/observed/static/headers/date", "http-date"),
    ("case", "behavior.sec4.rpc-wire", "/expected/observed/id0/headers/date", "http-date"),
    ("case", "behavior.sec4.rpc-wire", "/expected/observed/options/headers/date", "http-date"),
    ("case", "behavior.sec4.task-commands", "/expected/observed/add_bad/headers/date", "http-date"),
    ("case", "behavior.sec4.task-commands", "/expected/observed/add_exhentai/headers/date", "http-date"),
    ("case", "behavior.sec4.task-commands", "/expected/observed/add_ok/headers/date", "http-date"),
    ("case", "behavior.sec4.task-commands", "/expected/observed/delete/headers/date", "http-date"),
    ("case", "behavior.sec4.task-commands", "/expected/observed/pause/headers/date", "http-date"),
    ("case", "behavior.sec4.task-commands", "/expected/observed/resume/headers/date", "http-date"),
    ("case", "delta.content-length", "/expected/observed/headers/date", "http-date"),
    ("case", "delta.falsy-id", "/expected/observed/headers/date", "http-date"),
    ("case", "delta.get-extra-newline", "/expected/observed/get_jsonrpc/headers/date", "http-date"),
    ("case", "delta.get-extra-newline", "/expected/observed/options/headers/date", "http-date"),
    ("case", "delta.get-extra-newline", "/expected/observed/unmatched/headers/date", "http-date"),
    ("case", "delta.get-image-range", "/expected/observed/headers/date", "http-date"),
    ("case", "delta.malformed-json", "/expected/observed/parsed/headers/date", "http-date"),
    ("case", "delta.token-regex-log", "/expected/observed/embedded/headers/date", "http-date"),
    ("case", "rpc.method.xeH.addTask", "/expected/observed/headers/date", "http-date"),
    ("case", "rpc.method.xeH.delTask", "/expected/observed/headers/date", "http-date"),
    ("case", "rpc.method.xeH.getConfig", "/expected/observed/headers/date", "http-date"),
    ("case", "rpc.method.xeH.getImage", "/expected/observed/headers/date", "http-date"),
    ("case", "rpc.method.xeH.getInfo", "/expected/observed/headers/date", "http-date"),
    ("case", "rpc.method.xeH.listTasks", "/expected/observed/headers/date", "http-date"),
    ("case", "rpc.method.xeH.loadSession", "/expected/observed/headers/date", "http-date"),
    ("case", "rpc.method.xeH.pauseTask", "/expected/observed/headers/date", "http-date"),
    ("case", "rpc.method.xeH.resumeTask", "/expected/observed/headers/date", "http-date"),
    ("case", "rpc.method.xeH.saveSession", "/expected/observed/headers/date", "http-date"),
    ("case", "rpc.method.xeH.setCookie", "/expected/observed/headers/date", "http-date"),
    ("case", "rpc.method.xeH.updateConfig", "/expected/observed/headers/date", "http-date"),
)


def pointer_parts(pointer):
    return [part.replace("~1", "/").replace("~0", "~") for part in pointer.split("/")[1:]]


def get_pointer(value, pointer):
    current = value
    for part in pointer_parts(pointer):
        if isinstance(current, dict) and part in current:
            current = current[part]
        elif isinstance(current, list) and part.isdigit() and int(part) < len(current):
            current = current[int(part)]
        else:
            return None, False
    return current, True


def set_pointer(value, pointer, new):
    parts = pointer_parts(pointer)
    current = value
    for part in parts[:-1]:
        if isinstance(current, dict):
            current = current[part]
        else:
            current = current[int(part)]
    last = parts[-1]
    if isinstance(current, dict):
        current[last] = new
    else:
        current[int(last)] = new


def apply_rule_text(text, kind, root, ports):
    if kind == "http-date":
        if HTTP_DATE.match(text):
            return "<http-date>"
        return text
    if kind == "fixture-root":
        if root and root in text:
            return text.replace(root, "<fixture-root>")
        return text
    if kind == "bound-port":
        for port in ports:
            text = text.replace("127.0.0.1:%d" % port, "127.0.0.1:<ephemeral-port>")
            text = text.replace("[::1]:%d" % port, "[::1]:<ephemeral-port>")
        return text
    raise ValueError("unknown normalize rule %s" % kind)


def normalize_capture(cases, golden, root, ports=None):
    cases = copy.deepcopy(cases)
    golden = copy.deepcopy(golden)
    bound = [int(port) for port in (ports or [])]
    applied = []
    for scope, ident, pointer, kind in NORMALIZE_RULES:
        tree = cases if scope == "case" else golden
        if ident not in tree:
            continue
        current, found = get_pointer(tree[ident], pointer)
        if not found or not isinstance(current, str):
            continue
        updated = apply_rule_text(current, kind, root, bound)
        if updated == current:
            continue
        set_pointer(tree[ident], pointer, updated)
        applied.append({"scope": scope, "id": ident, "path": pointer, "kind": kind})
    return cases, golden, applied


def build_result(cases, golden):
    templates = {item["id"]: item for item in contract.behavior_templates()}
    for feature_id, item in templates.items():
        cases.setdefault(feature_id, item)
    apply_derived_counts(cases)
    features = [cases[key] for key in sorted(cases)]
    for item in features:
        if item["status"] in ("implemented", "accepted", "passing"):
            raise RuntimeError("refusing implementation status on %s" % item["id"])
    catalog_ids = [
        item["id"] for item in features
        if item["id"].startswith(("config.key.", "task.state.", "error.", "cli.option.", "cli.positional.", "rpc.method.", "i18n."))
    ]
    return {
        "manifest": {
            "schema": 1,
            "project_status": "compatibility_migration_in_progress",
            "implementation_status": "pending",
            "upstream": {
                "app": UPSTREAM,
                "webui": "fffonion/xeHentai-webui@4e3738ef463fef344e96df613de9a81b4cda4b4e",
            },
            "features": features,
        },
        "catalog": {"ids": catalog_ids},
        "behaviors": {"ids": contract.behavior_ids()},
        "golden": golden,
        "deltas_markdown": deltas_markdown(cases),
    }


def run_suite(suite):
    import io
    logs = io.StringIO()
    original = sys.stdout
    class Tee(object):
        def write(self, text):
            logs.write(text)
            original.write(text)
        def flush(self):
            original.flush()
    sys.stdout = Tee()
    cases = {}
    golden = {}
    try:
        catalog_cases(cases, golden)
        if suite in ("all", "config", "cli", "i18n"):
            config_and_cli(cases, golden)
        if suite in ("all", "session", "files"):
            session_and_files(cases, golden)
        if suite in ("all", "filter", "proxy"):
            filters_and_proxy(cases, golden)
            glype_case(cases, golden)
        if suite in ("all", "rpc"):
            rpc_cases(cases, golden, logs)
        raw = build_result(cases, golden)
        raw["volatile_fields"] = {
            "fields": [
                {"name": "fixture-root", "basis": "per-capture fixture directory prefix"},
                {"name": "headers.date", "basis": "HTTP Date header clock value"},
                {"name": "bound-port", "basis": "OS-assigned local listening port recorded for this capture"},
            ],
            "bound_ports": list(BOUND_PORTS),
        }
        with open(os.path.join(out_dir(), "result.json"), "w") as handle:
            json.dump(raw, handle, sort_keys=True)
        norm_cases, norm_golden, applied = normalize_capture(
            cases, golden, fixture_root(), list(BOUND_PORTS)
        )
        with open(os.path.join(out_dir(), "normalization-applied.json"), "w") as handle:
            json.dump(applied, handle, sort_keys=True)
            handle.write("\n")
        normalized = build_result(norm_cases, norm_golden)
        normalized["normalization_applied"] = applied
        with open(os.path.join(out_dir(), "normalized-result.json"), "w") as handle:
            json.dump(normalized, handle, sort_keys=True)
    finally:
        sys.stdout = original


def run_mode(mode, argv):
    if mode == "cli-parse":
        from xeHentai.cli import parse_opt
        sys.argv = ["xeH"] + argv
        try:
            opt = parse_opt()
        except SystemExit as exc:
            sys.stdout.write(json.dumps({"exit": exc.code}) + "\n")
            return
        sys.stdout.write(json.dumps({
            "exit": 0,
            "jpn_title": opt.jpn_title,
            "rename_ori": opt.rename_ori,
            "download_range": opt.download_range,
            "daemon": opt.daemon,
            "username": opt.username,
        }) + "\n")
        return
    if mode == "cli-help":
        from xeHentai.cli import parse_opt
        sys.argv = ["xeH", "--help"]
        try:
            parse_opt()
        except SystemExit as exc:
            raise SystemExit(exc.code)
        return
    if mode == "cli-version":
        from xeHentai.cli import parse_opt
        sys.argv = ["xeH", "--version"]
        try:
            parse_opt()
        except SystemExit as exc:
            raise SystemExit(exc.code)
        return
    if mode == "interactive":
        from xeHentai.cli import interactive
        from xeHentai.core import xeHentai
        result = interactive(xeHentai())
        sys.stdout.write(json.dumps(result) + "\n")
        return
    if mode == "locale":
        import xeHentai.i18n as i18n
        import xeHentai.const as const
        sys.stdout.write(json.dumps({
            "locale": const.LOCALE,
            "sample": i18n.i18n.XEH_OPT_DESC,
        }, ensure_ascii=False) + "\n")
        return
    raise SystemExit("unknown mode %s" % mode)
