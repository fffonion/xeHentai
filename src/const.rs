//! Fixed values from the reference `xeHentai/const.py`.
//!
//! Only constants are kept here; behaviour lives in the module that owns it.
//!
//! The reference table is complete so later milestones reuse the same names;
//! unused entries are expected until their owning module lands.
#![allow(dead_code)]

/// Reference release version, printed by the application.
pub const VERSION: &str = "2.023";
/// Version string that marks the Rust/RustScript implementation.
pub const VERSION_DISPLAY: &str = "2.023-rss";
pub const SCRIPT_NAME: &str = "xeHentai";

pub const DUMMY_FILENAME: &str = "-dummy-";
pub const RENAME_TMPDIR: &str = "-xeh-conflict-";
pub const STATIC_CACHE_FILE: &str = "webui.gz";
pub const STATIC_CACHE_TTL: u64 = 3600;
pub const STATIC_CACHE_VERSION: u32 = 1;
pub const SRC_UPDATE_FILE: &str = "src.zip";
pub const SRC_UPDATE_VERSION: u32 = 1;

pub const DEFAULT_MAX_REDIRECTS: usize = 30;

/// `const.py` fallback addresses for the fixed package mounts. These are only
/// used when no proxy is selected, and only for these exact hosts.
pub const FALLBACK_IP_MAP: &[(&str, &[&str])] = &[
    (
        "e-hentai.org",
        &["104.20.134.21", "104.20.135.21", "172.67.0.127"],
    ),
    (
        "forums.e-hentai.org",
        &[
            "94.100.18.243",
            "104.20.134.21",
            "104.20.135.21",
            "172.67.0.127",
        ],
    ),
    (
        "exhentai.org",
        &[
            "178.175.129.254",
            "178.175.128.252",
            "178.175.132.20",
            "178.175.129.252",
            "178.175.128.254",
            "178.175.132.22",
        ],
    ),
];
pub const QUOTA_EXCEEDED_CONTENT_LENGTHS: [u64; 5] = [925, 28658, 144, 210, 1009];

pub const XEH_STATE_RUNNING: i64 = 0;
pub const XEH_STATE_SOFT_EXIT: i64 = 1;
pub const XEH_STATE_FULL_EXIT: i64 = 2;
pub const XEH_STATE_CLEAN: i64 = 3;

pub const TASK_STATE_PAUSED: i64 = 0;
pub const TASK_STATE_WAITING: i64 = 1;
pub const TASK_STATE_GET_META: i64 = 2;
pub const TASK_STATE_SCAN_PAGE: i64 = 3;
pub const TASK_STATE_SCAN_IMG: i64 = 4;
pub const TASK_STATE_SCAN_ARCHIVE: i64 = 5;
pub const TASK_STATE_DOWNLOAD: i64 = 10;
pub const TASK_STATE_MAKE_ARCHIVE: i64 = 19;
pub const TASK_STATE_FINISHED: i64 = 20;
pub const TASK_STATE_FAILED: i64 = -1;

pub const ERR_NO_ERROR: i64 = 0;
pub const ERR_URL_NOT_RECOGNIZED: i64 = 1000;
pub const ERR_CANT_DOWNLOAD_EXH: i64 = 1001;
pub const ERR_ONLY_VISIBLE_EXH: i64 = 1002;
pub const ERR_MALFORMED_HATHDL: i64 = 1003;
pub const ERR_GALLERY_REMOVED: i64 = 1004;
pub const ERR_IMAGE_RESAMPLED: i64 = 1005;
pub const ERR_QUOTA_EXCEEDED: i64 = 1006;
pub const ERR_KEY_EXPIRED: i64 = 1007;
pub const ERR_NO_PAGEURL_FOUND: i64 = 1008;
pub const ERR_CONNECTION_ERROR: i64 = 1009;
pub const ERR_IP_BANNED: i64 = 1010;
pub const ERR_HATH_NOT_FOUND: i64 = 1011;
pub const ERR_IMAGE_BROKEN: i64 = 1012;
pub const ERR_SCAN_REGEX_FAILED: i64 = 1013;
pub const ERR_STREAM_NOT_IMPLEMENTED: i64 = 1014;
pub const ERR_TASK_NOT_FOUND: i64 = 1101;
pub const ERR_SAVE_SESSION_FAILED: i64 = 1103;
pub const ERR_TASK_LEVEL_UNDEF: i64 = 1104;
pub const ERR_DELETE_RUNNING_TASK: i64 = 1105;
pub const ERR_TASK_CANNOT_PAUSE: i64 = 1106;
pub const ERR_TASK_CANNOT_RESUME: i64 = 1107;
pub const ERR_RPC_UNAUTHORIZED: i64 = 1200;
pub const ERR_CANNOT_CREATE_DIR: i64 = 1300;
pub const ERR_CANNOT_MAKE_ARCHIVE: i64 = 1301;
pub const ERR_NOT_RANGE_FORMAT: i64 = 1302;
pub const ERR_RPC_PARSE_ERROR: i64 = -32700;
pub const ERR_RPC_INVALID_REQUEST: i64 = -32600;
pub const ERR_RPC_METHOD_NOT_FOUND: i64 = -32601;
pub const ERR_RPC_INVALID_PARAMS: i64 = -32602;
pub const ERR_RPC_EXEC_ERROR: i64 = -32603;

