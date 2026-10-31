#!/usr/bin/env python3
"""Isolated behaviour comparator for the xeHentai rewrite.

This is a development tool, not part of the shipped application. It runs the
fixed Python reference from the exported upstream tree in an isolated root and
compares its real output with the real Rust entry points. The Rust side is
reached through the environment-driven `tests::compat_probe` test; no product
debug CLI or second business implementation exists.

Examples:
    XEH_UPSTREAM=/path/to/upstream \
    CARGO_TARGET_DIR=/path/to/target \
    python3 tools/compat.py --suite config --output /fresh/evidence/config

    python3 tools/compat.py --verify-all --evidence /fresh/evidence
"""

from __future__ import annotations

import argparse
import copy
import json
import os
import re
import shutil
import subprocess
import sys
from datetime import datetime
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
CASES_FILE = REPO / "tools" / "cases.json"
CONFIG_FIELDS = [
    "daemon",
    "dir",
    "download_ori",
    "jpn_title",
    "rename_ori",
    "proxy",
    "proxy_image",
    "proxy_image_only",
    "rpc_interface",
    "rpc_port",
    "rpc_secret",
    "rpc_open_browser",
    "make_archive",
    "download_range",
    "scan_thread_cnt",
    "download_thread_cnt",
    "download_timeout",
    "ignored_errors",
    "log_path",
    "log_verbose",
    "save_tasks",
    "delete_task_files",
    "low_speed_threshold",
    "auto_update",
    "update_beta_channel",
]

# Independently written expectation for the 25 reference defaults.
EXPECTED_DEFAULTS = {
    "daemon": False,
    "dir": ".",
    "download_ori": False,
    "jpn_title": True,
    "rename_ori": False,
    "proxy": [],
    "proxy_image": True,
    "proxy_image_only": False,
    "rpc_interface": "localhost",
    "rpc_port": None,
    "rpc_secret": None,
    "rpc_open_browser": True,
    "make_archive": False,
    "download_range": None,
    "scan_thread_cnt": 1,
    "download_thread_cnt": 5,
    "download_timeout": 10,
    "ignored_errors": [],
    "log_path": "eh.log",
    "log_verbose": 2,
    "save_tasks": False,
    "delete_task_files": False,
    "low_speed_threshold": 10,
    "auto_update": "download",
    "update_beta_channel": False,
}

CONFIG_FIXTURE = """# fixed import fixture; the same text is executed by Python and imported by Rust
dir = "downloads"
download_thread_cnt = 7
download_ori = True
jpn_title = False
proxy = ["socks5://127.0.0.1:1080", "http://proxy.example:8080"]
ignored_errors = [1006, 1004]
download_range = [[1, 5], [8, 8]]
rpc_interface = '127.0.0.1'
rpc_port = 8765
rpc_secret = "se\\"cret"  # quote and comment
log_path = "C:\\\\temp\\\\eh.log"
log_verbose = 3
low_speed_threshold = 0
download_timeout = 30
auto_update = "check"
"""

EXPECTED_OVERRIDES = {
    "dir": "downloads",
    "download_thread_cnt": 7,
    "download_ori": True,
    "jpn_title": False,
    "proxy": ["socks5://127.0.0.1:1080", "http://proxy.example:8080"],
    "ignored_errors": [1006, 1004],
    "download_range": [[1, 5], [8, 8]],
    "rpc_interface": "127.0.0.1",
    "rpc_port": 8765,
    "rpc_secret": 'se"cret',
    "log_path": "C:\\temp\\eh.log",
    "log_verbose": 3,
    "low_speed_threshold": 0,
    "download_timeout": 30,
    "auto_update": "check",
}

COOKIE_TEXTS = [
    "a=1; secure; b=x=y; Path=/; expires=Wed; c=2; c=3; HttpOnly; Domain=x; max-age=9; comment=c",
    "",
    "secure=1",
    "  k = v  ",
]

HUMAN_TIMES = [
    "1 day 2 hours 3 minutes",
    "1 day",
    "2 hours",
    "5 minutes",
    "nothing",
    "1days",
    "10 days 5 minutes",
]

HTML_TEXTS = [
    "&amp;",
    "&nbsp;",
    "&#38;",
    "&bogus;",
    "&amp;lt;",
    "&copy; &reg; &quot; &lt; &gt;",
    "plain text",
    "&#x27;",
]

PATHS = [
    "a/b:c*d?e\\f",
    "中文/标题:01",
    'a/b:c*d?"e<f>g|h\'',
    "name... ",
    "name...",
    "a:b",
]

HUMAN_SIZES = [0, 1, 1000, 1001, 1024, 1536, 1_000_000, 1_048_576, 9223372036854775807]

ERROR_ARGS = {
    1010: ("5 minutes",),
    1300: ("downloads",),
    1301: ("gallery.zip",),
    1302: ("9-1",),
}

I18N_ERROR_CODES = [
    1000,
    1001,
    1002,
    1003,
    1004,
    1007,
    1008,
    1009,
    1010,
    1012,
    1013,
    1006,
    1101,
    1104,
    1105,
    1106,
    1107,
    1300,
    1301,
    1302,
    -32700,
    -32600,
    -32601,
    -32602,
    1200,
    -32603,
    1103,
    424242,
    -1,
]

LOCALE_TAGS = [("en_us", "en_us"), ("zh_CN", "zh_hans"), ("zh_TW", "zh_hant")]

PLACEHOLDER = re.compile(r"%\(default\)[sd]|%\.[0-9]f|%[sd]|\{\}")

# Cargo test names that prove cases which have no Python oracle.
NATIVE_CASE_MAP = {
    "config": {
        "config::tests::rss_encoding_round_trips_every_field_without_losing_false_zero_or_empty": [
            "config.rss.roundtrip"
        ],
        "config::tests::load_prefers_rss_and_never_opens_python": [
            "config.rss.priority",
            "config.rss.open-audit",
        ],
        "config::tests::broken_rss_does_not_fall_back_to_valid_python": [
            "config.rss.no-python-fallback"
        ],
        "config::tests::first_import_writes_rss_and_leaves_python_byte_identical": [
            "config.rss.first-import",
            "config.rss.source-unchanged",
        ],
        "config::tests::missing_configuration_writes_defaults_once": [
            "config.rss.defaults-create"
        ],
        "config::tests::failed_import_leaves_no_partial_rss": [
            "config.rss.no-partial-file"
        ],
        "config::tests::save_replaces_only_rss_and_reloads_identically": [
            "config.rss.save-atomic"
        ],
        "config::tests::failed_save_preserves_the_previous_rss": [
            "config.rss.save-failure-preserves"
        ],
        "config::tests::creating_a_new_rss_does_not_clobber_an_existing_valid_one": [
            "config.rss.concurrent-create"
        ],
        "config::tests::concurrent_first_import_keeps_one_complete_valid_rss": [
            "config.rss.concurrent-create"
        ],
        "config::tests::rss_decoding_reports_compile_runtime_and_shape_errors": [
            "config.rss.invalid-shape"
        ],
        "config::tests::config_module_cannot_spawn_a_process": [
            "config.rss.deny-process"
        ],
        "config::tests::config_module_cannot_write_files": [
            "config.rss.deny-file-write"
        ],
        "config::tests::config_module_cannot_call_standard_host_functions": [
            "config.rss.deny-host-imports"
        ],
        "config::tests::config_module_cannot_block_on_host_sleep": [
            "config.rss.deny-host-imports"
        ],
        "config::tests::config_module_cannot_reach_the_network": [
            "config.rss.deny-network"
        ],
        "config::tests::hostile_rss_never_reads_python_and_keeps_the_previous_configuration": [
            "config.rss.deny-loader"
        ],
        "config::tests::first_create_revalidates_an_existing_hostile_rss_without_side_effects": [
            "config.rss.deny-revalidation"
        ],
        "config::tests::python_import_rejects_everything_outside_the_subset": [
            "config.import.rejects-out-of-subset",
            "config.import.unknown-key",
            "config.import.range-validation",
            "delta.python-import-subset",
        ],
        "config::tests::python_import_enforces_the_input_budget": [
            "config.import.input-budget"
        ],
        "config::tests::python_import_handles_quotes_comments_crlf_and_multiline_arrays": [
            "config.import.quotes-comments",
            "config.import.multiline-arrays",
            "config.import.duplicate-key",
        ],
        "config::tests::python_import_allows_empty_values_and_trailing_commas": [
            "config.import.empty-values"
        ],
        "tests::migration_then_save_then_reload_survives_python_changes": [
            "config.rss.lifecycle"
        ],
        "tests::entry_point_initializes_configuration_and_log": [
            "main.config-init",
            "main.honest-exit",
        ],
        "tests::entry_point_imports_legacy_python_once": ["main.config-init"],
        "tests::rust_and_rss_sources_stay_in_separate_directories": [
            "structure.src-rss-separation"
        ],
        "tests::sdk_exports_named_structs_with_optional_and_array_fields": [
            "config.rss.sdk-shape"
        ],
        "r#const::tests::task_states_match_reference_values": [
            "const.state.paused",
            "const.state.waiting",
            "const.state.get_meta",
            "const.state.scan_page",
            "const.state.scan_img",
            "const.state.scan_archive",
            "const.state.download",
            "const.state.make_archive",
            "const.state.finished",
            "const.state.failed",
        ],
        "r#const::tests::error_codes_match_reference_values": [
            "const.error.no_error",
            "const.error.url_not_recognized",
            "const.error.cant_download_exh",
            "const.error.only_visible_exh",
            "const.error.malformed_hathdl",
            "const.error.gallery_removed",
            "const.error.image_resampled",
            "const.error.quota_exceeded",
            "const.error.key_expired",
            "const.error.no_pageurl_found",
            "const.error.connection_error",
            "const.error.ip_banned",
            "const.error.hath_not_found",
            "const.error.image_broken",
            "const.error.scan_regex_failed",
            "const.error.stream_not_implemented",
            "const.error.task_not_found",
            "const.error.save_session_failed",
            "const.error.task_level_undef",
            "const.error.delete_running_task",
            "const.error.task_cannot_pause",
            "const.error.task_cannot_resume",
            "const.error.rpc_unauthorized",
            "const.error.cannot_create_dir",
            "const.error.cannot_make_archive",
            "const.error.not_range_format",
            "const.error.rpc_parse_error",
            "const.error.rpc_invalid_request",
            "const.error.rpc_method_not_found",
            "const.error.rpc_invalid_params",
            "const.error.rpc_exec_error",
        ],
        "r#const::tests::every_translatable_error_code_has_a_key_and_no_extra_codes": [
            "const.err-key-map"
        ],
        "r#const::tests::version_is_printed_with_the_implementation_marker": [
            "delta.version-string"
        ],
    },
    "util": {
        "util::tests::cookie_parsing_matches_reference_rules": ["util.parse_cookie"],
        "util::tests::cookie_serialization_keeps_order_and_equals": ["util.make_cookie"],
        "util::tests::proxy_policy_matches_reference_precedence": ["util.get_proxy_policy"],
        "util::tests::human_time_matches_reference_regexes": ["util.parse_human_time"],
        "util::tests::html_escaping_matches_reference_and_is_single_pass": [
            "util.htmlescape",
            "delta.htmlescape-numeric-error",
        ],
        "util::tests::legal_path_filters_the_posix_set": ["util.legalpath.posix"],
        "util::tests::legal_path_filters_the_windows_set_and_trailing_dots": [
            "util.legalpath.windows"
        ],
        "util::tests::human_size_matches_reference_boundaries": ["util.human_size"],
        "util::tests::user_agent_keeps_the_reference_implicit_none_return": [
            "util.make_ua"
        ],
        "util::logger::tests::stdout_format_matches_the_reference": [
            "logger.stdout-format"
        ],
        "util::logger::tests::log_file_format_is_gmt8_with_month_name": [
            "logger.file-format"
        ],
        "util::logger::tests::set_level_disables_verbose_then_debug_then_info": [
            "logger.levels"
        ],
        "util::logger::tests::verbosity_uses_the_reference_warning_minus_value_expression": [
            "logger.levels"
        ],
        "util::logger::tests::tty_output_uses_the_reference_ansi_colors": [
            "logger.tty-colors"
        ],
    },
    "i18n": {
        "i18n::tests::locale_tags_map_like_the_reference": ["i18n.locale-mapping"],
        "i18n::tests::only_the_declared_locales_are_available": [
            "i18n.available-locales"
        ],
        "i18n::tests::messages_render_in_each_language": ["i18n.interpolation"],
        "i18n::tests::error_code_messages_and_numeric_fallback_match": [
            "i18n.nomsg"
        ],
        "i18n::tests::missing_keys_fall_back_to_english_then_none": ["i18n.fallback"],
        "i18n::tests::concurrent_lookups_do_not_share_locale_state": [
            "i18n.concurrent-isolation"
        ],
        "i18n::tests::every_reference_key_exists_in_all_three_locales": [
            "i18n.keys.all"
        ],
    },
    "task-api": {},
    "session": {},
    "files": {},
    "worker": {
        "worker::tests::one_worker_processes_items_sequentially": [
            "worker.single-persistent"
        ],
        "worker::tests::two_workers_have_requests_in_flight_at_the_same_time": [
            "worker.two-in-flight"
        ],
        "worker::tests::cancellation_waits_for_the_task_and_closes_the_connection": [
            "worker.cancel-join-connection"
        ],
    },
}

