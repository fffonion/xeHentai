"""Drive the original frozen xeHentai WebUI against the real compiled backend.

Portable copy of the accepted browser3c proof driver. All artifacts stay under
the explicitly supplied run root. The backend is
`tests::browser_fixture_server`: the real RSS dispatcher, App, TCP server and
frozen UI pack, with fixture tasks seeded through the same seam the
differential suites use.

This script does NOT start the backend. The caller starts it with
XEH_BROWSER_REQUEST=<run-root>/request.json, waits for <run-root>/ready.json,
then runs this driver. The 49 checks, their order, interactions, geometry and
click targets are byte-preserved from the accepted harness; only the paths,
port and Chromium executable are parameters.
"""
import argparse
import json
import sys
import time
import urllib.request
import zipfile
from pathlib import Path

from playwright.sync_api import sync_playwright


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--run-root", required=True, type=Path,
                        help="prepared run directory holding request.json/ready.json")
    parser.add_argument("--port", type=int, default=None,
                        help="listener port; must equal request/ready port (default: request port)")
    parser.add_argument("--host", default="127.0.0.1", help="listener host (default 127.0.0.1)")
    parser.add_argument("--exe", required=True,
                        help="explicit Chromium executable used for this run")
    return parser.parse_args()


ARGS = parse_args()
B = str(Path(ARGS.run_root).resolve())
EXE = ARGS.exe


def preflight():
    """Validate the prepared root, ready file, listener port and form port
    before the first browser launch. Nothing is created here."""
    request = json.load(open(f"{B}/request.json", encoding="utf-8"))
    ready = json.load(open(f"{B}/ready.json", encoding="utf-8"))
    assert request["root"] == f"{B}/backend", request["root"]
    assert request["ready"] == f"{B}/ready.json", request["ready"]
    assert request["shutdown"] == f"{B}/shutdown", request["shutdown"]
    port = int(request["port"])
    assert ready["port"] == port, (ready, port)
    if ARGS.port is not None:
        assert ARGS.port == port, (ARGS.port, port)
    assert 1 <= port <= 65535, port
    assert Path(f"{B}/backend/downloads/beta/4.webp").is_file()
    assert Path(EXE).is_file(), EXE
    return request, ready, port


request, ready, PORT = preflight()
BASE = f"http://{ARGS.host}:{PORT}"
URL = BASE + "/ui/"
SECRET = "s3cret"

results = []
trace = []
click_proof = {}


def check(name, ok, detail=""):
    results.append({"check": name, "ok": bool(ok), "detail": detail})
    print(("PASS " if ok else "FAIL ") + name + (" :: " + str(detail) if detail else ""))
    return ok


def assert_form_port(dialog):
    """The connect form must point at the same listener the harness drives.

    The frozen UI initializes its port field to the hardcoded 8010, so for a
    non-8010 listener the field is filled with the real port first (a normal
    user action), then asserted.
    """
    inputs = dialog.locator("input.el-input__inner")
    values = inputs.evaluate_all("els=>els.map(e=>e.value)")
    assert len(values) >= 2, values
    if str(values[1]).strip() != str(PORT):
        inputs.nth(1).fill(str(PORT))
        values = inputs.evaluate_all("els=>els.map(e=>e.value)")
    assert str(values[1]).strip() == str(PORT), f"connect form port {values[1]!r} != listener port {PORT}"


def rpc(method, args, kwargs=None, ident=99):
    body = json.dumps({"method": method, "id": ident, "params": [args, kwargs or {}]}).encode()
    req = urllib.request.Request(BASE + "/jsonrpc", data=body,
                                 headers={"Content-Type": "application/json-rpc"})
    return json.loads(urllib.request.urlopen(req, timeout=10).read())


def task_states(level="!finished"):
    out = rpc("xeH.listTasks", ["token:" + SECRET, level])
    return {t["guid"]: t["state"] for t in out.get("result") or []}


