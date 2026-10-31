//! Locale mapping, translation lookup, and error-message fallback.
//!
//! Translations are YAML resources loaded at compile time by `rust-i18n`.
//! Lookups always pass an explicit locale; the global locale is never changed.
//!
//! Message keys are exercised by this module and the compatibility oracle
//! before their CLI/RPC/task call sites land.
#![allow(dead_code)]

use crate::r#const as consts;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Locale {
    EnUs,
    ZhHans,
    ZhHant,
}

impl Locale {
    pub fn tag(self) -> &'static str {
        match self {
            Self::EnUs => "en_us",
            Self::ZhHans => "zh_hans",
            Self::ZhHant => "zh_hant",
        }
    }

    /// Reference mapping: `zh_CN/zh_SG -> zh_hans`,
    /// `zh_TW/zh_HK/zh_MO -> zh_hant`, unknown tags fall back to English.
    pub fn from_tag(tag: &str) -> Self {
        match tag.to_ascii_lowercase().as_str() {
            "zh_cn" | "zh_sg" | "zh_hans" => Self::ZhHans,
            "zh_tw" | "zh_hk" | "zh_mo" | "zh_hant" => Self::ZhHant,
            "en_us" => Self::EnUs,
            _ => Self::EnUs,
        }
    }
}

pub fn available_locales() -> Vec<String> {
    crate::_rust_i18n_available_locales()
        .into_iter()
        .map(|locale| locale.into_owned())
        .collect()
}

/// Raw localized template (placeholders are not substituted).
pub fn key(locale: Locale, name: &str) -> Option<String> {
    crate::_rust_i18n_try_translate(locale.tag(), name).map(|text| text.into_owned())
}

/// Localized template with positional `%{0}`, `%{1}`, ... substitution.
pub fn render(locale: Locale, name: &str, args: &[&str]) -> Option<String> {
    let template = key(locale, name)?;
    if args.is_empty() {
        return Some(template);
    }
    let patterns: Vec<String> = (0..args.len()).map(|index| index.to_string()).collect();
    let patterns: Vec<&str> = patterns.iter().map(String::as_str).collect();
    let values: Vec<String> = args.iter().map(|value| (*value).to_string()).collect();
    Some(rust_i18n::replace_patterns(&template, &patterns, &values))
}

/// Reference `i18n.c(code)`: localized `err_msg` entry, falling back to
/// the localized `ERR_NOMSG` template with the numeric code.
pub fn err_msg(locale: Locale, code: i64, args: &[&str]) -> String {
    if let Some(name) = consts::err_key(code) {
        let dotted = format!("err.{name}");
        if let Some(text) = render(locale, &dotted, args) {
            return text;
        }
    }
    let code_text = code.to_string();
    render(locale, "ERR_NOMSG", &[code_text.as_str()])
        .unwrap_or_else(|| format!("undefined error message with code {code}"))
}

