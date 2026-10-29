#!/usr/bin/env python3
# coding:utf-8
"""Test-only oracle runner for task control differential tests.

Copied into a throwaway fixture root and executed there. It imports the pinned
upstream tree from PYTHONPATH, patches the guid source with the same planned
prefixes the RustScript fixture receives, and calls the real upstream methods
(`add_task`, `del_task`, `pause_task`, `resume_task`, `update_config`). The
`set_login`, `seed_state`, and `seed_meta` steps are harness setup with no
upstream API; they are recorded but not compared.

It never reads a user config.py, h.json, cookie, proxy setting, or real site.
"""
import argparse
import json
import os
import sys
import types


def install_config(values):
    module = types.ModuleType("config")
    for key, value in values.items():
        setattr(module, key, value)
    sys.modules["config"] = module


class PlannedGuid(object):
    def __init__(self, text):
        self.text = text

    def __str__(self):
        return self.text


def make_guid_source(guids):
    state = {"cursor": 0, "fallback": 0}

    def uuid4():
        cursor = state["cursor"]
        if cursor < len(guids):
            prefix = guids[cursor]
            state["cursor"] = cursor + 1
            return PlannedGuid(prefix + "-0000-4000-8000-000000000000")
        # The script plans every guid it needs; a unique fallback keeps an
        # accidentally unplanned add from colliding with an earlier one.
        fallback = "e%07x" % state["fallback"]
        state["fallback"] += 1
        return PlannedGuid(fallback + "-0000-4000-8000-000000000000")

    return uuid4


def jsonable(value):
    if isinstance(value, tuple):
        return [jsonable(item) for item in value]
    if isinstance(value, list):
        return [jsonable(item) for item in value]
    if isinstance(value, dict):
        return {str(key): jsonable(item) for key, item in value.items()}
    if isinstance(value, set):
        return [jsonable(item) for item in value]
    return value


def snapshot_tasks(xe):
    return [jsonable(task.to_dict()) for task in xe._all_tasks.values()]


def snapshot_queue(xe):
    return list(xe.tasks.queue)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--script", required=True)
    parser.add_argument("--result", required=True)
    args = parser.parse_args()

    with open(args.script, "r") as handle:
        script = json.load(handle)

    import xeHentai.task as task_module
    from xeHentai.core import xeHentai

    task_module.uuid.uuid4 = make_guid_source(script["guids"])
    xe = xeHentai()
    records = []
    for index, step in enumerate(script["steps"]):
        op = step["op"]
        presence = None
        setup = op in ("set_login", "seed_state", "seed_meta")
        if op == "add":
            code, value = xe.add_task(step["url"], **step.get("overrides", {}))
        elif op == "del":
            code, value = xe.del_task(step["guid"])
        elif op == "pause":
            code, value = xe.pause_task(step["guid"])
        elif op == "resume":
            code, value = xe.resume_task(step["guid"])
        elif op == "update":
            overlay = step["overlay"]
            code, value = xe.update_config(**overlay)
            presence = {
                "proxy": "proxy" in overlay,
                "dir": "dir" in overlay,
                "ignored_errors": "ignored_errors" in overlay,
            }
        elif op == "set_login":
            xe.has_login = bool(step["value"])
            code, value = 0, ""
        elif op == "seed_state":
            xe._all_tasks[step["guid"]].state = int(step["state"])
            code, value = 0, ""
        elif op == "seed_meta":
            xe._all_tasks[step["guid"]].meta = dict(step["meta"])
            code, value = 0, ""
        else:
            raise SystemExit("unknown step op %s" % (op,))
        records.append(
            {
                "index": index,
                "op": op,
                "setup": setup,
                "ret": [jsonable(code), jsonable(value)],
                "presence": jsonable(presence),
                "tasks": snapshot_tasks(xe),
                "queue": snapshot_queue(xe),
                "config": jsonable(xe.cfg),
            }
        )

    with open(args.result, "w") as handle:
        json.dump({"records": records}, handle, sort_keys=True)
        handle.write("\n")


if __name__ == "__main__":
    main()