NATIVE_COMMANDS = {
    "config": [
        ["cargo", "test", "--bin", "xeHentai", "config::tests"],
        ["cargo", "test", "--bin", "xeHentai", "tests::"],
        ["cargo", "test", "--bin", "xeHentai", "r#const::tests"],
    ],
    "util": [["cargo", "test", "--bin", "xeHentai", "util::"]],
    "i18n": [["cargo", "test", "--bin", "xeHentai", "i18n::tests"]],
    "worker": [["cargo", "test", "--bin", "xeHentai", "worker::tests"]],
    "task-api": [
        ["cargo", "test", "--bin", "xeHentai", "tests::task_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::finished_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::guid_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::migrate_"],
    ],
    "session": [
        ["cargo", "test", "--bin", "xeHentai", "tests::session_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::legacy_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::mixed_"],
    ],
    "files": [
        ["cargo", "test", "--bin", "xeHentai", "tests::scan_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::duplicate_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::page_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::streaming_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::rename_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::archive_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::zip64_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::path_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::deleting_"],
    ],
}

TASK_API_CASES = {
    "task.create-config-snapshot": ["snapshot"],
    "task.state-api": ["states"],
    "task.guid": ["collision"],
    "task.resume": ["resumes"],
    "task.api-methods": ["methods", "methods_exceptions"],
}
SESSION_DELTA_SCENARIOS = {"dir_authorization"}
SESSION_CASES = {
    "task.session-save": ["roundtrip", "save_false"],
    "task.session-load": [
        "roundtrip",
        "bad_json",
        "missing_tasks",
        "missing_cookies",
        "duplicate_load",
        "state10",
        "migrate",
    ],
    "task.session-cookies": ["cookies", "logged_in"],
    "task.session-atomic": ["save_false"],
    "task.session-dir-authorization": ["dir_authorization"],
}
FILES_CASES = {
    "task.files-scan-downloaded": ["scan", "scan_markers"],
    "task.duplicates": ["duplicates"],
    "task.naming": ["naming", "rename_ori", "padding", "done_marker"],
    "task.files-rename-crash": ["rename_crash"],
    "task.files-stream": ["save"],
    "task.archive": ["archive"],
    "task.files-zip64": ["zip64"],
    "task.files-safety": ["archive_symlink", "path_safety"],
}

FILES_DELTA_SCENARIOS = {"archive_symlink", "path_safety"}


class CompatError(RuntimeError):
    pass


def log(message):
    print(message, flush=True)


def load_cases():
    if not CASES_FILE.is_file():
        raise CompatError(f"missing {CASES_FILE}")
    return json.loads(CASES_FILE.read_text(encoding="utf-8"))


def compare(expected, actual, path="root"):
    """Return human-readable differences; exact types and array order matter."""
    diffs = []
    if type(expected) is not type(actual):
        return [f"{path}: type {type(expected).__name__} != {type(actual).__name__}"]
    if isinstance(expected, dict):
        for key in sorted(set(expected) | set(actual)):
            if key not in expected:
                diffs.append(f"{path}.{key}: unexpected")
            elif key not in actual:
                diffs.append(f"{path}.{key}: missing")
            else:
                diffs.extend(compare(expected[key], actual[key], f"{path}.{key}"))
    elif isinstance(expected, list):
        if len(expected) != len(actual):
            diffs.append(f"{path}: length {len(expected)} != {len(actual)}")
        for index, (left, right) in enumerate(zip(expected, actual)):
            diffs.extend(compare(left, right, f"{path}[{index}]"))
    elif expected != actual:
        diffs.append(f"{path}: {expected!r} != {actual!r}")
    return diffs


def placeholder_args(template):
    """Build (probe string args, Python args, formatting kind) for a template."""
    probe = []
    py = []
    named = False
    percent = False
    formatted = False
    for match in PLACEHOLDER.finditer(template):
        token = match.group(0)
        if token.startswith("%("):
            named = True
            percent = True
            if token.endswith("d"):
                py.append(7)
                probe.append("7")
            else:
                py.append("VALUE")
                probe.append("VALUE")
        elif token.startswith("%."):
            percent = True
            value = 1.25
            py.append(value)
            probe.append(f"{value:.1f}")
        elif token == "%s":
            percent = True
            py.append("X")
            probe.append("X")
        elif token == "%d":
            percent = True
            py.append(7)
            probe.append("7")
        elif token == "{}":
            formatted = True
            py.append("Y")
            probe.append("Y")
    kind = "named" if named else ("percent" if percent else ("format" if formatted else "plain"))
    return probe, py, kind


def format_template(template, py_args, kind):
    if kind == "named":
        return template % {"default": py_args[0]}
    if kind == "percent":
        return template % tuple(py_args)
    if kind == "format":
        return template.format(*py_args)
    return template


def clean_env(root):
    env = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(root / "home"),
        "XDG_CACHE_HOME": str(root / "cache"),
        "TMPDIR": str(root / "tmp"),
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "PYTHONIOENCODING": "utf-8",
        "PYTHONDONTWRITEBYTECODE": "1",
    }
    (root / "home").mkdir(parents=True, exist_ok=True)
    (root / "cache").mkdir(parents=True, exist_ok=True)
    (root / "tmp").mkdir(parents=True, exist_ok=True)
    return env


def run_command(argv, cwd, env, log_path):
    log(f"$ {' '.join(argv)}")
    result = subprocess.run(
        argv,
        cwd=str(cwd),
        env=env,
        capture_output=True,
        text=True,
        timeout=900,
    )
    log_path.write_text(
        f"argv: {argv}\nexit: {result.returncode}\n--- stdout ---\n{result.stdout}\n--- stderr ---\n{result.stderr}\n",
        encoding="utf-8",
    )
    return result


def parse_test_results(output):
    results = {}
    for line in output.splitlines():
        match = re.match(r"^test ([\w:#]+) \.\.\. (ok|FAILED|ignored)$", line)
        if match:
            results[match.group(1)] = match.group(2)
    summary = re.search(
        r"test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored", output
    )
    if not summary:
        return results, None
    return results, {
        "status": summary.group(1),
        "passed": int(summary.group(2)),
        "failed": int(summary.group(3)),
        "ignored": int(summary.group(4)),
    }


def require_native(run, suite):
    results, summary = parse_test_results(run["stdout"])
    if summary is None:
        raise CompatError(f"{suite}: cargo output has no test summary")
    if summary["status"] != "ok" or summary["failed"] != 0 or summary["passed"] == 0:
        raise CompatError(f"{suite}: native tests failed: {summary}")
    return results, summary


def oracle_main(args):
    upstream = Path(args.upstream).resolve()
    output = Path(args.output)
    if not (upstream / "xeHentai" / "config.py").is_file():
        raise CompatError(f"invalid upstream tree: {upstream}")
    request = json.loads(Path(args.input).read_text(encoding="utf-8"))
    if args.oracle == "config":
        result = oracle_config(upstream, request)
    elif args.oracle == "util":
        result = oracle_util(upstream, request)
    elif args.oracle == "i18n":
        result = oracle_i18n(upstream, request, args.locale)
    elif args.oracle == "task-api":
        result = oracle_task_api(upstream, request)
    elif args.oracle == "session":
        result = oracle_session(upstream, request)
    elif args.oracle == "session-fixture":
        result = oracle_session_fixture(upstream, request)
    elif args.oracle == "files":
        result = oracle_files(upstream, request)
    else:
        raise CompatError(f"unknown oracle suite {args.oracle}")
    output.write_text(
        json.dumps(result, ensure_ascii=False, indent=2, sort_keys=True),
        encoding="utf-8",
    )


def execute_python_config(text, filename, require_all=True):
    namespace = {"__name__": "__xeh_oracle__", "__file__": filename}
    exec(compile(text, filename, "exec"), namespace)
    if require_all:
        return {field: namespace.get(field) for field in CONFIG_FIELDS}
    return {field: namespace[field] for field in CONFIG_FIELDS if field in namespace}


