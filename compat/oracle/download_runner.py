#!/usr/bin/env python3
"""Run frozen xeHentai download decisions against the download fixtures.

Network is limited to loopback hop fixtures started by this process. Every
other decision calls the fixed upstream objects directly.
"""

import json
import os
from pathlib import Path
import socket
import sys
import time
import urllib.request
from queue import Queue
from types import SimpleNamespace


def json_value(value):
    if isinstance(value, set):
        return sorted(json_value(item) for item in value)
    if isinstance(value, tuple):
        return [json_value(item) for item in value]
    if isinstance(value, list):
        return [json_value(item) for item in value]
    if isinstance(value, dict):
        return {str(key): json_value(item) for key, item in value.items()}
    if isinstance(value, float) and value.is_integer():
        return int(value)
    return value


def logger():
    notes = []

    def remember(kind):
        def record(message, *args):
            notes.append((kind, message))
            return None

        return record

    return SimpleNamespace(
        debug=remember("debug"),
        info=remember("info"),
        warning=remember("warning"),
        warn=remember("warning"),
        error=remember("error"),
        verbose=remember("verbose"),
        notes=notes,
    )


def task_for(request, root, task_mod):
    cfg = {
        "dir": str(root),
        "download_ori": bool(request.get("download_ori", False)),
        "download_range": request.get("download_range") or [],
        "download_thread_cnt": int(request.get("download_thread_cnt", 1)),
        "scan_thread_cnt": int(request.get("scan_thread_cnt", 1)),
        "jpn_title": False,
        "ignored_errors": request.get("ignored_errors") or [],
    }
    task = task_mod.Task(request.get("url") or "https://e-hentai.org/g/1/aaaaaaaaaa/", cfg, logger())
    task.meta["total"] = int(request.get("total", 1))
    finished = request.get("finished")
    if isinstance(finished, list):
        for fid in finished:
            if isinstance(fid, int):
                task._flist_done.add(fid)
        task.meta["finished"] = len(task._flist_done)
    elif isinstance(finished, int):
        task.meta["finished"] = finished
    else:
        task.meta["finished"] = 0
    task.meta["title"] = request.get("title") or "gallery"
    task.meta["thumbnail_cnt"] = int(request.get("thumbnail_cnt", 20))
    task.meta["use_multipage_viewer"] = False
    task.state = int(request.get("state", 1))
    task.page_q = Queue()
    task.img_q = Queue()
    return task


def queue_items(queue):
    if queue is None:
        return []
    return list(queue.queue)


def enqueued_during(before, after):
    before = list(before)
    after = list(after)
    if after[: len(before)] == before:
        return after[len(before) :]
    remaining = list(before)
    fresh = []
    for item in after:
        if item in remaining:
            remaining.remove(item)
        else:
            fresh.append(item)
    return fresh


def public_flags(task):
    exported = task.to_dict()
    return {
        "public_has_list_q": "list_q" in exported,
        "public_has_metadata_q": "metadata_q" in exported,
        "page_q": json_value(exported.get("page_q", [])),
        "img_q": json_value(exported.get("img_q", [])),
    }


def empty_queues():
    return {"metadata": [], "list": [], "scan": [], "download": []}


def empty_slots():
    return {"metadata": 1, "list": 1, "scan": 0, "download": 0}


def bundle(decisions, observable, queues=None, slots=None, submit_ids=None, submit_count=0, permits_end=0, route_kind=""):
    return {
        "decisions": list(decisions),
        "queues": queues if queues is not None else empty_queues(),
        "slots": slots if slots is not None else empty_slots(),
        "observable": observable,
        "submit_ids": list(submit_ids or []),
        "submit_count": submit_count,
        "permits_end": permits_end,
        "route_kind": route_kind,
    }


def prepare_task_config(task, request):
    task.config["make_archive"] = False
    task.config["proxy_image"] = bool(request.get("proxy_image", False))
    task.config["proxy_image_only"] = bool(request.get("proxy_image_only", False))
    task.config["download_timeout"] = int(request.get("timeout", 10))
    task.config["low_speed_threshold"] = int(request.get("low_speed_threshold", 0))
    task.config["ignored_errors"] = request.get("ignored_errors") or []


