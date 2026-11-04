#!/usr/bin/env python3
"""One small probe for the M8 migration chain, plus its report verifier.

Two independent modes, one file, stdlib-only report validation:

  probe:   drive the ORIGINAL frozen WebUI against the real resident host of
           the migration chain: connect through the production dialog, open the
           archived gallery and require every reader image to decode from the
           real `/img/` destination, change `download_thread_cnt`, confirm the
           memory-only `getConfig` value returned by the real RPC server, and
           refuse any non-owned network destination. It writes one fresh JSON
           report only after its own checks passed.

  verify:  validate the Rust chain report (`m8-migration-chain-report-v1`)
           produced by the ignored test. Missing/false/wrong-type/extra checks,
           a stale run root, a missing B wire marker or missing archive/resource
           assertions are rejected; the process exits non-zero with a fixed
           safe message (never a raw credential or page body).

This is NOT the 49-check WebUI gate and never replaces it. All data here is
synthetic and the only network destinations are the owned loopback listener.
"""

import argparse
import json
import re
import sys
import time
import urllib.request
from pathlib import Path

CASE = (
    "tests::native_rss_update::migration_chain::"
    "native_legacy_session_to_archived_ui_and_rss_b_same_host"
)
REPORT_SCHEMA = "m8-migration-chain-report-v1"
PROBE_SCHEMA = "m8-migration-ui-probe-v1"
B_MARKER = "&m8=b"
PYTHON_REVISION = "43fc55c0c662f195d048becf6276a26ce06b09d9"
UPDATE_ID = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678"

REQUIRED_CHECKS = [
    "oracle.python_session_fixture",
    "config.python_import_first_launch",
    "phaseA.pending_while_running",
    "phaseA.concurrent_missing_bodies",
    "phaseA.rename_archive",
    "phaseA.ui_reader_and_setting",
    "phaseA.sigterm_graceful_exit",
    "phaseB.same_exe_ordinary_start",
    "phaseB.applied_v2_four_scripts",
    "phaseB.b_gh_plan_wire",
    "phaseB.rpc_saved_task_and_zip",
    "phaseB.preservation_window",
    "negative.single_worker_barrier",
    "cleanup.owned_roots",
]

UI_CHECKS = [
    "connect.dialog",
    "connect.port",
    "connect.secret",
    "tasks.row",
    "gallery.opened",
    "gallery.images.decode",
    "gallery.images.identity",
    "settings.tab",
    "settings.threads.input",
    "settings.getconfig.value",
    "settings.updateconfig.sent",
    "network.owned",
    "network.no_external",
]

TOP_LEVEL = {
    "schema",
    "case",
    "run_root",
    "source",
    "host",
    "rss",
    "archive",
    "resources",
    "ui_probe",
    "fixture",
    "negative",
    "checks",
    "cleanup",
}


class VerificationError(RuntimeError):
    """A report contract violation; never a product verdict."""


def _exact_keys(value, expected, what):
    if not isinstance(value, dict):
        raise VerificationError(f"{what} must be an object")
    if set(value) != set(expected):
        extra = sorted(set(value) - set(expected))
        missing = sorted(set(expected) - set(value))
        raise VerificationError(f"{what} keys mismatch extra={extra} missing={missing}")


def _check_list(value, expected_names, what):
    if not isinstance(value, list):
        raise VerificationError(f"{what} must be a list")
    if [item.get("name") if isinstance(item, dict) else None for item in value] != expected_names:
        raise VerificationError(f"{what} names must be exactly {expected_names}")
    for item in value:
        _exact_keys(item, {"name", "ok", "detail"}, f"{what} entry")
        if item["ok"] is not True:
            raise VerificationError(f"{what} entry {item['name']} is not ok")
        if not isinstance(item["detail"], str) or len(item["detail"]) > 400:
            raise VerificationError(f"{what} entry {item['name']} detail shape")