def oracle_config(upstream, request):
    path = upstream / "xeHentai" / "config.py"
    defaults = execute_python_config(path.read_text(encoding="utf-8"), str(path))
    fixture_path = Path(request["fixture_path"])
    overrides = execute_python_config(
        fixture_path.read_text(encoding="utf-8"), str(fixture_path), require_all=False
    )
    # The reference application starts from config.py defaults and lets the
    # user module override fields; a partial module is the same semantic input
    # as the Rust importer's defaults overlay.
    imported = dict(defaults)
    imported.update(overrides)
    return {"defaults": defaults, "imported": imported}


def oracle_util(upstream, request):
    sys.path.insert(0, str(upstream))
    import os as os_module
    import re as re_module

    import xeHentai.util as util

    inputs = request["inputs"]
    cookies = [list(util.parse_cookie(text).items()) for text in inputs["cookie_texts"]]
    cookie_texts = [util.make_cookie(dict(util.parse_cookie(text))) for text in inputs["cookie_texts"]]
    human_time = [util.parse_human_time(text) for text in inputs["human_times"]]
    html = []
    for text in inputs["html"]:
        try:
            html.append({"ok": True, "value": util.htmlescape(text)})
        except ValueError as error:
            html.append({"ok": False, "error": str(error)})
    legalpath_posix = [util.legalpath(text) for text in inputs["paths"]]
    windows_filter = re_module.compile("[|:?\\\\/*'\"<>]|\\.+(?:$)")
    original_filter = util.filename_filter
    original_name = os_module.name
    try:
        util.filename_filter = windows_filter
        os_module.name = "nt"
        legalpath_windows = [util.legalpath(text) for text in inputs["paths"]]
    finally:
        util.filename_filter = original_filter
        os_module.name = original_name
    human_size = [util.human_size(size) for size in inputs["human_sizes"]]
    policies = []
    for proxy_image_only, proxy_image in ((True, True), (True, False), (False, True), (False, False)):
        policy = util.get_proxy_policy(
            {"proxy_image_only": proxy_image_only, "proxy_image": proxy_image}
        )
        if policy is util.RE_URL_IMAGE:
            policies.append("ImageOnly")
        elif policy is util.RE_URL_ALL:
            policies.append("All")
        elif policy is util.RE_URL_WEBPAGE:
            policies.append("Webpage")
        else:
            policies.append(f"unknown:{policy.pattern}")
    return {
        "cookies": cookies,
        "cookie_texts": cookie_texts,
        "human_time": human_time,
        "html": html,
        "legalpath_posix": legalpath_posix,
        "legalpath_windows": legalpath_windows,
        "human_size": human_size,
        "proxy": policies,
        "ua": [util.make_ua()],
    }


# --- M2 oracles: real reference App/Task against real Rust App/Task ---------

GALLERY_URL = "https://e-hentai.org/g/123456/0123456789/"
EXHENTAI_URL = "https://exhentai.org/g/123456/0123456789/"
IMAGE_HASH = "0123456789abcdef0123456789abcdef01234567"
TASK_BASE_KEYS = [
    "dir",
    "download_ori",
    "jpn_title",
    "rename_ori",
    "proxy_image",
    "proxy_image_only",
    "make_archive",
    "download_range",
    "scan_thread_cnt",
    "download_thread_cnt",
    "download_timeout",
    "ignored_errors",
    "delete_task_files",
    "low_speed_threshold",
]


def _load_upstream(upstream):
    import importlib

    if str(upstream) not in sys.path:
        sys.path.insert(0, str(upstream))
    import xeHentai.config as defaults
    core = importlib.import_module("xeHentai.core")
    task_module = importlib.import_module("xeHentai.task")
    const_module = importlib.import_module("xeHentai.const")
    return core, task_module, const_module, defaults


class _FakeUuid:
    def __init__(self, name):
        self.name = name

    def __str__(self):
        return f"{self.name}-0000-4000-8000-000000000000"


def _install_uuids(names):
    import uuid

    pending = list(names)
    fallback = [90000000]

    def next_uuid():
        if pending:
            return _FakeUuid(pending.pop(0))
        # Loading a saved task constructs a throwaway Task before from_dict
        # replaces its guid; those draws are not observable.
        fallback[0] += 1
        return _FakeUuid(f"f{fallback[0]}")

    uuid.uuid4 = next_uuid


def _new_app(core, defaults, work, uuid_names, overrides=None):
    import types

    _install_uuids(uuid_names)
    values = {k: v for k, v in vars(defaults).items() if not k.startswith("_")}
    values.update(
        {
            "dir": str(work / "downloads"),
            "save_tasks": True,
            "log_path": str(work / "eh.log"),
            "log_verbose": 2,
        }
    )
    values.update(overrides or {})
    namespace = types.SimpleNamespace(**values)
    core.default_config = namespace
    core.config = namespace
    core.FILEPATH = str(work)
    return core.xeHentai()


def _scenario(work, name):
    import os

    directory = work / name
    directory.mkdir(parents=True, exist_ok=True)
    os.chdir(directory)
    return directory


def _norm_path(value, work):
    prefix = str(work)
    if isinstance(value, str) and value.startswith(prefix):
        return "<ROOT>" + value[len(prefix) :]
    return value


def _config_obs(config, work):
    out = {}
    for key, value in config.items():
        out[key] = _norm_path(value, work)
    return out


def _task_obs(task, work):
    out = {
        "url": task.url,
        "failcode": task.failcode,
        "state": task.state,
        "guid": task.guid,
        "config": _config_obs(task.config, work),
        "meta": task.meta,
        "reload": [[key, [entry[0], entry[1]]] for key, entry in task.reload_map.items()],
        "duplicate": [[key, sorted(value)] for key, value in task.duplicate_map.items()],
        "renamed": [[key, value] for key, value in task.renamed_map.items()],
        "img_q": None if task.img_q is None else list(task.img_q.queue),
        "page_q": None if task.page_q is None else list(task.page_q.queue),
        "flist": sorted(task._flist_done),
    }
    if "gid" in task.__dict__:
        out["gid"] = task.gid
        out["sethash"] = task.sethash
    return out


def _app_obs(app, work):
    return {
        "tasks": [[guid, _task_obs(task, work)] for guid, task in app._all_tasks.items()],
        "queue": list(app.tasks.queue),
        "cookies": [[key, value] for key, value in app.cookies.items()],
        "headers": [[key, value] for key, value in app.headers.items()],
        "has_login": app.has_login,
    }


def _capture(function):
    try:
        return {"ok": True, "value": function()}
    except Exception as error:  # noqa: BLE001 - the oracle records the real class
        return {"ok": False, "error": type(error).__name__}


def _task_config(app, overrides=None):
    config = {key: app.cfg[key] for key in TASK_BASE_KEYS}
    config.update(overrides or {})
    return config


def _tree(root):
    import os

    entries = []
    excluded = {"h.json", "eh.log"}
    for directory, dirnames, filenames in os.walk(root):
        dirnames.sort()
        filenames.sort()
        rel_dir = os.path.relpath(directory, root)
        if rel_dir != ".":
            entries.append({"path": rel_dir.replace(os.sep, "/"), "type": "dir"})
        for name in filenames:
            if name in excluded:
                continue
            full = os.path.join(directory, name)
            rel = os.path.relpath(full, root).replace(os.sep, "/")
            if os.path.islink(full):
                entries.append(
                    {"path": rel, "type": "link", "target": os.readlink(full)}
                )
                continue
            with open(full, "rb") as handle:
                data = handle.read()
            text = None
            if len(data) <= 4096:
                try:
                    text = data.decode("utf-8")
                except UnicodeDecodeError:
                    text = None
            entries.append(
                {"path": rel, "type": "file", "size": len(data), "text": text}
            )
    entries.sort(key=lambda entry: entry["path"])
    return entries


def _capture_path(root):
    return _tree(root)


def oracle_task_api(upstream, request):
    core, task_module, _const, defaults = _load_upstream(upstream)
    work = Path(request["root"])
    result = {}

    snapshot_dir = _scenario(work, "snapshot")
    app = _new_app(core, defaults, snapshot_dir, ["00000001", "00000002"])
    first = app.add_task(
        GALLERY_URL,
        download_thread_cnt=9,
        dir=str(snapshot_dir / "task-dir"),
        extra_flag="kept",
        extra_number=7,
    )
    app.update_config(
        dir=str(snapshot_dir / "downloads"),
        download_thread_cnt=2,
        jpn_title=False,
        scan_thread_cnt=3,
        proxy=[],
    )
    second = app.add_task(GALLERY_URL)
    result["snapshot"] = {
        "first": _task_obs(app._all_tasks[first[1]], snapshot_dir),
        "second": _task_obs(app._all_tasks[second[1]], snapshot_dir),
        "queue": list(app.tasks.queue),
        "base_keys": list(app._all_tasks[first[1]].config.keys())[:14],
        "rpc_leaked": [
            key
            for key in app._all_tasks[first[1]].config
            if key.startswith("rpc_") or key in ("daemon", "save_tasks")
        ],
    }

    states_dir = _scenario(work, "states")
    app = _new_app(
        core,
        defaults,
        states_dir,
        ["00000001", "00000002", "00000003"],
    )
    steps = []

    def step(label, outcome):
        steps.append([label, outcome[0], outcome[1]])

    step("bad-url", app.add_task("https://example.com/nope"))
    step("exhentai", app.add_task(EXHENTAI_URL))
    added = app.add_task(GALLERY_URL)
    step("valid", added)
    guid = added[1]
    step("pause", app.pause_task(guid))
    step("pause-again", app.pause_task(guid))
    step("resume", app.resume_task(guid))
    step("resume-waiting", app.resume_task(guid))
    step("delete-running", app.del_task(guid))
    step("pause-before-delete", app.pause_task(guid))
    step("delete", app.del_task(guid))
    step("delete-missing", app.del_task(guid))
    step("pause-missing", app.pause_task("missing"))
    step("resume-missing", app.resume_task("missing"))
    result["states"] = {"steps": steps, "final": _app_obs(app, states_dir)}

    resumes_dir = _scenario(work, "resumes")
    app = _new_app(core, defaults, resumes_dir, ["00000001", "00000002"])
    finished = app.add_task(GALLERY_URL)[1]
    app._all_tasks[finished].state = 20
    finished_resume = app.resume_task(finished)
    failed = app.add_task("https://example.com/nope")
    failed_guid = list(app._all_tasks)[1]
    failed_resume = app.resume_task(failed_guid)
    result["resumes"] = {
        "finished": finished_resume,
        "finished_state": app._all_tasks[finished].state,
        "failed_guid": failed_guid,
        "failed": failed_resume,
        "failed_state": app._all_tasks[failed_guid].state,
        "queue": list(app.tasks.queue),
        "tasks": [[g, _task_obs(t, resumes_dir)] for g, t in app._all_tasks.items()],
        "bad_result": failed,
    }

    collision_dir = _scenario(work, "collision")
    app = _new_app(
        core,
        defaults,
        collision_dir,
        ["dupe0001", "dupe0001", "dupe0001"],
    )
    first = app.add_task(GALLERY_URL)
    second = app.add_task(GALLERY_URL)
    app._all_tasks["dupe0001"].state = 20
    third = app.add_task(GALLERY_URL)
    result["collision"] = {
        "first": first,
        "second": second,
        "third": third,
        "tasks": [[g, _task_obs(t, collision_dir)] for g, t in app._all_tasks.items()],
        "queue": list(app.tasks.queue),
    }

    methods_dir = _scenario(work, "methods")
    app = _new_app(core, defaults, methods_dir, ["00000001"])
    task = app._all_tasks[app.add_task(GALLERY_URL)[1]]
    values = {
        "base_url": _capture(task.base_url),
        "mpv_url": _capture(task.mpv_url),
        "gid": task.gid,
        "sethash": task.sethash,
        "fidpad": _capture(lambda: task.get_fidpad(7)),
        "fidpad_png": _capture(lambda: task.get_fidpad(1000, "png")),
    }
    task.meta.update({"total": 1155})
    values["unfinished"] = _capture(task.get_fid_unfinished)
    values["imghash"] = _capture(
        lambda: task.get_imghash(
            f"https://img.example/{IMAGE_HASH}-123-1-1-jpg/0123456789/sample.jpg"
        )
    )
    values["imgfid"] = _capture(
        lambda: task.get_imgfid("https://e-hentai.org/fullimg/1/456?x=1")
    )
    values["imgfid_missing"] = _capture(lambda: task.get_imgfid("https://example.com/x"))
    task.meta.update({"gnname": "English title", "gjname": "日本語タイトル"})
    task.update_meta({})
    values["title_jpn"] = task.meta["title"]
    task.config["jpn_title"] = False
    task.meta.update({"gjname": "日本語タイトル", "gnname": "English title"})
    task.update_meta({})
    values["title_eng"] = task.meta["title"]
    task.reload_map["0123456789"] = [
        "https://e-hentai.org/s/abcdef0123/111-2",
        "2.png",
    ]
    values["fname"] = _capture(lambda: task.get_fname("0123456789"))
    values["fname_missing"] = _capture(lambda: task.get_fname("ffffffffff"))
    values["set_fail"] = _capture(lambda: task.set_fail(1004))
    values["after_fail"] = [task.state, task.failcode, task.meta]
    values["migrate"] = _capture(task.migrate_exhentai)
    values["after_migrate"] = [task.url, task.state, task.failcode]
    values["migrate_again"] = _capture(task.migrate_exhentai)
    result["methods"] = values
    _install_uuids(["00000009"])
    result["methods_exceptions"] = {
        "fpath_missing_title": _capture(
            lambda: task_module.Task("", {}, core.logger.Logger()).get_fpath()
        ),
    }
    return result