def make_core(task, core_mod):
    core = core_mod.xeHentai.__new__(core_mod.xeHentai)
    core._exit = 0
    core._all_threads = [[] for _ in range(21)]
    core._all_tasks = {task.guid: task}
    core.logger = logger()
    core.headers = {}
    core.proxy = None
    core.has_login = False
    core.tasks = Queue()
    return core


def alive_workers(core):
    return sum(
        1
        for bucket in core._all_threads
        for thread in bucket
        if thread.__class__.__name__ == "HttpWorker" and thread.is_alive()
    )


def join_started(core, timeout=3):
    threads = [thread for bucket in core._all_threads for thread in bucket]
    for thread in threads:
        if thread.__class__.__name__ == "HttpWorker":
            thread.join(timeout)
    for thread in threads:
        if thread.__class__.__name__ == "Monitor":
            thread.join(timeout)


def started_worker_names(core):
    return [
        thread.name
        for bucket in core._all_threads
        for thread in bucket
        if thread.__class__.__name__ == "HttpWorker"
    ]


def retry_label(fail_code, exception, low):
    if low:
        return "retry:low-speed"
    if exception == "KeyError":
        return "retry:failed-reload"
    return {
        1009: "retry:connection-error",
        1007: "retry:key-expired",
        1011: "retry:not-found",
        1012: "retry:broken",
        1006: "retry:quota",
    }.get(fail_code, "retry:quota")


def monitor_label(reason):
    return {
        "quota": "monitor:quota-vote",
        "zombie": "monitor:zombie",
        "sweep": "monitor:sweep",
        "slow": "monitor:slow",
        "stuck": "monitor:stuck",
    }.get(reason, "monitor:clear")


def run_slots(request, root, task_mod, core_mod, worker_mod):
    task = task_for(request, root, task_mod)
    prepare_task_config(task, request)
    for url in request.get("page_q") or []:
        task.page_q.put(url)
    for url in request.get("img_q") or []:
        task.img_q.put(url)
    task.list_q = Queue()
    task.list_q.put("decoy-list")
    initial_state = task.state
    core = make_core(task, core_mod)
    requested = []

    class RecordingReq(object):
        def request(self, method, url, flt, suc, fail, **kwargs):
            requested.append(url)
            core._exit = 2
            return None

    core._get_httpreq = lambda policy: RecordingReq()
    created = []
    real_get = core_mod.xeHentai._get_httpworker
    fail_at = int(request.get("fail_download_start") or 0)
    download_started = {"n": 0}

    class DownloadStartFailed(Exception):
        pass

    def wrapped(self, tid, task_q, *args, **kwargs):
        worker = real_get(self, tid, task_q, *args, **kwargs)

        def start():
            if task_q is task.img_q:
                download_started["n"] += 1
                if fail_at and download_started["n"] == fail_at:
                    raise DownloadStartFailed()
            created.append(worker)

        worker.start = start
        return worker

    core._get_httpworker = wrapped.__get__(core, core_mod.xeHentai)
    monitors = []
    original_start = worker_mod.Monitor.start

    def no_start(self):
        monitors.append(self)

    before_page = queue_items(task.page_q)
    before_img = queue_items(task.img_q)
    worker_mod.Monitor.start = no_start
    try:
        try:
            core._do_task(task.guid)
        except DownloadStartFailed:
            pass
    finally:
        worker_mod.Monitor.start = original_start
    names = [worker.name for worker in created]
    spawned_scan = sum(1 for worker in created if worker.task_queue is task.page_q)
    spawned_download = sum(1 for worker in created if worker.task_queue is task.img_q)
    queues = empty_queues()
    if initial_state == 2:
        queues["metadata"] = list(requested)
    elif initial_state == 3:
        queues["list"] = list(requested)
    queues["scan"] = enqueued_during(before_page, queue_items(task.page_q))
    queues["download"] = enqueued_during(before_img, queue_items(task.img_q))
    slots = empty_slots()
    slots["scan"] = spawned_scan
    slots["download"] = spawned_download
    flags = public_flags(task)
    observable = {
        "worker_names": names,
        "spawned_scan": spawned_scan,
        "spawned_download": spawned_download,
        "public_has_list_q": flags["public_has_list_q"],
        "public_has_metadata_q": flags["public_has_metadata_q"],
        "page_q": flags["page_q"],
        "img_q": flags["img_q"],
        "pending_list_urls": list(requested) if initial_state == 3 else [],
    }
    return bundle(["slots:separated"], observable, queues=queues, slots=slots)