def wait_state(guid, expected, timeout=8.0):
    deadline = time.time() + timeout
    state = None
    while time.time() < deadline:
        state = task_states().get(guid)
        if state == expected:
            return state
        time.sleep(0.25)
    return state


def wait_images(page, minimum):
    for _ in range(40):
        info = page.eval_on_selector_all(
            ".gallery-view img",
            "els=>els.map(e=>[e.src,e.naturalWidth,e.naturalHeight,e.complete])",
        )
        if len(info) >= minimum and all(item[1] > 0 for item in info):
            return info
        page.wait_for_timeout(250)
    return page.eval_on_selector_all(
        ".gallery-view img", "els=>els.map(e=>[e.src,e.naturalWidth,e.naturalHeight,e.complete])")


def visible_overlays(page):
    return page.evaluate(
        """() => {
          const visible = el => {
            const style = getComputedStyle(el);
            const rect = el.getBoundingClientRect();
            return style.display !== 'none' && style.visibility !== 'hidden'
              && Number(style.opacity || '1') !== 0 && rect.width > 1 && rect.height > 1;
          };
          return Array.from(document.querySelectorAll(
            '.el-loading-mask,.el-loading-spinner,.el-image__placeholder,.el-image__error'
          )).filter(visible).map(el => el.className);
        }"""
    )


def gallery_geometry(page):
    return page.evaluate(
        """() => {
          const img = document.querySelector('.gallery-view img');
          const dialog = document.querySelector('.gallery-view .el-dialog');
          const rect = img ? img.getBoundingClientRect() : null;
          const active = document.querySelector('.gallery-view .el-pager li.active');
          const dialogVisible = !!(dialog && dialog.offsetParent !== null
            && getComputedStyle(dialog).display !== 'none'
            && getComputedStyle(dialog).visibility !== 'hidden');
          return {
            screen_width: window.screen.width,
            screen_height: window.screen.height,
            screen_x: window.screenX,
            screen_y: window.screenY,
            inner_width: window.innerWidth,
            inner_height: window.innerHeight,
            device_pixel_ratio: window.devicePixelRatio,
            img_src: img ? img.src : null,
            img_complete: img ? img.complete : null,
            img_natural: img ? [img.naturalWidth, img.naturalHeight] : null,
            img_rect: rect ? {
              x: rect.x, y: rect.y, width: rect.width, height: rect.height,
              top: rect.top, left: rect.left, right: rect.right, bottom: rect.bottom
            } : null,
            page_active: active ? active.textContent.trim() : null,
            dialog_visible: dialogVisible,
            overlays: Array.from(document.querySelectorAll(
              '.el-loading-mask,.el-loading-spinner,.el-image__placeholder,.el-image__error'
            )).filter(el => {
              const style = getComputedStyle(el);
              const r = el.getBoundingClientRect();
              return style.display !== 'none' && style.visibility !== 'hidden'
                && Number(style.opacity || '1') !== 0 && r.width > 1 && r.height > 1;
            }).map(el => el.className),
            element_from_point: null,
          };
        }"""
    )


def connect(page):
    page.goto(URL, wait_until="networkidle")
    page.wait_for_timeout(600)
    dialog = page.locator(".el-dialog:visible").first
    if dialog.count():
        assert_form_port(dialog)
        dialog.locator("input.el-input__inner").nth(2).fill(SECRET)
        dialog.locator("button.el-button--primary").last.click()
        page.wait_for_timeout(2500)


def row(page, text):
    return page.locator("tr.el-table__row").filter(has_text=text)


def attach_trace(page):
    def on_response(resp):
        url = resp.url
        if not any(part in url for part in ("/jsonrpc", "/img/", "/zip/", "/ui/", "/static/")):
            return
        entry = {
            "url": url,
            "status": resp.status,
            "content_type": resp.headers.get("content-type"),
            "content_length": resp.headers.get("content-length"),
        }
        request = resp.request
        entry["method"] = request.method
        if "/jsonrpc" in url:
            entry["post"] = (request.post_data or "")[:1200]
            try:
                entry["body"] = resp.text()[:1500]
            except Exception as exc:
                entry["body_error"] = str(exc)
        trace.append(entry)

    page.on("response", on_response)