def _wire_tasks(raw, work):
    document = json.loads(raw)
    tasks = document.get("tasks", {})
    for task in tasks.values():
        task["config"] = _config_obs(task["config"], work)
    return tasks


def _rename_key_count(raw):
    match = re.search(r'"renamed_map"\s*:\s*\{(.*?)\}', raw, re.S)
    if not match:
        return 0
    return len(re.findall(r'"1"\s*:', match.group(1)))


def oracle_session_fixture(upstream, request):
    core, _task_module, _const, defaults = _load_upstream(upstream)
    work = Path(request["root"])
    directory = _scenario(work, "migrate")
    app = _new_app(core, defaults, directory, ["5e551001"], {"dir": "downloads"})
    added = app.add_task(GALLERY_URL)
    guid = added[1]
    task = app._all_tasks[guid]
    task.meta.update(
        {
            "gnname": "Half",
            "gjname": None,
            "total": 3,
            "filelist": {"abcdef0123": ["1", "x"]},
            "probe_token": "from-python-session",
        }
    )
    task.update_meta({})
    task.state = 10
    import queue

    task.page_q = queue.Queue()
    task.page_q.put(GALLERY_URL + "?p=0")
    gallery = directory / "downloads" / "Half"
    gallery.mkdir(parents=True, exist_ok=True)
    (gallery / "1.jpg").write_bytes(b"one")
    (gallery / "3.jpg").write_bytes(b"three")
    assert app.save_session()[0] == 0
    return {"ok": True, "dir": str(directory), "guid": guid}


def oracle_session(upstream, request):
    core, _task_module, _const, defaults = _load_upstream(upstream)
    work = Path(request["root"])
    result = {}

    roundtrip_dir = _scenario(work, "roundtrip")
    app = _new_app(core, defaults, roundtrip_dir, ["00000001"])
    added = app.add_task(GALLERY_URL)
    guid = added[1]
    task = app._all_tasks[guid]
    task.meta.update(
        {
            "gnname": "T",
            "gjname": None,
            "total": 3,
            "filelist": {"abcdef0123": ["1", "x"]},
        }
    )
    task.update_meta({})
    task.config["make_archive"] = True
    task.reload_map["abcdef0123"] = ["https://e-hentai.org/s/abcdef0123/111-1", "1.jpg"]
    task.renamed_map[1] = "1.jpg"
    task.renamed_map["1"] = "renamed.jpg"
    import queue

    task.img_q = queue.Queue()
    task.img_q.put("https://img.example/a.jpg")
    task.page_q = queue.Queue()
    task.page_q.put(GALLERY_URL + "?p=0")
    app.set_cookie("a=1; b=x=y; Path=/; ipb_member_id=7; ipb_pass_hash=h")
    save = app.save_session()
    raw = (roundtrip_dir / "h.json").read_text(encoding="utf-8")
    reloaded = _new_app(core, defaults, roundtrip_dir, [])
    result["roundtrip"] = {
        "save": list(save),
        "wire_tasks": _wire_tasks(raw, roundtrip_dir),
        "rename_key_count": _rename_key_count(raw),
        "loaded": _app_obs(reloaded, roundtrip_dir),
    }
    reloaded.save_session()
    restarted = _new_app(core, defaults, roundtrip_dir, [])
    result["roundtrip"]["restarted"] = _app_obs(restarted, roundtrip_dir)

    save_false_dir = _scenario(work, "save_false")
    app = _new_app(
        core,
        defaults,
        save_false_dir,
        ["00000001"],
        {"save_tasks": False},
    )
    app.add_task(GALLERY_URL)
    app.set_cookie("a=1; b=x=y; ipb_member_id=7; ipb_pass_hash=h")
    save = app.save_session()
    raw = (save_false_dir / "h.json").read_text(encoding="utf-8")
    result["save_false"] = {
        "save": list(save),
        "wire_tasks": _wire_tasks(raw, save_false_dir),
        "loaded": _app_obs(
            _new_app(core, defaults, save_false_dir, [], {"save_tasks": False}),
            save_false_dir,
        ),
    }

    cookies_dir = _scenario(work, "cookies")
    (cookies_dir / "h.json").write_text(
        '{"tasks":{},"cookies":{"z":"1","nw":"9","a":"2"}}', encoding="utf-8"
    )
    (cookies_dir / ".ehentai.cookie").write_text("1,oldhash\n", encoding="utf-8")
    result["cookies"] = _app_obs(
        _new_app(core, defaults, cookies_dir, []), cookies_dir
    )

    logged_in_dir = _scenario(work, "logged_in")
    (logged_in_dir / "h.json").write_text(
        '{"tasks":{},"cookies":{"nw":"1","ipb_member_id":"9","ipb_pass_hash":"newhash"}}',
        encoding="utf-8",
    )
    (logged_in_dir / ".ehentai.cookie").write_text("1,oldhash\n", encoding="utf-8")
    result["logged_in"] = _app_obs(
        _new_app(core, defaults, logged_in_dir, []), logged_in_dir
    )

    bad_dir = _scenario(work, "bad_json")
    (bad_dir / "h.json").write_text("not json at all", encoding="utf-8")
    result["bad_json"] = _app_obs(_new_app(core, defaults, bad_dir, []), bad_dir)

    for name in ("missing_tasks", "missing_cookies"):
        directory = _scenario(work, name)
        payload = (
            '{"cookies":{}}' if name == "missing_tasks" else '{"tasks":{}}'
        )
        (directory / "h.json").write_text(payload, encoding="utf-8")
        outcome = _capture(lambda directory=directory: _app_obs(
            _new_app(core, defaults, directory, []), directory
        ))
        result[name] = outcome

    duplicate_dir = _scenario(work, "duplicate_load")
    app = _new_app(core, defaults, duplicate_dir, ["00000001"])
    guid = app.add_task(GALLERY_URL)[1]
    app._all_tasks[guid].state = 0
    app.save_session()
    app = _new_app(core, defaults, duplicate_dir, [])
    first_queue = list(app.tasks.queue)
    app.load_session()
    result["duplicate_load"] = {
        "first_queue": first_queue,
        "queue": list(app.tasks.queue),
        "tasks": list(app._all_tasks),
    }

    state10_dir = _scenario(work, "state10")
    app = _new_app(core, defaults, state10_dir, ["00000001"])
    guid = app.add_task(GALLERY_URL)[1]
    task = app._all_tasks[guid]
    task.meta.update(
        {
            "gnname": "T",
            "gjname": None,
            "total": 3,
            "filelist": {"abcdef0123": ["1", "x"]},
        }
    )
    task.update_meta({})
    gallery = state10_dir / "downloads" / "T"
    gallery.mkdir(parents=True, exist_ok=True)
    (gallery / "1.jpg").write_bytes(b"x")
    (gallery / "2.jpg").write_bytes(b"")
    task.state = 10
    task.page_q = queue.Queue()
    task.page_q.put(GALLERY_URL + "?p=0")
    app.save_session()
    reloaded = _new_app(core, defaults, state10_dir, [])
    result["state10"] = {
        "loaded": _app_obs(reloaded, state10_dir),
        "tree": _tree(state10_dir),
    }

    auth_dir = _scenario(work, "dir_authorization")
    auth_app = _new_app(core, defaults, auth_dir, ["00000001"], {"dir": "elsewhere"})
    auth_guid = auth_app.add_task(GALLERY_URL)[1]
    auth_app._all_tasks[auth_guid].meta.update(
        {"gnname": "T", "gjname": None, "total": 1}
    )
    auth_app._all_tasks[auth_guid].update_meta({})
    auth_app.save_session()
    auth_loaded = _new_app(core, defaults, auth_dir, [])
    auth_task = auth_loaded._all_tasks[auth_guid]
    result["dir_authorization"] = {
        "loader": {
            "ok": True,
            "value": os.path.relpath(auth_task.get_fpath(), auth_dir).replace(
                os.sep, "/"
            ),
        }
    }

    migrate_dir = Path(request["migrate_dir"])
    migration = _capture_directory_load(
        core, defaults, migrate_dir
    )
    result["migrate"] = migration
    return result