class Response(object):
    def __init__(self, request):
        self.status_code = request.get("status_code")
        self.url = request.get("requested_url") or ""
        self._real_url = request.get("real_url") or self.url
        self.headers = {}
        for name, value in zip(request.get("header_names") or [], request.get("header_values") or []):
            self.headers[name] = value
        self.text = request.get("text") or ""
        self.content_length = request.get("content_length")
        self._broken = bool(request.get("stream_broken"))

    def get(self, name):
        for key, value in self.headers.items():
            if key.lower() == name.lower():
                return value
        return None

    headers_get = get


def install_header_get(response):
    response.headers = HeaderMap(response.headers)


class HeaderMap(dict):
    def get(self, name, default=None):
        for key, value in self.items():
            if str(key).lower() == str(name).lower():
                return value
        return default


def register_reload(task, request):
    reload_url = request.get("reload_url") or ""
    if not reload_url:
        return
    image = request.get("requested_url") or ""
    try:
        key = task.get_imghash(image)
    except Exception:
        key = request.get("reload_hash") or ""
    if key:
        task.reload_map[key] = [reload_url, request.get("fname") or ""]


def checker_is_low(request, worker_mod):
    chunks = request.get("chunks") or []
    threshold = int(request.get("threshold", 0))
    if not chunks or threshold <= 0:
        return False
    clock = Clock(0, 0)
    original = worker_mod.time.time
    worker_mod.time.time = clock.time
    try:
        checker = worker_mod.speed_checker()
        for chunk in chunks:
            clock.now = int(chunk["t"])
            checker.check(int(chunk["bytes"]))
        average = checker.calc(full=True)
    finally:
        worker_mod.time.time = original
    return 0 < average < threshold


def run_retry(request, root, task_mod, filters, const, worker_mod):
    task = task_for(request, root, task_mod)
    register_reload(task, request)
    low = checker_is_low(request, worker_mod)
    if low:
        response = worker_mod._FakeResponse(request.get("requested_url") or "")
    else:
        response = Response(request)
        install_header_get(response)
    fails = []
    exception = None

    def fail(value):
        fails.append(json_value(value))

    def suc(value):
        holder["value"] = value
        return None

    holder = {"value": None}
    download = filters.download_file_wrapper(str(root))
    if request.get("stream_broken") and not low:
        def iter_content(_size=16384):
            raise filters_connection_error()

        response.iter_content = iter_content
        response.iter_content_cb = lambda _chunk: None
    try:
        download(response, suc, fail)
    except Exception as error:
        exception = type(error).__name__
    generated = holder["value"]
    if isinstance(generated, tuple) and generated and callable(generated[0]):
        try:
            for _chunk in generated[0]():
                pass
        except Exception as error:
            exception = type(error).__name__
            if isinstance(error, const.DownloadAbortedException):
                exception = "DownloadAbortedException"
    requeue_url = None
    page_q = []
    image_url = request.get("requested_url") or ""
    try:
        task.put_page_queue_retry(image_url)
        page_q = queue_items(task.page_q)
        requeue_url = page_q[0] if page_q else None
    except Exception as error:
        exception = exception or type(error).__name__
    fail_value = fails[0] if fails else [0, "", ""]
    observable = {
        "fail": fail_value,
        "requeue_url": requeue_url,
        "exception": exception,
        "page_q": page_q,
    }
    return bundle([retry_label(fail_value[0], exception or "", low)], observable)


def filters_connection_error():
    from requests.exceptions import ConnectionError

    return ConnectionError("fixture stream broke")