def verify_report(report, expected_root, require_clean=False):
    """Fail closed: any missing, extra, stale or mistyped field is an error."""
    _exact_keys(report, TOP_LEVEL, "report")
    if report["schema"] != REPORT_SCHEMA:
        raise VerificationError("schema identity")
    if report["case"] != CASE:
        raise VerificationError("case identity")
    root = Path(report["run_root"])
    if not root.is_absolute() or str(root) != str(expected_root):
        raise VerificationError("stale run root")
    source = report["source"]
    _exact_keys(source, {"head", "dirty", "worktree"}, "source")
    if not re.fullmatch(r"[0-9a-f]{40}", str(source["head"])):
        raise VerificationError("source head")
    if not isinstance(source["dirty"], bool):
        raise VerificationError("source dirty")
    if require_clean and source["dirty"]:
        raise VerificationError("source is dirty")
    if not isinstance(source["worktree"], str) or not source["worktree"]:
        raise VerificationError("source worktree")

    host = report["host"]
    _exact_keys(host, {"exe", "same_exe_path"}, "host")
    if host["exe"] != "xeHentai" or host["same_exe_path"] is not True:
        raise VerificationError("host executable identity")

    rss = report["rss"]
    _exact_keys(rss, {"files", "b_marker", "applied_update_id", "applied_v"}, "rss")
    expected_files = [
        "rss/filters/match.rss",
        "rss/rpc/handlers.rss",
        "rss/updater/github.rss",
        "rss/updater/updater.rss",
    ]
    if rss["files"] != expected_files:
        raise VerificationError("rss file set")
    if rss["b_marker"] != B_MARKER:
        raise VerificationError("missing B wire marker")
    if rss["applied_update_id"] != UPDATE_ID or rss["applied_v"] != 2:
        raise VerificationError("applied record identity")

    archive = report["archive"]
    _exact_keys(
        archive,
        {"zip_name", "entries", "compression", "four_images_byte_equal", "loose_dir_removed"},
        "archive",
    )
    if not isinstance(archive["entries"], list) or any(
        not isinstance(name, str) for name in archive["entries"]
    ):
        raise VerificationError("archive entries shape")
    required_entries = [f"unique-{fid}.jpg" for fid in (1, 2, 3, 4)]
    if not all(name in archive["entries"] for name in required_entries):
        raise VerificationError("archive is missing a renamed image")
    if archive["compression"] != "stored":
        raise VerificationError("archive compression")
    if archive["four_images_byte_equal"] is not True:
        raise VerificationError("archive image bytes")
    if archive["loose_dir_removed"] is not True:
        raise VerificationError("loose gallery directory")
    if not isinstance(archive["zip_name"], str) or not archive["zip_name"].endswith(".zip"):
        raise VerificationError("archive name")

    resources = report["resources"]
    _exact_keys(
        resources,
        {
            "ui_assets",
            "cache_fresh",
            "config_rss",
            "session_in_window",
            "exe_unchanged",
        },
        "resources",
    )
    if resources["ui_assets"] != 7:
        raise VerificationError("ui asset count")
    if resources["cache_fresh"] is not True:
        raise VerificationError("ui cache")
    if resources["config_rss"] != "unchanged":
        raise VerificationError("config.rss was changed")
    if resources["session_in_window"] != "unchanged":
        raise VerificationError("session changed during the RSS window")
    if resources["exe_unchanged"] is not True:
        raise VerificationError("host bytes changed")

    ui_probe = report["ui_probe"]
    _exact_keys(ui_probe, {"checks", "report"}, "ui_probe")
    _check_list(ui_probe["checks"], UI_CHECKS, "ui_probe.checks")
    probe_path = Path(ui_probe["report"])
    if not probe_path.is_absolute() or root not in probe_path.parents:
        raise VerificationError("ui probe report is outside the run root")
    if not probe_path.is_file():
        raise VerificationError("ui probe report is missing")

    fixture = report["fixture"]
    _exact_keys(
        fixture,
        {
            "python_source",
            "present",
            "missing",
            "image_requests",
            "old_image_requests",
            "barrier_distinct",
            "cookie_sent",
        },
        "fixture",
    )
    if fixture["python_source"] != PYTHON_REVISION:
        raise VerificationError("python oracle identity")
    if fixture["present"] != [1, 4] or fixture["missing"] != [2, 3]:
        raise VerificationError("fixture image identities")
    if fixture["image_requests"] != 2 or fixture["old_image_requests"] != 0:
        raise VerificationError("image request accounting")
    if fixture["barrier_distinct"] != 2:
        raise VerificationError("barrier overlap evidence")
    if fixture["cookie_sent"] is not True:
        raise VerificationError("session cookie observation")

    negative = report["negative"]
    _exact_keys(
        negative,
        {"attempted", "single_worker", "barrier_overlap", "barrier_timed_out", "clean_exit"},
        "negative",
    )
    if negative["attempted"] is not True or negative["single_worker"] is not True:
        raise VerificationError("single-worker negative did not run")
    if negative["barrier_overlap"] is not False:
        raise VerificationError("single-worker negative reported overlap")
    if negative["barrier_timed_out"] is not True:
        raise VerificationError("single-worker barrier did not expire")
    if negative["clean_exit"] is not True:
        raise VerificationError("single-worker host was not cleaned up")

    _check_list(report["checks"], REQUIRED_CHECKS, "checks")
    cleanup = report["cleanup"]
    _exact_keys(cleanup, {"install_retained", "temp_root"}, "cleanup")
    if cleanup["install_retained"] is not False:
        raise VerificationError("install root retained")