def _capture_directory_load(core, defaults, directory):
    import os

    os.chdir(directory)
    app = _new_app(core, defaults, directory, [])
    before = _app_obs(app, directory)
    save = app.save_session()
    restarted = _new_app(core, defaults, directory, [])
    return {
        "before": before,
        "save": list(save),
        "restarted": _app_obs(restarted, directory),
        "tree": _tree(directory),
    }


def oracle_files(upstream, request):
    import queue

    core, task_module, _const, defaults = _load_upstream(upstream)
    work = Path(request["root"])
    result = {}
    logger = core.logger.Logger()
    logger.set_level(core.logger.Logger.WARNING)

    def make_task(directory, title, total, overrides=None, rename_ori=False):
        _scenario(work, directory)
        scenario_dir = work / directory
        app = _new_app(core, defaults, scenario_dir, [])
        config = _task_config(
            app,
            {
                "dir": str(scenario_dir / "downloads"),
                "make_archive": False,
                "rename_ori": rename_ori,
                **(overrides or {}),
            },
        )
        (scenario_dir / "downloads").mkdir(exist_ok=True)
        task = task_module.Task(GALLERY_URL, config, logger)
        task.meta.update(
            {"gnname": title, "gjname": None, "total": total, "title": title}
        )
        task.img_q = queue.Queue()
        task.page_q = queue.Queue()
        return scenario_dir, task

    scan_dir, task = make_task("scan", "scan", 4, {"download_range": [[2, 3]]})
    gallery = Path(task.get_fpath())
    gallery.mkdir(parents=True, exist_ok=True)
    (gallery / "1.jpg").write_bytes(b"one")
    (gallery / "2.jpg").write_bytes(b"two")
    (gallery / "3.jpg").write_bytes(b"")
    (gallery / "4.jpg").write_bytes(b"four")
    task.scan_downloaded()
    result["scan"] = {
        "flist": sorted(task._flist_done),
        "finished": task.meta.get("finished"),
        "state": task.state,
        "tree": _tree(scan_dir),
    }
    markers = {}
    for marker in (".xehdone", "zip"):
        marker_dir, marker_task = make_task(
            f"scan-{marker}", f"scan-{marker}", 2
        )
        marker_gallery = Path(marker_task.get_fpath())
        marker_gallery.mkdir(parents=True, exist_ok=True)
        (marker_gallery / "1.jpg").write_bytes(b"")
        if marker == ".xehdone":
            (marker_gallery / ".xehdone").write_bytes(b"")
        else:
            (marker_dir / "downloads" / f"scan-{marker}.zip").write_bytes(b"")
        marker_task.scan_downloaded()
        markers[marker] = {
            "flist": sorted(marker_task._flist_done),
            "finished": marker_task.meta.get("finished"),
            "state": marker_task.state,
        }
    result["scan_markers"] = markers

    dup_dir, task = make_task("duplicates", "dups", 3)
    first_img = f"https://img.example/{IMAGE_HASH}-1-1-1-jpg/full.jpg"
    first_page = "https://e-hentai.org/s/abcdef0123/111-1"
    second_page = "https://e-hentai.org/s/abcdef0123/111-2"
    third_page = "https://e-hentai.org/s/abcdef0123/111-3"
    first_redirect = f"https://img.example/{IMAGE_HASH}-1-1-1-jpg/1.png"
    task.put_img_queue(first_img, first_page, "1.png")
    task.put_img_queue(first_img, second_page, "2.png")
    pending = {
        "img_q": list(task.img_q.queue),
        "duplicate": [[k, sorted(v)] for k, v in task.duplicate_map.items()],
        "renamed": [[k, v] for k, v in task.renamed_map.items()],
        "finished": task.meta.get("finished"),
    }
    gallery = Path(task.get_fpath())
    gallery.mkdir(parents=True, exist_ok=True)
    task._monitor = type("Monitor", (), {"_exit": lambda self, arg=None: False})()
    task.save_file(first_img, first_redirect, lambda: iter([b"image-bytes"]))
    after_copy = {
        "duplicate": [[k, sorted(v)] for k, v in task.duplicate_map.items()],
        "finished": task.meta.get("finished"),
        "tree": _tree(dup_dir),
    }
    task.put_img_queue(first_img, third_page, "3.png")
    after_third = {
        "finished": task.meta.get("finished"),
        "img_q": list(task.img_q.queue),
        "tree": _tree(dup_dir),
    }
    task.put_img_queue(first_img, first_page, "1.png")
    after_self = {
        "img_q": list(task.img_q.queue),
        "renamed": [[k, v] for k, v in task.renamed_map.items()],
        "reload": [[k, list(v)] for k, v in task.reload_map.items()],
    }
    result["duplicates"] = {
        "pending": pending,
        "after_copy": after_copy,
        "after_third": after_third,
        "after_self": after_self,
        "state": _task_obs(task, dup_dir),
    }

    page_dir, task = make_task("pages", "pages", 3)
    task._flist_done.add(1)
    task.put_page_queue("https://e-hentai.org/s/abcdef0123/111-1")
    task.put_page_queue("https://e-hentai.org/s/abcdef0123/111-2")
    task.reload_map["0123456789"] = [
        "https://e-hentai.org/s/abcdef0123/111-2",
        "2.png",
    ]
    queued = list(task.page_q.queue)
    retry = _capture(
        lambda: task.put_page_queue_retry(
            "https://img.example/redirect?redirect=https://e-hentai.org/s/0123456789/111-2"
        )
    )
    result["pages"] = {
        "queued": queued,
        "retry": retry,
        "queue": list(task.page_q.queue),
        "reload": [[k, list(v)] for k, v in task.reload_map.items()],
    }

    save_dir, task = make_task("save", "stream", 2)
    task.put_img_queue(first_img, first_page, "1.png")
    redirect = f"https://img.example/full/{IMAGE_HASH}-1-1-1-jpg/real-name.jpg?token=1"
    task._monitor = type("Monitor", (), {"_exit": lambda self, arg=None: False})()
    saved = task.save_file(first_img, redirect, lambda: iter([b"abc", b"def"]))
    first_save = {
        "saved": saved is True,
        "reload": [[k, list(v)] for k, v in task.reload_map.items()],
        "finished": task.meta.get("finished"),
        "tree": _tree(save_dir),
    }
    consumed = []

    def never_called():
        consumed.append(1)
        raise AssertionError("stream must not be consumed")

    existing = task.save_file(first_img, redirect, never_called)
    second_save = {
        "existing": existing is not None,
        "consumed": bool(consumed),
        "tree": _tree(save_dir),
    }
    gallery = Path(task.get_fpath())
    (gallery / "1.jpg").write_bytes(b"")
    replaced = task.save_file(first_img, redirect, lambda: iter([b"replaced"]))
    third_save = {
        "replaced": replaced is not None,
        "text": (gallery / "1.jpg").read_text(encoding="utf-8"),
    }
    cancel_dir, cancel_task = make_task("cancel", "cancel", 2)
    cancel_task.put_img_queue(first_img, first_page, "1.png")
    calls = []

    def cancelling_stream():
        calls.append(1)
        return iter([b"a", b"b"])

    class CancelledMonitor:
        def _exit(self, arg=None):
            return bool(calls)

    cancel_task._monitor = CancelledMonitor()
    cancelled = cancel_task.save_file(first_img, first_img, cancelling_stream)
    result["save"] = {
        "first": first_save,
        "second": second_save,
        "third": third_save,
        "cancel": {
            "result": cancelled is not None,
            "finished": cancel_task.meta.get("finished"),
            "tree": _tree(cancel_dir),
        },
    }

    rename_dir, task = make_task("naming", "rename", 5)
    gallery = Path(task.get_fpath())
    gallery.mkdir(parents=True, exist_ok=True)
    for fid in range(1, 6):
        (gallery / f"{fid}.jpg").write_bytes(b"x")
    task.renamed_map[1] = "1.jpg"
    task.renamed_map[2] = "2.png"
    task.renamed_map[3] = "3.png"
    task.renamed_map[4] = "4.jpg"
    task.renamed_map[5] = "noext"
    (gallery / "2.png").write_bytes(b"old")
    errors = task.rename_fname()
    result["naming"] = {
        "errors": errors,
        "renamed": [[k, v] for k, v in task.renamed_map.items()],
        "tree": _tree(rename_dir),
    }

    crash_dir, task = make_task("crash", "crash", 2)
    gallery = Path(task.get_fpath())
    gallery.mkdir(parents=True, exist_ok=True)
    (gallery / "1.jpg").write_bytes(b"x")
    (gallery / "1.png").write_bytes(b"x")
    (gallery / "1(1).png").write_bytes(b"x")
    task.renamed_map[1] = "1.png"
    result["rename_crash"] = {
        "crash": _capture(task.rename_fname),
        "tree": _tree(crash_dir),
    }

    ori_dir, task = make_task("ori", "ori", 2, rename_ori=True)
    gallery = Path(task.get_fpath())
    gallery.mkdir(parents=True, exist_ok=True)
    (gallery / "1.jpg").write_bytes(b"auto")
    (gallery / "cover.jpg").write_bytes(b"original-name")
    task.renamed_map[1] = "cover.jpg"
    result["rename_ori"] = {
        "errors": task.rename_fname(),
        "renamed": [[k, v] for k, v in task.renamed_map.items()],
        "tree": _tree(ori_dir),
    }

    pad_dir, task = make_task("pad", "pad", 1000)
    gallery = Path(task.get_fpath())
    gallery.mkdir(parents=True, exist_ok=True)
    (gallery / "0007.jpg").write_bytes(b"x")
    task.renamed_map["7"] = "page-7.png"
    result["padding"] = {
        "fidpad": task.get_fidpad(7),
        "errors": task.rename_fname(),
        "tree": _tree(pad_dir),
    }

    done_dir, task = make_task("done", "done", 2)
    gallery = Path(task.get_fpath())
    gallery.mkdir(parents=True, exist_ok=True)
    (gallery / "1.jpg").write_bytes(b"x")
    (gallery / "2.jpg").write_bytes(b"x")
    task.renamed_map["1"] = "1.png"
    task.renamed_map["2"] = "2.png"
    result["done_marker"] = {
        "errors": task.rename_fname(),
        "tree": _tree(done_dir),
    }

    archive_dir, task = make_task("archive", "archive", 2)
    gallery = Path(task.get_fpath())
    gallery.mkdir(parents=True, exist_ok=True)
    (gallery / "b.jpg").write_bytes(b"bbb")
    (gallery / "a.jpg").write_bytes(b"aaa")
    (gallery / ".xehdone").write_bytes(b"")
    (gallery / "sub").mkdir()
    (gallery / "sub" / "inner.txt").write_bytes(b"inner")
    arc = task.make_archive(False)
    again = task.make_archive(True)
    existing = {
        "arc": _rel_path(arc, archive_dir),
        "again": _rel_path(again, archive_dir),
        "tree": _tree(archive_dir),
        "archive": _zip_obs(arc),
    }
    os_remove(arc)
    made = task.make_archive(True)
    result["archive"] = {
        "first": existing,
        "made": _rel_path(made, archive_dir),
        "tree": _tree(archive_dir),
    }

    big_dir, task = make_task("zip64", "big", 0)
    gallery = Path(task.get_fpath())
    gallery.mkdir(parents=True, exist_ok=True)
    for index in range(65_537):
        (gallery / f"{index}.jpg").write_bytes(b"")
    arc = task.make_archive(False)
    result["zip64"] = _zip64_obs(arc)
    import shutil

    shutil.rmtree(gallery)
    os_remove(arc)

    symlink_dir, task = make_task("archive_link", "archive_link", 1)
    gallery = Path(task.get_fpath())
    gallery.mkdir(parents=True, exist_ok=True)
    (symlink_dir / "outside.txt").write_bytes(b"outside-data")
    (gallery / "1.jpg").write_bytes(b"inside")
    (gallery / "link.jpg").symlink_to(symlink_dir / "outside.txt")
    archive_link = _capture(lambda: _zip_obs(task.make_archive(False)))
    result["archive_symlink"] = {
        "oracle": archive_link,
        "tree": _tree(symlink_dir),
    }

    path_dir, task = make_task("path", "///", 1, rename_ori=True)
    downloads = path_dir / "downloads"
    (downloads / "1.jpg").write_bytes(b"x")
    task.renamed_map[1] = "x.png"
    result["path_safety"] = {
        "oracle": _capture(task.rename_fname),
        "tree": _tree(path_dir),
    }
    return result