class Clock(object):
    def __init__(self, start, step):
        self.now = start
        self.step = step
        self.iterations = 0
        self.stop_after = 0
        self.refresh = None
        self.warnings = None
        self.after_slow = ""
        self.task = None
        self.worker = None
        self.slow_handled = False
        self.release_workers = False
        self.bumped = False
        self.sleeps_after_bump = 0

    def time(self):
        return self.now

    def sleep(self, _seconds):
        self.iterations += 1
        self.now += self.step
        if not self.slow_handled and self._saw_slow():
            self.slow_handled = True
            if self.after_slow == "progress" and self.task is not None:
                self.task.meta["finished"] = int(self.task.meta.get("finished", 0)) + 1
                self.bumped = True
            elif self.after_slow == "stuck" and self.worker is not None:
                self.worker.stream_speed = None
            else:
                self.release_workers = True
        if self.bumped:
            self.sleeps_after_bump += 1
            if self.sleeps_after_bump >= 11:
                self.release_workers = True
        if self.refresh is not None:
            if self.release_workers:
                for name in list(self.refresh.thread_last_seen):
                    self.refresh.thread_last_seen[name] = self.now - 10**9
                if self.worker is not None:
                    self.worker.is_alive = lambda: False
            else:
                for name in list(self.refresh.thread_last_seen):
                    self.refresh.thread_last_seen[name] = self.now
        if self.stop_after and self.iterations >= self.stop_after:
            raise StopIteration()

    def _saw_slow(self):
        for message in self.warnings or []:
            text = str(message)
            if "slow" in text and "stuck" not in text:
                return True
        return False


def monitor_steps(warnings, request, task, initial_finished):
    steps = []
    for message in warnings:
        text = str(message)
        if "stuck" in text:
            steps.append("stuck")
        elif "slow" in text:
            steps.append("slow")
    if request.get("after_slow") == "progress" and int(task.meta["finished"]) > initial_finished:
        placed = []
        inserted = False
        for step in steps:
            placed.append(step)
            if step == "slow" and not inserted:
                placed.append("progress")
                inserted = True
        if not inserted:
            placed.append("progress")
        steps = placed
    return steps


def run_speed(request, worker_mod):
    clock = Clock(0, 0)
    original = worker_mod.time.time
    worker_mod.time.time = clock.time
    try:
        checker = worker_mod.speed_checker()
        for chunk in request.get("chunks") or []:
            clock.now = int(chunk["t"])
            checker.check(int(chunk["bytes"]))
        average = checker.calc(full=True)
    finally:
        worker_mod.time.time = original
    threshold = int(request.get("threshold", 0))
    low = bool(0 < average < threshold)
    decision = "speed:low" if low else "speed:ok"
    return bundle([decision], {
        "low_speed": low,
        "sample_count": len(checker.speed_buffer),
        "average_milli": int(round(average * 1000)),
    })


def monitor_for(request, root, task_mod, worker_mod):
    task = task_for(request, root, task_mod)
    task.img_q = Queue()
    task.page_q = Queue()
    monitor = worker_mod.Monitor(None, None, logger(), task, ignored_errors=request.get("ignored_errors") or [])
    monitor.set_title = lambda _title: None
    return task, monitor


