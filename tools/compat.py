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
}


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


def run_oracle(oracle, request, root, upstream, log_path, locale=None):
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
    isolated = root / "isolated"
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

    suites = ["config", "util", "i18n", "worker"] if args.suite == "all" else [args.suite]
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
    parser.add_argument("--suite", choices=["config", "util", "i18n", "worker", "all"])
    parser.add_argument("--output")
    parser.add_argument("--upstream")
    parser.add_argument("--verify-all", action="store_true")
    parser.add_argument("--evidence")
    parser.add_argument("--oracle", choices=["config", "util", "i18n"])
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