def _rel_path(path, root):
    import os

    return os.path.relpath(path, root).replace(os.sep, "/")


def os_remove(path):
    import os

    os.remove(path)


def _zip_obs(path):
    import zipfile

    with zipfile.ZipFile(path) as archive:
        entries = []
        for info in archive.infolist():
            entries.append(
                {
                    "name": info.filename,
                    "compress": info.compress_type,
                    "size": info.file_size,
                    "text": archive.read(info).decode("utf-8", "replace"),
                }
            )
        return {
            "comment": archive.comment.decode("utf-8", "replace"),
            "entries": entries,
        }


def _zip64_obs(path):
    import zipfile

    with open(path, "rb") as handle:
        data = handle.read()
    with zipfile.ZipFile(path) as archive:
        infos = archive.infolist()
        return {
            "zip64_eocd": b"PK\x06\x06" in data,
            "zip64_locator": b"PK\x06\x07" in data,
            "entry_count": len(infos),
            "first_entry": infos[0].filename,
            "stored": all(info.compress_type == 0 for info in infos),
            "comment": archive.comment.decode("utf-8", "replace"),
        }


EXCLUDED_I18N_NAMES = {
    "CODEPAGE",
    "DUMMY_FILENAME",
    "FILEPATH",
    "LOCALE",
    "RENAME_TMPDIR",
    "RESTR_SITE",
    "SCRIPT_NAME",
    "SRC_UPDATE_FILE",
    "STATIC_CACHE_FILE",
    "VERSION_UPDATE",
    "err_msg",
}


def oracle_i18n(upstream, request, tag):
    sys.path.insert(0, str(upstream))
    import xeHentai.const as const

    const.LOCALE = tag
    import xeHentai.i18n as package

    wrapper = package.i18n
    locale_module = package.lng
    keys = sorted(
        name
        for name, value in vars(locale_module).items()
        if isinstance(value, str)
        and not name.startswith("_")
        and name not in EXCLUDED_I18N_NAMES
    )
    messages = {}
    for entry in request["messages"]:
        template = getattr(wrapper, entry["key"])
        messages[entry["key"]] = format_template(
            template, entry["py_args"], entry["kind"]
        )
    errors = {}
    for entry in request["codes"]:
        code = entry["code"]
        template = wrapper.c(code)
        kind = entry["kind"]
        errors[str(code)] = format_template(template, entry["py_args"], kind)
    return {
        "tag": tag,
        "keys": keys,
        "messages": messages,
        "errors": errors,
    }


def build_i18n_messages(upstream):
    sys.path.insert(0, str(upstream))
    import xeHentai.i18n.en_us as en_us

    messages = []
    for name, template in sorted(vars(en_us).items()):
        if (
            not isinstance(template, str)
            or name.startswith("_")
            or name in EXCLUDED_I18N_NAMES
        ):
            continue
        probe, py, kind = placeholder_args(template)
        messages.append(
            {"key": name, "args": probe, "py_args": py, "kind": kind, "template": template}
        )
    known_codes = {1000, 1001, 1002, 1003, 1004, 1007, 1008, 1009, 1010, 1012,
                   1013, 1006, 1101, 1104, 1105, 1106, 1107, 1300, 1301, 1302,
                   -32700, -32600, -32601, -32602, 1200, -32603, 1103}
    codes = []
    for code in I18N_ERROR_CODES:
        args = list(ERROR_ARGS.get(code, ()))
        if args:
            probe = [str(value) for value in args]
            py = list(args)
            kind = "percent"
        else:
            # `i18n.c(code)` already interpolates ERR_NOMSG for unknown codes.
            probe, py, kind = [], [], "plain"
        codes.append(
            {"code": code, "args": probe, "py_args": py, "kind": kind}
        )
    return messages, codes


def write_json(path, value):
    path.write_text(
        json.dumps(value, ensure_ascii=False, indent=2, sort_keys=True),
        encoding="utf-8",
    )


def run_oracle(oracle, request, root, upstream, log_path, locale=None, cwd=None):
    request_path = root / f"oracle-request-{oracle}-{locale or 'default'}.json"
    output_path = root / f"oracle-{oracle}-{locale or 'default'}.json"
    write_json(request_path, request)
    command = [
        sys.executable,
        str(Path(__file__).resolve()),
        "--oracle",
        oracle,
        "--upstream",
        str(upstream),
        "--input",
        str(request_path),
        "--output",
        str(output_path),
    ]
    if locale:
        command.extend(["--locale", locale])
    isolated = cwd or (root / "isolated")
    isolated.mkdir(parents=True, exist_ok=True)
    result = run_command(command, isolated, clean_env(isolated), log_path)
    if result.returncode != 0:
        raise CompatError(f"oracle {oracle} failed: {result.stderr.strip() or result.stdout.strip()}")
    if not output_path.is_file():
        raise CompatError(f"oracle {oracle} produced no output")
    return json.loads(output_path.read_text(encoding="utf-8"))


def run_probe(suite, inputs, root, target_dir, log_path):
    request_path = root / "probe-request.json"
    output_path = root / "probe-candidate.json"
    write_json(request_path, {"suite": suite, "inputs": inputs})
    if output_path.exists():
        output_path.unlink()
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)
    env["XEH_COMPAT_REQUEST"] = str(request_path)
    env["XEH_COMPAT_OUTPUT"] = str(output_path)
    env["CARGO_TERM_COLOR"] = "never"
    result = run_command(
        [
            "cargo",
            "test",
            "--bin",
            "xeHentai",
            "tests::compat_probe",
            "--",
            "--exact",
            "--nocapture",
        ],
        REPO,
        env,
        log_path,
    )
    if result.returncode != 0:
        raise CompatError(f"Rust probe failed: {result.stderr.strip() or result.stdout.strip()}")
    _, summary = parse_test_results(result.stdout)
    if summary is None or summary["passed"] != 1 or summary["failed"] != 0:
        raise CompatError(f"Rust probe did not run exactly one passing test: {summary}")
    if not output_path.is_file():
        raise CompatError("Rust probe produced no candidate output")
    return json.loads(output_path.read_text(encoding="utf-8"))


def run_native(suite, root, target_dir):
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = str(target_dir)
    env["CARGO_TERM_COLOR"] = "never"
    evidence = []
    for index, argv in enumerate(NATIVE_COMMANDS[suite], start=1):
        log_path = root / f"native-{index}.log"
        result = run_command(argv, REPO, env, log_path)
        if result.returncode != 0:
            raise CompatError(f"native tests failed for {suite}: {' '.join(argv)}")
        _, summary = parse_test_results(result.stdout)
        if summary is None:
            raise CompatError(f"no test summary for {' '.join(argv)}")
        if summary["status"] != "ok" or summary["failed"] != 0 or summary["passed"] == 0:
            raise CompatError(f"native tests failed for {suite}: {summary}")
        evidence.append(
            {
                "argv": argv,
                "exit": result.returncode,
                "summary": summary,
                "stdout": result.stdout,
                "stderr": result.stderr,
                "log": str(log_path),
            }
        )
    return evidence


def compare_config(cases, oracle, candidate):
    diffs = []
    for field in CONFIG_FIELDS:
        expected = EXPECTED_DEFAULTS[field]
        key = f"config.default.{field}"
        left = compare(expected, oracle["defaults"][field], key)
        right = compare(expected, candidate["defaults"][field], key)
        diffs.extend(left + right)
        cases[key] = not left and not right
    expected_import = dict(EXPECTED_DEFAULTS)
    expected_import.update(EXPECTED_OVERRIDES)
    oracle_import = compare(expected_import, oracle["imported"], "config.import")
    candidate_import = compare(expected_import, candidate["imported"], "config.import")
    diffs.extend(oracle_import + candidate_import)
    cases["config.import.overrides"] = not oracle_import and not candidate_import
    cases["config.import.expected-values"] = not oracle_import and not candidate_import
    return diffs


def compare_util(cases, oracle, candidate):
    diffs = []
    sections = {
        "util.parse_cookie": "cookies",
        "util.make_cookie": "cookie_texts",
        "util.parse_human_time": "human_time",
        "util.htmlescape": "html",
        "util.legalpath.posix": "legalpath_posix",
        "util.legalpath.windows": "legalpath_windows",
        "util.human_size": "human_size",
        "util.get_proxy_policy": "proxy",
    }
    for case, section in sections.items():
        section_diffs = compare(oracle[section], candidate[section], f"util.{section}")
        diffs.extend(section_diffs)
        cases[case] = not section_diffs
    ua_diffs = compare(oracle["ua"], candidate["ua"], "util.ua")
    diffs.extend(ua_diffs)
    cases["util.make_ua"] = not ua_diffs
    return diffs


