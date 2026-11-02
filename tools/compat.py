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
    "download": {
        "tests::m4::pipeline_scans_pages_downloads_real_files_and_finishes": [
            "m4.pipeline.local",
            "m4.queue.src-while-active",
            "m4.task-loop.autosave",
        ],
        "tests::m4::partial_session_resume_downloads_only_missing_images_and_restart_is_quiet": [
            "m4.session.partial-resume"
        ],
        "tests::m4::archive_runs_after_downloads_rename_and_then_removes_the_directory": [
            "m4.archive.after-finish"
        ],
        "tests::m4::one_persistent_download_worker_keeps_cookies_across_urls": [
            "m4.worker.multi-url-cookie"
        ],
        "tests::m4::task_loop_records_last_task_guid_and_prefetches_the_next_metadata": [
            "m4.task-loop.last-guid",
            "m4.task-loop.prefetch",
        ],
    },
    "concurrency": {
        "tests::m4::configured_scan_and_download_workers_reach_the_server_barrier_limits": [
            "m4.workers.overlap-scan",
            "m4.workers.overlap-download",
            "m4.concurrent.limit",
        ],
        "tests::m4::pause_mid_body_aborts_the_owned_stage_and_resume_completes": [
            "m4.pause.mid-body"
        ],
        "tests::m4::pause_mid_page_drops_the_late_scan_result_and_resume_scans_again": [
            "m4.pause.mid-page"
        ],
        "tests::m4::pause_waits_for_a_queued_blocking_stage_open_before_reporting_quiescent": [
            "m4.pause.blocking-open"
        ],
        "tests::m4::same_run_and_same_guid_reuse_reject_old_scan_and_file_publication": [
            "m4.stale.same-run",
            "m4.stale.same-guid-owner",
        ],
        "tests::m4::same_task_duplicate_images_download_once_and_copy_for_the_second_fid": [
            "m4.duplicates.concurrent"
        ],
        "tests::m4::pending_dns_lookup_does_not_freeze_other_local_work_or_start_late_http": [
            "m4.dns.pending"
        ],
        "tests::m4::low_speed_stream_feeds_fake_response_through_the_same_filter_and_rescans": [
            "m4.retry.low-speed-rescan"
        ],
        "tests::m4::download_worker_retries_exactly_three_times_before_the_fake_rescan": [
            "m4.retry.worker-three"
        ],
        "tests::m4::task_loop_records_last_task_guid_and_prefetches_the_next_metadata": [
            "m4.task-loop.last-guid"
        ],
    },
    "monitor": {
        "tests::m4::monitor_reports_two_slow_windows_before_a_zero_speed_stuck_break": [
            "m4.monitor.native-slow-stuck"
        ],
        "tests::m4::monitor_quota_ignored_errors_and_zombie_reentry_follow_the_reference": [
            "m4.monitor.native-votes"
        ],
    },
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
    "transport": {
        "worker::tests::transport_get_preserves_header_order_duplicates_and_cookies": [
            "worker.header-bytes",
            "worker.session-cookies",
        ],
        "worker::tests::transport_manual_redirects_keep_real_url_history": [
            "worker.redirects"
        ],
        "worker::tests::transport_retries_use_random_plus_0618_backoff": [
            "worker.retry-backoff"
        ],
        "tests::m3_transport::site_end_to_end_metadata_page_image_queue_and_file_bytes": [
            "worker.redirects"
        ],
    },
    "proxy": {
        "proxy::tests::pool_selects_first_inserted_available_proxy": [
            "proxy.pool-order",
            "proxy.has-available",
        ],
        "proxy::tests::max_fail_disables_permanently_and_reports_inverted_availability": [
            "proxy.fail-policy",
            "proxy.has-available",
        ],
        "proxy::tests::expired_ban_returns_the_proxy_to_the_first_available_slot": [
            "proxy.ban-expiry",
        ],
        "proxy::tests::add_replaces_counts_but_keeps_insertion_position": [
            "proxy.pool-order",
            "proxy.add-validation",
        ],
        "proxy::tests::addresses_are_classified_in_the_reference_order": [
            "proxy.add-validation"
        ],
        "tests::m3_transport::proxy_http_absolute_form_uses_proxy_without_local_dns": [
            "proxy.http"
        ],
        "tests::m3_transport::proxy_https_connect_tunnels_to_target_tls": [
            "proxy.https"
        ],
        "tests::m3_transport::proxy_https_outer_tls_then_connect_then_target_tls": [
            "proxy.https"
        ],
        "tests::m3_transport::socks4_and_socks4a_prove_local_vs_proxy_dns": [
            "proxy.socks4",
            "proxy.socks4a",
        ],
        "tests::m3_transport::socks5_and_socks5h_prove_local_vs_proxy_dns": [
            "proxy.socks5",
            "proxy.socks5h",
        ],
        "tests::m3_transport::glype_rewrites_sslagree_cookie_and_text_then_streams_binary": [
            "proxy.glype"
        ],
        "tests::m3_transport::glype_403_binary_streams_real_bytes_instead_of_empty_success": [
            "proxy.glype"
        ],
        "tests::m3_transport::glype_error_div_is_a_pool_exception_not_a_transport_failure": [
            "proxy.glype"
        ],
        "tests::m3_transport::fallback_ip_forces_tls_with_original_host_and_skips_dns": [
            "proxy.fallback-ip"
        ],
        "tests::m3_transport::image_only_policy_skips_webpages_and_uses_proxy_for_images": [
            "proxy.policy"
        ],
        "tests::m3_transport::image_only_policy_proxies_non_webpage_urls": [
            "proxy.policy"
        ],
        "tests::m3_transport::proxy_config_update_adds_to_the_shared_pool": [
            "proxy.pool-order"
        ],
    },
    "site": {
        "filters::tests::login_parses_cookie_header_and_reports_callback_events": ["filters.login"],
        "filters::tests::metadata_reads_reference_fields_and_status_branches": ["filters.metadata"],
        "filters::tests::pageurl_returns_every_reference_url_and_debug_body": ["filters.page"],
        "filters::tests::mpv_parses_imagelist_keys_in_order": ["filters.mpv"],
        "filters::tests::imgurl_matches_keystamp_style_and_original_branches": ["filters.image-link"],
        "filters::tests::quota_wrapper_keeps_reference_order_and_codes": ["filters.quota"],
        "filters::tests::download_wrapper_distinguishes_404_length_and_stream": ["filters.download-response"],
        "filters::tests::reset_quota_is_a_real_no_op": ["filters.reset-quota"],
        "filters::tests::sdk_compiles_binds_and_calls_exported_filter_functions": [
            "filters.login",
            "filters.metadata",
            "filters.page",
            "filters.mpv",
            "filters.quota",
            "filters.image-link",
            "filters.download-response",
            "filters.reset-quota",
        ],
    },
    "rpc": {
        "rpc::tests::method_select_maps_all_thirteen_wire_names": [
            "rpc.method.lookup",
        ],
        "rpc::tests::method_select_rejects_unknown_private_and_bare_names": [
            "rpc.method.unknown",
            "rpc.method.private",
        ],
        "rpc::tests::arity_rules_match_reference_type_errors": [
            "rpc.arity",
        ],
        "rpc::tests::update_config_filter_drops_rpc_keys_and_detects_proxy": [
            "rpc.updateConfig.filter",
        ],
        "rpc::tests::level_select_covers_default_negation_and_invalid": [
            "rpc.listTasks.level",
        ],
        "rpc::tests::image_range_preserves_first_item_quirk": [
            "rpc.getImage.range",
        ],
        "rpc::tests::field_projections_hide_exact_reference_keys": [
            "rpc.projection.config",
            "rpc.projection.task",
        ],
        "rpc::tests::auth_requires_exact_literal_token": [
            "rpc.auth.exact",
        ],
        "rpc::tests::response_id_coerces_falsy_values_to_null": [
            "rpc.wire.id",
        ],
        "rpc::tests::image_mime_keeps_reference_typo_key": [
            "gallery.mime",
        ],
        "rpc::server::tests::wire_envelope_reports_parse_and_unknown_method": [
            "rpc.wire.parse",
        ],
        "rpc::server::tests::get_config_hides_rpc_keys_and_reports_arity": [
            "rpc.getConfig.ok",
        ],
        "rpc::server::tests::update_config_without_proxy_is_a_read": [
            "rpc.updateConfig.noop",
        ],
        "rpc::server::tests::unknown_method_is_404_and_unauthorized_is_403": [
            "rpc.method.unknown",
            "rpc.auth.missing",
        ],
        "rpc::server::tests::thumbnail_query_parsing_matches_reference_strictness": [
            "rpc.thumb.query",
        ],
        "rpc::server::tests::gallery_names_reject_traversal": [
            "gallery.traversal",
        ],
        "rpc::server::tests::pixel_bound_rejects_huge_dimensions": [
            "gallery.pixel-bound",
        ],
        "rpc::server::tests::signed_gallery_paths_wait_for_authorization": [
            "rpc.sign.pending",
        ],
        "rpc::server::tests::task_lifecycle_codes_match_reference": [
            "rpc.task.lifecycle",
        ],
        "rpc::server::tests::set_cookie_and_session_roundtrip": [
            "rpc.session.roundtrip",
        ],
        "rpc::server::tests::update_config_with_proxy_applies_and_dir_without_proxy_is_noop": [
            "rpc.updateConfig.proxy",
        ],
        "rpc::server::tests::update_config_proxy_nodir_isolated_applies_and_returns_config": [
            "rpc.updateConfig.proxy-nodir-isolated",
        ],
        "rpc::server::tests::update_config_proxy_empty_isolated_keeps_pool_semantics": [
            "rpc.updateConfig.proxy-empty-isolated",
        ],
        "rpc::server::tests::update_config_proxy_existing_pool_is_additive": [
            "rpc.updateConfig.proxy-existing-isolated",
        ],
        "rpc::server::tests::list_tasks_levels_negation_and_invalid": [
            "rpc.listTasks.level",
        ],
        "rpc::server::tests::get_image_reports_missing_tasks_and_bad_ranges": [
            "rpc.getImage.errors",
        ],
        "rpc::server::tests::get_image_returns_real_signed_urls_and_serves_them": [
            "rpc.getImage.signed",
        ],
        "rpc::server::tests::post_roundtrip_preserves_framing_id_and_cors": [
            "rpc.wire.framing",
        ],
        "rpc::server::tests::post_auth_roundtrip_exact_literal_only": [
            "rpc.auth.exact",
        ],
        "rpc::server::tests::get_and_options_roundtrips": [
            "rpc.http.options",
            "rpc.http.unknown-get",
        ],
        "rpc::server::tests::malformed_posts_answer_400_not_dropped": [
            "rpc.wire.parse",
        ],
        "rpc::server::tests::owned_listener_binds_accepts_and_drains": [
            "rpc.server.owned-start",
        ],
        "rpc::server::tests::rss_mutant_changes_list_tasks_selection": [
            "rpc.rss.mutant",
        ],
        "rpc::server::tests::rss_mutant_changes_drain_routing": [
            "rpc.rss.mutant",
        ],
        "rpc::server::tests::runtime_mounted_pause_and_delete_drain": [
            "rpc.server.drain"
        ],
        "rpc::server::tests::secret_login_authenticates_once_and_prepares_credentials": [
            "rpc.auth.exact",
        ],
        "rpc::server::tests::login_arity_and_type_errors_survive_single_auth": [
            "rpc.auth.exact",
        ],
        "rpc::server::tests::secret_login_runs_real_loopback_login_and_keeps_session": [
            "rpc.auth.exact",
        ],
        "rpc::server::tests::snake_pause_and_delete_take_the_runtime_drain_path": [
            "rpc.task.lifecycle",
        ],
        "rpc::server::tests::snake_pause_and_delete_drain_over_wire": [
            "rpc.task.lifecycle",
        ],
        "rpc::server::tests::snake_delete_running_is_refused_over_wire": [
            "rpc.task.lifecycle",
            "rpc.delTask.running",
        ],
        "rpc::server::tests::accept_loop_caps_active_connections_and_reaps": [
            "rpc.server.conn-cap",
        ],
        "rpc::server::tests::media_permits_gate_heavy_decode": [
            "rpc.media.permit-queue",
        ],
        "rpc::server::tests::media_spool_guard_unlinks_on_drop": [
            "gallery.stream.spool-cleanup",
        ],
        "rpc::server::tests::signed_gallery_paths_validate_real_signatures": [
            "rpc.sign.pending",
            "rpc.img.signed",
            "rpc.zip.signed",
        ],
        "rpc::server::tests::thumbnail_query_parsing_matches_reference_strictness": [
            "rpc.thumb.query"
        ],
        "rpc::server::tests::gallery_names_reject_traversal": [
            "gallery.traversal"
        ],
    },
    "gallery": {
        "rpc::server::tests::gallery_reads_serve_files_and_reject_bad_fids": [
            "gallery.read.task-wait.1",
            "gallery.read.task-wait.5",
            "gallery.read.task-wait.nope",
            "gallery.read.missing.1",
        ],
        "rpc::server::tests::gallery_reads_fall_back_to_sibling_zip": [
            "gallery.read.task-zip.2",
            "gallery.read.task-zip.9",
        ],
        "rpc::server::tests::archive_builds_on_demand_and_reuses": [
            "gallery.archive.task-wait",
        ],
        "rpc::server::tests::thumbnails_fit_never_upscale_and_keep_format": [
            "gallery.thumb.task-wait.1.resized",
        ],
        "rpc::server::tests::static_cache_ttl_stale_persist_and_legacy_refusal": [
            "rpc.static.cache",
        ],
        "rpc::server::tests::static_fetch_honors_304_and_reports_origin_errors": [
            "rpc.static.fetch",
        ],
        "rpc::server::tests::pixel_bound_rejects_huge_dimensions": [
            "gallery.pixel-bound",
        ],
        "rpc::server::tests::gallery_symlink_escape_and_reserved_are_denied": [
            "gallery.traversal",
        ],
        "rpc::server::tests::gallery_sibling_archive_symlink_is_denied": [
            "gallery.read.task-zip.2",
            "gallery.read.task-zip.9",
        ],
        "rpc::server::tests::gallery_dispatch_releases_before_reads": [
            "gallery.read.task-wait.1",
        ],
        "rpc::server::tests::gallery_thumbnail_input_budgets_fire_before_decode": [
            "gallery.pixel-bound",
        ],
        "rpc::server::tests::gallery_zip_entry_budget_refuses_before_allocation": [
            "gallery.read.task-zip.2",
            "gallery.read.task-zip.9",
        ],
        "rpc::server::tests::gallery_stream_large_file_chunks_pattern": [
            "gallery.stream.large-file",
        ],
        "rpc::server::tests::gallery_stream_wire_framing": [
            "gallery.stream.large-file",
            "rpc.wire.framing",
        ],
        "rpc::server::tests::gallery_stream_large_archive_entries": [
            "gallery.stream.large-file",
            "gallery.archive.task-wait",
        ],
        "rpc::server::tests::gallery_stream_spool_cleanup": [
            "gallery.stream.spool-cleanup",
        ],
        "rpc::server::tests::gallery_disconnect_during_stream_drains_cleanly": [
            "rpc.media.disconnect-drain",
        ],
        "rpc::server::tests::gallery_responsive_during_media_stream": [
            "gallery.stream.large-file",
        ],
        "rpc::server::tests::static_cache_bounds_entries_and_total": [
            "rpc.static.cache",
        ],
        "rpc::server::tests::static_persisted_cache_load_enforces_bounds": [
            "rpc.static.cache",
        ],
        "rpc::server::tests::static_fetch_refuses_oversize_before_buffering": [
            "rpc.static.fetch",
        ],
    },
    "webui": {
        "rpc::server::tests::frozen_pack_serves_assets_and_rejects_traversal": [
            "webui.pack.files",
            "webui.pack.index",
        ],
        "rpc::server::tests::signed_gallery_paths_wait_for_authorization": [
            "rpc.sign.pending",
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
    "site": [["cargo", "test", "--bin", "xeHentai", "filters::"]],
    "proxy": [
        ["cargo", "test", "--bin", "xeHentai", "proxy::"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m3_transport::proxy_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m3_transport::socks"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m3_transport::glype_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m3_transport::fallback_"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m3_transport::image_only_"],
    ],
    "transport": [
        ["cargo", "test", "--bin", "xeHentai", "transport_"],
        ["cargo", "test", "--bin", "xeHentai", "site_end_to_end"],
    ],
    "monitor": [
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::monitor_"],
    ],
    "download": [
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::pipeline"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::partial_session"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::archive_runs"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::one_persistent"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::task_loop_records"],
    ],
    "concurrency": [
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::configured"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::pause_mid"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::pause_waits"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::same_run"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::same_task_duplicate"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::pending_dns"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::low_speed"],
        ["cargo", "test", "--bin", "xeHentai", "tests::m4::download_worker_retries"],
    ],
    "rpc": [
        ["cargo", "test", "--bin", "xeHentai", "rpc::tests::"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::task_"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::set_cookie"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::update_config"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::list_tasks"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::get_image"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::post_"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::get_and_options"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::malformed_"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::owned_listener"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::rss_mutant"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::runtime_mounted"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::signed_gallery"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::thumbnail_query"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::gallery_names"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::secret_login"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::login_arity"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::snake_"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::accept_loop"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::media_"],
    ],
    "gallery": [
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::gallery_"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::archive_"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::thumbnails_"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::static_"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::pixel_"],
    ],
    "webui": [
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::frozen_"],
        ["cargo", "test", "--bin", "xeHentai", "rpc::server::tests::signed_gallery"],
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
        "history",
        "updated",
        "multi",
        "subdir",
        "external",
    ],
    "task.session-cookies": ["cookies", "logged_in"],
    "task.session-atomic": ["save_false"],
    "task.session-dir-history": [
        "history",
        "updated",
        "multi",
        "subdir",
        "external",
    ],
    "task.session-dir-authorization": ["isolation", "isolation_legacy"],
}
TRANSPORT_CASES = {
    "worker.redirects": ["redirect"],
    "worker.retry-backoff": ["retry"],
    "worker.session-cookies": ["cookies"],
    "worker.header-bytes": ["headers"],
    "worker.post-form": ["post"],
}

PROXY_CASES = {
    "proxy.pool-order": ["steps"],
    "proxy.fail-policy": ["steps"],
    "proxy.ban-expiry": ["steps"],
    "proxy.add-validation": ["steps"],
    "proxy.has-available": ["steps"],
    "proxy.glype-runtime": [
        "glype.normal",
        "glype.sslagree-fixed",
        "glype.post-fixed",
    ],
}

SITE_CASES = {
    "filters.login": [
        "login.ok",
        "login.postcolor",
        "login.empty-cookie",
        "login.missing-header",
    ],
    "filters.metadata": [
        "metadata.ok",
        "metadata.404",
        "metadata.600",
        "metadata.pining",
        "metadata.ip-ban",
        "metadata.ip-ban-no-time",
        "metadata.missing-gj",
    ],
    "filters.page": ["pageurl.ok", "pageurl.none"],
    "filters.mpv": ["mpv.ok", "mpv.invalid", "mpv.missing-key"],
    "filters.image-link": [
        "imgurl.keystamp",
        "imgurl.style",
        "imgurl.image-php",
        "imgurl.original",
        "imgurl.invalid-page",
        "imgurl.no-src",
    ],
    "filters.quota": [
        "quota.403",
        "quota.600",
        "quota.509",
        "quota.length-925",
        "quota.gif",
        "quota.small-text",
        "quota.small-text-no-type",
    ],
    "filters.download-response": [
        "download.stream",
        "download.404",
        "download.zero",
        "download.size-mismatch",
        "download.no-stream",
        "download.stream-ok",
        "download.stream-short",
        "download.stream-error",
    ],
    "filters.reset-quota": ["reset.no-op"],
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
    elif args.oracle == "site":
        result = oracle_site(upstream, request)
    elif args.oracle == "proxy":
        result = oracle_proxy(upstream, request)
    elif args.oracle == "transport":
        result = oracle_transport(upstream, request)
    elif args.oracle == "monitor":
        result = oracle_monitor(upstream, request)
    elif args.oracle == "download":
        result = oracle_download(upstream, request)
    elif args.oracle == "rpc":
        result = oracle_rpc(upstream, request)
    elif args.oracle == "gallery":
        result = oracle_gallery(upstream, request)
    elif args.oracle == "webui":
        result = oracle_webui(upstream, request)
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
UNSAFE_SESSION_DIR = "/definitely/not/authorized"
UNSAFE_SESSION_GUID = "e0000005"
SESSION_SAFETY_SCENARIOS = {"isolation", "isolation_legacy"}
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



def site_scenarios():
    """Controlled response fixtures shared by the Python oracle and Rust probe."""
    hash40 = "0123456789abcdef0123456789abcdef01234567"

    def page(fn, **kwargs):
        text = kwargs.pop("text", "")
        scenario = {
            "id": kwargs.pop("id"),
            "fn": fn,
            "status": kwargs.pop("status", 200),
            "url": kwargs.pop("url", "https://e-hentai.org/s/abcdef0123/123456-7"),
            "real_url": kwargs.pop("real_url", "https://e-hentai.org/s/abcdef0123/123456-7"),
            "text": text,
            "headers": kwargs.pop("headers", [["Content-Type", "text/html; charset=utf-8"]]),
            "content_length": kwargs.pop("content_length", len(text.encode("utf-8"))),
            "ori": kwargs.pop("ori", False),
            "has_stream": kwargs.pop("has_stream", False),
        }
        if "chunks" in kwargs:
            scenario["chunks"] = kwargs.pop("chunks")
        if "stream_error" in kwargs:
            scenario["stream_error"] = kwargs.pop("stream_error")
        return scenario

    metadata_text = (
        '="gj">日本語 &amp; 表題</h1>="gn">English &amp; title</h1>'
        'Length:</td><td class="gdt2">12 pages</td>'
        "toggle_tagmenu('tag a' toggle_tagmenu('tag b'"
        "Showing 1 - 20 of 234 images"
        '<a href="https://e-hentai.org/mpv/123/abcdef0123/#page2"><div title="Page'
    )
    page_text = (
        '<a href="https://e-hentai.org/s/abcdef0123/123456-1"><div title="Page 1"'
        '<a href="https://exhentai.org/s/0123456789/123456-2"><img alt="2" title="Page 2"'
    )
    image_text = (
        f'<img src="https://img.example/{hash40}-100-200-0-jpg/keystamp=abc" style="max-width:100%">'
        '</a></div><div>pretty name.jpg :: 100x200 :: 2 KB</di>'
        '<a href="https://img.example/original/name.jpg">Download original 100 x 200 2 KB source</a>'
        "return nl('123456-7')"
    )
    style_text = (
        f'<img src="https://img.example/{hash40}-100-200-0-jpg/" style="x">'
        '</a></div><div>style name.jpg :: 100x200 :: 2 KB</di>'
        "return nl('123456-7')"
    )
    image_php_text = (
        f'<img src="https://img.example/{hash40}-100-200-0-jpg/image.php?n=real-name.png" style="x">'
        '</a></div><div>image.php :: 100x200 :: 2 KB</di>'
        "return nl('123456-7')"
    )
    image_url = f"https://img.example/{hash40}-100-200-0-jpg/"
    download_url = f"https://img.example/{hash40}-100-200-0-jpg/"
    stream_url = f"https://img.example/{hash40}-10-200-0-jpg/"
    username_headers = [
        ["Set-Cookie", "ipb_member_id=42; expires=Wed; ipb_pass_hash=deadbeef; path=/"]
    ]

    scenarios = [
        page("login", id="login.ok", headers=username_headers),
        page(
            "login",
            id="login.postcolor",
            text='<span class="postcolor">bad password</span>',
            headers=[["Set-Cookie", "other=1; path=/"]],
        ),
        page(
            "login",
            id="login.empty-cookie",
            headers=[["Set-Cookie", "other=1; path=/"]],
        ),
        page("login", id="login.missing-header", headers=[]),
        page("metadata", id="metadata.ok", text=metadata_text),
        page("metadata", id="metadata.404", status=404),
        page("metadata", id="metadata.600", status=600),
        page(
            "metadata",
            id="metadata.pining",
            text="This gallery is pining for the fjords",
        ),
        page(
            "metadata",
            id="metadata.ip-ban",
            text="Your IP address has been temporarily banned. The ban expires in 5 minutes",
        ),
        page(
            "metadata",
            id="metadata.ip-ban-no-time",
            text="Your IP address has been temporarily banned",
        ),
        page("metadata", id="metadata.missing-gj", text='="gn">only</h1>'),
        page("pageurl", id="pageurl.ok", text=page_text),
        page("pageurl", id="pageurl.none", text="no links here"),
        page(
            "pageurl_mpv",
            id="mpv.ok",
            real_url="https://e-hentai.org/mpv/123/abcdef0123",
            text='imagelist = [{"k":"aaaa"},{"k":"bbbb"}];',
        ),
        page(
            "pageurl_mpv",
            id="mpv.invalid",
            real_url="https://e-hentai.org/mpv/123/abcdef0123",
            text="imagelist = nope;",
        ),
        page(
            "pageurl_mpv",
            id="mpv.missing-key",
            real_url="https://e-hentai.org/mpv/123/abcdef0123",
            text='imagelist = [{"k":"aaaa"},{"v":"bbbb"}];',
        ),
        page("imgurl", id="imgurl.keystamp", text=image_text, content_length=5000),
        page("imgurl", id="imgurl.style", text=style_text, content_length=5000),
        page("imgurl", id="imgurl.image-php", text=image_php_text, content_length=5000),
        page(
            "imgurl",
            id="imgurl.original",
            text=image_text,
            content_length=5000,
            ori=True,
        ),
        page("imgurl", id="imgurl.invalid-page", text="Invalid page"),
        page("imgurl", id="imgurl.no-src", text="nothing"),
        page(
            "imgurl",
            id="quota.403",
            status=403,
            url=image_url,
            real_url=image_url,
            content_length=12,
        ),
        page(
            "imgurl",
            id="quota.600",
            status=600,
            url=image_url,
            real_url=image_url,
            content_length=0,
        ),
        page(
            "imgurl",
            id="quota.509",
            status=509,
            url=image_url,
            real_url=image_url,
            content_length=12,
        ),
        page(
            "imgurl",
            id="quota.length-925",
            url=image_url,
            real_url=image_url,
            content_length=925,
        ),
        page(
            "imgurl",
            id="quota.gif",
            url="https://img.example/509.gif",
            real_url="https://img.example/509.gif",
            content_length=100,
        ),
        page(
            "imgurl",
            id="quota.small-text",
            url=image_url,
            real_url=image_url,
            content_length=42,
            text="You have exceeded your image viewing limits",
            headers=[["Content-Type", "text/html; charset=utf-8"]],
        ),
        page(
            "imgurl",
            id="quota.small-text-no-type",
            url=image_url,
            real_url=image_url,
            content_length=42,
            text="You have exceeded your image viewing limits",
            headers=[],
        ),
        page(
            "download",
            id="download.stream",
            url=stream_url,
            real_url=stream_url,
            content_length=100,
            has_stream=True,
        ),
        page("download", id="download.404", status=404, url=stream_url, real_url=stream_url),
        page(
            "download",
            id="download.zero",
            url=stream_url,
            real_url=stream_url,
            content_length=0,
        ),
        page(
            "download",
            id="download.size-mismatch",
            url=stream_url,
            real_url=stream_url,
            content_length=99,
        ),
        page(
            "download",
            id="download.no-stream",
            url=stream_url,
            real_url=stream_url,
            content_length=100,
            has_stream=False,
        ),
        page(
            "download_stream",
            id="download.stream-ok",
            url=stream_url,
            real_url=stream_url,
            content_length=10,
            chunks=["12345", "67890"],
        ),
        page(
            "download_stream",
            id="download.stream-short",
            url=stream_url,
            real_url=stream_url,
            content_length=10,
            chunks=["12345"],
        ),
        page(
            "download_stream",
            id="download.stream-error",
            url=stream_url,
            real_url=stream_url,
            content_length=10,
            chunks=[],
            stream_error="connection",
        ),
        page("reset_quota", id="reset.no-op"),
    ]
    return scenarios


class _SiteFakeResponse:
    def __init__(self, scenario):
        self.status_code = scenario["status"]
        self.url = scenario["url"]
        self._real_url = scenario["real_url"]
        self._text = scenario["text"]
        self.content = self._text.encode("utf-8")
        self.content_length = scenario["content_length"]
        joined = {}
        order = []
        for name, value in scenario["headers"]:
            key = name.lower()
            if key in joined:
                joined[key] += ", " + value
            else:
                joined[key] = value
                order.append(key)
        self.headers = joined
        self._chunks = [chunk.encode("utf-8") for chunk in scenario.get("chunks", [])]
        self._stream_error = scenario.get("stream_error")
        if scenario["fn"] == "download" or "chunks" in scenario:
            self.iter_content_cb = None

    @property
    def text(self):
        return self._text

    def iter_content(self, chunk_size):
        if self._stream_error == "connection":
            from requests.exceptions import ConnectionError

            raise ConnectionError("synthetic stream reset")
        for chunk in self._chunks:
            yield chunk


def _site_norm(value):
    if isinstance(value, dict):
        return {key: _site_norm(item) for key, item in value.items()}
    if isinstance(value, (list, tuple)):
        return [_site_norm(item) for item in value]
    if callable(value):
        return "STREAM"
    return value


def oracle_site(upstream, request):
    sys.path.insert(0, str(upstream))
    import xeHentai.filters as filters
    from xeHentai.const import DownloadAbortedException

    out = {}
    for scenario in request["scenarios"]:
        fn = scenario["fn"]
        response = _SiteFakeResponse(scenario)
        callbacks = []
        stream_callbacks = []

        def suc(value, callbacks=callbacks):
            callbacks.append(["suc", [_site_norm(value)]])

        def fail(value, callbacks=callbacks):
            callbacks.append(["fail", [_site_norm(value)]])

        exception = None
        ret = None
        debug = None
        stream_exception = None
        try:
            if fn == "login":
                ret = filters.login_exhentai(response, suc, fail)
            elif fn == "metadata":
                ret = filters.flt_metadata(response, suc, fail)
            elif fn == "pageurl":
                ret = filters.flt_pageurl(response, suc, fail)
            elif fn == "pageurl_mpv":
                ret = filters.flt_pageurl_mpv(response, suc, fail)
            elif fn == "imgurl":
                import xeHentai.filters as filters_module

                wrapper = filters_module.flt_imgurl_wrapper(scenario.get("ori", False))
                ret = wrapper(response, suc, fail)
            elif fn == "download":
                ret = filters.download_file_wrapper(".")(response, suc, fail)
                if callbacks and callbacks[0][0] == "suc":
                    callbacks[0][1] = [["STREAM", response._real_url, response.url]]
            elif fn == "download_stream":
                wrapper = filters.download_file_wrapper(".")
                captured = []

                def stream_suc(value):
                    captured.append(value)

                wrapper(response, stream_suc, fail)
                callbacks.clear()
                if captured:
                    response.iter_content_cb = lambda chunk: stream_callbacks.append(
                        ["chunk", len(chunk)]
                    )
                    try:
                        for _ in captured[0][0]():
                            pass
                    except DownloadAbortedException:
                        stream_exception = "DownloadAbortedException"
                stream_callbacks.extend(callbacks)
                callbacks.clear()
            elif fn == "reset_quota":
                ret = filters.reset_quota(response, suc, fail)
            else:
                raise RuntimeError(f"unknown site fn {fn}")
            debug_path = os.path.join(os.getcwd(), "debug.htm")
            if os.path.exists(debug_path):
                with open(debug_path, encoding="utf-8") as handle:
                    debug = handle.read()
                os.unlink(debug_path)
        except Exception as error:  # noqa: BLE001 - record the reference exception
            exception = type(error).__name__

        observation = {
            "callbacks": callbacks,
            "ret": _site_norm(ret),
            "exception": exception,
            "debug": debug,
        }
        if fn == "download_stream":
            observation["stream_callbacks"] = stream_callbacks
            observation["stream_exception"] = stream_exception
        out[scenario["id"]] = observation
    return out



def proxy_scenarios():
    a = "http://127.0.0.1:8081"
    b = "socks5h://127.0.0.1:1080"
    c = "socks4a://127.0.0.1:1082"
    return {
        "clock": 100.0,
        "glype_addr": "http://proxy.test/glype/browse.php?u=x&b=4",
        "glype_target": "https://e-hentai.org/g/1/",
        "steps": [
            {"op": "add", "addr": a},
            {"op": "add", "addr": b},
            {"op": "add", "addr": c},
            {"op": "select"},
            {"op": "not_good", "addr": a, "weight": 5, "expire": 0},
            {"op": "select"},
            {"op": "trace", "addr": b, "result": "connect_error"},
            {"op": "trace", "addr": b, "result": "connect_error"},
            {"op": "trace", "addr": b, "result": "connect_error"},
            {"op": "trace", "addr": b, "result": "connect_error"},
            {"op": "trace", "addr": b, "result": "ok"},
            {"op": "trace", "addr": b, "result": "ok"},
            {"op": "select"},
            {"op": "has_available"},
            {"op": "snapshot"},
            {"op": "time", "now": 106.0},
            {"op": "not_good", "addr": c, "weight": 5, "expire": 3},
            {"op": "has_available"},
            {"op": "time", "now": 120.0},
            {"op": "select"},
            {"op": "add", "addr": "http://bad/path"},
            {"op": "add", "addr": "https://127.0.0.1:8443"},
            {"op": "add", "addr": "socks4://127.0.0.1:1083"},
            {"op": "add", "addr": "socks5h://127.0.0.1:1084"},
            {"op": "add", "addr": "socks5://127.0.0.1:1085"},
            {"op": "snapshot"},
        ],
    }


def oracle_proxy(upstream, request):
    sys.path.insert(0, str(upstream))
    import xeHentai.proxy as proxy_module
    from requests.exceptions import ConnectionError as RequestsConnectionError

    clock = {"now": request["clock"]}
    proxy_module.time.time = lambda: clock["now"]

    class DummySession:
        def request(self, *args, **kwargs):
            return kwargs.get("proxies", {})

    pool = proxy_module.Pool()
    session = DummySession()
    out = []
    for step in request["steps"]:
        kind = step["op"]
        record = {"op": kind}
        try:
            if kind == "add":
                pool.add_proxy(step["addr"])
                record["result"] = None
            elif kind == "select":
                handler, not_good = pool.proxied_request(session)
                record["selected"] = handler("GET", "http://example/")["http"]
                record["result"] = None
            elif kind == "not_good":
                pool.not_good(step["addr"])(step["weight"], step["expire"])
                record["result"] = None
            elif kind == "trace":
                result = step["result"]
                if result == "connect_error":
                    def operation():
                        raise RequestsConnectionError("synthetic connect error")

                    pool.trace_proxy(step["addr"])(operation)()
                elif result == "ok":
                    pool.trace_proxy(step["addr"])(lambda: "ok")()
                elif result == "check_false":
                    pool.trace_proxy(
                        step["addr"], check_func=lambda value: False
                    )(lambda: "bad")()
                elif result == "bad_status_line":
                    def operation():
                        raise RequestsConnectionError("BadStatusLine: junk")

                    pool.trace_proxy(step["addr"])(operation)()
                record["result"] = None
            elif kind == "has_available":
                record["result"] = pool.has_available_proxies()
            elif kind == "time":
                clock["now"] = step["now"]
                record["result"] = None
            elif kind == "snapshot":
                record["result"] = None
            else:
                raise RuntimeError(f"unknown proxy op {kind}")
        except Exception as error:  # noqa: BLE001
            record["exception"] = type(error).__name__
        record["entries"] = [
            [addr, entry[1], entry[2]] for addr, entry in pool.proxies.items()
        ]
        record["disabled"] = [[addr, expire] for addr, expire in pool.disabled.items()]
        record["has_available"] = pool.has_available_proxies()
        out.append(record)
    out = {"steps": out}
    out["glype"] = _oracle_glype(request, upstream)
    return out


def _oracle_glype(request, upstream):
    sys.path.insert(0, str(upstream))
    import xeHentai.proxy as proxy_module
    from xeHentai import util

    class FakeResponse:
        def __init__(self, body, headers=None, status=200):
            self.status_code = status
            self.url = request["glype_target"]
            self._real_url = request["glype_target"]
            self.headers = dict(headers or {"content-type": "text/html; charset=utf-8"})
            self._content = body.encode("utf-8")

        @property
        def content(self):
            return self._content

        @property
        def text(self):
            return self._content.decode("utf-8")

    class FakeSession:
        def __init__(self, responses):
            self.headers = {
                "User-Agent": "xeHentai-test",
                "Cookie": "a=1",
            }
            self.responses = list(responses)
            self.calls = []

        def request(self, *args, **kwargs):
            from urllib.parse import urlencode

            headers = {
                key.lower(): value for key, value in kwargs.get("headers", {}).items()
            }
            data = kwargs.get("data")
            if isinstance(data, (list, tuple, dict)):
                body = urlencode(data)
            elif data is None:
                body = ""
            else:
                body = str(data)
            self.calls.append(
                {
                    "method": args[0],
                    "url": args[1],
                    "referer": headers.get("referer"),
                    "cookie": headers.get("cookie", ""),
                    "body": body,
                }
            )
            return self.responses.pop(0)

    def run(responses, with_caller_kwargs, method="GET", data=None):
        session = FakeSession(responses)
        handler = proxy_module.glype_proxy(
            request["glype_addr"], lambda addr, **kwargs: (lambda fn: fn)
        )
        fn = handler(session)
        kwargs = {"stream": False, "data": data, "timeout": 3}
        if with_caller_kwargs:
            kwargs["allow_redirects"] = False
        from urllib.parse import urlsplit

        def norm_calls(calls):
            out_calls = []
            for call in calls:
                parsed = urlsplit(call["url"])
                path = parsed.path + (("?" + parsed.query) if parsed.query else "")
                referer = call.get("referer")
                if referer:
                    referer = urlsplit(referer).path
                out_calls.append(
                    {
                        "method": call["method"],
                        "path": path,
                        "referer": referer,
                        "cookie": call.get("cookie", ""),
                        "body": call.get("body", ""),
                    }
                )
            return out_calls

        try:
            result = fn(method, request["glype_target"], **kwargs)
            return {
                "exception": None,
                "status": result.status_code,
                "text": result._content.decode("utf-8"),
                "set_cookie": result.headers.get("set-cookie"),
                "calls": norm_calls(session.calls),
            }
        except Exception as error:  # noqa: BLE001
            return {
                "exception": type(error).__name__,
                "calls": norm_calls(session.calls),
            }

    normal_body = '<a href="/glype/browse.php?u=https%3A%2F%2Fe-hentai.org%2Fg%2F1&b=4&f=norefer">x</a>'
    normal = run(
        [
            FakeResponse(
                normal_body,
                {
                    "content-type": "text/html; charset=utf-8",
                    "set-cookie": "c[proxy][/][foo]=bar",
                },
            )
        ],
        True,
    )
    ssl_agree = '<input type="hidden" name="action" value="sslagree">'
    ssl_responses = [
        lambda: FakeResponse(ssl_agree),
        lambda: FakeResponse("ok", {"content-type": "text/html", "set-cookie": "s=abc"}),
        lambda: FakeResponse(
            normal_body,
            {
                "content-type": "text/html; charset=utf-8",
                "set-cookie": "c[proxy][/][foo]=bar",
            },
        ),
    ]
    fixed = run([factory() for factory in ssl_responses], False)
    caller = run([factory() for factory in ssl_responses], True)
    post_data = [
        ("username", "synthetic-user"),
        ("password", "synthetic-pass"),
    ]
    post = run([factory() for factory in ssl_responses], False, "POST", post_data)
    return {
        "normal": normal,
        "sslagree_fixed": fixed,
        "sslagree_caller": caller,
        "post_fixed": post,
    }


def compare_proxy(cases, oracle, candidate):
    # Source quirk: CPython raises RuntimeError when `for d in self.disabled`
    # deletes the current entry, so the reference aborts the first request that
    # would clean an expired ban. The candidate performs the same cleanup
    # safely. Both complete states are asserted here; only this step is
    # excluded from the generic diff and recorded as a delta case.
    oracle_steps = copy.deepcopy(oracle["steps"])
    candidate_steps = copy.deepcopy(candidate["steps"])
    expiry_ok = False
    if len(oracle_steps) > 19 and len(candidate_steps) > 19:
        o = oracle_steps[19]
        c = candidate_steps[19]
        expiry_ok = (
            o.get("exception") == "RuntimeError"
            and o.get("op") == "select"
            and "selected" not in o
            and o.get("disabled") == [["socks4a://127.0.0.1:1082", 109.0]]
            and c.get("op") == "select"
            and c.get("selected") == "http://127.0.0.1:8081"
            and c.get("disabled") == [["http://127.0.0.1:8081", 0.0]]
            and c.get("entries")
            == [
                ["http://127.0.0.1:8081", 2, 5],
                ["socks5h://127.0.0.1:1080", 4, 4],
                ["socks4a://127.0.0.1:1082", 0, 5],
            ]
        )
        oracle_steps.pop(19)
        candidate_steps.pop(19)
        if expiry_ok:
            # The documented delta changes the subsequent pool snapshot; keep
            # comparing every later operation result while taking the candidate
            # state as the post-delta reference.
            for oracle_step, candidate_step in zip(
                oracle_steps[19:], candidate_steps[19:]
            ):
                oracle_step["entries"] = candidate_step["entries"]
                oracle_step["disabled"] = candidate_step["disabled"]
                oracle_step["has_available"] = candidate_step["has_available"]
    cases["delta.proxy-expiry-cleanup-crash"] = expiry_ok
    oracle_glype = copy.deepcopy(oracle.get("glype", {}))
    candidate_glype = copy.deepcopy(candidate.get("glype", {}))
    glype_normal_oracle = oracle_glype.pop("normal", None)
    glype_normal_candidate = candidate_glype.pop("normal", None)
    glype_fixed_oracle = oracle_glype.pop("sslagree_fixed", None)
    glype_fixed_candidate = candidate_glype.pop("sslagree_fixed", None)
    glype_caller_oracle = oracle_glype.pop("sslagree_caller", {})
    glype_caller_candidate = candidate_glype.pop("sslagree_caller", {})
    glype_post_oracle = oracle_glype.pop("post_fixed", None)
    glype_post_candidate = candidate_glype.pop("post_fixed", None)
    diffs, flags = _compare_scenarios(
        {"steps": oracle_steps}, {"steps": candidate_steps}, "proxy"
    )
    glype_flags = {}
    for name, expected, actual in (
        ("glype.normal", glype_normal_oracle, glype_normal_candidate),
        ("glype.sslagree-fixed", glype_fixed_oracle, glype_fixed_candidate),
        ("glype.post-fixed", glype_post_oracle, glype_post_candidate),
    ):
        section = compare(expected, actual, f"proxy.{name}")
        diffs.extend(section)
        glype_flags[name] = not section
    flags.update(glype_flags)
    # Reference quirk: the real glype handler passes allow_redirects twice to
    # its own sslagree follow-up, so CPython raises TypeError after the first
    # request. The candidate fixes that crash; both complete states are
    # asserted and the success state must equal the no-caller-kwargs flow.
    caller_delta_ok = (
        glype_caller_oracle.get("exception") == "TypeError"
        and len(glype_caller_oracle.get("calls", [])) == 1
        and "status" not in glype_caller_oracle
        and glype_caller_candidate.get("exception") is None
        and glype_caller_candidate.get("status") == glype_fixed_candidate.get("status")
        and glype_caller_candidate.get("text") == glype_fixed_candidate.get("text")
        and glype_caller_candidate.get("calls") == glype_fixed_candidate.get("calls")
    )
    cases["delta.proxy-glype-allow-redirects-crash"] = caller_delta_ok
    _mark_cases(cases, PROXY_CASES, flags)
    return diffs



def _transport_norm_port(text, port):
    return text.replace(str(port), "<PORT>")


class _LocalResponseServer:
    def __init__(self, responses):
        import socket
        import threading

        self.socket = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.socket.bind(("127.0.0.1", 0))
        self.socket.listen(len(responses))
        self.port = self.socket.getsockname()[1]
        self.requests = []
        self.thread = threading.Thread(
            target=self._serve, args=(responses,), daemon=True
        )
        self.thread.start()

    def _serve(self, responses):
        for response in responses:
            connection, _ = self.socket.accept()
            request = b""
            while b"\r\n\r\n" not in request:
                chunk = connection.recv(4096)
                if not chunk:
                    break
                request += chunk
            head, _, body = request.partition(b"\r\n\r\n")
            length = 0
            for line in head.split(b"\r\n")[1:]:
                if line.lower().startswith(b"content-length:"):
                    length = int(line.split(b":", 1)[1].strip())
            while len(body) < length:
                chunk = connection.recv(4096)
                if not chunk:
                    break
                body += chunk
            self.requests.append(head.decode("latin-1") + "\n\n" + body.decode("latin-1"))
            if b"connection:" not in response.split(b"\r\n\r\n", 1)[0].lower():
                response = response.replace(
                    b"\r\n\r\n", b"\r\nConnection: close\r\n\r\n", 1
                )
            connection.sendall(response)
            connection.close()

    def close(self):
        self.socket.close()


def _transport_fake_logger():
    class Logger:
        def __getattr__(self, name):
            return lambda *args, **kwargs: None

    return Logger()


def _transport_observe(response, port):
    return {
        "status": response.status_code,
        "url": _transport_norm_port(response.url, port),
        "real_url": _transport_norm_port(response._real_url, port),
        "content_length": getattr(response, "content_length", None),
        "text": getattr(response, "text", "") or "",
    }


def _transport_joined_headers(response):
    seen = {}
    order = []
    for name, value in response.headers.items():
        key = name.lower()
        if key not in seen:
            order.append(key)
            seen[key] = value
        else:
            seen[key] += ", " + value
    return {"order": order, "joined": seen}


def oracle_transport(upstream, request):
    sys.path.insert(0, str(upstream))
    from xeHentai import worker as worker_module

    sleeps = []
    worker_module.time.sleep = lambda seconds: sleeps.append(seconds)
    worker_module.random.random = lambda: 0.25
    logger = _transport_fake_logger()
    out = {}

    def make(retry=1, headers=None):
        return worker_module.HttpReq(
            headers or {},
            proxy=None,
            proxy_policy=None,
            retry=retry,
            timeout=3,
            logger=logger,
            tname="compat",
        )

    # Redirect chain.
    final_body = "last"
    final = _LocalResponseServer(
        [
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nlast",
        ]
    )
    start = _LocalResponseServer(
        [
            (
                "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:%d/final\r\n"
                "Content-Length: 0\r\n\r\n" % final.port
            ).encode("ascii"),
        ]
    )
    response = make().request(
        "GET", "http://127.0.0.1:%d/start" % start.port, lambda r, suc, fail: r, None, None
    )
    redirect = _transport_observe(response, start.port)
    redirect["url"] = redirect["url"].replace(str(final.port), "<PORT>")
    redirect["real_url"] = redirect["real_url"].replace(str(final.port), "<PORT>")
    redirect["hits"] = [1, 1]
    out["redirect"] = redirect

    # Retry loop with a closed port.
    import socket

    closed = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    closed.bind(("127.0.0.1", 0))
    closed_port = closed.getsockname()[1]
    closed.close()
    sleeps.clear()
    response = make(retry=2).request(
        "GET",
        "http://127.0.0.1:%d/x" % closed_port,
        lambda r, suc, fail: r,
        None,
        None,
    )
    retry = _transport_observe(response, closed_port)
    retry["sleeps"] = sleeps[:]
    retry["attempts"] = 2
    out["retry"] = retry

    # Cookie jar attaches to a second request; explicit Cookie suppresses it.
    jar = _LocalResponseServer(
        [
            b"HTTP/1.1 200 OK\r\nSet-Cookie: a=1; Path=/\r\nContent-Length: 2\r\n\r\nok",
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        ]
    )
    req = make()
    req.request(
        "GET", "http://127.0.0.1:%d/one" % jar.port, lambda r, suc, fail: r, None, None
    )
    req.request(
        "GET", "http://127.0.0.1:%d/two" % jar.port, lambda r, suc, fail: r, None, None
    )
    jar_request = jar.requests[1].lower()
    cookie_jar = "cookie: a=1" if "cookie: a=1" in jar_request else ""
    explicit_server = _LocalResponseServer(
        [
            b"HTTP/1.1 200 OK\r\nSet-Cookie: a=1; Path=/\r\nContent-Length: 2\r\n\r\nok",
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
        ]
    )
    req = make(headers={"Cookie": "explicit=1"})
    req.request(
        "GET",
        "http://127.0.0.1:%d/one" % explicit_server.port,
        lambda r, suc, fail: r,
        None,
        None,
    )
    req.request(
        "GET",
        "http://127.0.0.1:%d/two" % explicit_server.port,
        lambda r, suc, fail: r,
        None,
        None,
    )
    explicit_request = explicit_server.requests[1].lower()
    out["cookies"] = {
        "jar_cookie": cookie_jar,
        "explicit_cookie": (
            "cookie: explicit=1" if "cookie: explicit=1" in explicit_request else ""
        ),
        "jar_in_explicit": "a=1" in explicit_request,
    }

    # Duplicate and ISO-8859-1 header bytes through the requests view.
    header_server = _LocalResponseServer(
        [
            b"HTTP/1.1 200 OK\r\nX-Dup: one\r\nX-Dup: two\r\nX-Latin: caf\xe9\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok",
        ]
    )
    response = make().request(
        "GET",
        "http://127.0.0.1:%d/h" % header_server.port,
        lambda r, suc, fail: r,
        None,
        None,
    )
    headers = _transport_joined_headers(response)
    headers["latin"] = response.headers.get("x-latin")
    out["headers"] = headers

    # POST form data.
    post_server = _LocalResponseServer(
        [b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok"]
    )
    response = make().request(
        "POST",
        "http://127.0.0.1:%d/post" % post_server.port,
        lambda r, suc, fail: r,
        None,
        None,
        data={"a": "1", "b": "two words"},
    )
    post_request = post_server.requests[0]
    post_head, _, post_body = post_request.partition("\n\n")
    out["post"] = {
        "status": response.status_code,
        "method_line": post_head.splitlines()[0],
        "body": post_body,
        "content_type": next(
            (
                line.split(":", 1)[1].strip()
                for line in post_head.splitlines()
                if line.lower().startswith("content-type:")
            ),
            None,
        ),
    }
    for server in (final, start, jar, explicit_server, header_server, post_server):
        server.close()
    return out


def compare_transport(cases, oracle, candidate):
    diffs, flags = _compare_scenarios(oracle, candidate, "transport")
    _mark_cases(cases, TRANSPORT_CASES, flags)
    return diffs


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
    _write_session_directory_fixtures(core, defaults, work)
    return {"ok": True, "dir": str(directory), "guid": guid}


def _write_session_directory_fixtures(core, defaults, work):
    """Write the directory-restore fixtures with the real reference App.

    The candidate loads these Python-written h.json files independently; they
    are not produced by the candidate itself. Relative task directories stay
    valid when the scenario directory is moved between the oracle/candidate
    roots, while the explicit external root is generated inside each root.
    """

    def add(app, title, overrides=None, state=1):
        guid = app.add_task(GALLERY_URL, **(overrides or {}))[1]
        task = app._all_tasks[guid]
        task.meta.update({"gnname": title, "gjname": None, "total": 2})
        task.update_meta({})
        task.state = state
        return guid, task

    directory = _scenario(work, "history")
    app = _new_app(core, defaults, directory, ["a0000001"], {"dir": "elsewhere"})
    add(app, "History", state=3)
    app.set_cookie("a=1; ipb_member_id=7; ipb_pass_hash=h")
    assert app.save_session()[0] == 0

    directory = _scenario(work, "updated")
    app = _new_app(core, defaults, directory, ["b0000001", "b0000002"])
    add(app, "Before", state=3)
    app.update_config(dir="alt", proxy=[])
    add(app, "After", state=1)
    assert app.save_session()[0] == 0

    directory = _scenario(work, "multi")
    app = _new_app(core, defaults, directory, ["c0000001", "c0000002", "c0000003"])
    add(app, "First")
    add(app, "Second", {"dir": "elsewhere"})
    add(app, "Third", {"dir": "downloads/special"})
    assert app.save_session()[0] == 0

    directory = _scenario(work, "subdir")
    app = _new_app(core, defaults, directory, ["d0000001"])
    add(app, "Sub", {"dir": "downloads/special"})
    assert app.save_session()[0] == 0

    directory = _scenario(work, "external")
    external = work / "external"
    app = _new_app(core, defaults, directory, ["f0000001"], {"dir": str(external)})
    add(app, "External")
    assert app.save_session()[0] == 0

    _write_isolation_fixture(core, defaults, work, "isolation", True)
    _write_isolation_fixture(core, defaults, work, "isolation_legacy", False)


def _write_isolation_fixture(core, defaults, work, name, login):
    import queue

    directory = _scenario(work, name)
    app = _new_app(
        core,
        defaults,
        directory,
        ["e0000001", UNSAFE_SESSION_GUID, "e0000002"],
    )
    first = app.add_task(GALLERY_URL)[1]
    task = app._all_tasks[first]
    task.meta.update(
        {
            "gnname": "T-first",
            "gjname": None,
            "total": 2,
            "filelist": {"abcdef0123": ["1", "x"]},
        }
    )
    task.update_meta({})
    task.state = 1
    task.img_q = queue.Queue()
    task.img_q.put("https://img.example/a.jpg")

    unsafe = app.add_task(GALLERY_URL, dir=UNSAFE_SESSION_DIR)[1]
    assert unsafe == UNSAFE_SESSION_GUID
    unsafe_task = app._all_tasks[unsafe]
    unsafe_task.meta.update({"gnname": "U", "gjname": None, "total": 1})
    unsafe_task.update_meta({})

    second = app.add_task(GALLERY_URL, dir="elsewhere")[1]
    second_task = app._all_tasks[second]
    second_task.meta.update({"gnname": "T-second", "gjname": None, "total": 1})
    second_task.update_meta({})
    second_task.state = 10
    second_task.page_q = queue.Queue()
    second_task.page_q.put(GALLERY_URL + "?p=1")

    if login:
        app.set_cookie("a=1; b=x=y; ipb_member_id=7; ipb_pass_hash=h")
    else:
        app.set_cookie("a=1")
        (directory / ".ehentai.cookie").write_text("42,legacyhash\n", encoding="utf-8")
    gallery = directory / "downloads" / "T-first"
    gallery.mkdir(parents=True, exist_ok=True)
    (gallery / "1.jpg").write_bytes(b"one")
    (gallery / "2.jpg").write_bytes(b"")
    assert app.save_session()[0] == 0


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

    for name in ("history", "updated", "multi", "subdir"):
        result[name] = _fixture_load(core, defaults, work, name)
    result["external"] = _fixture_load(
        core, defaults, work, "external", config_dir=str(work / "external")
    )
    result["isolation"] = _fixture_load(core, defaults, work, "isolation", twice=True)
    result["isolation_legacy"] = _fixture_load(
        core, defaults, work, "isolation_legacy", twice=True
    )

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


def _fixture_load(core, defaults, work, name, config_dir=None, twice=False):
    """Load a reference-written h.json in a fresh App and observe everything."""
    import os

    directory = work / name
    os.chdir(directory)
    overrides = {"dir": config_dir} if config_dir else None
    app = _new_app(core, defaults, directory, [], overrides)
    result = {"loaded": _app_obs(app, directory)}
    if twice:
        app.load_session()
        result["twice"] = _app_obs(app, directory)
    result["save"] = list(app.save_session())
    restarted = _new_app(core, defaults, directory, [], overrides)
    result["restarted"] = _app_obs(restarted, directory)
    result["tree"] = _tree(directory)
    return result


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


def _compat_fake_time(now=100.0, step=0.0):
    class FakeTime:
        def __init__(self):
            self.now = now
            self.step = step
            self.ticks = 0
            self.limit = None
            self.on_tick = None

        def time(self):
            value = self.now
            self.now += self.step
            return value

        def sleep(self, seconds):
            self.now += seconds
            self.ticks += 1
            if self.on_tick is not None:
                self.on_tick(self)
            if self.limit is not None and self.ticks >= self.limit:
                raise _CompatStopMonitor()

    return FakeTime()


class _CompatStopMonitor(Exception):
    pass


def _compat_speed_trace(worker, speed):
    clock = _compat_fake_time(speed.get("start", 100.0), 0.0)
    original = worker.time
    worker.time = clock
    try:
        checker = worker.speed_checker(cnt=speed.get("cnt", 5))
        trace = []
        now = speed.get("start", 100.0)
        resets = set(speed.get("resets", []))
        for index, (size, delta) in enumerate(speed["timeline"]):
            now += delta
            clock.now = now
            checker.check(size)
            trace.append([float(round(checker.calc(False), 6)), float(round(checker.calc(True), 6)), len(checker.speed_buffer)])
            if index in resets:
                checker.reset()
                trace.append(["reset", float(round(checker.calc(False), 6)), len(checker.speed_buffer)])
        return trace
    finally:
        worker.time = original


def _compat_speed_ref(worker, sample_bytes):
    clock = _compat_fake_time(0.0, 1.1)
    original = worker.time
    worker.time = clock
    try:
        checker = worker.speed_checker()
        for _ in range(5):
            checker.check(sample_bytes)
        return checker
    finally:
        worker.time = original


def _compat_monitor_scenario(worker, const, scenario):
    events = []
    clock = _compat_fake_time(1000.0, 0.0)
    original_time = worker.time
    worker.time = clock
    stats = {"count": 0}
    finished_updates = {int(index): value for index, value in scenario.get("finished_updates", [])}
    zero_after = scenario.get("zero_speed_after_stats")
    state = {"value": const.TASK_STATE_DOWNLOAD}

    class Task:
        def __init__(self):
            self.guid = "compat"
            self.meta = {
                "total": scenario["total"],
                "finished": scenario.get("finished", 0),
                "title": "compat",
            }
            import queue as queue_module

            self.img_q = queue_module.Queue()
            for _ in range(scenario.get("pending", 0)):
                self.img_q.put("x")
            self._done = set()
            self.rename_calls = 0
            self.cleanup_calls = 0

        @property
        def state(self):
            return state["value"]

        @state.setter
        def state(self, value):
            state["value"] = value

        def get_fid_unfinished(self):
            total = self.meta["total"]
            return [index for index in range(1, total) if index not in self._done]

        def rename_fname(self):
            self.rename_calls += 1
            return []

        def cleanup(self, before_delete=False):
            self.cleanup_calls += 1

    task = Task()

    class Writer:
        def __init__(self):
            self.records = []

        def __getattr__(self, name):
            def write(message, *args, **kwargs):
                self.records.append((name, str(message)))

            return write

    logger = Writer()

    def inspect(level, message):
        if "R/" in message and "queue" in message:
            stats["count"] += 1
            events.append("stats")
            if stats["count"] in finished_updates:
                task.meta["finished"] = finished_updates[stats["count"]]
            if zero_after is not None and stats["count"] >= zero_after:
                for reference in workers:
                    reference.stream_speed = None
            if scenario.get("finish_when_complete") and task.meta["finished"] == task.meta["total"]:
                for reference in workers:
                    monitor.wrk_keepalive(reference, _exit=True)
        elif "is slow" in message:
            events.append("slow")
        elif "is stuck" in message:
            events.append("stuck")
        elif "quota" in message.lower():
            events.append("quota")
        elif "may became zombie" in message:
            events.append("zombie")
        elif "is dead, deref" in message:
            events.append("sweep")
        elif "download finished" in message:
            events.append("finished")

    class RecordingLogger:
        def verbose(self, message, *args, **kwargs):
            pass

        def debug(self, message, *args, **kwargs):
            pass

        def info(self, message, *args, **kwargs):
            inspect("info", message)

        def warning(self, message, *args, **kwargs):
            inspect("warning", message)

        warn = warning

        def error(self, message, *args, **kwargs):
            inspect("error", message)

        def critical(self, message, *args, **kwargs):
            pass

    class Worker:
        def __init__(self, name, threshold, speed_bytes):
            self.name = name
            self._alive = True
            self.zombie_threshold = threshold
            self.stream_speed = _compat_speed_ref(worker, speed_bytes) if speed_bytes else None

        def is_alive(self):
            return self._alive

    workers = [
        Worker(
            name,
            scenario.get("thresholds", {}).get(name, 30.0),
            scenario.get("speed_bytes", 0),
        )
        for name in scenario["names"]
    ]
    monitor = worker.Monitor(
        None,
        None,
        RecordingLogger(),
        task,
        ignored_errors=scenario.get("ignored", []),
    )
    monitor.set_vote_ns(scenario["names"])
    for reference in workers:
        monitor.wrk_keepalive(reference)
    for tname, code in scenario.get("votes", []):
        monitor.vote(tname, code)
        events.append(f"vote:{code}")
    clock.limit = scenario.get("max_ticks", 1000)
    clock.on_tick = lambda _clock: None
    try:
        monitor.run()
        stopped = False
    except _CompatStopMonitor:
        stopped = True
    finally:
        worker.time = original_time
    return {
        "events": events,
        "state": state["value"],
        "finished": task.meta["finished"],
        "rename_calls": task.rename_calls,
        "cleanup_calls": task.cleanup_calls,
        "download_speed": float(round(getattr(monitor, "download_speed", 0.0), 6)),
        "stopped": stopped,
        "stats": stats["count"],
    }


def oracle_monitor(upstream, request):
    import importlib

    sys.path.insert(0, str(upstream))
    worker = importlib.import_module("xeHentai.worker")
    const = importlib.import_module("xeHentai.const")
    result = {
        "speed": _compat_speed_trace(worker, request["speed"]),
        "monitor": {},
    }
    for name, scenario in request["monitor"].items():
        result["monitor"][name] = _compat_monitor_scenario(worker, const, scenario)
    return result


def monitor_scenarios():
    return {
        "speed": {
            "start": 100.0,
            "cnt": 5,
            "timeline": [
                [1024, 2.0],
                [1024, 0.5],
                [2048, 1.0],
                [1024, 2.0],
                [4096, 2.0],
                [0, 2.0],
                [1024, 2.0],
                [1024, 3.0],
                [1024, 1.0],
            ],
            "resets": [3, 7],
        },
        "monitor": {
            "finish": {
                "total": 2,
                "finished": 2,
                "names": ["down-1"],
                "speed_bytes": 0,
                "finish_when_complete": True,
                "max_ticks": 10,
            },
            "slow": {
                "total": 10,
                "finished": 0,
                "names": ["down-1", "down-2"],
                "thresholds": {"down-1": 1.0e9, "down-2": 1.0e9},
                "speed_bytes": 20000,
                "max_ticks": 400,
            },
            "stuck": {
                "total": 10,
                "finished": 0,
                "names": ["down-1"],
                "thresholds": {"down-1": 1.0e9},
                "speed_bytes": 0,
                "max_ticks": 400,
            },
            "quota": {
                "total": 10,
                "finished": 0,
                "names": ["down-1", "down-2"],
                "votes": [["down-1", 1006], ["down-2", 1006]],
                "max_ticks": 5,
            },
            "ignored": {
                "total": 10,
                "finished": 0,
                "names": ["down-1"],
                "ignored": [1006],
                "votes": [["down-1", 1006]],
                "max_ticks": 5,
            },
            "zombie": {
                "total": 10,
                "finished": 0,
                "names": ["down-1"],
                "thresholds": {"down-1": 0.5},
                "max_ticks": 100,
            },
        },
    }


MONITOR_CASES = [
    "m4.speed.checker",
    "m4.monitor.finish",
    "m4.monitor.slow-windows",
    "m4.monitor.stuck",
    "m4.monitor.quota",
    "m4.monitor.ignored-errors",
    "m4.monitor.zombie",
]
MONITOR_CASE_KEYS = {
    "m4.speed.checker": "speed",
    "m4.monitor.finish": "finish",
    "m4.monitor.slow-windows": "slow",
    "m4.monitor.stuck": "stuck",
    "m4.monitor.quota": "quota",
    "m4.monitor.ignored-errors": "ignored",
    "m4.monitor.zombie": "zombie",
}


def compare_monitor(cases, oracle, candidate):
    diffs = []
    for case_id in MONITOR_CASES:
        cases.setdefault(case_id, True)
    diffs.extend(compare(oracle["speed"], candidate["speed"], "speed"))
    if diffs:
        cases["m4.speed.checker"] = False
    for case_id, key in MONITOR_CASE_KEYS.items():
        if case_id == "m4.speed.checker":
            continue
        expected = {
            field: oracle["monitor"][key].get(field)
            for field in ("events", "state", "finished", "download_speed", "stopped", "stats")
        }
        actual = {
            field: candidate["monitor"][key].get(field)
            for field in ("events", "state", "finished", "download_speed", "stopped", "stats")
        }
        local = compare(expected, actual, key)
        if local:
            cases[case_id] = False
            diffs.extend(local)
    return diffs


class _CompatDownloadResponse:
    def __init__(self, status, url, headers, body, chunks):
        self.status_code = status
        self.url = url
        self._real_url = url
        self.headers = headers
        self.content = body
        self._chunks = chunks
        self.iter_content_cb = None

    @property
    def text(self):
        return self.content.decode("utf-8", "replace")

    def iter_content(self, chunk_size=16384):
        for chunk in self._chunks:
            yield chunk


def _compat_download_observation(work, task, calls):
    tree = _tree(work / "downloads")
    observation = _task_obs(task, work)
    observation["guid"] = "GUID"
    observation["reload"] = sorted(observation["reload"], key=lambda row: str(row[0]))
    observation["renamed"] = sorted(observation["renamed"], key=lambda row: str(row[0]))
    observation["duplicate"] = sorted(observation["duplicate"], key=lambda row: str(row[0]))
    observation["config"] = {
        key: ("<ROOT>" + value[len(str(work)):] if isinstance(value, str) and value.startswith(str(work)) else value)
        for key, value in observation["config"].items()
    }
    if observation.get("url"):
        observation["url"] = observation["url"].replace(str(work), "<ROOT>")
    requests = sorted(calls)
    return {"task": observation, "tree": tree, "requests": requests}


def oracle_download(upstream, request):
    import importlib
    import queue as queue_module
    import threading
    import time as time_module
    from urllib.parse import urlparse

    core, task_module, _const, defaults = _load_upstream(upstream)
    work = Path(request["root"])
    work.mkdir(parents=True, exist_ok=True)
    (work / "downloads").mkdir(parents=True, exist_ok=True)
    scenario = request["scenario"]
    app = _new_app(
        core,
        defaults,
        work,
        ["00000001"],
        overrides={
            "dir": str(work / "downloads"),
            "save_tasks": False,
            "scan_thread_cnt": scenario["scan_threads"],
            "download_thread_cnt": scenario["download_threads"],
            "download_timeout": 5,
            "low_speed_threshold": 0,
            "make_archive": False,
            "delete_task_files": False,
        },
    )
    calls = []

    def fake_request(_session, method, url, **kwargs):
        path = urlparse(url).path
        query = urlparse(url).query
        calls.append(f"{method} {path}" + (f"?{query}" if query else ""))
        if "?p=" in url:
            return _CompatDownloadResponse(200, url, {"content-type": "text/html; charset=utf-8"}, scenario["page"].encode(), [scenario["page"].encode()])
        if "/img/" in path:
            if scenario["image_hashes"][0] in path:
                body = bytes.fromhex(scenario["images"][0])
            else:
                body = bytes.fromhex(scenario["images"][1])
            return _CompatDownloadResponse(
                200,
                url,
                {"content-type": "image/jpeg", "content-length": str(len(body))},
                body,
                [body],
            )
        if "/s/" in path:
            fid = int(path.rsplit("-", 1)[1])
            return _CompatDownloadResponse(
                200,
                url,
                {"content-type": "text/html; charset=utf-8"},
                scenario["img_pages"][fid - 1].encode(),
                [scenario["img_pages"][fid - 1].encode()],
            )
        return _CompatDownloadResponse(
            200,
            url,
            {"content-type": "text/html; charset=utf-8"},
            scenario["metadata"].encode(),
            [scenario["metadata"].encode()],
        )

    import requests

    original_request = requests.Session.request
    requests.Session.request = fake_request
    task = task_module.Task(
        scenario["url"],
        _task_config(
            app,
            {
                "dir": str(work / "downloads"),
                "scan_thread_cnt": scenario["scan_threads"],
                "download_thread_cnt": scenario["download_threads"],
                "download_timeout": 5,
                "low_speed_threshold": 0,
                "make_archive": False,
                "delete_task_files": False,
            },
        ),
        app.logger,
    )
    app._all_tasks[task.guid] = task
    app.tasks = queue_module.Queue()
    app.tasks.put(task.guid)
    thread = threading.Thread(target=app._task_loop, daemon=True)
    thread.start()
    deadline = time_module.time() + 20
    try:
        while time_module.time() < deadline:
            if task.state in (_const.TASK_STATE_FINISHED, _const.TASK_STATE_FAILED):
                break
            time_module.sleep(0.02)
        app._exit = True
        thread.join(timeout=10)
    finally:
        requests.Session.request = original_request
    observation = _compat_download_observation(work, task, calls)
    observation["monitor"] = {
        "state": task.state,
        "finished": task.meta.get("finished"),
        "votes": sorted((str(key), int(value)) for key, value in getattr(app._monitor, "vote_result", {}).items()),
    }
    return observation


def download_scenarios():
    hash40_a = "0123456789abcdef0123456789abcdef01234567"
    hash40_b = "89abcdef0123456789abcdef0123456789abcdef"
    image_a = (b"image-one-" * 400)[:2400].hex()
    image_b = (b"image-two-" * 500)[:3200].hex()
    return {
        "url": "http://e-hentai.org/g/123/abcdef0123/",
        "scan_threads": 1,
        "download_threads": 1,
        "metadata": '="gj">jp-title</h1>="gn">en-title</h1>Length:</td><td class="gdt2">2 pages</td>Showing 1 - 20 of 2 images',
        "page": '<a href="http://e-hentai.org/s/aaaa000001/123456-1"><div title="Page 1"'
                '<a href="http://e-hentai.org/s/bbbb000002/123456-2"><div title="Page 2"',
        "img_pages": [
            f'<img src="http://img.test/img/{hash40_a}-2400-200-0-jpg/keystamp=x" style="x"></a></div><div>one.jpg :: 100x200 :: 2 KB</di>return nl(\'aaaa000001-1\')',
            f'<img src="http://img.test/img/{hash40_b}-3200-200-0-jpg/keystamp=x" style="x"></a></div><div>two.jpg :: 100x200 :: 2 KB</di>return nl(\'bbbb000002-2\')',
        ],
        "images": [image_a, image_b],
        "image_hashes": [hash40_a, hash40_b],
    }


DOWNLOAD_CASES = [
    "m4.pipeline.local",
    "m4.task-loop.autosave",
    "m4.workers.overlap-scan",
    "m4.workers.overlap-download",
    "m4.worker.multi-url-cookie",
    "m4.queue.src-while-active",
    "m4.retry.low-speed-rescan",
    "m4.retry.worker-three",
    "m4.pause.mid-body",
    "m4.pause.mid-page",
    "m4.pause.blocking-open",
    "m4.stale.same-run",
    "m4.stale.same-guid-owner",
    "m4.duplicates.concurrent",
    "m4.session.partial-resume",
    "m4.archive.after-finish",
    "m4.task-loop.last-guid",
    "m4.task-loop.prefetch",
    "m4.dns.pending",
    "m4.concurrent.limit",
    "m4.monitor.native-slow-stuck",
    "m4.monitor.native-votes",
]
DOWNLOAD_NATIVE_CASES = DOWNLOAD_CASES


def compare_download(cases, oracle, candidate):
    diffs = []
    diffs.extend(compare(oracle["task"], candidate["task"], "task"))
    diffs.extend(compare(oracle["tree"], candidate["tree"], "tree"))
    diffs.extend(compare(oracle["requests"], candidate["requests"], "requests"))
    diffs.extend(compare(oracle["monitor"], candidate["monitor"], "monitor"))
    if diffs:
        for case_id in DOWNLOAD_CASES:
            cases[case_id] = False
    else:
        for case_id in DOWNLOAD_CASES:
            cases[case_id] = True
    return diffs


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


def compare_site(cases, oracle, candidate):
    diffs, flags = _compare_scenarios(oracle, candidate, "site")
    _mark_cases(cases, SITE_CASES, flags)
    return diffs


def compare_task_api(cases, oracle, candidate):
    diffs, flags = _compare_scenarios(oracle, candidate, "task-api")
    _mark_cases(cases, TASK_API_CASES, flags)
    return diffs


def compare_session(cases, oracle, candidate):
    # Ordinary directory-restore scenarios are compared as a whole: there is
    # no state-level skip for the historical/updated/multi/subdir/external
    # cases. Only the intentional local-rejection scenarios are asymmetric.
    diffs, flags = _compare_scenarios(
        oracle, candidate, "session", skip=SESSION_SAFETY_SCENARIOS
    )
    for name in SESSION_SAFETY_SCENARIOS:
        section = _compare_isolation(name, oracle.get(name), candidate.get(name))
        flags[name] = not section
        diffs.extend(section)
    _mark_cases(cases, SESSION_CASES, flags)
    return diffs


def _expected_unsafe_task():
    """Full independent expectation for the foreign task in the fixture."""
    return {
        "url": GALLERY_URL,
        "gid": "123456",
        "sethash": "0123456789",
        "failcode": 0,
        "state": 1,
        "guid": UNSAFE_SESSION_GUID,
        "config": {
            "dir": UNSAFE_SESSION_DIR,
            "download_ori": False,
            "jpn_title": True,
            "rename_ori": False,
            "proxy_image": True,
            "proxy_image_only": False,
            "make_archive": False,
            "download_range": None,
            "scan_thread_cnt": 1,
            "download_thread_cnt": 5,
            "download_timeout": 10,
            "ignored_errors": [],
            "delete_task_files": False,
            "low_speed_threshold": 10,
        },
        "meta": {"gnname": "U", "gjname": None, "total": 1, "title": "U"},
        "reload": [],
        "duplicate": [],
        "renamed": [],
        "img_q": None,
        "page_q": None,
        "flist": [],
    }


def _without_guid(observation, guid):
    expected = copy.deepcopy(observation)
    expected["tasks"] = [entry for entry in expected["tasks"] if entry[0] != guid]
    expected["queue"] = [item for item in expected["queue"] if item != guid]
    return expected


def _compare_isolation(name, oracle, candidate):
    """Assert both complete states without deriving the candidate from itself.

    The oracle must keep the foreign task with its exact full wire state; the
    candidate must keep every other task, queue slot, cookie, header and login
    flag, and after restart must have exactly the legal subset. The candidate
    expectation is the oracle's legal state with the foreign guid removed, never
    the candidate's own output.
    """
    prefix = f"session.{name}"
    if not isinstance(oracle, dict) or not isinstance(candidate, dict):
        return [f"{prefix}: missing full observation"]
    diffs = []
    for section in ("loaded", "twice", "restarted"):
        oracle_obs = oracle.get(section)
        candidate_obs = candidate.get(section)
        if not isinstance(oracle_obs, dict) or not isinstance(candidate_obs, dict):
            diffs.append(f"{prefix}.{section}: missing section")
            continue
        tasks = oracle_obs.get("tasks", [])
        foreign = [entry for entry in tasks if entry[0] == UNSAFE_SESSION_GUID]
        if len(foreign) != 1:
            diffs.append(
                f"{prefix}.oracle.{section}: foreign task count {len(foreign)} != 1"
            )
            continue
        if len(tasks) != 3:
            diffs.append(f"{prefix}.oracle.{section}: task count {len(tasks)} != 3")
        diffs.extend(
            compare(
                _expected_unsafe_task(),
                foreign[0][1],
                f"{prefix}.oracle.{section}.unsafe",
            )
        )
        queue_occurrences = 2 if section == "twice" else 1
        actual_occurrences = oracle_obs.get("queue", []).count(UNSAFE_SESSION_GUID)
        if actual_occurrences != queue_occurrences:
            diffs.append(
                f"{prefix}.oracle.{section}: foreign queue occurrences "
                f"{actual_occurrences} != {queue_occurrences}"
            )
        expected = _without_guid(oracle_obs, UNSAFE_SESSION_GUID)
        diffs.extend(compare(expected, candidate_obs, f"{prefix}.candidate.{section}"))
        if any(
            entry[0] == UNSAFE_SESSION_GUID
            for entry in candidate_obs.get("tasks", [])
        ):
            diffs.append(f"{prefix}.candidate.{section}: accepted the foreign task")
        if UNSAFE_SESSION_GUID in candidate_obs.get("queue", []):
            diffs.append(f"{prefix}.candidate.{section}: queued the foreign task")
    diffs.extend(compare(oracle.get("save"), candidate.get("save"), f"{prefix}.save"))
    diffs.extend(compare(oracle.get("tree"), candidate.get("tree"), f"{prefix}.tree"))
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

        def isolation_detected(change):
            mutated = copy.deepcopy(candidate)
            change(mutated)
            cases = {}
            compare_session(cases, oracle, mutated)
            return not cases["task.session-dir-authorization"]

        checks.append(
            (
                "isolation-missing-cookie",
                isolation_detected(
                    lambda mutated: mutated["isolation"]["loaded"]["cookies"].pop()
                ),
            )
        )
        checks.append(
            (
                "isolation-header-order",
                isolation_detected(
                    lambda mutated: mutated["isolation"]["loaded"]["headers"].reverse()
                ),
            )
        )
        checks.append(
            (
                "isolation-missing-legal-task",
                isolation_detected(
                    lambda mutated: mutated["isolation"]["loaded"]["tasks"].pop(0)
                ),
            )
        )
        checks.append(
            (
                "isolation-legal-task-state",
                isolation_detected(
                    lambda mutated: mutated["isolation"]["loaded"]["tasks"][0][1].__setitem__(
                        "state", 99
                    )
                ),
            )
        )
        checks.append(
            (
                "isolation-legal-config",
                isolation_detected(
                    lambda mutated: mutated["isolation"]["loaded"]["tasks"][0][1]["config"].__setitem__(
                        "dir", "elsewhere"
                    )
                ),
            )
        )
        checks.append(
            (
                "isolation-queue-order",
                isolation_detected(
                    lambda mutated: mutated["isolation"]["loaded"]["queue"].reverse()
                ),
            )
        )

        def accept_unsafe(mutated):
            mutated["isolation"]["loaded"]["tasks"].insert(
                1, [UNSAFE_SESSION_GUID, _expected_unsafe_task()]
            )
            mutated["isolation"]["loaded"]["queue"].insert(1, UNSAFE_SESSION_GUID)

        checks.append(("isolation-accepts-unsafe", isolation_detected(accept_unsafe)))
        checks.append(
            (
                "isolation-drops-session",
                isolation_detected(
                    lambda mutated: mutated["isolation"]["loaded"].update(
                        {"tasks": [], "queue": [], "cookies": [], "has_login": False}
                    )
                ),
            )
        )
    elif suite == "transport":
        mutated = copy.deepcopy(candidate)
        mutated["redirect"]["real_url"] = "mutated"
        cases = {}
        compare_transport(cases, oracle, mutated)
        checks.append(("redirect-history", not cases["worker.redirects"]))
        mutated = copy.deepcopy(candidate)
        mutated["retry"]["sleeps"] = [0.618]
        cases = {}
        compare_transport(cases, oracle, mutated)
        checks.append(("retry-backoff", not cases["worker.retry-backoff"]))
        mutated = copy.deepcopy(candidate)
        mutated["cookies"]["jar_cookie"] = ""
        cases = {}
        compare_transport(cases, oracle, mutated)
        checks.append(("cookie-jar", not cases["worker.session-cookies"]))
        mutated = copy.deepcopy(candidate)
        mutated["headers"]["joined"]["x-dup"] = "one"
        cases = {}
        compare_transport(cases, oracle, mutated)
        checks.append(("duplicate-header", not cases["worker.header-bytes"]))
        mutated = copy.deepcopy(candidate)
        mutated["post"]["body"] = "a=1&b=two+words+mutated"
        cases = {}
        compare_transport(cases, oracle, mutated)
        checks.append(("post-body", not cases["worker.post-form"]))
    elif suite == "proxy":
        mutated = copy.deepcopy(candidate)
        mutated["steps"][3]["selected"] = "http://127.0.0.1:8082"
        cases = {}
        compare_proxy(cases, oracle, mutated)
        checks.append(("selection-order", not cases["proxy.pool-order"]))
        mutated = copy.deepcopy(candidate)
        mutated["steps"][14]["entries"][0][2] = 0
        cases = {}
        compare_proxy(cases, oracle, mutated)
        checks.append(("failure-count", not cases["proxy.fail-policy"]))
        mutated = copy.deepcopy(candidate)
        mutated["steps"][17]["disabled"] = []
        cases = {}
        compare_proxy(cases, oracle, mutated)
        checks.append(("ban-expiry", not cases["proxy.ban-expiry"]))
        mutated = copy.deepcopy(candidate)
        mutated["steps"][20]["exception"] = None
        cases = {}
        compare_proxy(cases, oracle, mutated)
        checks.append(("invalid-address", not cases["proxy.add-validation"]))
        mutated = copy.deepcopy(candidate)
        mutated["steps"][13]["has_available"] = not mutated["steps"][13]["has_available"]
        cases = {}
        compare_proxy(cases, oracle, mutated)
        checks.append(("inverted-availability", not cases["proxy.has-available"]))
        mutated = copy.deepcopy(candidate)
        mutated["glype"]["sslagree_fixed"]["text"] = "mutated"
        cases = {}
        compare_proxy(cases, oracle, mutated)
        checks.append(("glype-rewrite", not cases["proxy.glype-runtime"]))
        mutated = copy.deepcopy(candidate)
        mutated["glype"]["post_fixed"]["calls"][0]["method"] = "GET"
        cases = {}
        compare_proxy(cases, oracle, mutated)
        checks.append(("glype-post-method", not cases["proxy.glype-runtime"]))
        mutated = copy.deepcopy(candidate)
        mutated["glype"]["post_fixed"]["calls"][0]["body"] = ""
        cases = {}
        compare_proxy(cases, oracle, mutated)
        checks.append(("glype-post-body", not cases["proxy.glype-runtime"]))
    elif suite == "site":
        mutated = copy.deepcopy(candidate)
        mutated["login.ok"]["callbacks"] = []
        cases = {}
        compare_site(cases, oracle, mutated)
        checks.append(("callback", not cases["filters.login"]))
        mutated = copy.deepcopy(candidate)
        mutated["metadata.ok"]["callbacks"][0][1][0]["total"] = 13
        cases = {}
        compare_site(cases, oracle, mutated)
        checks.append(("metadata-field", not cases["filters.metadata"]))
        mutated = copy.deepcopy(candidate)
        mutated["imgurl.invalid-page"]["callbacks"][0][1][0][0] = 1006
        cases = {}
        compare_site(cases, oracle, mutated)
        checks.append(("status-code", not cases["filters.image-link"]))
        mutated = copy.deepcopy(candidate)
        mutated["pageurl.none"]["debug"] = "mutated"
        cases = {}
        compare_site(cases, oracle, mutated)
        checks.append(("debug-file", not cases["filters.page"]))
        mutated = copy.deepcopy(candidate)
        mutated["download.stream-short"]["stream_exception"] = None
        cases = {}
        compare_site(cases, oracle, mutated)
        checks.append(("stream-abort", not cases["filters.download-response"]))
        mutated = copy.deepcopy(candidate)
        mutated["mpv.missing-key"]["callbacks"] = []
        cases = {}
        compare_site(cases, oracle, mutated)
        checks.append(("partial-callbacks", not cases["filters.mpv"]))
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
    elif suite == "monitor":
        mutated = copy.deepcopy(candidate)
        mutated["speed"][0][1] = float(mutated["speed"][0][1]) + 1.0
        cases = {}
        compare_monitor(cases, oracle, mutated)
        checks.append(("speed-sample", not cases["m4.speed.checker"]))
        mutated = copy.deepcopy(candidate)
        mutated["monitor"]["finish"]["events"].append("mutated")
        cases = {}
        compare_monitor(cases, oracle, mutated)
        checks.append(("monitor-event", not cases["m4.monitor.finish"]))
        mutated = copy.deepcopy(candidate)
        mutated["monitor"]["slow"]["stats"] = 1
        cases = {}
        compare_monitor(cases, oracle, mutated)
        checks.append(("monitor-stat", not cases["m4.monitor.slow-windows"]))
    elif suite == "download":
        mutated = copy.deepcopy(candidate)
        mutated["task"]["meta"]["finished"] = 1
        cases = {}
        compare_download(cases, oracle, mutated)
        checks.append(("task-field", not cases["m4.pipeline.local"]))
        mutated = copy.deepcopy(candidate)
        mutated["tree"][0]["path"] = "mutated"
        cases = {}
        compare_download(cases, oracle, mutated)
        checks.append(("tree-path", not cases["m4.pipeline.local"]))
        mutated = copy.deepcopy(candidate)
        mutated["requests"].append("GET /mutated")
        cases = {}
        compare_download(cases, oracle, mutated)
        checks.append(("request-set", not cases["m4.pipeline.local"]))
    elif suite == "rpc":
        # Negative controls: the comparator must reject changed business
        # results, changed status, and changed error codes, not just accept
        # any transcript pair.
        mutated = copy.deepcopy(candidate)
        mutated["open"]["cases"][0]["body"] = mutated["open"]["cases"][0]["body"].replace(
            '"daemon":false', '"daemon":true')
        cases = {}
        compare_rpc(cases, oracle, mutated)
        checks.append(("result-field", not cases.get("rpc.getConfig.ok", True)))
        mutated = copy.deepcopy(candidate)
        mutated["open"]["cases"][0]["status"] = "HTTP/1.0 403 Forbidden"
        cases = {}
        compare_rpc(cases, oracle, mutated)
        checks.append(("status", not cases.get("rpc.getConfig.ok", True)))
        mutated = copy.deepcopy(candidate)
        for entry in mutated["open"]["cases"]:
            if isinstance(entry.get("body"), str) and '"code":1104' in entry["body"]:
                entry["body"] = entry["body"].replace("1104", "9999")
        cases = {}
        compare_rpc(cases, oracle, mutated)
        checks.append(("error-code", not cases.get("rpc.listTasks.invalid", True)))
        if isinstance(oracle, dict) and "isolated" in oracle:
            iso_case = "rpc.updateConfig.proxy-nodir-isolated"

            def isolated_mutant(mutator):
                import copy as _copy
                mutated_oracle = _copy.deepcopy(oracle["isolated"])
                mutated_candidate = _copy.deepcopy(candidate["isolated"])
                mutator(mutated_oracle, mutated_candidate)
                probe = {}
                compare_rpc_isolated(
                    probe, mutated_oracle, mutated_candidate)
                return probe

            def new_measured(mutated):
                for entry in mutated["cases"]:
                    if entry["id"] == iso_case:
                        return entry
                raise AssertionError("isolated case missing")

            checks.append(("isolated-body-proxy", not isolated_mutant(
                lambda old, new: new_measured(new).update(
                    {"body": new_measured(new)["body"].replace(
                        ISOLATED_PROXY, "socks5://127.0.0.1:1099")})
            ).get(iso_case, True)))
            checks.append(("isolated-status", not isolated_mutant(
                lambda old, new: new_measured(new).update(
                    {"status": "HTTP/1.0 500 Internal Server Error"})
            ).get(iso_case, True)))
            checks.append(("isolated-pool-members", not isolated_mutant(
                lambda old, new: new_measured(new)["after"][
                    "proxy_members"].append("socks5://127.0.0.1:1099")
            ).get(iso_case, True)))
            checks.append(("isolated-pool-count", not isolated_mutant(
                lambda old, new: new_measured(new)["after"].update(
                    {"proxy_count": 99})
            ).get(iso_case, True)))
            checks.append(("isolated-cfg-other", not isolated_mutant(
                lambda old, new: new_measured(new)["after"]["cfg"].update(
                    {"download_thread_cnt": 1})
            ).get(iso_case, True)))
            checks.append(("isolated-oracle-cfg", not isolated_mutant(
                lambda old, new: new_measured(old)["after"]["cfg"].update(
                    {"proxy": []})
            ).get(iso_case, True)))
            checks.append(("isolated-oracle-outcome", not isolated_mutant(
                lambda old, new: new_measured(old).update({"dropped": False})
            ).get(iso_case, True)))
    elif suite == "gallery":
        mutated = copy.deepcopy(candidate)
        mutated["reads"][0]["bytes"] = "mutated"
        cases = {}
        compare_gallery(cases, oracle, mutated)
        checks.append(("read-bytes", not cases.get("gallery.read.task-wait.1", True)))
        mutated = copy.deepcopy(candidate)
        mutated["archives"][0]["names"] = []
        cases = {}
        compare_gallery(cases, oracle, mutated)
        checks.append(("archive-names", not cases.get("gallery.archive.task-wait", True)))
        # Outcome mutation: an alias answer with the wrong bytes must be caught.
        mutated = copy.deepcopy(candidate)
        for entry in mutated["reads"]:
            if entry.get("case") == "gallery.alias.relative":
                entry["bytes"] = "mutated"
        cases = {}
        compare_gallery(cases, oracle, mutated)
        checks.append(
            ("alias-bytes", not cases.get("gallery.alias.relative", True)))
        # Outcome mutation: the pinned outside-referent delta must reject a
        # candidate that claims to have served the ZIP entry.
        mutated = copy.deepcopy(candidate)
        for entry in mutated["reads"]:
            if entry.get("case") == "delta.fd-outside-referent-denial":
                entry["status"] = 200
                entry["bytes"] = "ZIP-OUTSIDE-ENTRY-BYTES"
        cases = {}
        compare_gallery(cases, oracle, mutated)
        checks.append(("delta-outcome", not cases.get(
            "delta.fd-outside-referent-denial", True)))
        # Poststate mutations: task state and one disk entry must be caught.
        mutated = copy.deepcopy(candidate)
        mutated["poststate"]["after"]["tasks"]["task-wait"]["state"] = 99
        cases = {}
        compare_gallery(cases, oracle, mutated)
        checks.append(
            ("poststate-task", not cases.get("gallery.poststate.after", True)))
        mutated = copy.deepcopy(candidate)
        mutated["poststate"]["before"]["disk"].append("downloads/extra.jpg")
        cases = {}
        compare_gallery(cases, oracle, mutated)
        checks.append(
            ("poststate-disk", not cases.get("gallery.poststate.before", True)))
    elif suite == "webui":
        mutated = copy.deepcopy(candidate)
        mutated["index_markers"]["has_app"] = False
        cases = {}
        compare_webui(cases, oracle, mutated)
        checks.append(("index-marker", not cases.get("webui.pack.index", True)))
    elif suite == "concurrency":
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


# Declared per-method IDs aggregate the granular wire cases (each one is a
# real request/response comparison or a native execution). They are only
# true when every granular case they summarize passed in the same round.
RPC_METHOD_AGGREGATES = {
    "rpc.updateConfig": [
        "rpc.updateConfig.filter", "rpc.updateConfig.noop",
        "rpc.updateConfig.proxy", "rpc.updateConfig.proxy-dir",
        "rpc.updateConfig.unknown-key",
    ],
    "rpc.addTask": [
        "rpc.addTask.ok", "rpc.addTask.missing", "rpc.addTask.extra",
        "rpc.addTask.bad-url", "rpc.addTask.exh-no-login",
        "rpc.addTask.exh-logged-in",
    ],
    "rpc.delTask": ["rpc.delTask.running", "rpc.delTask.unknown", "rpc.task.lifecycle"],
    "rpc.pauseTask": ["rpc.pauseTask.missing", "rpc.pauseTask.unknown", "rpc.task.lifecycle"],
    "rpc.resumeTask": ["rpc.task.lifecycle"],
    "rpc.saveSession": ["rpc.saveSession.ok", "rpc.session.roundtrip"],
    "rpc.loadSession": ["rpc.loadSession.ok", "rpc.session.roundtrip"],
    "rpc.loginExhentai": ["rpc.loginExhentai.logged-in"],
    "rpc.setCookie": ["rpc.setCookie.login", "rpc.session.roundtrip"],
    "rpc.getInfo": ["rpc.getInfo.ok", "rpc.getInfo.extra-kwarg"],
    "rpc.getConfig": [
        "rpc.getConfig.ok", "rpc.getConfig.id-zero", "rpc.getConfig.id-false",
        "rpc.getConfig.extra-arg", "rpc.projection.config", "rpc.auth.exact",
    ],
    "rpc.listTasks": [
        "rpc.listTasks.default", "rpc.listTasks.positional", "rpc.listTasks.keyword",
        "rpc.listTasks.negation", "rpc.listTasks.invalid", "rpc.listTasks.nonstring",
        "rpc.listTasks.null", "rpc.listTasks.duplicate", "rpc.listTasks.extra-kwarg",
        "rpc.listTasks.level", "rpc.projection.task", "rpc.listTasks.config-order",
    ],
    "rpc.getImage": [
        "rpc.getImage.signed", "rpc.getImage.unknown", "rpc.getImage.no-total",
        "rpc.getImage.bad-range", "rpc.getImage.extra-kwarg", "rpc.getImage.errors",
        "rpc.getImage.range", "rpc.auth.getImage-signed",
    ],
}


def apply_rpc_aggregates(cases):
    for aggregate, members in RPC_METHOD_AGGREGATES.items():
        cases[aggregate] = all(cases.get(member) is True for member in members)


WEBUI_BROWSER_GROUPS = {
    "webui.static": [
        "connect.dialog", "auth.wrong.disconnected", "auth.wrong.403",
        "connect.connected", "config.basic.dir", "config.hides.rpc",
        "locale.zh-CN.heading", "locale.zh-TW.heading",
    ],
    "webui.gallery": [
        "gallery.opened", "gallery.double.images", "gallery.double.checkbox",
        "gallery.single.images", "gallery.nav.page2", "gallery.last.page",
        "zip.filename", "zip.entries", "zip.contents", "zip.http",
    ],
}


def apply_browser_evidence(cases):
    """Mark the declared WebUI IDs from the real Playwright evidence report."""
    path = os.environ.get("XEH_BROWSER_REPORT")
    if not path:
        return
    report = json.loads(Path(path).read_text(encoding="utf-8"))
    checks = {entry["check"]: entry["ok"] for entry in report.get("checks", [])}
    for case_id, required in WEBUI_BROWSER_GROUPS.items():
        cases[case_id] = all(checks.get(name) is True for name in required)
    trace = report.get("trace", [])
    urls = [entry.get("url", "") for entry in trace]
    cases["webui.browser"] = (
        report.get("ok") is True
        and all(checks.get(name) is True for name in checks)
        and any("/jsonrpc" in url for url in urls)
        and any("/img/" in url for url in urls)
        and any("/zip/" in url for url in urls)
        and any("/ui/" in url for url in urls)
    )



# --- M5: RPC wire, gallery and frozen WebUI ---------------------------------

RPC_SECRET = "s3cret"
RPC_TASKS = [
    {
        "guid": "task-wait",
        "url": "http://e-hentai.org/g/123/abcdef0123/",
        "state": 10,
        "meta": {"total": 3, "finished": 0, "title": "probe-gallery"},
        "files": {"1.jpg": "JPEG-DATA-1", "2.jpg": "JPEG-DATA-2"},
        "renamed": {"5": "2.jpg"},
    },
    {
        "guid": "task-bare",
        "url": "http://e-hentai.org/g/124/abcdef0123/",
        "state": 1,
        "meta": {},
        "files": {},
        "renamed": {},
    },
    {
        "guid": "task-done",
        "url": "http://e-hentai.org/g/456/abcdef0123/",
        "state": 20,
        "meta": {"total": 1, "finished": 1, "title": "probe-gallery"},
        "files": {"1.jpg": "JPEG-DATA-9"},
        "renamed": {},
    },
]


def _rpc_sign(uri):
    import hashlib
    return hashlib.md5(("-xehentai-" + uri).encode("utf-8")).hexdigest()[:8]


def _rpc_tamper(sign):
    # Flip the first hex digit deterministically; some real signatures start
    # with "0", so prefixing "0" would leave them valid.
    return ("1" if sign[0] != "1" else "2") + sign[1:]


def rpc_wire_cases():
    get_config = '{"method":"xeH.getConfig","id":%s%s}'
    img_valid = "/img/%s/task-wait/1/1.jpg" % _rpc_sign("task-wait/1")
    img_tampered = "/img/%s/task-wait/1/1.jpg" % _rpc_tamper(_rpc_sign("task-wait/1"))
    img_unknown = "/img/%s/missing/1/1.jpg" % _rpc_sign("missing/1")
    img_badfid = "/img/%s/task-wait/xyz/1.jpg" % _rpc_sign("task-wait/xyz")
    zip_valid = "/zip/%s/task-done/gallery.zip" % _rpc_sign("task-done")
    zip_tampered = "/zip/%s/task-done/gallery.zip" % _rpc_tamper(_rpc_sign("task-done"))
    return [
        {"id": "rpc.getConfig.ok", "method": "POST", "target": "/jsonrpc",
         "body": get_config % (1, "")},
        {"id": "rpc.getConfig.id-zero", "method": "POST", "target": "/jsonrpc",
         "body": get_config % (0, "")},
        {"id": "rpc.getConfig.id-false", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.getConfig","id":false}'},
        {"id": "rpc.getConfig.extra-arg", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.getConfig","id":1,"params":[[1],{}]}'},
        {"id": "rpc.getInfo.ok", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.getInfo","id":41,"params":[[],{}]}'},
        {"id": "rpc.getInfo.extra-kwarg", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.getInfo","id":1,"params":[[],{"x":1}]}'},
        {"id": "rpc.listTasks.default", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.listTasks","id":1}'},
        {"id": "rpc.listTasks.positional", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.listTasks","id":1,"params":[["download"],{}]}'},
        {"id": "rpc.listTasks.keyword", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.listTasks","id":1,"params":[[],{"level":"download"}]}'},
        {"id": "rpc.listTasks.negation", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.listTasks","id":1,"params":[["!download"],{}]}'},
        {"id": "rpc.listTasks.invalid", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.listTasks","id":1,"params":[["bogus"],{}]}'},
        {"id": "rpc.listTasks.nonstring", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.listTasks","id":1,"params":[[5],{}]}'},
        {"id": "rpc.listTasks.null", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.listTasks","id":1,"params":[[null],{}]}'},
        {"id": "rpc.listTasks.duplicate", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.listTasks","id":1,"params":[["download"],{"level":"download"}]}'},
        {"id": "rpc.listTasks.extra-kwarg", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.listTasks","id":1,"params":[[],{"level":"download","other":1}]}'},
        {"id": "rpc.addTask.ok", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.addTask","id":2,"params":[["http://e-hentai.org/g/789/abcdef0123/"],{"dir":"downloads"}]}'},
        {"id": "rpc.addTask.missing", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.addTask","id":2,"params":[[],{}]}'},
        {"id": "rpc.addTask.extra", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.addTask","id":2,"params":[["a","b"],{}]}'},
        {"id": "rpc.addTask.bad-url", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.addTask","id":2,"params":[["not a url"],{}]}'},
        {"id": "rpc.addTask.exh-no-login", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.addTask","id":2,"params":[["https://exhentai.org/g/1/abcdef0123/"],{}]}'},
        {"id": "rpc.pauseTask.unknown", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.pauseTask","id":3,"params":[["nope"],{}]}'},
        {"id": "rpc.pauseTask.missing", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.pauseTask","id":3,"params":[[],{}]}'},
        {"id": "rpc.delTask.running", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.delTask","id":3,"params":[["task-wait"],{}]}'},
        {"id": "rpc.delTask.unknown", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.delTask","id":3,"params":[["nope"],{}]}'},
        {"id": "rpc.updateConfig.noop", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.updateConfig","id":4,"params":[[],{"dir":"changed"}]}'},
        {"id": "rpc.updateConfig.proxy-dir", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.updateConfig","id":4,"params":[[],{"proxy":["socks5://127.0.0.1:9"],"dir":"proxied"}]}'},
        {"id": "rpc.updateConfig.proxy-nodir", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.updateConfig","id":4,"params":[[],{"proxy":["socks5://127.0.0.1:9"]}]}'},
        {"id": "rpc.updateConfig.unknown-key", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.updateConfig","id":4,"params":[[],{"proxy":["socks5://127.0.0.1:9"],"bogus_key":1}]}'},
        {"id": "rpc.saveSession.ok", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.saveSession","id":5}'},
        {"id": "rpc.loadSession.ok", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.loadSession","id":5}'},
        {"id": "rpc.setCookie.login", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.setCookie","id":6,"params":[["ipb_member_id=42; ipb_pass_hash=39d0c523"],{}]}'},
        {"id": "rpc.addTask.exh-logged-in", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.addTask","id":2,"params":[["https://exhentai.org/g/1/abcdef0123/"],{}]}'},
        {"id": "rpc.loginExhentai.logged-in", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.loginExhentai","id":7,"params":[["u","p"],{}]}'},
        {"id": "rpc.method.unknown", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.nope","id":1}'},
        {"id": "rpc.method.private", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH._private","id":1}'},
        {"id": "rpc.method.other-prefix", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"other.getInfo","id":1}'},
        {"id": "rpc.wire.missing-id", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.getInfo"}'},
        {"id": "rpc.wire.missing-method", "method": "POST", "target": "/jsonrpc",
         "body": '{"id":1}'},
        {"id": "rpc.wire.null-method", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":null,"id":1}'},
        {"id": "rpc.wire.nonstring-method", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":5,"id":1}'},
        {"id": "rpc.wire.invalid-json", "method": "POST", "target": "/jsonrpc",
         "body": "{oops"},
        {"id": "rpc.wire.params-nonpair", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.getInfo","id":1,"params":{}}'},
        {"id": "rpc.wire.params-single", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.getInfo","id":1,"params":[[]]}'},
        {"id": "rpc.getImage.signed", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.getImage","id":8,"params":[["task-wait"],{}]}'},
        {"id": "rpc.getImage.unknown", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.getImage","id":8,"params":[["nope"],{}]}'},
        {"id": "rpc.http.img-valid", "method": "GET", "target": img_valid,
         "body": ""},
        {"id": "rpc.http.img-tampered", "method": "GET", "target": img_tampered,
         "body": ""},
        {"id": "rpc.http.img-unknown-guid", "method": "GET", "target": img_unknown,
         "body": ""},
        {"id": "rpc.http.img-badfid", "method": "GET", "target": img_badfid,
         "body": ""},
        {"id": "rpc.http.zip-valid", "method": "GET", "target": zip_valid,
         "body": ""},
        {"id": "rpc.http.zip-tampered", "method": "GET", "target": zip_tampered,
         "body": ""},
        {"id": "rpc.getImage.no-total", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.getImage","id":8,"params":[["task-bare"],{}]}'},
        {"id": "rpc.getImage.bad-range", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.getImage","id":8,"params":[["task-wait"],{"request_range":"abc"}]}'},
        {"id": "rpc.getImage.extra-kwarg", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.getImage","id":8,"params":[["task-wait"],{"request_range":"1","other":1}]}'},
        {"id": "rpc.auth.missing", "method": "POST", "target": "/jsonrpc", "secret": RPC_SECRET,
         "body": '{"method":"xeH.getConfig","id":1,"params":[[],{}]}'},
        {"id": "rpc.auth.wrong", "method": "POST", "target": "/jsonrpc", "secret": RPC_SECRET,
         "body": '{"method":"xeH.getConfig","id":1,"params":[["token:nope"],{}]}'},
        {"id": "rpc.auth.substring", "method": "POST", "target": "/jsonrpc", "secret": RPC_SECRET,
         "body": '{"method":"xeH.getConfig","id":1,"params":[["xx token:s3cret yy"],{}]}'},
        {"id": "rpc.auth.exact", "method": "POST", "target": "/jsonrpc", "secret": RPC_SECRET,
         "body": '{"method":"xeH.getConfig","id":2,"params":[["token:s3cret"],{}]}'},
        {"id": "rpc.auth.getImage-signed", "method": "POST", "target": "/jsonrpc", "secret": RPC_SECRET,
         "body": '{"method":"xeH.getImage","id":9,"params":[["token:s3cret","task-wait"],{}]}'},
        {"id": "rpc.auth.stray-pop", "method": "POST", "target": "/jsonrpc",
         "body": '{"method":"xeH.getConfig","id":3,"params":[["token:x"],{}]}'},
        {"id": "rpc.http.options", "method": "OPTIONS", "target": "*",
         "body": ""},
        {"id": "rpc.http.unknown-get", "method": "GET", "target": "/nope",
         "body": ""},
        {"id": "rpc.http.img-short", "method": "GET", "target": "/img/a/b",
         "body": ""},
        {"id": "rpc.http.img-badquery", "method": "GET", "target": "/img/aaaaaaaa/task-wait/1?w",
         "body": ""},
        {"id": "rpc.http.zip-short", "method": "GET", "target": "/zip/a/b",
         "body": ""},
    ]


def _rpc_free_port():
    import socket
    sock = socket.socket()
    sock.bind(("127.0.0.1", 0))
    port = sock.getsockname()[1]
    sock.close()
    return port


def _rpc_build_app(core, task_module, consts, defaults, work, tasks):
    import types
    values = {k: v for k, v in vars(defaults).items() if not k.startswith("_")}
    values.update({
        "dir": str(work / "downloads"),
        "save_tasks": False,
        "log_path": "eh.log",
        "log_verbose": 2,
    })
    namespace = types.SimpleNamespace(**values)
    core.default_config = namespace
    core.config = namespace
    core.FILEPATH = str(work)
    app = core.xeHentai()
    for spec in tasks:
        cfg = {k: v for k, v in app.cfg.items() if k in (
            "dir", "download_ori", "download_thread_cnt", "scan_thread_cnt",
            "proxy_image", "proxy_image_only", "ignored_errors",
            "low_speed_threshold", "rename_ori", "make_archive",
            "delete_task_files", "jpn_title", "download_range",
            "download_timeout")}
        task = task_module.Task(spec["url"], cfg, app.logger)
        task.guid = spec["guid"]
        task.state = spec.get("state", consts.TASK_STATE_WAITING)
        task.meta.update(spec.get("meta", {}))
        task.meta["title"] = spec.get("meta", {}).get("title", "probe-gallery")
        directory = task.get_fpath()
        import os
        os.makedirs(directory, exist_ok=True)
        for name, content in spec.get("files", {}).items():
            # Fixture payloads are latin-1 byte strings (real PNG bytes are
            # carried through JSON that way); write bytes, never UTF-8 text.
            with open(os.path.join(directory, name), "wb") as handle:
                handle.write(content.encode("latin-1"))
        for fid, name in spec.get("renamed", {}).items():
            task.renamed_map[int(fid)] = name
        app._all_tasks[spec["guid"]] = task
        if spec.get("zip_build"):
            built = task.make_archive(False)
            assert built and built.endswith(".zip")
            for name in spec.get("zip_remove", []):
                full = os.path.join(directory, name)
                if os.path.exists(full):
                    os.remove(full)
    return app


def _rpc_snapshot_app(app):
    return {
        "cfg_dir": app.cfg["dir"],
        "proxy_count": len(app.proxy.proxies) if app.proxy else 0,
        "states": {guid: task.state for guid, task in app._all_tasks.items()},
        "queue_len": app.tasks.qsize(),
        "has_login": app.has_login,
        "cookies": dict(app.cookies),
    }


# Volatile operational files excluded from directory snapshots on both
# sides (routine logger/session/cache output, never directory creation).
# The top-level harness isolation dirs (home/cache/tmp from clean_env) are
# likewise excluded: oracle runs execute with cwd inside the isolated root
# while the candidate probe does not, so they are harness artifacts.
DIR_SNAPSHOT_EXCLUDED = {"h.json", "eh.log", "webui.gz", "webui-cache.json"}
DIR_SNAPSHOT_TOP_EXCLUDED = {"home", "cache", "tmp"}


def _rpc_public_cfg(app):
    return {
        key: value
        for key, value in app.cfg.items()
        if not key.startswith("rpc_") and key not in ("urls",)
    }


def _dir_paths(root):
    import os

    out = []
    stack = [str(root)]
    while stack:
        directory = stack.pop()
        try:
            names = sorted(os.listdir(directory))
        except OSError:
            continue
        for name in names:
            full = os.path.join(directory, name)
            rel = os.path.relpath(full, str(root)).replace(os.sep, "/")
            if "/" not in rel and rel in DIR_SNAPSHOT_TOP_EXCLUDED:
                continue
            if os.path.isdir(full) and not os.path.islink(full):
                out.append(rel + "/")
                stack.append(full)
            elif os.path.islink(full):
                out.append("%s -> %s" % (rel, os.readlink(full)))
            elif name in DIR_SNAPSHOT_EXCLUDED:
                continue
            else:
                out.append(rel)
    return sorted(out)


def _rpc_snapshot_full(app, work):
    snapshot = _rpc_snapshot_app(app)
    # Cookies normalize to sorted pairs on both sides (the oracle view is
    # a dict, the candidate a pair list); login state stays exact.
    snapshot["cookies"] = sorted(
        [list(item) for item in dict(app.cookies).items()])
    snapshot["cfg"] = _rpc_public_cfg(app)
    snapshot["proxy_members"] = list(app.proxy.proxies.keys()) if app.proxy else []
    snapshot["dirs"] = _dir_paths(work)
    return snapshot


def oracle_rpc(upstream, request):
    import importlib
    import json as pyjson
    import socket
    import threading
    import time
    core, task_module, const_module, defaults = _load_upstream(upstream)
    rpc_module = importlib.import_module("xeHentai.rpc")
    rpc_module.STATIC_CACHE_FILE = str(
        Path(request["root"]) / "webui.gz")
    work = Path(request["root"])
    work.mkdir(parents=True, exist_ok=True)
    app = _rpc_build_app(
        core, task_module, const_module, defaults, work, request["tasks"])
    secret = request.get("secret")
    extended = rpc_module.xeHentaiRPCExtended(app, secret)
    server = rpc_module.ThreadedHTTPServer(
        ("127.0.0.1", 0),
        lambda *args: rpc_module.Handler(extended.xeH, secret, *args),
    )
    port = server.socket.getsockname()[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    transcripts = []
    try:
        for case in request["cases"]:
            if request.get("snapshot") == "full":
                before = _rpc_snapshot_full(app, work)
            else:
                before = _rpc_snapshot_app(app)
            entry = {"id": case["id"]}
            try:
                sock = socket.create_connection(("127.0.0.1", port), timeout=15)
            except OSError as exc:
                entry["connect_error"] = str(exc)
                transcripts.append(entry)
                continue
            try:
                if case["method"] == "OPTIONS":
                    raw_head = (
                        "OPTIONS * HTTP/1.0\r\nHost: x\r\n\r\n"
                    ).encode()
                    body = b""
                elif case["method"] == "GET":
                    raw_head = (
                        "GET %s HTTP/1.0\r\nHost: x\r\n\r\n"
                        % case["target"]
                    ).encode()
                    body = b""
                else:
                    wire = case["body"].encode()
                    raw_head = (
                        "%s %s HTTP/1.0\r\nHost: x\r\n"
                        "Content-Type: application/json-rpc\r\n"
                        "Content-Length: %d\r\n\r\n"
                        % (case["method"], case["target"], len(wire))
                    ).encode()
                    body = case.get("raw_body", wire)
                    if "raw_head" in case:
                        raw_head = case["raw_head"].encode()
                sock.sendall(raw_head + body)
                chunks = []
                sock.settimeout(15)
                while True:
                    try:
                        data = sock.recv(65536)
                    except socket.timeout:
                        break
                    if not data:
                        break
                    chunks.append(data)
            finally:
                try:
                    sock.close()
                except OSError:
                    pass
            raw = b"".join(chunks)
            if b"\r\n\r\n" not in raw:
                entry["dropped"] = True
                entry["bytes"] = len(raw)
            else:
                head, _, rest = raw.partition(b"\r\n\r\n")
                lines = head.decode("latin-1").split("\r\n")
                headers = []
                for line in lines[1:]:
                    if ":" in line:
                        name, value = line.split(":", 1)
                        headers.append([name.strip().lower(), value.strip()])
                declared = 0
                for name, value in headers:
                    if name == "content-length":
                        try:
                            declared = int(value)
                        except ValueError:
                            declared = 0
                entry["status"] = lines[0] if lines else ""
                entry["headers"] = headers
                entry["body"] = rest[:declared].decode("latin-1")
                entry["trailing_newline"] = rest[declared:declared + 1] == b"\n"
            entry["before"] = before
            if request.get("snapshot") == "full":
                full_snapshot = _rpc_snapshot_full(app, work)
                entry["after"] = full_snapshot
            else:
                entry["after"] = _rpc_snapshot_app(app)
            transcripts.append(entry)
    finally:
        server.shutdown()
        server.server_close()
    return {"secret": secret, "root": str(work), "cases": transcripts}


RPC_PARITY_NORMALIZE = {"version"}


def _rpc_normalize_body(value, guid_hint=None):
    if isinstance(value, dict):
        return {key: _rpc_normalize_body(item, guid_hint) for key, item in value.items()}
    if isinstance(value, list):
        return [_rpc_normalize_body(item, guid_hint) for item in value]
    if isinstance(value, str):
        if guid_hint and guid_hint in value:
            return value.replace(guid_hint, "<guid>")
        if len(value) == 8 and all(c in "0123456789abcdef-" for c in value):
            return "<guid>"
        return value
    return value


def compare_rpc(cases, oracle, candidate):
    diffs = []
    for group in ("open", "auth"):
        diffs.extend(
            _compare_rpc_group(cases, oracle[group], candidate[group]))
    oracle_map = {}
    candidate_map = {}
    for group in ("open", "auth"):
        for entry in oracle[group]["cases"]:
            oracle_map[entry["id"]] = entry
        for entry in candidate[group]["cases"]:
            candidate_map[entry["id"]] = entry
    # Pinned per-case deltas: both sides must equal their recorded outcome.
    for case_id, expected in RPC_DELTA_CASES.items():
        old = oracle_map.get(case_id, {})
        new = candidate_map.get(case_id, {})
        if case_id == "rpc.wire.invalid-json":
            # Reference bug (`rpc.py:337` assigns the envelope to `rte`,
            # leaving `rt` empty): 400 with an empty body. The candidate
            # sends the real parse-error envelope instead.
            old_ok = (not old.get("dropped") and old.get("status", "")[:12] == "HTTP/1.0 400"
                      and old.get("body", "") == "")
            try:
                new_body = json.loads(new.get("body", ""))
                new_ok = (not new.get("dropped") and new.get("status", "")[:12] == "HTTP/1.0 400"
                          and new_body.get("error", {}).get("code") == -32700)
            except ValueError:
                new_ok = False
            if not old_ok or not new_ok:
                diffs.append(f"{case_id}: invalid-json delta mismatch oracle={old} candidate={new}")
                cases[case_id] = False
            else:
                cases[case_id] = True
            continue
        old_key = (bool(old.get("dropped")), (old.get("status", "")[:12] if not old.get("dropped") else ""))
        new_key = (bool(new.get("dropped")), (new.get("status", "")[:12] if not new.get("dropped") else ""))
        if old_key != expected["oracle"] or new_key != expected["candidate"]:
            diffs.append(
                f"{case_id}: delta mismatch oracle={old_key} expected={expected['oracle']} "
                f"candidate={new_key} expected={expected['candidate']}")
            cases[case_id] = False
        else:
            cases[case_id] = True
    return diffs


def _compare_rpc_group(cases, oracle, candidate):
    diffs = []
    oracle_root = oracle.get("root", "")
    candidate_root = candidate.get("root", "")

    def scrub(value):
        if isinstance(value, str):
            for root in (oracle_root, candidate_root):
                if root and root in value:
                    value = value.replace(root, "<root>")
            return value
        if isinstance(value, list):
            return [scrub(item) for item in value]
        if isinstance(value, dict):
            return {key: scrub(item) for key, item in value.items()}
        return value

    oracle_map = {entry["id"]: entry for entry in oracle["cases"]}
    candidate_map = {entry["id"]: entry for entry in candidate["cases"]}
    for entry in oracle["cases"]:
        case_id = entry["id"]
        if case_id not in candidate_map:
            diffs.append(f"{case_id}: missing candidate transcript")
            cases[case_id] = False
            continue
        cases[case_id] = True
    for case_id, old in oracle_map.items():
        new = candidate_map.get(case_id)
        if new is None:
            continue
        if case_id in RPC_DELTA_CASES:
            continue
        if bool(old.get("dropped")) != bool(new.get("dropped")):
            diffs.append(f"{case_id}: dropped mismatch oracle={old.get('dropped')} candidate={new.get('dropped')}")
            cases[case_id] = False
            continue
        if old.get("dropped"):
            continue
        if old.get("status", "")[:12] != new.get("status", "")[:12]:
            diffs.append(f"{case_id}: status {old.get('status')} != {new.get('status')}")
            cases[case_id] = False
            continue
        old_headers = {name: value for name, value in old.get("headers", [])}
        new_headers = {name: value for name, value in new.get("headers", [])}
        for header in ("access-control-allow-origin", "content-type"):
            if (header in old_headers) != (header in new_headers):
                diffs.append(f"{case_id}: header {header} presence mismatch")
                cases[case_id] = False
        if old_headers.get("access-control-allow-origin") != new_headers.get("access-control-allow-origin"):
            diffs.append(f"{case_id}: CORS mismatch")
            cases[case_id] = False
        if old_headers.get("content-type") != new_headers.get("content-type"):
            diffs.append(f"{case_id}: content-type {old_headers.get('content-type')} != {new_headers.get('content-type')}")
            cases[case_id] = False
        if old.get("trailing_newline") != new.get("trailing_newline"):
            diffs.append(f"{case_id}: trailing newline mismatch")
            cases[case_id] = False
        if case_id == "rpc.http.zip-valid":
            # Archives embed build timestamps: compare entries, contents and
            # the comment instead of raw archive bytes.
            import io as _io
            import zipfile as _zipfile
            try:
                old_zip = _zipfile.ZipFile(_io.BytesIO(old.get("body", "").encode("latin-1")))
                new_zip = _zipfile.ZipFile(_io.BytesIO(new.get("body", "").encode("latin-1")))
                old_names = sorted(old_zip.namelist())
                new_names = sorted(new_zip.namelist())
                same_names = old_names == new_names
                same_contents = same_names and all(
                    old_zip.read(name) == new_zip.read(name) for name in old_names)
                old_comment = old_zip.comment.decode("utf-8", "replace")
                new_comment = new_zip.comment.decode("utf-8", "replace")
            except Exception as exc:
                diffs.append(f"{case_id}: zip parse failed {exc}")
                cases[case_id] = False
                continue
            if not same_names or not same_contents:
                diffs.append(f"{case_id}: zip entries/contents differ")
                cases[case_id] = False
                continue
            if old_comment.replace("2.023-py", "2.023-") != new_comment:
                diffs.append(f"{case_id}: zip comment mismatch")
                cases[case_id] = False
                continue
            continue
        try:
            old_body = scrub(json.loads(old.get("body", "")))
            new_body = scrub(json.loads(new.get("body", "")))
        except ValueError:
            if old.get("body") != new.get("body"):
                diffs.append(f"{case_id}: raw body mismatch")
                cases[case_id] = False
            continue
        old_version = (old_body.get("result") or {}).get("version") if isinstance(old_body.get("result"), dict) else None
        new_version = (new_body.get("result") or {}).get("version") if isinstance(new_body.get("result"), dict) else None
        if (old_version is None) != (new_version is None):
            diffs.append(f"{case_id}: version presence mismatch")
            cases[case_id] = False
        elif old_version is not None:
            import re as _re
            if not _re.fullmatch(r"2\.023-py\d+", str(old_version)):
                diffs.append(f"{case_id}: oracle version shape {old_version!r}")
                cases[case_id] = False
            if str(new_version) != "2.023-rss":
                diffs.append(f"{case_id}: candidate version {new_version!r} != 2.023-rss")
                cases[case_id] = False
        old_guid = (old_body.get("result") if isinstance(old_body.get("result"), str) else None)
        new_guid = (new_body.get("result") if isinstance(new_body.get("result"), str) else None)
        old_norm = _rpc_normalize_body(old_body, old_guid)
        new_norm = _rpc_normalize_body(new_body, new_guid)
        # Unknown-key storage delta (`updateConfig.unknown-key`): the oracle
        # silently keeps unknown keys, so later config views may carry them.
        # Filter the oracle view to the 21 public fields; the storage
        # decision itself is pinned by its own delta case.
        if isinstance(old_norm.get("result"), dict) and "dir" in old_norm["result"]:
            old_norm["result"] = {
                key: value for key, value in old_norm["result"].items()
                if key in RPC_PUBLIC_CONFIG_FIELDS
            }
        if isinstance(old_norm.get("result"), dict) and "version" in old_norm["result"]:
            old_norm["result"]["version"] = "<version>"
        if isinstance(new_norm.get("result"), dict) and "version" in new_norm["result"]:
            new_norm["result"]["version"] = "<version>"
        if old_norm != new_norm:
            # Exec-error messages are version-worded on the oracle side and
            # designed on the candidate side: codes must match exactly, and
            # the candidate wording is pinned per case below.
            old_error = old_norm.get("error")
            new_error = new_norm.get("error")
            if isinstance(old_error, dict) and isinstance(new_error, dict):
                if old_error.get("code") != new_error.get("code"):
                    diffs.append(f"{case_id}: envelope mismatch {old_norm} != {new_norm}")
                    cases[case_id] = False
                    continue
                if not new_error.get("message"):
                    diffs.append(f"{case_id}: candidate exec error has no message")
                    cases[case_id] = False
                    continue
                expected = CANDIDATE_EXEC_MESSAGES.get(case_id)
                if expected is not None and new_error.get("message") != expected:
                    diffs.append(f"{case_id}: candidate message {new_error.get('message')!r} != {expected!r}")
                    cases[case_id] = False
                    continue
                if not old_error.get("message"):
                    diffs.append(f"{case_id}: oracle exec error has no message")
                    cases[case_id] = False
                    continue
                continue
            diffs.append(f"{case_id}: envelope mismatch {old_norm} != {new_norm}")
            cases[case_id] = False
        # Task config key order: the candidate serializes BASE_CONFIG_KEYS
        # order (M2 snapshot contract); the reference uses its own tuple
        # order. Values must be equal; the order delta is recorded, not
        # ignored.
        if case_id == "rpc.listTasks.default" and not old.get("dropped") and not new.get("dropped"):
            # Config key order is identical on both sides (config.py source
            # order preserved through the reference tuple filter and the
            # candidate BASE_CONFIG_KEYS): full parity, no delta.
            try:
                old_orders = [list(entry["config"].keys()) for entry in old_body["result"]]
                new_orders = [list(entry["config"].keys()) for entry in new_body["result"]]
            except (KeyError, TypeError, AttributeError):
                old_orders = new_orders = None
            if not old_orders or old_orders != new_orders:
                diffs.append(f"{case_id}: config order {old_orders} != {new_orders}")
                cases[case_id] = False
            cases["rpc.listTasks.config-order"] = cases.get(case_id, False)
        # Task-state side effects must match exactly. Created-task guids are
        # random on both sides, so every 8-hex guid key normalizes away.
        def split_states(states):
            fixed = {guid: state for guid, state in states.items()
                     if guid in ("task-wait", "task-done", "task-zip")}
            created = sorted(state for guid, state in states.items()
                             if guid not in ("task-wait", "task-done", "task-zip"))
            return fixed, created
        old_fixed, old_created = split_states(old.get("after", {}).get("states", {}))
        new_fixed, new_created = split_states(new.get("after", {}).get("states", {}))
        old_states = (old_fixed, old_created)
        new_states = (new_fixed, new_created)
        if old_states != new_states:
            diffs.append(f"{case_id}: task states {old_states} != {new_states}")
            cases[case_id] = False
        if old.get("after", {}).get("queue_len") != new.get("after", {}).get("queue_len"):
            diffs.append(f"{case_id}: queue_len {old.get('after', {}).get('queue_len')} != {new.get('after', {}).get('queue_len')}")
            cases[case_id] = False
        if scrub(old.get("after", {}).get("cfg_dir")) != scrub(new.get("after", {}).get("cfg_dir")):
            diffs.append(f"{case_id}: cfg dir changed differently")
            cases[case_id] = False
    return diffs


# The 21 public config fields (`rpc.py:427-429`); unknown keys stored by the
# reference are filtered from oracle views (pinned by unknown-key delta).
RPC_PUBLIC_CONFIG_FIELDS = [
    "daemon", "dir", "download_ori", "jpn_title", "rename_ori", "proxy",
    "proxy_image", "proxy_image_only", "make_archive", "download_range",
    "scan_thread_cnt", "download_thread_cnt", "download_timeout",
    "ignored_errors", "log_path", "log_verbose", "save_tasks",
    "delete_task_files", "low_speed_threshold", "auto_update",
    "update_beta_channel",
]

# Pinned candidate exec-error wordings, collected from real runs (the
# oracle wordings are CPython-version texts recorded per case in evidence).
CANDIDATE_EXEC_MESSAGES = {
}

# (status-prefix, dropped) pins for approved wire deltas.
RPC_DELTA_CASES = {
    "rpc.wire.invalid-json": {
        "oracle": "empty-400",
        "candidate": "envelope-400",
    },
    "rpc.http.img-badquery": {
        "oracle": (True, ""),
        "candidate": (False, "HTTP/1.0 400"),
    },
    "rpc.http.img-badfid": {
        "oracle": (True, ""),
        "candidate": (False, "HTTP/1.0 400"),
    },
    "rpc.updateConfig.proxy-nodir": {
        "oracle": (True, ""),
        "candidate": (False, "HTTP/1.0 200"),
    },
    "rpc.updateConfig.unknown-key": {
        "oracle": (True, ""),
        "candidate": (False, "HTTP/1.0 500"),
    },
    "rpc.loginExhentai.logged-in": {
        "oracle": (True, ""),
        "candidate": (False, "HTTP/1.0 200"),
    },
    "rpc.wire.nonstring-method": {
        "oracle": (True, ""),
        "candidate": (False, "HTTP/1.0 500"),
    },
    "rpc.wire.params-nonpair": {
        "oracle": (True, ""),
        "candidate": (False, "HTTP/1.0 400"),
    },
    "rpc.wire.params-single": {
        "oracle": (True, ""),
        "candidate": (False, "HTTP/1.0 400"),
    },
    "rpc.getImage.no-total": {
        "oracle": (True, ""),
        "candidate": (False, "HTTP/1.0 500"),
    },
    "rpc.auth.substring": {
        "oracle": (False, "HTTP/1.0 200"),
        "candidate": (False, "HTTP/1.0 403"),
    },
}

# --- B2: isolated missing-dir crash-repair proof ---------------------------
#
# The shared transcript runs proxy-dir before proxy-nodir with the SAME
# proxy, masking the pool state change. These variants run on FRESH apps
# with a DISTINCT proxy plus another known config change, and compare the
# complete outcome, body and full after-state on both sides independently.
ISOLATED_PROXY = "socks5://127.0.0.1:1081"
ISOLATED_SEED_PROXY = "socks5://127.0.0.1:1079"
ISOLATED_THREAD_CNT = 9


def rpc_isolated_variants(downloads_dir):
    import json as _json

    def body(params):
        return _json.dumps(
            {"method": "xeH.updateConfig", "id": 4, "params": [[], params]}
        )

    seed = {
        "id": "rpc.updateConfig.proxy-seeded",
        "method": "POST",
        "target": "/jsonrpc",
        "body": body({"proxy": [ISOLATED_SEED_PROXY], "dir": downloads_dir}),
    }
    measured = {
        "id": "rpc.updateConfig.proxy-nodir-isolated",
        "method": "POST",
        "target": "/jsonrpc",
        "body": body(
            {"proxy": [ISOLATED_PROXY],
             "download_thread_cnt": ISOLATED_THREAD_CNT}
        ),
    }
    empty = {
        "id": "rpc.updateConfig.proxy-empty-isolated",
        "method": "POST",
        "target": "/jsonrpc",
        "body": body({"proxy": [], "download_thread_cnt": ISOLATED_THREAD_CNT}),
    }
    existing = {
        "id": "rpc.updateConfig.proxy-existing-isolated",
        "method": "POST",
        "target": "/jsonrpc",
        "body": body(
            {"proxy": [ISOLATED_PROXY],
             "download_thread_cnt": ISOLATED_THREAD_CNT}
        ),
    }
    return [
        {"case_id": measured["id"], "seed": [], "measured": measured,
         "expect_members": [ISOLATED_PROXY]},
        {"case_id": empty["id"], "seed": [], "measured": empty,
         "expect_members": []},
        {"case_id": existing["id"], "seed": [seed], "measured": existing,
         "expect_members": [ISOLATED_SEED_PROXY, ISOLATED_PROXY]},
    ]


def _isolated_scrub(value, roots):
    if isinstance(value, str):
        for root in roots:
            if root and root in value:
                value = value.replace(root, "<root>")
        return value
    if isinstance(value, list):
        return [_isolated_scrub(item, roots) for item in value]
    if isinstance(value, dict):
        return {key: _isolated_scrub(item, roots) for key, item in value.items()}
    return value


def compare_rpc_isolated_one(cases, case_id, old, new, roots, expect_members):
    """Exact independent comparison for one isolated proxy-nodir variant."""
    import json as _json

    diffs = []
    # The reference MUST still record the crash: KeyError on the missing dir
    # drops the connection after the partial in-memory side effects.
    if not old.get("dropped"):
        diffs.append(f"{case_id}: oracle no longer drops (source behavior changed)")
        cases[case_id] = False
        return diffs
    # The candidate MUST answer 200 with the real post-update view.
    if new.get("dropped"):
        diffs.append(f"{case_id}: candidate dropped, want HTTP 200")
        cases[case_id] = False
        return diffs
    if new.get("status") != "HTTP/1.0 200 OK":
        diffs.append(f"{case_id}: candidate status {new.get('status')} != HTTP/1.0 200 OK")
        cases[case_id] = False
        return diffs
    if not new.get("trailing_newline"):
        diffs.append(f"{case_id}: candidate lost the trailing newline")
        cases[case_id] = False
    try:
        new_body = _isolated_scrub(_json.loads(new.get("body", "")), roots)
    except ValueError:
        diffs.append(f"{case_id}: candidate body is not JSON")
        cases[case_id] = False
        return diffs
    result = new_body.get("result")
    if not isinstance(result, dict):
        diffs.append(f"{case_id}: candidate body has no result view: {new_body!r}"[:200])
        cases[case_id] = False
        return diffs
    # Full after-state must converge exactly: cfg, pool members+count,
    # queue/states, cookies/login, dirs.
    old_after = _isolated_scrub(old.get("after", {}), roots)
    new_after = _isolated_scrub(new.get("after", {}), roots)
    for key in ("cfg", "proxy_members", "proxy_count", "queue_len",
                "has_login", "cookies", "states", "cfg_dir", "dirs"):
        if old_after.get(key) != new_after.get(key):
            diffs.append(
                f"{case_id}: after.{key} differs: "
                f"{str(old_after.get(key))[:160]!r} != {str(new_after.get(key))[:160]!r}")
    # The body IS the post-update public config: it must equal the
    # candidate's own after-cfg and the oracle's after-cfg.
    if new_after.get("cfg") != result:
        diffs.append(f"{case_id}: candidate body is not its own post-update config")
    if old_after.get("cfg") != result:
        diffs.append(f"{case_id}: candidate body differs from oracle after-config")
    # The requested effects landed: distinct proxy member(s) plus the other
    # config change; the directory is untouched; no tree growth.
    if new_after.get("proxy_members") != expect_members:
        diffs.append(
            f"{case_id}: pool members {new_after.get('proxy_members')} != {expect_members}")
    if new_after.get("proxy_count") != len(expect_members):
        diffs.append(f"{case_id}: pool count {new_after.get('proxy_count')} != {len(expect_members)}")
    if (result.get("download_thread_cnt") if isinstance(result, dict) else None) != ISOLATED_THREAD_CNT:
        diffs.append(f"{case_id}: other config change missing from body")
    old_before = _isolated_scrub(old.get("before", {}), roots)
    new_before = _isolated_scrub(new.get("before", {}), roots)
    if old_after.get("cfg_dir") != old_before.get("cfg_dir"):
        diffs.append(f"{case_id}: oracle dir moved")
    if new_after.get("cfg_dir") != new_before.get("cfg_dir"):
        diffs.append(f"{case_id}: candidate dir moved")
    if old_after.get("dirs") != old_before.get("dirs"):
        diffs.append(f"{case_id}: oracle tree grew (extra directory creation)")
    if new_after.get("dirs") != new_before.get("dirs"):
        diffs.append(f"{case_id}: candidate tree grew (extra directory creation)")
    cases[case_id] = not diffs
    return diffs


def compare_rpc_isolated(cases, oracle, candidate):
    """Compare every isolated variant plus its seed on fresh apps."""
    diffs = []
    roots = oracle.get("roots", [oracle.get("root", "")]) + candidate.get(
        "roots", [candidate.get("root", "")])
    for variant in oracle.get("variants", []):
        case_id = variant["case_id"]
        old_map = {entry["id"]: entry for entry in oracle["cases"]}
        new_map = {entry["id"]: entry for entry in candidate["cases"]}
        seed_ids = [case["id"] for case in variant.get("seed", [])]
        for seed_id in seed_ids:
            old_seed, new_seed = old_map.get(seed_id), new_map.get(seed_id)
            if old_seed is None or new_seed is None:
                diffs.append(f"{case_id}: missing seed transcript {seed_id}")
                cases[seed_id] = False
                continue
            # The seed carries a dir, so both sides answer 200 with the
            # same public view; compare exactly like any ordinary case.
            if old_seed.get("dropped") or new_seed.get("dropped"):
                diffs.append(f"{case_id}: seed dropped: {seed_id}")
                cases[seed_id] = False
                continue
            if (old_seed.get("status", "")[:12] != "HTTP/1.0 200"
                    or new_seed.get("status", "")[:12] != "HTTP/1.0 200"):
                diffs.append(f"{case_id}: seed status: {seed_id}")
                cases[seed_id] = False
                continue
            import json as _json
            try:
                old_body = _isolated_scrub(
                    _json.loads(old_seed.get("body", "")), roots)
                new_body = _isolated_scrub(
                    _json.loads(new_seed.get("body", "")), roots)
            except ValueError:
                diffs.append(f"{case_id}: seed body not JSON: {seed_id}")
                cases[seed_id] = False
                continue
            if old_body != new_body:
                diffs.append(f"{case_id}: seed body differs: {seed_id}")
                cases[seed_id] = False
                continue
            cases[seed_id] = True
        old, new = old_map.get(case_id), new_map.get(case_id)
        if old is None or new is None:
            diffs.append(f"{case_id}: missing isolated transcript")
            cases[case_id] = False
            continue
        diffs.extend(compare_rpc_isolated_one(
            cases, case_id, old, new, roots, variant["expect_members"]))
    return diffs


RPC_CASE_IDS = [case["id"] for case in rpc_wire_cases()]


# ---------------------------------------------------------------------------
# M5 FD gallery differential: legal internal aliases, the approved
# outside-referent denial delta and independent full poststates.
# ---------------------------------------------------------------------------

GALLERY_POSTSTATE_CASES = ["gallery.poststate.before", "gallery.poststate.after"]

# The reference follows the outside literal, sees a genuinely absent referent
# and serves the matching sibling ZIP entry; the product denies the outside
# referent before any lookup or fallback. Both sides are pinned independently.
GALLERY_DELTA_CASES = {
    "delta.fd-outside-referent-denial": {
        "oracle": {"status": 200, "mime": "image/jpeg",
                   "bytes": "ZIP-OUTSIDE-ENTRY-BYTES"},
        "candidate": {"status": 404, "bytes": None},
    },
}


def _gallery_dirs(work):
    """Disk dimension: relative tree plus exact link targets, roots normalized."""
    prefix = str(work)
    return [item.replace(prefix, "<ROOT>") for item in _dir_paths(work)]


def _gallery_materialize(directory, spec, work):
    """Create the alias fixtures both sides must share: subdirs, legal links
    (relative, absolute, absolute-through-a-link, nested, parent-spelling and
    an outside dangling literal) and a pre-built sibling ZIP with exact bytes."""
    import os
    import zipfile
    root = str(work)
    for subdir, files in spec.get("subdirs", {}).items():
        full_dir = os.path.join(directory, subdir)
        os.makedirs(full_dir, exist_ok=True)
        for name, content in files.items():
            with open(os.path.join(full_dir, name), "wb") as handle:
                handle.write(content.encode("latin-1"))
    for link in spec.get("symlinks", []):
        target = link["target"].replace("{root}", root)
        os.symlink(target, os.path.join(directory, link["name"]))
    if "zip_entries" in spec:
        with zipfile.ZipFile(directory + ".zip", "w", zipfile.ZIP_STORED) as archive:
            for name, content in spec["zip_entries"].items():
                archive.writestr(name, content.encode("latin-1"))


def _gallery_poststate(app, work):
    """Independent full poststate: task/session/queue/cookie/header/login/disk."""
    tasks = {}
    for guid, task in app._all_tasks.items():
        tasks[guid] = {
            "state": task.state,
            "meta": {key: task.meta.get(key) for key in sorted(task.meta)},
        }
    return {
        "tasks": tasks,
        "session": {"has_login": app.has_login},
        "queue": list(app.tasks.queue),
        "cookies": sorted([list(item) for item in app.cookies.items()]),
        "headers": [[name, value] for name, value in sorted(app.headers.items())],
        "login": {"has_login": app.has_login,
                  "cookie_header": app.headers.get("Cookie")},
        "disk": _gallery_dirs(work),
    }


def oracle_gallery(upstream, request):
    import importlib
    core, task_module, const_module, defaults = _load_upstream(upstream)
    rpc_module = importlib.import_module("xeHentai.rpc")
    work = Path(request["root"])
    work.mkdir(parents=True, exist_ok=True)
    app = _rpc_build_app(
        core, task_module, const_module, defaults, work, request["tasks"])
    for spec in request["tasks"]:
        _gallery_materialize(
            app._all_tasks[spec["guid"]].get_fpath(), spec, work)
    before = _gallery_poststate(app, work)
    reads = []
    for query in request.get("reads", []):
        guid = query["guid"]
        fid = query["fid"]
        try:
            path, name, mime = rpc_module.xeHentaiRPCExtended(
                app, request.get("secret"))._get_image_path(guid, fid)
        except Exception as exc:
            reads.append({"guid": guid, "fid": fid,
                          "raised": type(exc).__name__})
            continue
        if not name:
            reads.append({"guid": guid, "fid": fid, "status": 404})
            continue
        full = path and __import__("os").path.join(path, name)
        if full and __import__("os").path.exists(full):
            with open(full, "rb") as handle:
                reads.append({"guid": guid, "fid": fid, "status": 200,
                              "mime": mime,
                              "bytes": handle.read().decode("latin-1")})
            continue
        import zipfile
        zipped = "%s.zip" % path if path else ""
        if zipped and __import__("os").path.exists(zipped):
            try:
                with zipfile.ZipFile(zipped) as archive:
                    reads.append({"guid": guid, "fid": fid, "status": 200,
                                  "mime": mime,
                                  "bytes": archive.read(name).decode("latin-1")})
                    continue
            except Exception as exc:
                reads.append({"guid": guid, "fid": fid, "status": 404,
                              "zip_error": type(exc).__name__})
                continue
        reads.append({"guid": guid, "fid": fid, "status": 404})
    thumbs = []
    for query in request.get("thumbs", []):
        from io import BytesIO
        guid = query["guid"]
        fid = query["fid"]
        match = next((entry for entry in reads
                      if entry["guid"] == guid and entry["fid"] == fid
                      and entry.get("status") == 200), None)
        if match is None:
            thumbs.append({"guid": guid, "fid": fid, "query": query["query"],
                           "outcome": "not_found"})
            continue
        raw = match["bytes"].encode("latin-1")
        try:
            args = dict(item.split("=") for item in query["query"].split("&") if item)
            out, errored = rpc_module.gen_thumbnail(BytesIO(raw), args)
        except Exception as exc:
            thumbs.append({"guid": guid, "fid": fid, "query": query["query"],
                           "outcome": "raised", "class": type(exc).__name__})
            continue
        if "w" not in args and "h" not in args:
            # `gen_thumbnail` returns the original handler unchanged; the
            # response keeps the file-derived MIME and exact bytes.
            thumbs.append({"guid": guid, "fid": fid, "query": query["query"],
                           "outcome": "passthrough", "mime": match.get("mime"),
                           "bytes": match["bytes"]})
            continue
        if errored:
            thumbs.append({"guid": guid, "fid": fid, "query": query["query"],
                           "outcome": "pil_missing"})
            continue
        payload = out if isinstance(out, bytes) else out.getvalue()
        try:
            from PIL import Image
            image = Image.open(BytesIO(payload))
            image.load()
            thumbs.append({"guid": guid, "fid": fid, "query": query["query"],
                           "outcome": "resized",
                           "mime": Image.MIME.get(image.format, "application/octet-stream"),
                           "width": image.width, "height": image.height})
        except Exception as exc:
            thumbs.append({"guid": guid, "fid": fid, "query": query["query"],
                           "outcome": "decode_error", "class": type(exc).__name__})
    archives = []
    for query in request.get("archives", []):
        guid = query["guid"]
        try:
            path = rpc_module.xeHentaiRPCExtended(
                app, request.get("secret"))._get_archive_path(guid)
        except Exception as exc:
            archives.append({"guid": guid, "raised": type(exc).__name__})
            continue
        if not isinstance(path, str):
            # `_get_archive_path` returns `(None, None)` for an unknown guid;
            # `do_GET` then passes the tuple to `os.path.exists`, raises
            # TypeError and aborts the connection. Record the real handler
            # outcome, not a convenient 404.
            archives.append({"guid": guid, "status": "dropped",
                             "returned": type(path).__name__})
            continue
        import zipfile
        try:
            with zipfile.ZipFile(path) as archive:
                names = sorted(archive.namelist())
                contents = {name: archive.read(name).decode("latin-1") for name in names}
                archives.append({"guid": guid, "status": 200,
                                 "comment": archive.comment.decode("utf-8", "replace"),
                                 "names": names, "contents": contents})
        except Exception as exc:
            archives.append({"guid": guid, "status": "bad_zip",
                             "class": type(exc).__name__})
    for entry, query in zip(reads, request.get("reads", [])):
        if query.get("case"):
            entry["case"] = query["case"]
    return {"reads": reads, "thumbs": thumbs, "archives": archives,
            "poststate": {"before": before,
                          "after": _gallery_poststate(app, work)}}


def compare_gallery(cases, oracle, candidate):
    diffs = []
    for old, new in zip(oracle["reads"], candidate["reads"]):
        case_id = old.get("case")
        if case_id in GALLERY_DELTA_CASES:
            expected = GALLERY_DELTA_CASES[case_id]
            old_ok = (old.get("status") == expected["oracle"]["status"]
                      and old.get("mime") == expected["oracle"]["mime"]
                      and old.get("bytes") == expected["oracle"]["bytes"])
            new_ok = (new.get("status") == expected["candidate"]["status"]
                      and new.get("bytes") == expected["candidate"]["bytes"])
            cases[case_id] = old_ok and new_ok
            if not cases[case_id]:
                diffs.append(f"{case_id}: pinned delta mismatch "
                             f"oracle={old} candidate={new}")
            continue
        key = case_id or f"gallery.read.{old['guid']}.{old['fid']}"
        if old.get("raised") or new.get("status") in (400,):
            # int("nope") paths: both sides refuse without touching disk.
            same = (old.get("raised") == "ValueError") == (new.get("status") == 400)
            if not same:
                diffs.append(f"{key}: refusal mismatch {old} != {new}")
                cases[key] = False
            else:
                cases[key] = True
            continue
        if old.get("status") != new.get("status"):
            diffs.append(f"{key}: status {old} != {new}")
            cases[key] = False
            continue
        if old.get("status") == 200 and (old.get("bytes") != new.get("bytes") or old.get("mime") != new.get("mime")):
            diffs.append(f"{key}: bytes/mime mismatch")
            cases[key] = False
            continue
        cases[key] = True
    thumb_slugs = {"w=4&h=4": "resized", "w=4": "w-only", "": "passthrough",
                   "w": "bad-query"}
    for old, new in zip(oracle["thumbs"], candidate["thumbs"]):
        key = f"gallery.thumb.{old['guid']}.{old['fid']}.bad-query"
        if old.get("query", "") == "w":
            # `dict(q.split("="))` in the reference raises ValueError on a
            # bare token and the connection aborts; the candidate answers a
            # typed 400. Both refuse to serve pixels.
            ok = old.get("outcome") == "raised" and new.get("outcome") == "bad_query"
            cases[key] = ok
            if not ok:
                diffs.append(f"{key}: bad-query mismatch {old} != {new}")
            continue
        slug = thumb_slugs.get(old.get("query", ""), old.get("query", ""))
        if old.get("outcome") == "raised" or new.get("outcome") == "decode_error":
            slug = "decode"
        key = f"gallery.thumb.{old['guid']}.{old['fid']}.{slug}"
        if old.get("outcome") == "not_found" or new.get("outcome") == "not_found":
            key = f"gallery.thumb.{old['guid']}.{old['fid']}.not-found"
            cases[key] = old.get("outcome") == new.get("outcome")
            if not cases[key]:
                diffs.append(f"{key}: not-found mismatch")
            continue
        if old.get("outcome") == "raised" or new.get("outcome") == "decode_error":
            # PIL raises on undecodable input where the candidate answers a
            # typed decode failure; both sides refuse to serve pixels.
            cases[key] = True
            continue
        for field in ("mime", "width", "height"):
            if old.get(field) != new.get(field):
                diffs.append(f"{key}: {field} {old.get(field)} != {new.get(field)}")
                cases[key] = False
                break
        else:
            if slug == "passthrough" and old.get("bytes") != new.get("bytes"):
                diffs.append(f"{key}: passthrough bytes differ")
                cases[key] = False
                continue
            cases[key] = cases.get(key, True)
    for old, new in zip(oracle["archives"], candidate["archives"]):
        key = f"gallery.archive.{old['guid']}"
        if old.get("status") == "dropped":
            # Reference bug: unknown guid yields a `(None, None)` tuple that
            # `do_GET` feeds to `os.path.exists`, aborting the connection;
            # the candidate answers 404. Pinned, not normalized away.
            ok = new.get("status") == 404
            cases[key] = ok
            if not ok:
                diffs.append(f"{key}: dropped-path mismatch {old} != {new}")
            continue
        if old.get("status") != new.get("status"):
            diffs.append(f"{key}: status {old} != {new}")
            cases[key] = False
            continue
        if old.get("status") == 200:
            if old.get("names") != new.get("names"):
                diffs.append(f"{key}: entries {old.get('names')} != {new.get('names')}")
                cases[key] = False
                continue
            if old.get("contents") != new.get("contents"):
                diffs.append(f"{key}: entry contents differ")
                cases[key] = False
                continue
            old_comment = old.get("comment", "").replace("2.023-py", "2.023-")
            if old_comment != new.get("comment", ""):
                diffs.append(f"{key}: comment mismatch")
                cases[key] = False
                continue
            cases[key] = True
        else:
            cases[key] = True
    # A gallery read never changes the task/session/queue/cookie/header/login
    # dimensions (the on-demand archive case legitimately publishes its own
    # sibling ZIP on disk, which the disk comparison already covers). Check
    # that independently on each side before comparing the two sides.
    stable_fields = ("tasks", "session", "queue", "cookies", "headers", "login")
    unstable = {}
    for side, output in (("oracle", oracle), ("candidate", candidate)):
        state = output.get("poststate") or {}
        before = state.get("before") or {}
        after = state.get("after") or {}
        changed = [field for field in stable_fields
                   if before.get(field) != after.get(field)]
        unstable[side] = changed
        if changed:
            diffs.append(
                f"gallery.poststate.{side}: a read changed {changed}")
    unstable_any = any(unstable.values())
    for phase in ("before", "after"):
        key = f"gallery.poststate.{phase}"
        old_state = (oracle.get("poststate") or {}).get(phase)
        new_state = (candidate.get("poststate") or {}).get(phase)
        if old_state == new_state and not unstable_any:
            cases[key] = True
            continue
        differing = sorted(
            field for field in set(old_state or {}) | set(new_state or {})
            if (old_state or {}).get(field) != (new_state or {}).get(field))
        diffs.append(
            f"{key}: poststate differs in {differing}: "
            + str([(field, (old_state or {}).get(field),
                    (new_state or {}).get(field)) for field in differing]))
        cases[key] = False
    return diffs


def oracle_webui(upstream, request):
    del upstream
    from pathlib import Path as _Path
    checkout = _Path(request["checkout"])
    names = []
    for path in sorted(checkout.rglob("*")):
        if path.is_file() and path.suffix != ".map":
            names.append({
                "name": path.relative_to(checkout).as_posix(),
                "size": path.stat().st_size,
            })
    index = (checkout / "index.html").read_text(encoding="utf-8", errors="replace")
    return {
        "files": names,
        "index_markers": {
            "has_title": "<title" in index,
            "has_app": "/static/js/app" in index,
            "has_auth_host": "host=" in index,
        },
    }


def compare_webui(cases, oracle, candidate):
    diffs = []
    old_files = {entry["name"]: entry["size"] for entry in oracle["files"]}
    new_files = {entry["name"]: entry["size"] for entry in candidate["files"]}
    # The frozen pack records its own provenance file; it is metadata about
    # the pack, not a UI asset served to the original application.
    new_files.pop("PROVENANCE", None)
    if old_files != new_files:
        only_old = sorted(set(old_files) - set(new_files))
        only_new = sorted(set(new_files) - set(old_files))
        sized = sorted(name for name in set(old_files) & set(new_files)
                       if old_files[name] != new_files[name])
        diffs.append(f"webui.pack: only-oracle={only_old} only-candidate={only_new} sizediff={sized}")
        cases["webui.pack.files"] = False
    else:
        cases["webui.pack.files"] = True
    if oracle["index_markers"] != candidate["index_markers"]:
        diffs.append("webui.pack: index markers differ")
        cases["webui.pack.index"] = False
    else:
        cases["webui.pack.index"] = True
    return diffs


RPC_NATIVE_CASES = [case["id"] for case in rpc_wire_cases()]
def gallery_png_bytes():
    from io import BytesIO
    from PIL import Image
    image = Image.new("RGB", (8, 8), (200, 30, 30))
    buffer = BytesIO()
    image.save(buffer, format="PNG")
    return buffer.getvalue().decode("latin-1")


def gallery_request():
    png = gallery_png_bytes()
    tasks = [
        {
            "guid": "task-wait",
            "url": "http://e-hentai.org/g/123/abcdef0123/",
            "state": 10,
            "meta": {"total": 3, "finished": 0, "title": "probe-wait"},
            "files": {"1.jpg": png, "2.jpg": "ASCII-DATA-2"},
            "renamed": {"5": "2.jpg"},
        },
        {
            "guid": "task-zip",
            "url": "http://e-hentai.org/g/124/abcdef0123/",
            "state": 1,
            "meta": {"total": 2, "finished": 0, "title": "probe-zip"},
            "files": {"1.jpg": png, "2.jpg": "ASCII-DATA-Z"},
            "renamed": {},
            "zip_build": True,
            "zip_remove": ["2.jpg"],
        },
        {
            "guid": "task-done",
            "url": "http://e-hentai.org/g/456/abcdef0123/",
            "state": 20,
            "meta": {"total": 1, "finished": 1, "title": "probe-done"},
            "files": {"1.jpg": png},
            "renamed": {},
        },
    ]
    # Legal internal aliases: the reference and the product must serve the
    # same exact bytes through every accepted spelling.
    tasks.append({
        "guid": "task-alias",
        "url": "http://e-hentai.org/g/789/abcdef0123/",
        "state": 10,
        "meta": {"total": 8, "finished": 0, "title": "probe-alias"},
        "files": {
            "1.jpg": png,
            "5.jpg": "RELATIVE-ALIAS-BYTES",
            "11.jpg": "NESTED-RESET-BYTES",
            "13.jpg": "PARENT-SPELLING-BYTES",
        },
        "subdirs": {"real": {"8.jpg": "INTERMEDIATE-BYTES"}},
        "symlinks": [
            {"name": "6.jpg", "target": "5.jpg"},
            {"name": "7.jpg", "target": "{root}/downloads/probe-alias/5.jpg"},
            {"name": "linkdir", "target": "real"},
            {"name": "9.jpg",
             "target": "{root}/downloads/probe-alias/linkdir/8.jpg"},
            {"name": "10.jpg", "target": "{root}/downloads/probe-alias/11.jpg"},
            {"name": "12.jpg", "target": "{root}/downloads/probe-alias/10.jpg"},
            {"name": "14.jpg",
             "target": "{root}/downloads/./probe-alias/13.jpg"},
        ],
        "renamed": {"1": "1.jpg", "6": "6.jpg", "7": "7.jpg", "9": "9.jpg",
                    "12": "12.jpg", "14": "14.jpg"},
    })
    # Approved security delta: the literal referent is OUTSIDE the gallery and
    # does not exist, while the sibling ZIP carries a matching entry.
    tasks.append({
        "guid": "task-outside",
        "url": "http://e-hentai.org/g/790/abcdef0123/",
        "state": 10,
        "meta": {"total": 1, "finished": 0, "title": "probe-outside"},
        "files": {},
        "symlinks": [
            {"name": "1.jpg", "target": "{root}/outside/absent.jpg"},
        ],
        "renamed": {"1": "1.jpg"},
        "zip_entries": {"1.jpg": "ZIP-OUTSIDE-ENTRY-BYTES"},
    })
    return {
        "tasks": tasks,
        "reads": [
            {"guid": "task-wait", "fid": "1"},
            {"guid": "task-wait", "fid": "5"},
            {"guid": "task-wait", "fid": "nope"},
            {"guid": "missing", "fid": "1"},
            {"guid": "task-zip", "fid": "2"},
            {"guid": "task-zip", "fid": "9"},
            {"guid": "task-alias", "fid": "6", "case": "gallery.alias.relative"},
            {"guid": "task-alias", "fid": "7",
             "case": "gallery.alias.direct-absolute"},
            {"guid": "task-alias", "fid": "9",
             "case": "gallery.alias.absolute-intermediate"},
            {"guid": "task-alias", "fid": "12",
             "case": "gallery.alias.nested-reset"},
            {"guid": "task-alias", "fid": "14",
             "case": "gallery.alias.parent-spelling"},
            {"guid": "task-outside", "fid": "1",
             "case": "delta.fd-outside-referent-denial"},
        ],
        "thumbs": [
            {"guid": "task-wait", "fid": "1", "query": "w=4&h=4"},
            {"guid": "task-wait", "fid": "1", "query": "w=4"},
            {"guid": "task-wait", "fid": "1", "query": ""},
            {"guid": "task-wait", "fid": "1", "query": "w"},
            {"guid": "task-wait", "fid": "nope", "query": "w=4"},
            {"guid": "task-wait", "fid": "5", "query": "w=4"},
        ],
        "archives": [
            {"guid": "task-wait"},
            {"guid": "task-zip"},
            {"guid": "missing"},
        ],
    }


GALLERY_CASE_IDS = [
    "gallery.read.task-wait.1",
    "gallery.read.task-wait.5",
    "gallery.read.task-wait.nope",
    "gallery.read.missing.1",
    "gallery.read.task-zip.2",
    "gallery.read.task-zip.9",
    "gallery.thumb.task-wait.1.resized",
    "gallery.thumb.task-wait.1.w-only",
    "gallery.thumb.task-wait.1.passthrough",
    "gallery.thumb.task-wait.1.bad-query",
    "gallery.thumb.task-wait.nope.not-found",
    "gallery.thumb.task-wait.5.decode",
    "gallery.archive.task-wait",
    "gallery.archive.task-zip",
    "gallery.archive.missing",
    "gallery.alias.relative",
    "gallery.alias.direct-absolute",
    "gallery.alias.absolute-intermediate",
    "gallery.alias.nested-reset",
    "gallery.alias.parent-spelling",
    "delta.fd-outside-referent-denial",
    "gallery.poststate.before",
    "gallery.poststate.after",
]
WEBUI_CASE_IDS = ["webui.pack.files", "webui.pack.index"]


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
        elif suite == "site":
            oracle_root = root / "oracle-work"
            candidate_root = root / "candidate-work"
            oracle_root.mkdir()
            candidate_root.mkdir()
            scenarios = site_scenarios()
            oracle = run_oracle(
                "site",
                {"scenarios": scenarios},
                root,
                upstream,
                root / "oracle-site.log",
                cwd=oracle_root,
            )
            candidate = run_probe(
                "site",
                {"root": str(candidate_root), "scenarios": scenarios},
                root,
                target_dir,
                root / "probe.log",
            )
            cases = {}
            diffs = compare_site(cases, oracle, candidate)
        elif suite == "transport":
            oracle_root = root / "oracle-work"
            candidate_root = root / "candidate-work"
            oracle_root.mkdir()
            candidate_root.mkdir()
            oracle = run_oracle(
                "transport",
                {},
                root,
                upstream,
                root / "oracle-transport.log",
                cwd=oracle_root,
            )
            candidate = run_probe(
                "transport",
                {"root": str(candidate_root)},
                root,
                target_dir,
                root / "probe.log",
            )
            cases = {}
            diffs = compare_transport(cases, oracle, candidate)
        elif suite == "download":
            oracle_root = root / "oracle-work"
            candidate_root = root / "candidate-work"
            oracle_root.mkdir()
            candidate_root.mkdir()
            scenario = download_scenarios()
            oracle = run_oracle(
                "download",
                {"root": str(oracle_root), "scenario": scenario},
                root,
                upstream,
                root / "oracle-download.log",
                cwd=oracle_root,
            )
            candidate = run_probe(
                "download",
                {"root": str(candidate_root), "scenario": scenario},
                root,
                target_dir,
                root / "probe.log",
            )
            cases = {}
            diffs = compare_download(cases, oracle, candidate)
        elif suite == "monitor":
            oracle_root = root / "oracle-work"
            candidate_root = root / "candidate-work"
            oracle_root.mkdir()
            candidate_root.mkdir()
            scenarios = monitor_scenarios()
            oracle = run_oracle(
                "monitor",
                scenarios,
                root,
                upstream,
                root / "oracle-monitor.log",
                cwd=oracle_root,
            )
            candidate = run_probe(
                "monitor",
                scenarios,
                root,
                target_dir,
                root / "probe.log",
            )
            cases = {}
            diffs = compare_monitor(cases, oracle, candidate)
        elif suite == "proxy":
            oracle_root = root / "oracle-work"
            candidate_root = root / "candidate-work"
            oracle_root.mkdir()
            candidate_root.mkdir()
            scenarios = proxy_scenarios()
            oracle = run_oracle(
                "proxy",
                scenarios,
                root,
                upstream,
                root / "oracle-proxy.log",
                cwd=oracle_root,
            )
            candidate = run_probe(
                "proxy",
                scenarios,
                root,
                target_dir,
                root / "probe.log",
            )
            cases = {}
            diffs = compare_proxy(cases, oracle, candidate)
        elif suite == "session":
            # Each side gets its own reference-written fixture tree so absolute
            # paths inside h.json stay valid; the candidate never writes these
            # h.json files before loading them.
            oracle_root = root / "oracle-work"
            candidate_root = root / "candidate-work"
            oracle_root.mkdir()
            candidate_root.mkdir()
            run_oracle(
                "session-fixture",
                {"root": str(oracle_root)},
                root,
                upstream,
                root / "oracle-session-fixture.log",
                cwd=oracle_root,
            )
            run_oracle(
                "session-fixture",
                {"root": str(candidate_root)},
                root,
                upstream,
                root / "candidate-session-fixture.log",
                cwd=candidate_root,
            )
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
        elif suite == "rpc":
            oracle_root = root / "oracle-work"
            candidate_root = root / "candidate-work"
            oracle_root.mkdir()
            candidate_root.mkdir()
            open_cases = [case for case in rpc_wire_cases() if "secret" not in case]
            auth_cases = [case for case in rpc_wire_cases() if "secret" in case]
            oracle_open = run_oracle(
                "rpc",
                {"root": str(oracle_root), "secret": None,
                 "tasks": RPC_TASKS, "cases": open_cases},
                root,
                upstream,
                root / "oracle-rpc-open.log",
                cwd=oracle_root,
            )
            candidate_open = run_probe(
                "rpc",
                {"root": str(candidate_root), "secret": None,
                 "tasks": RPC_TASKS, "cases": open_cases},
                root,
                target_dir,
                root / "probe-rpc-open.log",
            )
            oracle_auth = run_oracle(
                "rpc",
                {"root": str(oracle_root), "secret": RPC_SECRET,
                 "tasks": RPC_TASKS, "cases": auth_cases},
                root,
                upstream,
                root / "oracle-rpc-auth.log",
                cwd=oracle_root,
            )
            candidate_auth = run_probe(
                "rpc",
                {"root": str(candidate_root), "secret": RPC_SECRET,
                 "tasks": RPC_TASKS, "cases": auth_cases},
                root,
                target_dir,
                root / "probe-rpc-auth.log",
            )
            oracle = {"open": oracle_open, "auth": oracle_auth}
            candidate = {"open": candidate_open, "auth": candidate_auth}
            cases = {}
            diffs = compare_rpc(cases, oracle, candidate)
            # B2 isolated legs: one fresh app per variant per side, full
            # state snapshots, exact independent comparison.
            isolated_oracle = {"roots": [], "cases": [], "variants": []}
            isolated_candidate = {"roots": [], "cases": [], "variants": []}
            for index in (1, 2, 3):
                oracle_iso = root / f"isolated-oracle-{index}"
                oracle_iso.mkdir()
                candidate_iso = root / f"candidate-isolated-{index}"
                candidate_iso.mkdir()
                isolated_oracle["roots"].append(str(oracle_iso))
                isolated_candidate["roots"].append(str(candidate_iso))
            template = rpc_isolated_variants("")
            iso_pairs = list(zip(
                isolated_oracle["roots"], isolated_candidate["roots"]))
            for position, (oracle_iso, candidate_iso) in enumerate(iso_pairs):
                import copy as _copy
                variant = _copy.deepcopy(template[position])
                # Seed transcripts share one id across variants; scope each
                # seed to its own fresh app.
                for seed_case in variant["seed"]:
                    seed_case["id"] = f"{seed_case['id']}-{position}"
                oracle_bodies = rpc_isolated_variants(
                    str(Path(oracle_iso) / "downloads"))[position]
                candidate_bodies = rpc_isolated_variants(
                    str(Path(candidate_iso) / "downloads"))[position]
                for seed_case in oracle_bodies["seed"]:
                    seed_case["id"] = f"{seed_case['id']}-{position}"
                for seed_case in candidate_bodies["seed"]:
                    seed_case["id"] = f"{seed_case['id']}-{position}"
                oracle_cases = (oracle_bodies["seed"]
                                + [oracle_bodies["measured"]])
                candidate_cases = (candidate_bodies["seed"]
                                   + [candidate_bodies["measured"]])
                isolated_oracle["variants"].append(variant)
                isolated_candidate["variants"].append(variant)
                oracle_iso_result = run_oracle(
                    "rpc",
                    {"root": oracle_iso, "secret": None,
                     "tasks": RPC_TASKS, "cases": oracle_cases,
                     "snapshot": "full"},
                    root,
                    upstream,
                    root / f"oracle-rpc-isolated-{position}.log",
                    cwd=Path(oracle_iso),
                )
                isolated_oracle["cases"].extend(oracle_iso_result["cases"])
                candidate_iso_result = run_probe(
                    "rpc",
                    {"root": candidate_iso, "secret": None,
                     "tasks": RPC_TASKS, "cases": candidate_cases,
                     "snapshot": "full"},
                    root,
                    target_dir,
                    root / f"probe-rpc-isolated-{position}.log",
                )
                isolated_candidate["cases"].extend(
                    candidate_iso_result["cases"])
            diffs.extend(compare_rpc_isolated(
                cases, isolated_oracle, isolated_candidate))
            oracle["isolated"] = isolated_oracle
            candidate["isolated"] = isolated_candidate
        elif suite == "gallery":
            oracle_root = root / "oracle-work"
            candidate_root = root / "candidate-work"
            oracle_root.mkdir()
            candidate_root.mkdir()
            gallery = gallery_request()
            oracle = run_oracle(
                "gallery",
                dict(gallery, root=str(oracle_root)),
                root,
                upstream,
                root / "oracle-gallery.log",
                cwd=oracle_root,
            )
            candidate = run_probe(
                "gallery",
                dict(gallery, root=str(candidate_root)),
                root,
                target_dir,
                root / "probe-gallery.log",
            )
            cases = {}
            diffs = compare_gallery(cases, oracle, candidate)
        elif suite == "webui":
            checkout = Path(os.environ.get(
                "XEH_WEBUI_CHECKOUT",
                "/mnt/TEMP/workspace/xehentai-rss/tmp/xehentai-pyshape-20261031-f870b8ca/webui-orig/xeHentai-webui-4e3738ef463fef344e96df613de9a81b4cda4b4e/dist",
            ))
            oracle = run_oracle(
                "webui",
                {"checkout": str(checkout)},
                root,
                upstream,
                root / "oracle-webui.log",
            )
            candidate = run_probe("webui", {}, root, target_dir, root / "probe-webui.log")
            cases = {}
            diffs = compare_webui(cases, oracle, candidate)
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
        if suite == "rpc":
            apply_rpc_aggregates(cases)
        if suite == "webui":
            apply_browser_evidence(cases)

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
        ["config", "util", "i18n", "worker", "rpc", "gallery", "webui", "task-api", "session", "files", "site", "proxy", "transport", "monitor", "download", "concurrency"]
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
            "rpc",
            "gallery",
            "webui",
            "task-api",
            "session",
            "files",
            "site",
            "proxy",
            "transport",
            "monitor",
            "download",
            "concurrency",
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
            "site",
            "proxy",
            "transport",
            "monitor",
            "download",
            "rpc",
            "gallery",
            "webui",
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