pub const ERR_NOMSG: &str = "undefined error message with code %d";

/// Symbolic translation key for an error code.
///
/// The reference implementation keys `err_msg` by code. The locale files keep
/// the symbolic names so the mapping stays readable and typo-checked.
pub fn err_key(code: i64) -> Option<&'static str> {
    Some(match code {
        ERR_URL_NOT_RECOGNIZED => "err_url_not_recognized",
        ERR_CANT_DOWNLOAD_EXH => "err_cant_download_exh",
        ERR_ONLY_VISIBLE_EXH => "err_only_visible_exh",
        ERR_MALFORMED_HATHDL => "err_malformed_hathdl",
        ERR_GALLERY_REMOVED => "err_gallery_removed",
        ERR_KEY_EXPIRED => "err_key_expired",
        ERR_NO_PAGEURL_FOUND => "err_no_pageurl_found",
        ERR_CONNECTION_ERROR => "err_connection_error",
        ERR_IP_BANNED => "err_ip_banned",
        ERR_IMAGE_BROKEN => "err_image_broken",
        ERR_SCAN_REGEX_FAILED => "err_scan_regex_failed",
        ERR_QUOTA_EXCEEDED => "err_quota_exceeded",
        ERR_TASK_NOT_FOUND => "err_task_not_found",
        ERR_TASK_LEVEL_UNDEF => "err_task_level_undef",
        ERR_DELETE_RUNNING_TASK => "err_delete_running_task",
        ERR_TASK_CANNOT_PAUSE => "err_task_cannot_pause",
        ERR_TASK_CANNOT_RESUME => "err_task_cannot_resume",
        ERR_CANNOT_CREATE_DIR => "err_cannot_create_dir",
        ERR_CANNOT_MAKE_ARCHIVE => "err_cannot_make_archive",
        ERR_NOT_RANGE_FORMAT => "err_not_range_format",
        ERR_RPC_PARSE_ERROR => "err_rpc_parse_error",
        ERR_RPC_INVALID_REQUEST => "err_rpc_invalid_request",
        ERR_RPC_METHOD_NOT_FOUND => "err_rpc_method_not_found",
        ERR_RPC_INVALID_PARAMS => "err_rpc_invalid_params",
        ERR_RPC_UNAUTHORIZED => "err_rpc_unauthorized",
        ERR_RPC_EXEC_ERROR => "err_rpc_exec_error",
        ERR_SAVE_SESSION_FAILED => "err_save_session_failed",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_states_match_reference_values() {
        let states = [
            (TASK_STATE_PAUSED, 0),
            (TASK_STATE_WAITING, 1),
            (TASK_STATE_GET_META, 2),
            (TASK_STATE_SCAN_PAGE, 3),
            (TASK_STATE_SCAN_IMG, 4),
            (TASK_STATE_SCAN_ARCHIVE, 5),
            (TASK_STATE_DOWNLOAD, 10),
            (TASK_STATE_MAKE_ARCHIVE, 19),
            (TASK_STATE_FINISHED, 20),
            (TASK_STATE_FAILED, -1),
        ];
        for (actual, expected) in states {
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn error_codes_match_reference_values() {
        let codes = [
            (ERR_NO_ERROR, 0),
            (ERR_URL_NOT_RECOGNIZED, 1000),
            (ERR_CANT_DOWNLOAD_EXH, 1001),
            (ERR_ONLY_VISIBLE_EXH, 1002),
            (ERR_MALFORMED_HATHDL, 1003),
            (ERR_GALLERY_REMOVED, 1004),
            (ERR_IMAGE_RESAMPLED, 1005),
            (ERR_QUOTA_EXCEEDED, 1006),
            (ERR_KEY_EXPIRED, 1007),
            (ERR_NO_PAGEURL_FOUND, 1008),
            (ERR_CONNECTION_ERROR, 1009),
            (ERR_IP_BANNED, 1010),
            (ERR_HATH_NOT_FOUND, 1011),
            (ERR_IMAGE_BROKEN, 1012),
            (ERR_SCAN_REGEX_FAILED, 1013),
            (ERR_STREAM_NOT_IMPLEMENTED, 1014),
            (ERR_TASK_NOT_FOUND, 1101),
            (ERR_SAVE_SESSION_FAILED, 1103),
            (ERR_TASK_LEVEL_UNDEF, 1104),
            (ERR_DELETE_RUNNING_TASK, 1105),
            (ERR_TASK_CANNOT_PAUSE, 1106),
            (ERR_TASK_CANNOT_RESUME, 1107),
            (ERR_RPC_UNAUTHORIZED, 1200),
            (ERR_CANNOT_CREATE_DIR, 1300),
            (ERR_CANNOT_MAKE_ARCHIVE, 1301),
            (ERR_NOT_RANGE_FORMAT, 1302),
            (ERR_RPC_PARSE_ERROR, -32700),
            (ERR_RPC_INVALID_REQUEST, -32600),
            (ERR_RPC_METHOD_NOT_FOUND, -32601),
            (ERR_RPC_INVALID_PARAMS, -32602),
            (ERR_RPC_EXEC_ERROR, -32603),
        ];
        for (actual, expected) in codes {
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn every_translatable_error_code_has_a_key_and_no_extra_codes() {
        let translatable = [
            ERR_URL_NOT_RECOGNIZED,
            ERR_CANT_DOWNLOAD_EXH,
            ERR_ONLY_VISIBLE_EXH,
            ERR_MALFORMED_HATHDL,
            ERR_GALLERY_REMOVED,
            ERR_KEY_EXPIRED,
            ERR_NO_PAGEURL_FOUND,
            ERR_CONNECTION_ERROR,
            ERR_IP_BANNED,
            ERR_IMAGE_BROKEN,
            ERR_SCAN_REGEX_FAILED,
            ERR_QUOTA_EXCEEDED,
            ERR_TASK_NOT_FOUND,
            ERR_TASK_LEVEL_UNDEF,
            ERR_DELETE_RUNNING_TASK,
            ERR_TASK_CANNOT_PAUSE,
            ERR_TASK_CANNOT_RESUME,
            ERR_CANNOT_CREATE_DIR,
            ERR_CANNOT_MAKE_ARCHIVE,
            ERR_NOT_RANGE_FORMAT,
            ERR_RPC_PARSE_ERROR,
            ERR_RPC_INVALID_REQUEST,
            ERR_RPC_METHOD_NOT_FOUND,
            ERR_RPC_INVALID_PARAMS,
            ERR_RPC_UNAUTHORIZED,
            ERR_RPC_EXEC_ERROR,
            ERR_SAVE_SESSION_FAILED,
        ];
        for code in translatable {
            assert!(err_key(code).is_some(), "missing key for {code}");
        }
        assert_eq!(err_key(ERR_NO_ERROR), None);
        assert_eq!(err_key(ERR_IMAGE_RESAMPLED), None);
        assert_eq!(err_key(ERR_STREAM_NOT_IMPLEMENTED), None);
        assert_eq!(err_key(424242), None);
    }

    #[test]
    fn version_is_printed_with_the_implementation_marker() {
        assert_eq!(VERSION, "2.023");
        assert_eq!(VERSION_DISPLAY, "2.023-rss");
    }
}
