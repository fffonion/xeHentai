#!/usr/bin/env python3
# coding:utf-8
"""Test-only oracle runner for configuration differential tests.

This script is copied into a throwaway fixture root and executed there. It
imports the pinned upstream tree from PYTHONPATH, injects the same JSON data
the RustScript implementation reads as the upstream `config` module, and calls
the real upstream methods (`xeHentai`, `update_config`, `get_config`,
`add_task`). It never reads a user config.py, h.json, cookie or proxy setting.
"""
import argparse
import json
import os
import sys
import types

SYNTHETIC_URL = "https://e-hentai.org/g/123456/0123456789/"


def read_json(path):
    with open(path, "r") as handle:
        return json.load(handle)


def config_path(explicit, base):
    if explicit:
        return explicit
    candidate = os.path.join(base, "config.json")
    return candidate if os.path.exists(candidate) else None


def install_config(values):
    module = types.ModuleType("config")
    for key, value in values.items():
        setattr(module, key, value)
    sys.modules["config"] = module


def make_app(values):
    if values is not None:
        install_config(values)
    from xeHentai.core import xeHentai

    return xeHentai()


def public_view(xe):
    from xeHentai.rpc import xeHentaiRPCExtended

    code, config = xeHentaiRPCExtended(xe, None).get_config()
    if code != 0:
        raise SystemExit("upstream get_config failed with %s" % (code,))
    return config


def make_task(xe):
    code, guid = xe.add_task(SYNTHETIC_URL)
    if code != 0:
        raise SystemExit("upstream add_task failed with %s" % (code,))
    if guid not in xe._all_tasks:
        raise SystemExit("upstream add_task did not register the task")
    return guid, xe._all_tasks[guid].config


def jsonable(value):
    if isinstance(value, tuple):
        return [jsonable(item) for item in value]
    if isinstance(value, list):
        return [jsonable(item) for item in value]
    if isinstance(value, dict):
        return {str(key): jsonable(item) for key, item in value.items()}
    return value


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--mode", choices=["load", "update"], required=True)
    parser.add_argument("--base", required=True)
    parser.add_argument("--explicit", default="")
    parser.add_argument("--overlay", default="")
    parser.add_argument("--result", required=True)
    args = parser.parse_args()

    path = config_path(args.explicit, args.base)
    values = read_json(path) if path else None
    xe = make_app(values)
    guid, task_before = make_task(xe)
    result = {
        "ok": True,
        "error": "",
        "source": "file" if values is not None else "defaults",
        "config": jsonable(xe.cfg),
        "public": jsonable(public_view(xe)),
        "task": jsonable(task_before),
        "upstream": os.path.abspath(
            getattr(sys.modules.get("xeHentai"), "__file__", "") or ""
        ),
    }
    if args.mode == "update":
        overlay = read_json(args.overlay)
        xe.update_config(**overlay)
        result["config"] = jsonable(xe.cfg)
        result["public"] = jsonable(public_view(xe))
        result["task_before"] = jsonable(task_before)
        result["task_after"] = jsonable(xe._all_tasks[guid].config)
        result["presence"] = {
            "proxy": "proxy" in overlay,
            "dir": "dir" in overlay,
            "ignored_errors": "ignored_errors" in overlay,
        }
    with open(args.result, "w") as handle:
        json.dump(result, handle, sort_keys=True)
        handle.write("\n")


if __name__ == "__main__":
    main()