def run_monitor(request, root, task_mod, worker_mod, const):
    kind = request.get("kind")
    task, monitor = monitor_for(request, root, task_mod, worker_mod)
    if kind == "quota-vote":
        votes = {int(key): int(value) for key, value in (request.get("votes") or {}).items()}
        monitor.vote_result = votes
        monitor.thread_last_seen = {"down-%d" % (index + 1): 0 for index in range(int(request.get("thread_count", 0)))}
        monitor._check_vote()
        reason = "quota" if task.state == const.TASK_STATE_FAILED else ""
        return bundle([monitor_label(reason)], {
            "reason": reason,
            "zombies": [],
            "swept": [],
            "task_state": task.state,
            "stopped": task.state == const.TASK_STATE_FAILED,
            "votes": json_value(monitor.vote_result),
        })

    if kind in ("zombie", "sweep"):
        name = request.get("name") or "down-1"
        threshold = int(request.get("timeout", 10)) * (int(request.get("retry", 0)) + 1)
        worker = SimpleNamespace(
            name=name,
            zombie_threshold=threshold,
            stream_speed=None,
            is_alive=lambda: bool(request.get("alive")),
        )
        monitor.thread_ref[name] = worker
        monitor.thread_last_seen[name] = int(request.get("last_seen", 0))
        clock = Clock(int(request.get("now", 0)), 0)
        original = worker_mod.time.time
        original_sleep = worker_mod.time.sleep
        worker_mod.time.time = clock.time
        worker_mod.time.sleep = lambda _seconds: None
        try:
            monitor.run()
        finally:
            worker_mod.time.time = original
            worker_mod.time.sleep = original_sleep
        zombies = sorted(monitor.thread_zombie)
        swept = [name] if name not in monitor.thread_last_seen and name not in zombies else []
        reason = "zombie" if zombies else "sweep" if name not in monitor.thread_last_seen else ""
        return bundle([monitor_label(reason)], {
            "reason": reason,
            "zombies": zombies,
            "swept": swept,
            "task_state": task.state,
            "stopped": name not in monitor.thread_last_seen,
            "votes": {},
        })

    name = request.get("name") or "down-1"
    threshold = int(request.get("timeout", 10)) * (int(request.get("retry", 0)) + 1)
    speed = int(request.get("total_speed", 0))
    worker = SimpleNamespace(
        name=name,
        zombie_threshold=threshold,
        stream_speed=SimpleNamespace(calc=lambda full=False: speed) if speed else None,
        is_alive=lambda: True,
    )
    monitor.thread_ref[name] = worker
    task.meta["finished"] = int(request.get("finished", 0))
    task.meta["total"] = max(task.meta["finished"] + 1, int(request.get("total", task.meta["finished"] + 1)))
    clock = Clock(0, int(request.get("clock_step", 10)))
    clock.refresh = monitor
    clock.after_slow = request.get("after_slow") or ""
    clock.task = task
    clock.worker = worker
    initial_finished = int(task.meta["finished"])
    original = worker_mod.time.time
    original_sleep = worker_mod.time.sleep
    worker_mod.time.time = clock.time
    worker_mod.time.sleep = clock.sleep
    monitor.set_vote_ns([name])
    warnings = []
    clock.warnings = warnings
    monitor.logger.warning = lambda message, *args: warnings.append(message)
    try:
        monitor.run()
    except StopIteration:
        pass
    finally:
        worker_mod.time.time = original
        worker_mod.time.sleep = original_sleep
    stuck = any("stuck" in str(message) for message in warnings)
    slow = any("slow" in str(message) for message in warnings)
    reason = "stuck" if stuck else "slow" if slow else ""
    return bundle([monitor_label(reason)], {
        "reason": reason,
        "zombies": [],
        "swept": [],
        "task_state": task.state,
        "stopped": stuck,
        "votes": {},
        "steps": monitor_steps(warnings, request, task, initial_finished),
    })


def run_duplicates(request, root, task_mod, const):
    task = task_for(request, root, task_mod)
    task._monitor = SimpleNamespace(_exit=lambda _value: False)
    old_reload = request["old_reload_url"]
    old_hash = task_mod.RE_GALLERY.findall(old_reload)[0][0]
    task.reload_map[old_hash] = [old_reload, request.get("old_fname") or ""]
    if request.get("old_exists"):
        folder = Path(task.get_fpath())
        folder.mkdir(parents=True, exist_ok=True)
        (folder / task.get_fidpad(int(request["old_fid"]))).write_bytes(b"old")
    task.put_img_queue(request["img_url"], request["reload_url"], request["fname"])
    decisions = []
    new_fid = int(task_mod.RE_GALLERY.findall(request["reload_url"])[0][1])
    if new_fid in task._flist_done and request.get("old_exists"):
        decisions.append("duplicate:copied")
    elif int(request["old_fid"]) in task.duplicate_map:
        decisions.append("duplicate:pending")
    if request.get("save_original"):
        def binary():
            yield b"img"

        task.save_file(request["img_url"], request["img_url"], binary)
        if int(request["old_fid"]) in task._flist_done and new_fid in task._flist_done:
            decisions.append("duplicate:saved-copy")
    return bundle(decisions, {
        "finished": sorted(task._flist_done),
        "duplicate_map": json_value(task.duplicate_map) if task.duplicate_map else {},
        "img_q": queue_items(task.img_q),
        "copied": new_fid in task._flist_done,
    })


