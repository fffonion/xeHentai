#!/usr/bin/env python3
"""Run frozen xeHentai download decisions against the download fixtures.

Network is limited to loopback hop fixtures started by this process. Every
other decision calls the fixed upstream objects directly.
"""

import json
import math
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


def public_flags(task):
    exported = task.to_dict()
    return {
        "public_has_list_q": "list_q" in exported,
        "public_has_metadata_q": "metadata_q" in exported,
        "page_q": json_value(exported.get("page_q", [])),
        "img_q": json_value(exported.get("img_q", [])),
    }


def worker_names(state, scan_count, download_count):
    if state < 4:
        return []
    names = ["down-%d" % (index + 1) for index in range(download_count)]
    names.extend("scan-%d" % (index + 1) for index in range(scan_count))
    return names


def list_urls(task):
    count = int(math.ceil(1.0 * task.meta["total"] / int(task.meta["thumbnail_cnt"])))
    return ["%s/?p=%d" % (task.url, index) for index in range(count)]


def run_slots(request, root, task_mod):
    task = task_for(request, root, task_mod)
    for url in request.get("page_q") or []:
        task.page_q.put(url)
    for url in request.get("img_q") or []:
        task.img_q.put(url)
    pending = list_urls(task) if task.state == 3 else []
    names = worker_names(
        task.state,
        int(request.get("scan_thread_cnt", 0)),
        int(request.get("download_thread_cnt", 0)),
    )
    flags = public_flags(task)
    return {
        "worker_names": names,
        "spawned_scan": int(request.get("scan_thread_cnt", 0)) if task.state >= 4 else 0,
        "spawned_download": int(request.get("download_thread_cnt", 0)) if task.state >= 4 else 0,
        "public_has_list_q": flags["public_has_list_q"],
        "public_has_metadata_q": flags["public_has_metadata_q"],
        "page_q": flags["page_q"],
        "img_q": flags["img_q"],
        "pending_list_urls": pending,
    }


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


def run_retry(request, root, task_mod, filters, const):
    task = task_for(request, root, task_mod)
    register_reload(task, request)
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
    if request.get("stream_broken"):
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
    return {
        "fail": fail_value,
        "requeue_url": requeue_url,
        "exception": exception,
        "page_q": page_q,
    }


def filters_connection_error():
    from requests.exceptions import ConnectionError

    return ConnectionError("fixture stream broke")


class Clock(object):
    def __init__(self, start, step):
        self.now = start
        self.step = step
        self.iterations = 0
        self.stop_after = 0

    def time(self):
        return self.now

    def sleep(self, _seconds):
        self.iterations += 1
        self.now += self.step
        if self.stop_after and self.iterations >= self.stop_after:
            raise StopIteration()


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
    return {
        "low_speed": bool(0 < average < threshold),
        "sample_count": len(checker.speed_buffer),
        "average_milli": int(round(average * 1000)),
    }


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
        return {
            "reason": "quota" if task.state == const.TASK_STATE_FAILED else "",
            "zombies": [],
            "swept": [],
            "task_state": task.state,
            "stopped": task.state == const.TASK_STATE_FAILED,
            "votes": json_value(monitor.vote_result),
        }

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
        reason = "zombie" if zombies else "sweep"
        return {
            "reason": reason,
            "zombies": zombies,
            "swept": swept,
            "task_state": task.state,
            "stopped": name not in monitor.thread_last_seen,
            "votes": {},
        }

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
    clock.stop_after = 40
    original = worker_mod.time.time
    original_sleep = worker_mod.time.sleep
    worker_mod.time.time = clock.time
    worker_mod.time.sleep = clock.sleep
    monitor.set_vote_ns([name])
    warnings = []
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
    return {
        "reason": reason,
        "zombies": [],
        "swept": [],
        "task_state": task.state,
        "stopped": stuck,
        "votes": {},
    }


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
    copied = request.get("old_exists", False)
    if request.get("save_original"):
        def binary():
            yield b"img"

        task.save_file(request["img_url"], request["img_url"], binary)
        copied = True
    return {
        "finished": sorted(task._flist_done),
        "duplicate_map": json_value(task.duplicate_map) if task.duplicate_map else {},
        "img_q": queue_items(task.img_q),
        "copied": bool(copied),
    }


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
    if kind == "last-page":
        probe = task_for(request, root / "probe", task_mod)
        folder = Path(probe.get_fpath())
        folder.mkdir(parents=True, exist_ok=True)
        (folder / probe.get_fidpad(int(request["total"]))).write_bytes(b"last")
        probe.scan_downloaded()
        scanned_last = int(request["total"]) in probe._flist_done
    return {
        "page_q": queue_items(task.page_q),
        "queued_count": task.page_q.qsize(),
        "finished": sorted(task._flist_done),
        "finished_count": len(task._flist_done),
        "state": task.state,
        "unfinished": unfinished,
        "unfinished_count": len(unfinished),
        "scanned_includes_last": scanned_last,
    }


def run_lifecycle(request, root, task_mod, core_mod):
    task = task_for(request, root, task_mod)
    task._monitor = SimpleNamespace(_exit=lambda _value: False)
    owner = SimpleNamespace(_all_tasks={task.guid: task}, tasks=Queue())
    codes = []
    details = []
    states = []
    for step in request.get("steps") or []:
        if step == "start":
            continue
        if step == "pause":
            code, detail = core_mod.xeHentai.pause_task(owner, task.guid)
        elif step == "resume":
            code, detail = core_mod.xeHentai.resume_task(owner, task.guid)
        else:
            task._monitor._exit = lambda _value: True
            continue
        codes.append(code)
        details.append(detail if detail is not None else None)
        states.append(task.state)
    return {
        "codes": codes,
        "details": json_value(details),
        "states": states,
        "queue_puts": owner.tasks.qsize(),
        "exit_after_pause": bool(task._monitor._exit(None)),
        "finished": int(request.get("finished", 0)),
    }


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
    return {"path": path, "status": int(status)}


def observable_for(case, root, modules):
    request = case["request"]
    operation = request["operation"]
    if operation == "slots":
        return run_slots(request, root, modules["task"])
    if operation == "retry":
        return run_retry(request, root, modules["task"], modules["filters"], modules["const"])
    if operation == "speed":
        return run_speed(request, modules["worker"])
    if operation == "monitor":
        return run_monitor(request, root, modules["task"], modules["worker"], modules["const"])
    if operation == "duplicates":
        return run_duplicates(request, root, modules["task"], modules["const"])
    if operation == "pages":
        return run_pages(request, root, modules["task"])
    if operation == "lifecycle":
        return run_lifecycle(request, root, modules["task"], modules["core"])
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
        results.append({"id": case["id"], "observable": observable_for(case, case_root, modules)})
    result_path.write_text(
        json.dumps({"cases": results}, ensure_ascii=True, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print("download-oracle cases=%d" % len(results))


if __name__ == "__main__":
    main()
