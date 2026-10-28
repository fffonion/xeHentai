# SPDX-License-Identifier: GPL-3.0-or-later
"""Acceptance ids that the oracle must enumerate.

Executed cases may replace a template. A template that is not executed
stays unverified. None of these statuses claim the rewrite is implemented.
"""

UPSTREAM = "fffonion/xeHentai@43fc55c0c662f195d048becf6276a26ce06b09d9"
WEBUI = "fffonion/xeHentai-webui@4e3738ef463fef344e96df613de9a81b4cda4b4e"

SECTION4 = [
    ("behavior.sec4.config-defaults", "xeHentai/config.py", "25 configuration defaults"),
    ("behavior.sec4.config-priority", "xeHentai/cli.py", "built-in, user config, CLI, interactive priority"),
    ("behavior.sec4.rpc-hidden-keys", "xeHentai/rpc.py", "rpc_* and urls hidden from get/update views"),
    ("behavior.sec4.ignored-errors-merge", "xeHentai/core.py", "ignored_errors merge and user-config replacement"),
    ("behavior.sec4.cli-options", "xeHentai/cli.py", "CLI aliases, multiple URLs, help and version"),
    ("behavior.sec4.interactive", "xeHentai/cli.py", "interactive prompt order and defaults"),
    ("behavior.sec4.daemon-lifecycle", "xeHentai/cli.py", "URL exit, idle stay, POSIX daemon, signals"),
    ("behavior.sec4.logging", "xeHentai/util/logger.py", "log level, file and terminal, GMT+8"),
    ("behavior.sec4.task-states", "xeHentai/const.py", "public task state numbers"),
    ("behavior.sec4.guid", "xeHentai/task.py", "8-character GUID, no URL dedupe"),
    ("behavior.sec4.task-commands", "xeHentai/core.py", "add, delete, pause, resume and error codes"),
    ("behavior.sec4.session-shape", "xeHentai/core.py", "h.json tasks and cookies"),
    ("behavior.sec4.session-json-keys", "xeHentai/task.py", "integer keys and non-serializable maps"),
    ("behavior.sec4.save-tasks-false", "xeHentai/core.py", "save_tasks false still stores cookies"),
    ("behavior.sec4.legacy-cookie", "xeHentai/core.py", "legacy .ehentai.cookie import"),
    ("behavior.sec4.corrupt-session", "xeHentai/core.py", "corrupt h.json is kept and reported"),
    ("behavior.sec4.url-recognition", "xeHentai/core.py", "gallery URL recognition and exhentai login gate"),
    ("behavior.sec4.login-cookie", "xeHentai/core.py", "login, setCookie, default nw cookie"),
    ("behavior.sec4.metadata-filters", "xeHentai/filters.py", "metadata, page URL and MPV filters"),
    ("behavior.sec4.error-pages", "xeHentai/filters.py", "removed, exh-only, ban and quota pages"),
    ("behavior.sec4.reset-quota", "xeHentai/filters.py", "reset_quota remains a no-op"),
    ("behavior.sec4.download-workers", "xeHentai/core.py", "scan and download workers"),
    ("behavior.sec4.retry-policy", "xeHentai/worker.py", "retry, timeout and redirect limit"),
    ("behavior.sec4.range-parse", "xeHentai/cli.py", "download range including the ideographic comma"),
    ("behavior.sec4.naming", "xeHentai/task.py", "padding, original names and conflict suffix"),
    ("behavior.sec4.archive", "xeHentai/task.py", "STORE zip, ZIP64, comment, directory removal"),
    ("behavior.sec4.proxy-schemes", "xeHentai/proxy.py", "HTTP, HTTPS, SOCKS and Glype schemes"),
    ("behavior.sec4.proxy-pool", "xeHentai/proxy.py", "first available proxy, failure score, empty pool"),
    ("behavior.sec4.rpc-methods", "xeHentai/rpc.py", "13 public RPC methods"),
    ("behavior.sec4.rpc-wire", "xeHentai/rpc.py", "HTTP status, id, CORS and malformed body"),
    ("behavior.sec4.gallery-routes", "xeHentai/rpc.py", "image, zip and static routes"),
    ("behavior.sec4.hash-link", "xeHentai/rpc.py", "hash_link signature contract"),
    ("behavior.sec4.updater", "xeHentai/cli.py", "update check, download and next-start apply"),
    ("behavior.sec4.i18n", "xeHentai/i18n", "three locales, key sets and error maps"),
]

SECTION9 = [
    ("behavior.sec9.config-cli-daemon", "plan section 9", "configuration, CLI, interactive and POSIX daemon"),
    ("behavior.sec9.rpc-gallery", "plan section 9", "13 RPC methods and gallery routes"),
    ("behavior.sec9.task-session", "plan section 9", "task states, error codes and session migration"),
    ("behavior.sec9.site-rules", "plan section 9", "page parsers and active rules on RSS"),
    ("behavior.sec9.concurrency", "plan section 9", "parallel scan and download with cancellation"),
    ("behavior.sec9.naming-zip64", "plan section 9", "naming conflicts, archive and ZIP64"),
    ("behavior.sec9.proxy", "plan section 9", "listed proxy protocols and Glype"),
    ("behavior.sec9.gallery-race", "plan section 9", "gallery directory and zip reads, thumbnails, races"),
    ("behavior.sec9.webui", "plan section 9", "frozen WebUI browser flow"),
    ("behavior.sec9.updater", "plan section 9", "update download, next start and rollback"),
    ("behavior.sec9.i18n", "plan section 9", "i18n keys, placeholders and locale fallback"),
    ("behavior.sec9.python-config", "plan section 9", "dynamic Python config bridge"),
    ("behavior.sec9.platform-ci", "plan section 9", "Linux, macOS and Windows CI"),
    ("behavior.sec9.manifest-closed", "plan section 9", "manifest closed and delta ledger closed"),
    ("behavior.sec9.rss-body", "plan section 9", "application body is RSS"),
    ("behavior.sec9.core-reuse", "plan section 9", "Core host functions reused before new descriptors"),
]

DELTAS = [
    "delta.token-placement",
    "delta.update-config-proxy",
    "delta.get-image-range",
    "delta.falsy-id",
    "delta.token-regex-log",
    "delta.argparse-bool",
    "delta.download-range-default",
    "delta.proxy-availability",
    "delta.rename-key-types",
    "delta.scan-downloaded-comparison",
    "delta.unfinished-end",
    "delta.get-extra-newline",
    "delta.malformed-json",
    "delta.content-length",
    "delta.xehdone-zip-recovery",
]


def pending_feature(feature_id, source, summary):
    return {
        "id": feature_id,
        "source": source,
        "input": {"summary": summary},
        "expected": {
            "measured": False,
            "implementation": "pending",
            "reason": "not executed by the compatibility oracle in this scope",
        },
        "target": "pending",
        "tests": ["crates/xeh-xtask/tests/oracle_contract.rs"],
        "status": "unverified",
    }


def behavior_templates():
    items = []
    for feature_id, source, summary in SECTION4 + SECTION9:
        items.append(pending_feature(feature_id, source, summary))
    for feature_id in DELTAS:
        items.append(pending_feature(feature_id, UPSTREAM, feature_id))
    items.append(pending_feature("upstream.webui", WEBUI, "frozen WebUI revision is recorded and not executed"))
    return items


def behavior_ids():
    return [item["id"] for item in behavior_templates()]