def run_pages(request, root, task_mod):
    task = task_for(request, root, task_mod)
    kind = request.get("kind")
    if kind in ("repeated-unfinished", "repeated-finished"):
        task.put_page_queue(request["url"])
        task.put_page_queue(request["url"])
    if kind in ("out-of-range", "last-page"):
        task.scan_downloaded()
    unfinished = task.get_fid_unfinished()
    scanned_last = False
    if request.get("present_last"):
        probe = task_for(request, root / "probe", task_mod)
        folder = Path(probe.get_fpath())
        folder.mkdir(parents=True, exist_ok=True)
        (folder / probe.get_fidpad(int(request["total"]))).write_bytes(b"last")
        probe.scan_downloaded()
        scanned_last = int(request["total"]) in probe._flist_done
    decisions = []
    if kind in ("repeated-unfinished", "repeated-finished"):
        decisions.append("page:queued" if task.page_q.qsize() else "page:skipped")
    elif kind == "out-of-range":
        decisions.append("page:out-of-range")
    elif kind == "last-page":
        decisions.append("page:last-excluded")
    return bundle(decisions, {
        "page_q": queue_items(task.page_q),
        "queued_count": task.page_q.qsize(),
        "finished": sorted(task._flist_done),
        "finished_count": len(task._flist_done),
        "state": task.state,
        "unfinished": unfinished,
        "unfinished_count": len(unfinished),
        "scanned_includes_last": scanned_last,
    })


def run_lifecycle(request, root, task_mod, core_mod, worker_mod):
    task = task_for(request, root, task_mod)
    prepare_task_config(task, request)
    core = make_core(task, core_mod)
    core._get_httpreq = lambda _policy: SimpleNamespace()
    codes = []
    details = []
    states = []
    submit_ids = []
    permit_trace = []
    started = False
    original_sleep = worker_mod.time.sleep
    worker_mod.time.sleep = lambda _seconds: None
    try:
        for step in request.get("steps") or []:
            if step == "start" and not started and task.state == 10:
                core._do_task(task.guid)
                submit_ids = started_worker_names(core)
                started = True
            elif step == "pause":
                code, detail = core_mod.xeHentai.pause_task(core, task.guid)
                join_started(core)
                codes.append(code)
                details.append(detail if detail is not None else None)
                states.append(task.state)
            elif step == "resume":
                code, detail = core_mod.xeHentai.resume_task(core, task.guid)
                codes.append(code)
                details.append(detail if detail is not None else None)
                states.append(task.state)
            elif step == "exit":
                if getattr(task, "_monitor", None):
                    task._monitor._exit = lambda _value: True
                for bucket in core._all_threads:
                    for thread in bucket:
                        thread._exit = lambda _value: True
                join_started(core)
            permit_trace.append(alive_workers(core))
    finally:
        worker_mod.time.sleep = original_sleep
        join_started(core)
    observable = {
        "codes": codes,
        "details": json_value(details),
        "states": states,
        "queue_puts": core.tasks.qsize(),
        "exit_after_pause": bool(getattr(task, "_monitor", None) and task._monitor._exit(None)),
        "finished": int(request.get("finished", 0)),
        "permit_trace": permit_trace,
    }
    return bundle(
        ["lifecycle:no-double-submit"],
        observable,
        submit_ids=submit_ids,
        submit_count=len(submit_ids),
        permits_end=alive_workers(core),
    )