def stable_click(locator, label, attempts=10):
    """Click a row control at a freshly verified real hit target.

    The frozen table re-sorts on its 5s auto-refresh, so the control can move
    between locator resolution and the click, and a title-cell hover popover
    can cover the action column. Resolve the box, prove elementFromPoint
    lands on this control, click immediately with the real mouse, then park
    the pointer off the table.
    """
    for _ in range(attempts):
        box = locator.bounding_box()
        if box is not None:
            x = box["x"] + box["width"] / 2
            y = box["y"] + box["height"] / 2
            handle = locator.element_handle()
            same = page.evaluate(
                """([x, y, el]) => {
                  const hit = document.elementFromPoint(x, y);
                  if (!hit || !el) return false;
                  return hit === el || el.contains(hit) || hit.closest('button') === el
                    || hit.closest('a') === el;
                }""",
                [x, y, handle])
            if same:
                page.mouse.move(x, y)
                page.wait_for_timeout(80)
                page.mouse.click(x, y)
                page.mouse.move(10, 10)
                return
        page.mouse.move(10, 10)
        page.wait_for_timeout(300)
    raise AssertionError("could not hit " + label)


Path(f"{B}/downloads").mkdir(parents=True, exist_ok=True)

with sync_playwright() as p:
    browser = p.chromium.launch(
        headless=True, executable_path=EXE, args=["--no-sandbox"],
        downloads_path=f"{B}/downloads",
    )
    ctx = browser.new_context(locale="en-US", viewport={"width": 1440, "height": 1000},
                              screen={"width": 1440, "height": 1000},
                              accept_downloads=True)
    page = ctx.new_page()
    attach_trace(page)

    # 1) connect with a wrong secret: real 403, UI stays disconnected
    page.goto(URL, wait_until="networkidle")
    page.wait_for_timeout(600)
    dlg = page.locator(".el-dialog:visible").first
    check("connect.dialog", dlg.count() == 1, dlg.inner_text()[:80] if dlg.count() else "missing")
    assert_form_port(dlg)
    dlg.locator("input.el-input__inner").nth(2).fill("wrong")
    dlg.get_by_role("button", name="Confirm").click()
    page.wait_for_timeout(1200)
    check("auth.wrong.disconnected", "disconneted" in page.inner_text("body"))
    wrong_403 = [e for e in trace if e["url"].endswith("/jsonrpc") and e["status"] == 403]
    check("auth.wrong.403", len(wrong_403) >= 1, len(wrong_403))
    page.screenshot(path=f"{B}/01-wrong-auth.png", full_page=True)

    # 2) correct secret through the same dialog
    page.locator("header button").last.click()
    page.wait_for_timeout(500)
    dlg = page.locator(".el-dialog:visible").first
    assert_form_port(dlg)
    dlg.locator("input.el-input__inner").nth(2).fill(SECRET)
    dlg.get_by_role("button", name="Confirm").click()
    page.wait_for_timeout(2600)
    body = page.inner_text("body")
    check("connect.connected", "2.023-rss" in body and "disconneted" not in body)
    check("tasks.rows", page.locator("tr.el-table__row").count() == 3,
          page.locator("tr.el-table__row").count())
    for name in ("alpha", "beta", "gamma"):
        check(f"tasks.row.{name}", row(page, name).count() == 1)
    check("tasks.beta.finished-progress",
          row(page, "beta").locator(".el-progress").get_attribute("aria-valuenow") == "100")
    page.screenshot(path=f"{B}/02-connected.png", full_page=True)

    # 3) configuration round-trip and rpc_* hiding
    page.locator("header button").last.click()
    page.wait_for_timeout(500)
    dlg = page.locator(".el-dialog:visible").first
    dlg.locator(".el-tabs__item").nth(1).click()
    page.wait_for_timeout(600)
    values = dlg.locator("input.el-input__inner").evaluate_all("els=>els.map(e=>e.value)")
    basic = dlg.inner_text()
    check("config.basic.dir", f"{B}/backend/downloads" in values, values)
    check("config.basic.keys", "download_ori" in basic and "jpn_title" in basic and "make_archive" in basic)
    page.locator(".el-dialog:visible").first.get_by_role("button", name="Cancel").click()
    page.wait_for_timeout(300)
    config_doc = page.inner_text("body")
    check("config.hides.rpc", "rpc_port" not in config_doc and "rpc_secret" not in config_doc)
    cfg = rpc("xeH.getConfig", ["token:" + SECRET])["result"]
    check("config.getConfig.dir", cfg.get("dir", "").endswith("/backend/downloads"), cfg.get("dir"))
    check("config.getConfig.no-rpc", "rpc_port" not in cfg and "rpc_secret" not in cfg)
    page.screenshot(path=f"{B}/03-config.png", full_page=True)

    # 4) title / tag search and the tag column filter
    search = page.locator("input[placeholder='Type to search']")
    search.fill("Alpha")
    page.wait_for_timeout(400)
    check("search.title", page.locator("tr.el-table__row").count() == 1,
          page.locator("tr.el-table__row").count())
    search.fill("ベータ")
    page.wait_for_timeout(400)
    check("search.gjname", page.locator("tr.el-table__row").count() == 1)
    search.fill("artist:probe")
    page.wait_for_timeout(400)
    check("search.tag", page.locator("tr.el-table__row").count() == 2,
          page.locator("tr.el-table__row").count())
    page.screenshot(path=f"{B}/04-search.png", full_page=True)
    search.fill("")
    page.wait_for_timeout(400)
    page.locator(".el-table__column-filter-trigger").first.click()
    page.wait_for_timeout(400)
    panel = page.locator(".el-table-filter:visible").first
    panel.locator("label.el-checkbox").filter(has_text="language:japanese").click()
    panel.locator(".el-table-filter__bottom button").first.click()
    page.wait_for_timeout(500)
    check("filter.tag.japanese", page.locator("tr.el-table__row").count() == 1,
          page.locator("tr.el-table__row").count())
    page.screenshot(path=f"{B}/05-tag-filter.png", full_page=True)
    page.locator(".el-table__column-filter-trigger").first.click()
    page.wait_for_timeout(300)
    panel = page.locator(".el-table-filter:visible").first
    panel.locator(".el-table-filter__bottom button").nth(1).click()
    page.wait_for_timeout(500)
    check("filter.reset", page.locator("tr.el-table__row").count() == 3)
    # Element keeps the filter dropdown mounted after applying/resetting;
    # close it so it cannot intercept the following row clicks.
    page.keyboard.press("Escape")
    page.mouse.click(300, 20)
    page.wait_for_timeout(400)
    check("filter.closed", page.locator(".el-table-filter:visible").count() == 0,
          page.locator(".el-table-filter:visible").count())

    # 5) pause alpha, resume gamma: backend states and UI controls
    page.mouse.move(10, 10)
    stable_click(row(page, "alpha").locator("button").nth(1), "alpha.pause")
    paused = wait_state("alpha0001", 0)
    check("pause.alpha.state", paused == 0, task_states())
    page.wait_for_timeout(800)
    check("pause.alpha.ui-play", not row(page, "alpha").locator("button").nth(0).is_disabled())
    page.mouse.move(10, 10)
    stable_click(row(page, "gamma").locator("button").nth(0), "gamma.resume")
    resumed = wait_state("gamma003", 1)
    check("resume.gamma.state", resumed == 1, task_states())
    page.wait_for_timeout(800)
    check("resume.gamma.ui-play", row(page, "gamma").locator("button").nth(0).is_disabled())
    page.screenshot(path=f"{B}/06-pause-resume.png", full_page=True)

    # 6) finished / !finished negation really reached the wire
    posts = [e.get("post", "") for e in trace if "/jsonrpc" in e["url"]]
    check("lists.finished", any('"finished"' in post and "!finished" not in post for post in posts))
    check("lists.unfinished", any("!finished" in post for post in posts))

    # 7) gallery: double page, single page, navigation, last page
    page.mouse.move(10, 10)
    stable_click(row(page, "beta").locator("button").nth(0), "beta.gallery")
    page.wait_for_timeout(1200)
    check("gallery.opened", page.locator(".gallery-view .el-dialog").is_visible())
    info = wait_images(page, 2)
    check("gallery.double.images", len(info) == 2 and all(i[1] > 0 for i in info), info)
    if info:
        check("gallery.double.formats",
              info[0][0].endswith("/1/1.jpg") and info[1][0].endswith("/2/2.png"), info[:2])
    check("gallery.double.checkbox",
          page.locator(".gallery-view .el-checkbox").first.evaluate("e=>e.classList.contains('is-checked')"))
    page.screenshot(path=f"{B}/07-gallery-double.png", full_page=True)
    # Single page mode.
    page.locator(".gallery-view .el-checkbox").first.click()
    page.wait_for_timeout(500)
    info = wait_images(page, 1)
    check("gallery.single.images", len(info) == 1 and info[0][1] > 0, info)
    # Pager jump to page 2, then last page 4.
    page.locator(".gallery-view .el-pager li.number").nth(1).click()
    page.wait_for_timeout(500)
    info = wait_images(page, 1)
    check("gallery.nav.page2", len(info) == 1 and info[0][0].endswith("/2/2.png"), info)
    jumper = page.locator(".gallery-view .el-pagination__jump input")
    jumper.fill("4")
    jumper.press("Enter")
    page.wait_for_timeout(600)
    info = wait_images(page, 1)
    check("gallery.last.page", len(info) == 1 and info[0][0].endswith("/4/4.webp"), info)
    page.screenshot(path=f"{B}/08-gallery-single-last.png", full_page=True)
    # Clicking the right half on the last page closes the reader (reference
    # quirk). Wait for the real image/overlay state, measure the geometry the
    # original handler will see, then perform a real browser mouse click in
    # the right-half zone of the image itself.
    for _ in range(40):
        if not visible_overlays(page):
            break
        page.wait_for_timeout(100)
    page.wait_for_function(
        "() => { const img = document.querySelector('.gallery-view img');"
        " return img && img.complete && img.naturalWidth > 0; }",
        timeout=10000,
    )
    page.wait_for_timeout(300)
    before = gallery_geometry(page)
    box = page.locator(".gallery-view img").first.bounding_box()
    assert box is not None, "gallery image has no bounding box"
    x = box["x"] + box["width"] - 1.5
    y = box["y"] + box["height"] / 2
    hit = page.evaluate(
        """([x, y]) => {
          const el = document.elementFromPoint(x, y);
          return el ? {tag: el.tagName, cls: el.className, src: el.src || null} : null;
        }""",
        [x, y],
    )
    before["click_xy"] = [round(x, 2), round(y, 2)]
    before["click_zone"] = "next" if x >= before["screen_width"] / 2 else "prev"
    before["element_from_point"] = hit
    page.mouse.move(x, y)
    page.wait_for_timeout(120)
    page.mouse.click(x, y)
    page.wait_for_timeout(900)
    after = gallery_geometry(page)
    after["dialog_visible"] = page.locator(".gallery-view .el-dialog").is_visible()
    click_proof.update({"before": before, "after": after})
    check(
        "gallery.click-last-closes",
        not after["dialog_visible"],
        {"screen": before["screen_width"], "x": before["click_xy"][0],
         "zone": before["click_zone"], "active": before["page_active"],
         "before_visible": before["dialog_visible"],
         "after_visible": after["dialog_visible"]},
    )

    # 8) archive download through the original UI link
    page.mouse.move(10, 10)
    with page.expect_download(timeout=20000) as dl:
        stable_click(row(page, "beta").locator("a[download]"), "beta.archive")
    download = dl.value
    target = f"{B}/downloads/{download.suggested_filename}"
    download.save_as(target)
    check("zip.filename", download.suggested_filename == "beta.zip", download.suggested_filename)
    with zipfile.ZipFile(target) as archive:
        names = sorted(archive.namelist())
        comment = archive.comment.decode("utf-8", "replace")
        check("zip.entries", names == ["1.jpg", "2.png", "3.gif", "4.webp"], names)
        expected = {
            "1.jpg": f"{B}/backend/downloads/beta/1.jpg",
            "2.png": f"{B}/backend/downloads/beta/2.png",
            "3.gif": f"{B}/backend/downloads/beta/3.gif",
            "4.webp": f"{B}/backend/downloads/beta/4.webp",
        }
        same = all(archive.read(name) == open(path, "rb").read()
                   for name, path in expected.items())
        check("zip.contents", same)
        check("zip.comment", "Title:beta" in comment and "g/9002/abcdef0002" in comment, comment)
    zip_trace = [e for e in trace if "/zip/" in e["url"]]
    zip_url = download.url
    # Chrome serves downloads through the network service, outside the
    # renderer's response events: verify the exact signed URL the original
    # UI produced with one direct GET against the same real backend, and
    # compare those bytes with the browser-saved download.
    try:
        with urllib.request.urlopen(zip_url, timeout=15) as response:
            raw = response.read()
            status = response.status
            ctype = response.headers.get("Content-Type")
        check("zip.http", status == 200 and ctype == "application/zip"
              and raw == open(target, "rb").read(), {"url": zip_url, "status": status, "ctype": ctype})
        trace.append({"source": "download-url", "url": zip_url, "status": status,
                      "content_type": ctype, "content_length": str(len(raw)),
                      "browser_download": download.suggested_filename})
    except Exception as exc:
        check("zip.http", False, f"{zip_url}: {exc}")

    # 9) delete through the UI and verify the registry
    page.mouse.move(10, 10)
    stable_click(row(page, "alpha").locator("button").nth(2), "alpha.delete")
    page.wait_for_timeout(1000)
    check("delete.row", row(page, "alpha").count() == 0, page.locator("tr.el-table__row").count())
    check("delete.backend", "alpha0001" not in task_states())
    page.screenshot(path=f"{B}/09-after-delete.png", full_page=True)
    ctx.close()

    # 10) the same original UI in the two Chinese locales
    for locale, heading, progress, host in (
        ("zh-CN", "绅士WebUI", "进度", "主机名"),
        ("zh-TW", "紳士WebUI", "進度", "主機名"),
    ):
        lctx = browser.new_context(locale=locale, viewport={"width": 1440, "height": 1000},
                                   screen={"width": 1440, "height": 1000})
        lpage = lctx.new_page()
        connect(lpage)
        text = lpage.inner_text("body")
        check(f"locale.{locale}.heading", heading in text, text[:60])
        check(f"locale.{locale}.progress", progress in text)
        check(f"locale.{locale}.host", lpage.locator("input[placeholder]").first.get_attribute("placeholder") == host)
        check(f"locale.{locale}.tasks", lpage.locator("tr.el-table__row").count() == 2,
              lpage.locator("tr.el-table__row").count())
        lpage.screenshot(path=f"{B}/10-locale-{locale}.png", full_page=True)
        lctx.close()

    browser.close()

summary = {
    "ok": all(r["ok"] for r in results),
    "checks": results,
    "trace": trace,
    "click_proof": click_proof,
    "meta": {
        "base": BASE,
        "run_root": B,
        "port": PORT,
        "viewport": {"width": 1440, "height": 1000},
        "screen": {"width": 1440, "height": 1000},
        "executable": EXE,
        "check_count": len(results),
        "trace_count": len(trace),
        "generated": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    },
}
with open(f"{B}/browser-report.json", "w", encoding="utf-8") as handle:
    json.dump(summary, handle, indent=1)
print("SUMMARY", summary["ok"], sum(1 for r in results if r["ok"]), "/", len(results))
sys.exit(0 if summary["ok"] and len(results) == 49 else 1)
