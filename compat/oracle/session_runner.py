#!/usr/bin/env python3
# coding:utf-8
"""Test-only session oracle using the pinned upstream implementation.

Copied into a throwaway fixture root and executed there. Every case gets its
own owned directory tree (session root, legacy root, downloads root, boot
directory) and its own HOME/TMPDIR. The runner only calls the real upstream
`xeHentai.save_session` / `load_session` and `Task.from_dict` / `to_dict` /
`scan_downloaded`; it never models those rules itself and never contacts a
site, proxy or account.
"""
import argparse
import json
import os
import shutil
import sys
import traceback


def jsonable(value):
    if isinstance(value, tuple):
        return [jsonable(item) for item in value]
    if isinstance(value, list):
        return [jsonable(item) for item in value]
    if isinstance(value, dict):
        return {str(key): jsonable(item) for key, item in value.items()}
    if isinstance(value, set):
        return sorted(jsonable(item) for item in value)
    return value


def snapshot(xe, root):
    cookies = [[str(key), value] for key, value in xe.cookies.items()]
    disk = []
    downloads = os.path.join(root, "downloads")
    for base, _dirs, files in os.walk(downloads):
        for name in files:
            path = os.path.join(base, name)
            disk.append([os.path.relpath(path, downloads), os.path.getsize(path)])
    disk.sort()
    return {
        "tasks": [jsonable(task.to_dict()) for task in xe._all_tasks.values()],
        "queue": [str(guid) for guid in xe.tasks.queue],
        "cookies": cookies,
        "header": xe.headers.get("Cookie"),
        "login": bool(xe.has_login),
        "disk": disk,
    }


def substitute(value, downloads):
    if isinstance(value, str):
        return value.replace("{downloads}", downloads)
    if isinstance(value, list):
        return [substitute(item, downloads) for item in value]
    if isinstance(value, dict):
        return {key: substitute(item, downloads) for key, item in value.items()}
    return value


def build_document(case, downloads):
    tasks = {}
    for record in substitute(case.get("tasks", []), downloads):
        tasks[str(record["guid"])] = record
    cookies = {}
    for name, value in case.get("cookies", [["nw", "1"]]):
        cookies[name] = value
    return json.dumps({"tasks": tasks, "cookies": cookies})


def prepare_case(root, case):
    case_root = os.path.join(root, "cases", case["name"])
    if os.path.exists(case_root):
        shutil.rmtree(case_root)
    session_dir = os.path.join(case_root, "session")
    legacy_dir = os.path.join(case_root, "legacy")
    downloads = os.path.join(case_root, "downloads")
    boot = os.path.join(case_root, "boot")
    boot_legacy = os.path.join(case_root, "boot-legacy")
    for path in (session_dir, legacy_dir, downloads, boot, boot_legacy):
        os.makedirs(path)
    if case["mode"] == "save":
        document = build_document(case, downloads)
    else:
        document = case.get("document")
    if document is not None:
        session_file = os.path.join(session_dir, "h.json")
        with open(session_file, "w") as handle:
            handle.write(substitute(document, downloads))
        if case.get("unreadable"):
            os.chmod(session_file, 0)
    if case.get("legacy") is not None:
        with open(os.path.join(legacy_dir, ".ehentai.cookie"), "w") as handle:
            handle.write(case["legacy"])
    for relative in case.get("zero_files", []):
        target = os.path.join(downloads, relative)
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "wb") as handle:
            handle.write(b"")
    for item in case.get("files", []):
        target = os.path.join(downloads, item["path"])
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "wb") as handle:
            handle.write(item.get("text", "").encode("utf-8"))
    for relative in case.get("markers", []):
        target = os.path.join(downloads, relative)
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "wb") as handle:
            handle.write(b"synthetic\n")
    for relative in case.get("zips", []):
        target = os.path.join(downloads, relative)
        os.makedirs(os.path.dirname(target), exist_ok=True)
        with open(target, "ab") as handle:
            handle.write(b"")
    return case_root, session_dir, legacy_dir, downloads, boot, boot_legacy


def clean_instance(core, boot, boot_legacy, legacy_dir):
    previous = os.getcwd()
    os.chdir(boot)
    try:
        core.FILEPATH = boot_legacy
        xe = core.xeHentai()
    finally:
        os.chdir(previous)
    core.FILEPATH = legacy_dir
    return xe


def make_task(core, record):
    from xeHentai.task import Task
    task = Task("", {}, core.xeHentai().logger)
    task.from_dict(record)
    return task


def run_case(root, case, core):
    case_root, session_dir, legacy_dir, downloads, boot, boot_legacy = prepare_case(root, case)
    xe = clean_instance(core, boot, boot_legacy, legacy_dir)
    os.chdir(session_dir)
    result = {"name": case["name"], "mode": case["mode"]}
    try:
        if case["mode"] == "save":
            loaded = xe.load_session()
            if loaded[0] != 0:
                result["outcome"] = "return"
                result["ret"] = [loaded[0], loaded[1]]
            else:
                xe.cfg["save_tasks"] = bool(case.get("save_tasks", False))
                ret = xe.save_session()
                result["outcome"] = "return"
                result["ret"] = [ret[0], ret[1]]
                saved_path = os.path.join(session_dir, "h.json")
                with open(saved_path) as handle:
                    result["saved"] = json.load(handle)
                result["saved_size"] = os.path.getsize(saved_path)
                # A fresh instance loads the saved file: this is where task and
                # cookie member order becomes observable.
                fresh = clean_instance(core, boot, boot_legacy, legacy_dir)
                os.chdir(session_dir)
                fresh.load_session()
                result.update(snapshot(fresh, case_root))
        else:
            repeat = int(case.get("repeat", 1))
            ret = None
            try:
                for _ in range(repeat):
                    ret = xe.load_session()
                    if ret[0] != 0:
                        break
                result["outcome"] = "return"
                result["ret"] = [ret[0], ret[1] if ret[1] is None else str(ret[1])]
            except Exception as exc:  # noqa: BLE001 - the class is the observation
                result["outcome"] = "exception"
                result["exception"] = type(exc).__name__
            result.update(snapshot(xe, case_root))
    except Exception as exc:  # noqa: BLE001
        result["outcome"] = "exception"
        result["exception"] = type(exc).__name__
        result["trace"] = traceback.format_exc().splitlines()[-1]
        try:
            result.update(snapshot(xe, case_root))
        except Exception:
            pass
    finally:
        os.chdir(root)
        session_file = os.path.join(session_dir, "h.json")
        if os.path.exists(session_file):
            try:
                os.chmod(session_file, 0o600)
            except OSError:
                pass
    return result


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--script", required=True)
    parser.add_argument("--result", required=True)
    args = parser.parse_args()
    with open(args.script) as handle:
        script = json.load(handle)
    root = os.path.dirname(os.path.abspath(args.result))
    import xeHentai.core as core
    records = []
    for case in script["cases"]:
        records.append(run_case(root, case, core))
    with open(args.result, "w") as handle:
        json.dump({"records": records}, handle, sort_keys=True)
        handle.write("\n")


if __name__ == "__main__":
    main()