def serve_ok():
    server = socket.socket()
    server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    server.bind(("127.0.0.1", 0))
    server.listen(1)
    port = server.getsockname()[1]

    def accept_one():
        try:
            conn, _addr = server.accept()
        except OSError:
            return
        data = b""
        conn.settimeout(5)
        try:
            while b"\r\n\r\n" not in data and len(data) < 8192:
                chunk = conn.recv(1024)
                if not chunk:
                    break
                data += chunk
            conn.sendall(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        except OSError:
            pass
        finally:
            conn.close()

    import threading

    thread = threading.Thread(target=accept_one)
    thread.daemon = True
    thread.start()
    return port, server, thread


def run_hop(request, util):
    policy = util.get_proxy_policy(
        {
            "proxy_image": bool(request.get("proxy_image")),
            "proxy_image_only": bool(request.get("proxy_image_only")),
        }
    )
    port, server, thread = serve_ok()
    try:
        if policy.match("http://127.0.0.1/image.bin"):
            proxy_port, proxy_server, proxy_thread = serve_ok()
            try:
                opener = urllib.request.build_opener(
                    urllib.request.ProxyHandler({"http": "http://127.0.0.1:%d" % proxy_port})
                )
                status = opener.open("http://127.0.0.1:%d/image.bin" % port, timeout=5).status
            finally:
                proxy_server.close()
                proxy_thread.join(timeout=2)
            path = "proxied"
        else:
            status = urllib.request.urlopen("http://127.0.0.1:%d/image.bin" % port, timeout=5).status
            path = "direct"
    finally:
        server.close()
        thread.join(timeout=2)
    return bundle(
        ["hop:proxy_request" if path == "proxied" else "hop:admit+exchange"],
        {"path": path, "status": int(status)},
        route_kind="http" if path == "proxied" else "direct",
    )


def observable_for(case, root, modules):
    request = case["request"]
    operation = request["operation"]
    if operation == "slots":
        return run_slots(request, root, modules["task"], modules["core"], modules["worker"])
    if operation == "retry":
        return run_retry(request, root, modules["task"], modules["filters"], modules["const"], modules["worker"])
    if operation == "speed":
        return run_speed(request, modules["worker"])
    if operation == "monitor":
        return run_monitor(request, root, modules["task"], modules["worker"], modules["const"])
    if operation == "duplicates":
        return run_duplicates(request, root, modules["task"], modules["const"])
    if operation == "pages":
        return run_pages(request, root, modules["task"])
    if operation == "lifecycle":
        return run_lifecycle(request, root, modules["task"], modules["core"], modules["worker"])
    if operation == "hop":
        return run_hop(request, modules["util"])
    raise RuntimeError("unknown download operation %s" % operation)


def main():
    fixture_root = Path(os.environ["XEH_FIXTURE_ROOT"]).resolve()
    upstream = Path(os.environ["XEH_UPSTREAM"]).resolve()
    cases_path = Path(os.environ["XEH_CASES"]).resolve()
    result_path = Path(os.environ["XEH_RESULT"]).resolve()
    if Path.cwd().resolve() != fixture_root:
        raise SystemExit("download oracle cwd does not match its owned fixture root")
    if not (upstream / "xeHentai" / "worker.py").is_file():
        raise SystemExit("fixed upstream worker.py is absent")
    sys.path.insert(0, str(upstream))
    modules = {
        "task": __import__("xeHentai.task", fromlist=["Task"]),
        "filters": __import__("xeHentai.filters", fromlist=["download_file_wrapper"]),
        "worker": __import__("xeHentai.worker", fromlist=["Monitor"]),
        "const": __import__("xeHentai.const", fromlist=["TASK_STATE_FAILED"]),
        "core": __import__("xeHentai.core", fromlist=["xeHentai"]),
        "util": __import__("xeHentai.util", fromlist=["get_proxy_policy"]),
    }
    with cases_path.open(encoding="utf-8") as handle:
        cases = json.load(handle)["cases"]
    results = []
    for index, case in enumerate(cases):
        case_root = fixture_root / ("case-%03d" % index)
        case_root.mkdir()
        os.chdir(case_root)
        report = observable_for(case, case_root, modules)
        row = {"id": case["id"]}
        row.update(report)
        results.append(row)
    result_path.write_text(
        json.dumps({"cases": results}, ensure_ascii=True, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print("download-oracle cases=%d" % len(results))


if __name__ == "__main__":
    main()