# --- probe -----------------------------------------------------------------


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--base", help="owned loopback origin, e.g. http://127.0.0.1:PORT")
    parser.add_argument("--port", type=int)
    parser.add_argument("--secret")
    parser.add_argument("--title")
    parser.add_argument("--guid")
    parser.add_argument("--exe", help="explicit Chromium executable")
    parser.add_argument("--report", type=Path, help="fresh probe report path")
    parser.add_argument("--verify-report", type=Path, help="validate a chain report")
    parser.add_argument("--expected-root", type=Path)
    parser.add_argument("--require-clean", action="store_true")
    return parser.parse_args(argv)


def ensure_fresh(path):
    path = path.resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    if path.exists():
        raise SystemExit(f"probe: refusing to reuse an existing report: {path}")
    return path


def write_report(path, payload):
    temporary = path.with_suffix(".tmp")
    temporary.write_text(json.dumps(payload, indent=1, sort_keys=True) + "\n", encoding="utf-8")
    temporary.replace(path)


def run_probe(args):
    from playwright.sync_api import sync_playwright

    base = args.base.rstrip("/")
    if not base.startswith("http://127.0.0.1:") or not base.endswith(str(args.port)):
        raise SystemExit("probe: --base must be the owned loopback origin of --port")
    report_path = ensure_fresh(args.report)
    secret = args.secret
    title = args.title
    guid = args.guid
    checks = []
    trace = []
    external = []
    default_refused = []

    def check(name, ok, detail=""):
        checks.append({"name": name, "ok": bool(ok), "detail": str(detail)[:300]})
        print(("PASS " if ok else "FAIL ") + name)
        return ok

    def record(route):
        url = route.request.url
        if url.startswith(base + "/") or url == base:
            trace.append(
                {
                    "url": url,
                    "method": route.request.method,
                    "post": (route.request.post_data or "")[:4000],
                }
            )
            route.continue_()
            return
        if url.startswith("data:") or url.startswith("about:"):
            route.continue_()
            return
        # The frozen original UI initializes its connect form to the hardcoded
        # 8010 and fires its first refresh there before any user input. That
        # destination is refused (never fulfilled) and is not an external
        # network destination; anything else non-owned is a failure.
        if url.startswith("http://127.0.0.1:8010/") or url.startswith("http://localhost:8010/"):
            default_refused.append(url.split("?", 1)[0])
            route.abort()
            return
        external.append(url.split("?", 1)[0])
        route.abort()

    def rpc(method, params):
        body = json.dumps({"method": method, "id": 7, "params": [params, {}]}).encode()
        request = urllib.request.Request(
            base + "/jsonrpc", data=body, headers={"Content-Type": "application/json-rpc"}
        )
        return json.loads(urllib.request.urlopen(request, timeout=10).read())

    def wait_image(page):
        for _ in range(40):
            info = page.eval_on_selector_all(
                ".gallery-view img",
                "els=>els.map(e=>[e.src,e.naturalWidth,e.naturalHeight,e.complete])",
            )
            if len(info) == 1 and info[0][1] > 0 and info[0][3]:
                return info[0]
            page.wait_for_timeout(200)
        return None

    screenshots = report_path.parent / "probe-gallery.png"
    browser = None
    try:
        with sync_playwright() as playwright:
            browser = playwright.chromium.launch(
                headless=True, executable_path=args.exe, args=["--no-sandbox"]
            )
            context = browser.new_context(
                locale="en-US",
                viewport={"width": 1440, "height": 1000},
                screen={"width": 1440, "height": 1000},
            )
            page = context.new_page()
            page.route("**/*", record)
            page.goto(base + "/ui/", wait_until="networkidle")
            page.wait_for_timeout(600)
            dialog = page.locator(".el-dialog:visible").first
            check("connect.dialog", dialog.count() == 1)
            inputs = dialog.locator("input.el-input__inner")
            values = inputs.evaluate_all("els=>els.map(e=>e.value)")
            if len(values) >= 2 and str(values[1]).strip() != str(args.port):
                inputs.nth(1).fill(str(args.port))
                values = inputs.evaluate_all("els=>els.map(e=>e.value)")
            check("connect.port", len(values) >= 2 and str(values[1]).strip() == str(args.port),
                  values[1] if len(values) > 1 else None)
            inputs.nth(2).fill(secret)
            check("connect.secret", inputs.nth(2).input_value() == secret)
            dialog.locator("button.el-button--primary").last.click()
            page.wait_for_timeout(2600)
            body = page.inner_text("body")
            rows = page.locator("tr.el-table__row")
            check(
                "tasks.row",
                "disconneted" not in body and rows.filter(has_text=title).count() == 1,
                rows.count(),
            )

            rows.filter(has_text=title).locator("button").nth(0).click()
            page.wait_for_timeout(1200)
            gallery = page.locator(".gallery-view .el-dialog")
            check("gallery.opened", gallery.is_visible())
            checkbox = page.locator(".gallery-view .el-checkbox").first
            if "is-checked" in (checkbox.get_attribute("class") or ""):
                checkbox.click()
                page.wait_for_timeout(400)
            jumper = page.locator(".gallery-view .el-pagination__jump input")
            images = {}
            for number in range(1, 5):
                jumper.fill(str(number))
                jumper.press("Enter")
                item = wait_image(page)
                if item is None:
                    break
                if item[1] > 0:
                    images[number] = (item[0], item[1], item[2])
            check(
                "gallery.images.decode",
                len(images) == 4 and all(dim > 0 for _, width, dim in images.values()),
                {number: [value[1], value[2]] for number, value in images.items()},
            )
            expected_names = [f"unique-{fid}.jpg" for fid in (1, 2, 3, 4)]
            check(
                "gallery.images.identity",
                len(images) == 4
                and all(
                    base + "/img/" in images[number][0]
                    and f"/{guid}/" in images[number][0]
                    and images[number][0].endswith(expected_names[number - 1])
                    for number in images
                ),
                sorted(images),
            )
            page.screenshot(path=str(screenshots), full_page=True)
            # Close the reader before touching the header: the original dialog
            # keeps a full-screen overlay that intercepts clicks.
            closer = page.locator(".gallery-view .el-dialog__headerbtn")
            if closer.count():
                closer.first.click()
            else:
                page.keyboard.press("Escape")
            page.wait_for_timeout(900)

            page.locator("header button").last.click()
            page.wait_for_timeout(500)
            settings = page.locator(".el-dialog:visible").first
            settings.locator(".el-tabs__item").nth(2).click()
            page.wait_for_timeout(600)
            check("settings.tab", settings.locator(".el-tabs__item").nth(2).get_attribute(
                "aria-selected") == "true")
            item = settings.locator(".el-form-item").filter(has_text="download_thread_cnt")
            if item.count() and not item.first.is_visible():
                settings.locator(".el-tabs__item").nth(2).click()
                page.wait_for_timeout(300)
            field = item.locator("input")
            field.fill("3")
            field.press("Enter")
            page.wait_for_timeout(300)
            check(
                "settings.threads.input",
                field.input_value() == "3" and item.count() == 1,
                field.input_value(),
            )
            settings.get_by_role("button", name="Confirm").click()
            page.wait_for_timeout(900)
            config = rpc("xeH.getConfig", ["token:" + secret]).get("result") or {}
            check("settings.getconfig.value", config.get("download_thread_cnt") == 3,
                  config.get("download_thread_cnt"))
            posts = [entry["post"] for entry in trace if entry["url"].endswith("/jsonrpc")]
            check(
                "settings.updateconfig.sent",
                any("updateConfig" in post and "download_thread_cnt" in post for post in posts),
            )
            check("network.owned", len(trace) > 0 and len(external) == 0,
                  {"continued": len(trace), "default_port_refused": len(default_refused)})
            check("network.no_external", not external, external[:3])
            context.close()
    finally:
        if browser is not None:
            try:
                browser.close()
            except Exception:
                pass

    ok = all(item["ok"] for item in checks)
    report = {
        "schema": PROBE_SCHEMA,
        "case": "m8-migration-ui-probe",
        "base_origin": base,
        "port": args.port,
        "guid": guid,
        "checks": checks,
        "screenshot": str(screenshots),
    }
    write_report(report_path, report)
    print(json.dumps({"probe": "ok" if ok else "failed", "checks": len(checks),
                      "external": external[:3]}, indent=1))
    return 0 if ok else 1


def main(argv=None):
    args = parse_args(argv)
    if args.verify_report:
        if args.expected_root is None:
            raise SystemExit("verify: --expected-root is required")
        try:
            report = json.loads(args.verify_report.read_text(encoding="utf-8"))
            verify_report(report, args.expected_root.resolve(), args.require_clean)
        except (OSError, ValueError, VerificationError) as error:
            print(f"migration report: REJECTED ({error})")
            return 1
        print("migration report: ACCEPTED")
        return 0
    if not (args.base and args.port and args.secret and args.title and args.guid):
        raise SystemExit("probe: --base/--port/--secret/--title/--guid are required")
    if not args.exe:
        raise SystemExit("probe: --exe is required")
    if args.report is None:
        raise SystemExit("probe: --report is required")
    return run_probe(args)


if __name__ == "__main__":
    sys.exit(main())