/// Every translation key defined by the reference locale modules.
pub const REFERENCE_KEYS: [&str; 97] = [
    "ERR_NOMSG",
    "PROXY_CANDIDATE_CNT",
    "PROXY_DISABLE_BANNED",
    "PS_DOWNLOAD_DIR",
    "PS_DOWNLOAD_ORI",
    "PS_DOWNLOAD_RANGE",
    "PS_JPN_TITLE",
    "PS_LOGIN",
    "PS_MAKE_ARCHIVE",
    "PS_PASSWD",
    "PS_PROXY",
    "PS_RENAME_ORI",
    "PS_URL",
    "PS_USERNAME",
    "QUEUE",
    "RPC_CANNOT_BIND",
    "RPC_STARTED",
    "RPC_TOO_OPEN",
    "RPC_WEBUI_PATH",
    "SESSION_LOAD_EXCEPTION",
    "SESSION_WRITE_EXCEPTION",
    "TASK_ERROR",
    "TASK_FINISHED",
    "TASK_MAKE_ARCHIVE_FINISHED",
    "TASK_MIGRATE_EXH",
    "TASK_PUT_INTO_WAIT",
    "TASK_SLOW",
    "TASK_START",
    "TASK_START_MAKE_ARCHIVE",
    "TASK_START_PAGE_RESCAN",
    "TASK_STOP_QUOTA_EXCEEDED",
    "TASK_STUCK",
    "TASK_TITLE",
    "TASK_UNFINISHED",
    "TASK_WILL_DOWNLOAD_CNT",
    "THREAD",
    "THREAD_MAY_BECOME_ZOMBIE",
    "THREAD_SPEED_TOO_LOW",
    "THREAD_SWEEP_OUT",
    "THREAD_UNCAUGHT_EXCEPTION",
    "UPDATE_AVAILABLE",
    "UPDATE_CHANNEL",
    "UPDATE_COMPLETE",
    "UPDATE_DEV_CHANNEL",
    "UPDATE_DOWNLOAD_MANUALLY",
    "UPDATE_FAILED",
    "UPDATE_NO_UPDATE",
    "UPDATE_RELEASE_CHANNEL",
    "XEH_CLEANUP",
    "XEH_CRITICAL_ERROR",
    "XEH_DAEMON_START",
    "XEH_DOWNLOAD_HAS_ERROR",
    "XEH_DOWNLOAD_ORI_NEED_LOGIN",
    "XEH_FILE_DOWNLOADED",
    "XEH_LOAD_OLD_COOKIE",
    "XEH_LOAD_TASKS_CNT",
    "XEH_LOGIN_EXHENTAI",
    "XEH_LOGIN_FAILED",
    "XEH_LOGIN_OK",
    "XEH_LOOP_FINISHED",
    "XEH_OPT_IGNORING_I",
    "XEH_OPT_a",
    "XEH_OPT_c",
    "XEH_OPT_auto_update",
    "XEH_OPT_d",
    "XEH_OPT_daemon",
    "XEH_OPT_delete_task_files",
    "XEH_OPT_download_range",
    "XEH_OPT_f",
    "XEH_OPT_h",
    "XEH_OPT_i",
    "XEH_OPT_j",
    "XEH_OPT_k",
    "XEH_OPT_l",
    "XEH_OPT_low_speed",
    "XEH_OPT_o",
    "XEH_OPT_p",
    "XEH_OPT_proxy_image",
    "XEH_OPT_proxy_image_only",
    "XEH_OPT_r",
    "XEH_OPT_rpc_interface",
    "XEH_OPT_rpc_open_browser",
    "XEH_OPT_rpc_port",
    "XEH_OPT_rpc_secret",
    "XEH_OPT_t",
    "XEH_OPT_timeout",
    "XEH_OPT_u",
    "XEH_OPT_update_beta_channel",
    "XEH_OPT_URLS",
    "XEH_OPT_v",
    "XEH_OPT_version",
    "XEH_OPT_DESC",
    "XEH_OPT_EPILOG",
    "XEH_PLATFORM_NO_DAEMON",
    "XEH_RENAME_HAS_ERRORS",
    "XEH_SCAN_FAILED",
    "XEH_STARTED",
];