def compare_i18n(cases, oracle, candidate, messages, codes):
    diffs = []
    expected_locales = {"en_us", "zh_hans", "zh_hant"}
    if set(candidate["locales"]) != expected_locales:
        diffs.append(f"candidate locales {sorted(candidate['locales'])}")
        cases["i18n.available-locales"] = False
    else:
        cases["i18n.available-locales"] = True
    for tag, mapped in LOCALE_TAGS:
        oracle_locale = oracle[tag]
        candidate_locale = candidate["locales"][mapped]
        message_diffs = compare(
            oracle_locale["messages"], candidate_locale["messages"], f"i18n.{mapped}.messages"
        )
        error_diffs = compare(
            oracle_locale["errors"], candidate_locale["errors"], f"i18n.{mapped}.errors"
        )
        diffs.extend(message_diffs + error_diffs)
        cases[f"i18n.messages.{mapped}"] = not message_diffs
        cases[f"i18n.errors.{mapped}"] = not error_diffs
    key_diffs = compare(oracle["en_us"]["keys"], sorted(candidate["reference_keys"]), "i18n.keys")
    diffs.extend(key_diffs)
    cases["i18n.keys.all"] = not key_diffs
    cases["i18n.interpolation"] = all(
        cases.get(f"i18n.messages.{locale}", False) for locale in expected_locales
    )
    cases["i18n.err_msg.all"] = all(
        cases.get(f"i18n.errors.{locale}", False) for locale in expected_locales
    )
    cases["i18n.nomsg"] = all(
        "424242" in oracle[tag]["errors"]
        and oracle[tag]["errors"]["424242"] == candidate["locales"][mapped]["errors"]["424242"]
        for tag, mapped in LOCALE_TAGS
    )
    return diffs


def _compare_scenarios(oracle, candidate, prefix, skip=()):
    diffs = []
    flags = {}
    for name, value in oracle.items():
        if name in skip:
            continue
        section = compare(value, candidate.get(name), f"{prefix}.{name}")
        flags[name] = not section
        diffs.extend(section)
    for name in candidate:
        if name not in oracle and name not in skip:
            diffs.append(f"{prefix}.{name}: unexpected")
    return diffs, flags


def _mark_cases(cases, mapping, flags):
    for case_id, scenarios in mapping.items():
        cases[case_id] = all(flags.get(scenario, False) for scenario in scenarios)


def compare_task_api(cases, oracle, candidate):
    diffs, flags = _compare_scenarios(oracle, candidate, "task-api")
    _mark_cases(cases, TASK_API_CASES, flags)
    return diffs


def compare_session(cases, oracle, candidate):
    diffs, flags = _compare_scenarios(
        oracle, candidate, "session", skip=SESSION_DELTA_SCENARIOS
    )
    delta_diffs = []
    reference = oracle["dir_authorization"]["loader"]
    candidate_loader = candidate["dir_authorization"]["loader"]
    if reference != {"ok": True, "value": "elsewhere/T"}:
        delta_diffs.append(
            f"session.dir_authorization: oracle loaded a foreign directory: {reference}"
        )
    if candidate_loader != {"ok": False, "error": "UnsafePathDelta"}:
        delta_diffs.append(
            f"session.dir_authorization: candidate loader outcome: {candidate_loader}"
        )
    diffs.extend(delta_diffs)
    flags["dir_authorization"] = not delta_diffs
    _mark_cases(cases, SESSION_CASES, flags)
    return diffs


def _files_delta_checks(oracle, candidate):
    diffs = []
    link_oracle = oracle["archive_symlink"]["oracle"]
    link_candidate = candidate["archive_symlink"]["oracle"]
    if not link_oracle.get("ok"):
        diffs.append(f"files.archive_symlink: reference did not archive: {link_oracle}")
    else:
        entries = link_oracle["value"]["entries"]
        if not any(
            entry["name"] == "link.jpg" and entry["text"] == "outside-data"
            for entry in entries
        ):
            diffs.append(f"files.archive_symlink: reference did not follow the link: {entries}")
    if link_candidate != {"ok": False, "error": "UnsafePathDelta"}:
        diffs.append(f"files.archive_symlink: candidate did not refuse: {link_candidate}")
    candidate_tree = {entry["path"]: entry for entry in candidate["archive_symlink"]["tree"]}
    if candidate_tree.get("downloads/archive_link/1.jpg", {}).get("text") != "inside":
        diffs.append("files.archive_symlink: candidate lost the image")
    if any(path.endswith("archive_link.zip") for path in candidate_tree):
        diffs.append("files.archive_symlink: candidate published a partial archive")

    path_oracle = oracle["path_safety"]["oracle"]
    path_candidate = candidate["path_safety"]["oracle"]
    if not path_oracle.get("ok"):
        diffs.append(f"files.path_safety: reference did not rename: {path_oracle}")
    if path_candidate != {"ok": False, "error": "UnsafePathDelta"}:
        diffs.append(f"files.path_safety: candidate did not refuse: {path_candidate}")
    candidate_tree = {entry["path"]: entry for entry in candidate["path_safety"]["tree"]}
    if "downloads/1.jpg" not in candidate_tree:
        diffs.append("files.path_safety: candidate moved the image")
    if "downloads/x.png" in candidate_tree:
        diffs.append("files.path_safety: candidate wrote outside the gallery")
    return diffs


def compare_files(cases, oracle, candidate):
    diffs, flags = _compare_scenarios(
        oracle, candidate, "files", skip=FILES_DELTA_SCENARIOS
    )
    delta_diffs = _files_delta_checks(oracle, candidate)
    diffs.extend(delta_diffs)
    flags["archive_symlink"] = not delta_diffs
    flags["path_safety"] = not delta_diffs
    _mark_cases(cases, FILES_CASES, flags)
    return diffs


def self_check(suite, oracle, candidate):
    """Mutate the candidate and require the same comparison helpers to fail."""
    checks = []
    if suite == "config":
        mutated = copy.deepcopy(candidate)
        mutated["defaults"]["dir"] = "mutated"
        cases = {}
        compare_config(cases, oracle, mutated)
        checks.append(("default-value", not cases["config.default.dir"]))
        mutated = copy.deepcopy(candidate)
        mutated["imported"]["download_thread_cnt"] = 6
        cases = {}
        compare_config(cases, oracle, mutated)
        checks.append(("imported-field", not cases["config.import.overrides"]))
        mutated = copy.deepcopy(candidate)
        mutated["imported"]["proxy"] = "not-an-array"
        cases = {}
        compare_config(cases, oracle, mutated)
        checks.append(("field-type", not cases["config.import.overrides"]))
    elif suite == "util":
        mutated = copy.deepcopy(candidate)
        mutated["human_size"][2] = "1001 B"
        cases = {}
        compare_util(cases, oracle, mutated)
        checks.append(("human-size", not cases["util.human_size"]))
        mutated = copy.deepcopy(candidate)
        mutated["cookies"][0] = list(reversed(mutated["cookies"][0]))
        cases = {}
        compare_util(cases, oracle, mutated)
        checks.append(("cookie-order", not cases["util.parse_cookie"]))
    elif suite == "i18n":
        mutated = copy.deepcopy(candidate)
        mutated["locales"]["zh_hans"]["messages"]["XEH_STARTED"] = "mutated"
        cases = {}
        compare_i18n(cases, oracle, mutated, [], [])
        checks.append(("message", not cases["i18n.messages.zh_hans"]))
        mutated = copy.deepcopy(candidate)
        mutated["locales"]["en_us"]["errors"]["1006"] = "mutated"
        cases = {}
        compare_i18n(cases, oracle, mutated, [], [])
        checks.append(("error-message", not cases["i18n.errors.en_us"]))
    elif suite == "worker":
        checks.append(("zero-tests", detect_zero_tests("running 0 tests\n\ntest result: ok. 0 passed")))
    elif suite == "task-api":
        mutated = copy.deepcopy(candidate)
        mutated["states"]["steps"][0][1] = 0
        cases = {}
        compare_task_api(cases, oracle, mutated)
        checks.append(("state-code", not cases["task.state-api"]))
        mutated = copy.deepcopy(candidate)
        mutated["snapshot"]["first"]["config"]["download_thread_cnt"] = 4
        cases = {}
        compare_task_api(cases, oracle, mutated)
        checks.append(("snapshot-value", not cases["task.create-config-snapshot"]))
        mutated = copy.deepcopy(candidate)
        mutated["collision"]["queue"] = []
        cases = {}
        compare_task_api(cases, oracle, mutated)
        checks.append(("queue-order", not cases["task.guid"]))
    elif suite == "session":
        mutated = copy.deepcopy(candidate)
        mutated["roundtrip"]["loaded"]["cookies"] = list(
            reversed(mutated["roundtrip"]["loaded"]["cookies"])
        )
        cases = {}
        compare_session(cases, oracle, mutated)
        checks.append(("cookie-order", not cases["task.session-save"]))
        mutated = copy.deepcopy(candidate)
        mutated["state10"]["loaded"]["tasks"][0][1]["state"] = 10
        cases = {}
        compare_session(cases, oracle, mutated)
        checks.append(("state-fallback", not cases["task.session-load"]))
        mutated = copy.deepcopy(candidate)
        mutated["roundtrip"]["rename_key_count"] = 1
        cases = {}
        compare_session(cases, oracle, mutated)
        checks.append(("rename-key-count", not cases["task.session-save"]))
        mutated = copy.deepcopy(candidate)
        mutated["dir_authorization"]["loader"] = {"ok": True, "value": "elsewhere/T"}
        cases = {}
        compare_session(cases, oracle, mutated)
        checks.append(("dir-authorization", not cases["task.session-dir-authorization"]))
    elif suite == "files":
        mutated = copy.deepcopy(candidate)
        mutated["scan"]["tree"][0]["text"] = "mutated"
        cases = {}
        compare_files(cases, oracle, mutated)
        checks.append(("file-content", not cases["task.files-scan-downloaded"]))
        mutated = copy.deepcopy(candidate)
        mutated["naming"]["renamed"][1][1] = "mutated.png"
        cases = {}
        compare_files(cases, oracle, mutated)
        checks.append(("renamed-name", not cases["task.naming"]))
        mutated = copy.deepcopy(candidate)
        mutated["archive_symlink"]["oracle"] = {"ok": True, "value": {}}
        cases = {}
        compare_files(cases, oracle, mutated)
        checks.append(("safety-delta", not cases["task.files-safety"]))
    if not checks or not all(detected for _, detected in checks):
        raise CompatError(f"{suite}: comparator self-check failed: {checks}")
    return [{"mutation": name, "detected": detected} for name, detected in checks]


def detect_zero_tests(output):
    return bool(re.search(r"test result: ok\. 0 passed", output))


def merge_native_cases(suite, native_runs, cases):
    for run in native_runs:
        results = run["results"]
        for test_name, status in results.items():
            for case_id in NATIVE_CASE_MAP[suite].get(test_name, []):
                if status == "ok":
                    cases.setdefault(case_id, True)
                else:
                    cases[case_id] = False


def run_suite(suite, output_root, upstream, target_dir):
    if suite not in NATIVE_COMMANDS:
        raise CompatError(f"unknown suite {suite}")
    suite_root = output_root / suite
    suite_root.mkdir(parents=True, exist_ok=False)
    upstream = Path(upstream).resolve()
    if not (upstream / "xeHentai" / "config.py").is_file():
        raise CompatError(f"upstream tree is missing xeHentai/config.py: {upstream}")

    i18n_messages, i18n_codes = build_i18n_messages(upstream)
    rounds = []
    for round_number in (1, 2):
        root = suite_root / f"round-{round_number}"
        root.mkdir()
        input_root = root / "input"
        input_root.mkdir()
        input_root.joinpath("home").mkdir()
        input_root.joinpath("cache").mkdir()
        input_root.joinpath("tmp").mkdir()
        fixture_path = input_root / "config_import.py"
        fixture_path.write_text(CONFIG_FIXTURE, encoding="utf-8")

        if suite == "config":
            oracle_request = {"fixture_path": str(fixture_path)}
            oracle = run_oracle(
                "config", oracle_request, root, upstream, root / "oracle.log"
            )
            candidate = run_probe(
                "config", {"python": CONFIG_FIXTURE}, root, target_dir, root / "probe.log"
            )
            cases = {}
            diffs = compare_config(cases, oracle, candidate)
        elif suite == "util":
            inputs = {
                "cookie_texts": COOKIE_TEXTS,
                "human_times": HUMAN_TIMES,
                "html": HTML_TEXTS,
                "paths": PATHS,
                "human_sizes": HUMAN_SIZES,
                "ua_samples": 200,
            }
            oracle = run_oracle(
                "util", {"inputs": inputs}, root, upstream, root / "oracle.log"
            )
            candidate = run_probe("util", inputs, root, target_dir, root / "probe.log")
            cases = {}
            diffs = compare_util(cases, oracle, candidate)
        elif suite == "i18n":
            oracle = {}
            for tag, _ in LOCALE_TAGS:
                oracle[tag] = run_oracle(
                    "i18n",
                    {"messages": i18n_messages, "codes": i18n_codes},
                    root,
                    upstream,
                    root / f"oracle-{tag}.log",
                    locale=tag,
                )
            candidate = run_probe(
                "i18n",
                {"messages": i18n_messages, "codes": i18n_codes},
                root,
                target_dir,
                root / "probe.log",
            )
            cases = {}
            diffs = compare_i18n(cases, oracle, candidate, i18n_messages, i18n_codes)
        elif suite in ("task-api", "files"):
            oracle_root = root / "oracle-work"
            candidate_root = root / "candidate-work"
            oracle_root.mkdir()
            candidate_root.mkdir()
            oracle = run_oracle(
                suite,
                {"root": str(oracle_root)},
                root,
                upstream,
                root / f"oracle-{suite}.log",
                cwd=oracle_root,
            )
            candidate = run_probe(
                suite,
                {"root": str(candidate_root)},
                root,
                target_dir,
                root / "probe.log",
            )
            cases = {}
            if suite == "task-api":
                diffs = compare_task_api(cases, oracle, candidate)
            else:
                diffs = compare_files(cases, oracle, candidate)
        elif suite == "session":
            fixture_root = root / "fixture"
            fixture_root.mkdir()
            run_oracle(
                "session-fixture",
                {"root": str(fixture_root)},
                root,
                upstream,
                root / "oracle-session-fixture.log",
                cwd=fixture_root,
            )
            oracle_root = root / "oracle-work"
            candidate_root = root / "candidate-work"
            oracle_root.mkdir()
            candidate_root.mkdir()
            shutil.copytree(fixture_root / "migrate", oracle_root / "migrate")
            shutil.copytree(fixture_root / "migrate", candidate_root / "migrate")
            oracle = run_oracle(
                "session",
                {
                    "root": str(oracle_root),
                    "migrate_dir": str(oracle_root / "migrate"),
                },
                root,
                upstream,
                root / "oracle-session.log",
                cwd=oracle_root,
            )
            candidate = run_probe(
                "session",
                {
                    "root": str(candidate_root),
                    "migrate_dir": str(candidate_root / "migrate"),
                },
                root,
                target_dir,
                root / "probe.log",
            )
            cases = {}
            diffs = compare_session(cases, oracle, candidate)
        else:
            oracle = None
            candidate = None
            cases = {}
            diffs = []

        native_runs = run_native(suite, root, target_dir)
        ready_native = []
        for run in native_runs:
            results, summary = require_native(run, suite)
            ready_native.append({"results": results, "summary": summary, "argv": run["argv"]})
        merge_native_cases(suite, ready_native, cases)

        missing = [case_id for case_id, passed in cases.items() if not passed]
        write_json(root / "comparison.json", {
            "suite": suite,
            "round": round_number,
            "ok": not diffs and not missing,
            "diffs": diffs,
            "cases": cases,
            "native": [
                {
                    "argv": run["argv"],
                    "summary": run["summary"],
                    "tests": run["results"],
                }
                for run in ready_native
            ],
        })
        if oracle is not None:
            write_json(root / "oracle.json", oracle)
        if candidate is not None:
            write_json(root / "candidate.json", candidate)
        if diffs or missing:
            raise CompatError(
                f"{suite} round {round_number} failed: {len(diffs)} diffs, {len(missing)} failing cases: "
                f"{diffs[:5]} {missing[:5]}"
            )
        rounds.append({"root": str(root), "cases": cases})

    checks = self_check(
        suite,
        oracle if suite != "worker" else None,
        candidate if suite != "worker" else None,
    )
    write_json(suite_root / "self-check.json", {"suite": suite, "checks": checks})
    return {"suite": suite, "rounds": rounds, "self_check": checks}


def verify_all(evidence_dir):
    evidence = Path(evidence_dir)
    cases_doc = load_cases()
    manifest_path = evidence / "manifest.json"
    if not manifest_path.is_file():
        raise CompatError(f"missing evidence manifest at {manifest_path}")
    manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
    if not manifest.get("suites"):
        raise CompatError("evidence manifest has no suites")
    unimplemented = [
        case["id"]
        for case in cases_doc["cases"]
        if case["status"] not in ("implemented", "delta")
    ]
    failures = []
    evidence_cases = {}
    for suite in manifest["suites"]:
        suite_root = evidence / suite
        rounds = sorted(suite_root.glob("round-*"))
        if len(rounds) < 2:
            failures.append(f"{suite}: fewer than two independent rounds")
            continue
        for round_dir in rounds:
            comparison_path = round_dir / "comparison.json"
            if not comparison_path.is_file():
                failures.append(f"{round_dir}: missing comparison.json")
                continue
            comparison = json.loads(comparison_path.read_text(encoding="utf-8"))
            if not comparison.get("ok"):
                failures.append(f"{round_dir}: comparison not ok")
            for case_id, passed in comparison.get("cases", {}).items():
                evidence_cases.setdefault(case_id, []).append(bool(passed))
    for case in cases_doc["cases"]:
        if case["id"] in unimplemented:
            continue
        rounds = evidence_cases.get(case["id"], [])
        if len(rounds) < 2 or not all(rounds):
            failures.append(f"case {case['id']}: missing or failing two-round evidence")
    if unimplemented:
        failures.append(
            f"{len(unimplemented)} cases are still unimplemented: {', '.join(unimplemented)}"
        )
    if failures:
        print("VERIFY-ALL REJECTED")
        for failure in failures:
            print(f"- {failure}")
        return 1
    print("VERIFY-ALL ACCEPTED")
    return 0


def suite_command(args):
    output = Path(args.output).resolve()
    if output.exists():
        raise CompatError(f"evidence output already exists: {output}")
    output.mkdir(parents=True)
    target_dir = os.environ.get("CARGO_TARGET_DIR")
    if not target_dir:
        raise CompatError("CARGO_TARGET_DIR must be set to the lease target")
    upstream = args.upstream or os.environ.get("XEH_UPSTREAM")
    if not upstream:
        raise CompatError("--upstream or XEH_UPSTREAM is required")
    upstream_path = Path(upstream)
    if not (upstream_path / "xeHentai" / "config.py").is_file():
        raise CompatError(f"invalid upstream tree: {upstream_path}")

    suites = (
        ["config", "util", "i18n", "worker", "task-api", "session", "files"]
        if args.suite == "all"
        else [args.suite]
    )
    cases_doc = load_cases()
    implemented = {
        case["id"] for case in cases_doc["cases"] if case["status"] in ("implemented", "delta")
    }
    head = subprocess.run(
        ["git", "rev-parse", "HEAD"], cwd=REPO, capture_output=True, text=True
    )
    status = subprocess.run(
        ["git", "status", "--porcelain"], cwd=REPO, capture_output=True, text=True
    )
    manifest = {
        "tool": "tools/compat.py",
        "started": datetime.now().isoformat(timespec="seconds"),
        "head": head.stdout.strip(),
        "dirty": bool(status.stdout.strip()),
        "upstream": str(upstream_path.resolve()),
        "suites": suites,
        "complete": False,
    }
    results = []
    for suite in suites:
        log(f"=== suite {suite} ===")
        results.append(run_suite(suite, output, upstream_path, Path(target_dir)))
    manifest["results"] = results
    manifest["self_checks"] = {result["suite"]: result["self_check"] for result in results}
    write_json(output / "manifest.json", manifest)
    covered = set()
    for result in results:
        for round_info in result["rounds"]:
            covered.update(round_info["cases"])
    manifest["covered_cases"] = sorted(covered)
    manifest["uncovered_implemented_cases"] = sorted(implemented - covered)
    write_json(output / "manifest.json", manifest)
    log(f"evidence written to {output}")
    return 0


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--suite",
        choices=[
            "config",
            "util",
            "i18n",
            "worker",
            "task-api",
            "session",
            "files",
            "all",
        ],
    )
    parser.add_argument("--output")
    parser.add_argument("--upstream")
    parser.add_argument("--verify-all", action="store_true")
    parser.add_argument("--evidence")
    parser.add_argument(
        "--oracle",
        choices=[
            "config",
            "util",
            "i18n",
            "task-api",
            "session",
            "session-fixture",
            "files",
        ],
    )
    parser.add_argument("--locale")
    parser.add_argument("--input")
    args = parser.parse_args(argv)

    try:
        if args.oracle:
            if not args.input or not args.output:
                raise CompatError("--oracle needs --input and --output")
            oracle_main(args)
            return 0
        if args.verify_all:
            if not args.evidence:
                raise CompatError("--verify-all needs --evidence")
            return verify_all(args.evidence)
        if not args.suite:
            raise CompatError("--suite is required")
        if not args.output:
            raise CompatError("--output is required")
        return suite_command(args)
    except CompatError as error:
        print(f"compat error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