/// Translation keys for the reference `err_msg` map (27 codes).
pub const ERROR_KEYS: [&str; 27] = [
    "err_cannot_create_dir",
    "err_cannot_make_archive",
    "err_cant_download_exh",
    "err_connection_error",
    "err_delete_running_task",
    "err_gallery_removed",
    "err_image_broken",
    "err_ip_banned",
    "err_key_expired",
    "err_malformed_hathdl",
    "err_no_pageurl_found",
    "err_not_range_format",
    "err_only_visible_exh",
    "err_quota_exceeded",
    "err_rpc_exec_error",
    "err_rpc_invalid_params",
    "err_rpc_invalid_request",
    "err_rpc_method_not_found",
    "err_rpc_parse_error",
    "err_rpc_unauthorized",
    "err_save_session_failed",
    "err_scan_regex_failed",
    "err_task_cannot_pause",
    "err_task_cannot_resume",
    "err_task_level_undef",
    "err_task_not_found",
    "err_url_not_recognized",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_tags_map_like_the_reference() {
        let cases = [
            ("zh_CN", Locale::ZhHans),
            ("zh_SG", Locale::ZhHans),
            ("zh_Hans", Locale::ZhHans),
            ("zh_TW", Locale::ZhHant),
            ("zh_HK", Locale::ZhHant),
            ("zh_MO", Locale::ZhHant),
            ("zh_Hant", Locale::ZhHant),
            ("en_US", Locale::EnUs),
            ("en_GB", Locale::EnUs),
            ("zh", Locale::EnUs),
            ("", Locale::EnUs),
            ("C", Locale::EnUs),
        ];
        for (tag, expected) in cases {
            assert_eq!(Locale::from_tag(tag), expected, "tag {tag}");
        }
    }

    #[test]
    fn every_reference_key_exists_in_all_three_locales() {
        for name in REFERENCE_KEYS {
            for locale in [Locale::EnUs, Locale::ZhHans, Locale::ZhHant] {
                assert!(
                    key(locale, name).is_some(),
                    "missing {name} for {}",
                    locale.tag()
                );
            }
        }
        for name in ERROR_KEYS {
            for locale in [Locale::EnUs, Locale::ZhHans, Locale::ZhHant] {
                let dotted = format!("err.{name}");
                assert!(
                    key(locale, &dotted).is_some(),
                    "missing {dotted} for {}",
                    locale.tag()
                );
            }
        }
    }

    #[test]
    fn only_the_declared_locales_are_available() {
        assert_eq!(available_locales(), ["en_us", "zh_hans", "zh_hant"]);
    }

    #[test]
    fn messages_render_in_each_language() {
        assert_eq!(
            render(Locale::EnUs, "XEH_STARTED", &["2.023"]).unwrap(),
            "xeHentai 2.023 started."
        );
        assert_eq!(
            render(Locale::ZhHans, "XEH_STARTED", &["2.023"]).unwrap(),
            "xeHentai 2.023 已启动"
        );
        assert_eq!(
            render(Locale::ZhHant, "XEH_STARTED", &["2.023"]).unwrap(),
            "xeHentai 2.023 已啟動"
        );
        assert_eq!(
            render(
                Locale::ZhHans,
                "TASK_MAKE_ARCHIVE_FINISHED",
                &["7", "/tmp/a.zip", "1.5"]
            )
            .unwrap(),
            "任务 #7 打包完成，保存在: /tmp/a.zip, 用时1.5秒"
        );
    }

    #[test]
    fn error_code_messages_and_numeric_fallback_match() {
        assert_eq!(
            err_msg(Locale::EnUs, consts::ERR_IP_BANNED, &["5 minutes"]),
            "IP has been banned, retry in 5 minutes"
        );
        assert_eq!(
            err_msg(Locale::ZhHans, consts::ERR_IP_BANNED, &["5 minutes"]),
            "IP被ban了, 恢复时间: 5 minutes"
        );
        assert_eq!(
            err_msg(Locale::ZhHant, consts::ERR_NOT_RANGE_FORMAT, &["9-1"]),
            "'9-1'不符合範圍的格式, 正確的格式為 1-3 或者 5"
        );
        assert_eq!(
            err_msg(Locale::EnUs, 424242, &[]),
            "undefined error message with code 424242"
        );
        assert_eq!(
            err_msg(Locale::ZhHans, 424242, &[]),
            "未指定的错误，错误号 424242"
        );
        assert_eq!(err_msg(Locale::ZhHant, -1, &[]), "未指定的錯誤，錯誤號 -1");
    }

    #[test]
    fn missing_keys_fall_back_to_english_then_none() {
        assert_eq!(
            render(Locale::ZhHans, "z_test_fallback_probe", &[]).as_deref(),
            Some("english fallback probe")
        );
        assert_eq!(key(Locale::EnUs, "no_such_key"), None);
        assert_eq!(key(Locale::ZhHant, "no_such_key"), None);
    }

    #[test]
    fn concurrent_lookups_do_not_share_locale_state() {
        let expectations = [
            (Locale::EnUs, "task #1 error: boom"),
            (Locale::ZhHans, "任务 #1 发生错误: boom"),
            (Locale::ZhHant, "任務 #1 發生錯誤: boom"),
        ];
        std::thread::scope(|scope| {
            for (locale, expected) in expectations {
                for _ in 0..4 {
                    scope.spawn(move || {
                        for _ in 0..200 {
                            let rendered = render(locale, "TASK_ERROR", &["1", "boom"]).unwrap();
                            assert_eq!(rendered, expected);
                            let error = err_msg(locale, consts::ERR_CONNECTION_ERROR, &[]);
                            assert!(!error.is_empty());
                        }
                    });
                }
            }
        });
    }
}
