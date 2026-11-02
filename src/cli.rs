//! Reference `xeHentai/cli.py`: argument parsing and the interactive prompts.
//!
//! Tokenization, aliases, repeated/append flags, required values, help and
//! version are owned by the mature `clap` crate. Only the three reference
//! behaviours clap does not express directly are adapted here:
//!
//! * `type=bool` options (`-j`, `-r`, `-a`, `--delete-task-files`,
//!   `--rpc-open-browser`, `--update-beta-channel`) take a *value string* and
//!   apply the reference `bool(value)`: any non-empty text is true, the empty
//!   string is false. `-j False` is therefore `true`.
//! * `-f/--force` uses a custom count action whose `default` is `None`: absent
//!   means `ignored_errors = None` (the file value is *not* inherited), each
//!   occurrence appends the quota-exceeded code, and repeats are kept.
//! * `-p/--proxy` appends the command-line entries to the configured list, and
//!   `-v/--verbose` counts occurrences on top of the configured verbosity.
//!
//! `_parse_range` keeps the reference regex grammar (Chinese comma, single
//! page, closed range, token stripping, sorting, reversed/zero/overlapping
//! ranges left alone) and the reference's Unicode decimal digits.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{Arg, ArgAction, ArgMatches, Command};
use regex::Regex;

use crate::config::Config;
use tokio::io::{AsyncBufReadExt, BufReader, Stdin};

use crate::r#const as consts;
use crate::core::App;
use crate::i18n::{self, Locale};

/// One parsed command line: the reference `argparse.Namespace` projection.
///
/// Key names and `None`/empty distinctions match the reference namespace, so
/// the compatibility probe can compare them field by field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Opts {
    pub username: Option<String>,
    pub key: Option<String>,
    pub cookie: Option<String>,
    pub interactive: bool,
    pub daemon: bool,
    pub urls: Vec<String>,
    pub dir: String,
    pub download_ori: bool,
    pub jpn_title: bool,
    pub rename_ori: bool,
    pub proxy: Vec<String>,
    pub proxy_image: bool,
    pub proxy_image_only: bool,
    pub rpc_interface: String,
    pub rpc_port: Option<i64>,
    pub rpc_secret: Option<String>,
    pub rpc_open_browser: bool,
    pub delete_task_files: bool,
    pub make_archive: bool,
    pub download_range: Option<Vec<[i64; 2]>>,
    pub download_thread_cnt: i64,
    pub download_timeout: i64,
    pub low_speed_threshold: i64,
    /// `None` when `-f` was absent, the quota-exceeded codes when present.
    pub ignored_errors: Option<Vec<i64>>,
    pub auto_update: String,
    pub update_beta_channel: bool,
    pub log_path: String,
    /// The reference namespace name for `-v`: `verbose`, not `log_verbose`.
    pub verbose: i64,
}

/// Result of parsing a real command line.
///
/// The parse happens once per process, so keeping the whole namespace inline
/// is cheaper than an indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum Parsed {
    Help(String),
    Version(String),
    Opts(Opts),
}

/// A real usage error. The rendered text already contains the usage block and
/// the reference exit status is 2.
#[derive(Debug)]
pub struct UsageError {
    pub message: String,
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// Reference `type=bool`: the raw argument string's truthiness, not a keyword.
pub fn py_bool(text: &str) -> bool {
    !text.is_empty()
}

/// Format a value the way the reference help renders `(current: %s)`.
fn py_repr(text: &str) -> String {
    text.to_string()
}

fn py_repr_bool(value: bool) -> String {
    if value {
        "True".to_string()
    } else {
        "False".to_string()
    }
}

fn py_repr_list(values: &[String]) -> String {
    if values.is_empty() {
        return "[]".to_string();
    }
    let items: Vec<String> = values
        .iter()
        .map(|value| format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'")))
        .collect();
    format!("[{}]", items.join(", "))
}

fn py_repr_opt_int(value: Option<i64>) -> String {
    match value {
        Some(number) => number.to_string(),
        None => "None".to_string(),
    }
}

fn py_repr_opt_str(value: Option<&str>) -> String {
    match value {
        Some(text) => py_repr(text),
        None => "None".to_string(),
    }
}

/// Render one localized help line with its current value.
fn option_help(locale: Locale, name: &str, current: &str) -> String {
    i18n::render(locale, name, &[current]).unwrap_or_else(|| name.to_string())
}

/// The reference `logger.safestr` keeps the text usable on the console; the
/// Rust strings are already valid UTF-8.
fn prompt(locale: Locale, name: &str, args: &[&str]) -> String {
    i18n::render(locale, name, args).unwrap_or_else(|| name.to_string())
}

// MARK: - Unicode decimal digits

/// Unicode `Nd` digit runs (`[start, start+9]`), Unicode 16.0.
///
/// `_parse_range` captures with `\d`, and CPython's `int()` accepts every Nd
/// code point. The table supplies the digit value for those runs; it is data,
/// not a second number parser.
const ND_RUNS: [(u32, u32); 76] = [
    (0x0030, 0x0039),
    (0x0660, 0x0669),
    (0x06F0, 0x06F9),
    (0x07C0, 0x07C9),
    (0x0966, 0x096F),
    (0x09E6, 0x09EF),
    (0x0A66, 0x0A6F),
    (0x0AE6, 0x0AEF),
    (0x0B66, 0x0B6F),
    (0x0BE6, 0x0BEF),
    (0x0C66, 0x0C6F),
    (0x0CE6, 0x0CEF),
    (0x0D66, 0x0D6F),
    (0x0DE6, 0x0DEF),
    (0x0E50, 0x0E59),
    (0x0ED0, 0x0ED9),
    (0x0F20, 0x0F29),
    (0x1040, 0x1049),
    (0x1090, 0x1099),
    (0x17E0, 0x17E9),
    (0x1810, 0x1819),
    (0x1946, 0x194F),
    (0x19D0, 0x19D9),
    (0x1A80, 0x1A89),
    (0x1A90, 0x1A99),
    (0x1B50, 0x1B59),
    (0x1BB0, 0x1BB9),
    (0x1C40, 0x1C49),
    (0x1C50, 0x1C59),
    (0xA620, 0xA629),
    (0xA8D0, 0xA8D9),
    (0xA900, 0xA909),
    (0xA9D0, 0xA9D9),
    (0xA9F0, 0xA9F9),
    (0xAA50, 0xAA59),
    (0xABF0, 0xABF9),
    (0xFF10, 0xFF19),
    (0x104A0, 0x104A9),
    (0x10D30, 0x10D39),
    (0x10D40, 0x10D49),
    (0x11066, 0x1106F),
    (0x110F0, 0x110F9),
    (0x11136, 0x1113F),
    (0x111D0, 0x111D9),
    (0x112F0, 0x112F9),
    (0x11450, 0x11459),
    (0x114D0, 0x114D9),
    (0x11650, 0x11659),
    (0x116C0, 0x116C9),
    (0x116D0, 0x116D9),
    (0x116DA, 0x116E3),
    (0x11730, 0x11739),
    (0x118E0, 0x118E9),
    (0x11950, 0x11959),
    (0x11BF0, 0x11BF9),
    (0x11C50, 0x11C59),
    (0x11D50, 0x11D59),
    (0x11DA0, 0x11DA9),
    (0x11F50, 0x11F59),
    (0x16130, 0x16139),
    (0x16A60, 0x16A69),
    (0x16AC0, 0x16AC9),
    (0x16B50, 0x16B59),
    (0x16D70, 0x16D79),
    (0x1CCF0, 0x1CCF9),
    (0x1D7CE, 0x1D7D7),
    (0x1D7D8, 0x1D7E1),
    (0x1D7E2, 0x1D7EB),
    (0x1D7EC, 0x1D7F5),
    (0x1D7F6, 0x1D7FF),
    (0x1E140, 0x1E149),
    (0x1E2F0, 0x1E2F9),
    (0x1E4F0, 0x1E4F9),
    (0x1E5F1, 0x1E5FA),
    (0x1E950, 0x1E959),
    (0x1FBF0, 0x1FBF9),
];

fn decimal_digit(character: char) -> Option<u32> {
    let code = character as u32;
    ND_RUNS
        .iter()
        .find(|(start, end)| code >= *start && code <= *end)
        .map(|(start, _)| code - start)
}

/// Decimal value of a `\d+` capture. `None` reports an out-of-range number.
fn decimal_value(text: &str) -> Option<i64> {
    let mut value: i128 = 0;
    for character in text.chars() {
        let digit = decimal_digit(character)?;
        value = value.checked_mul(10)?.checked_add(i128::from(digit))?;
        if value > i128::from(i64::MAX) {
            return None;
        }
    }
    i64::try_from(value).ok()
}

/// Reference `_parse_range`: Chinese comma, per-token strip, `a` or `a-b`,
/// then plain tuple sort.
pub fn parse_range(text: &str, locale: Locale) -> Result<Vec<[i64; 2]>, String> {
    static RANGE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"^(\d+)(?:-(\d+))?$").expect("range grammar"));
    let normalized = text.replace('，', ",");
    let mut ranges = Vec::new();
    for token in normalized.split(',') {
        let token = token.trim();
        let Some(captures) = RANGE.captures(token) else {
            return Err(range_error(locale, token));
        };
        let start = decimal_value(captures.get(1).expect("group 1").as_str());
        let end = match captures.get(2) {
            Some(group) => decimal_value(group.as_str()),
            None => start,
        };
        let (Some(start), Some(end)) = (start, end) else {
            return Err(range_error(locale, token));
        };
        ranges.push([start, end]);
    }
    ranges.sort();
    Ok(ranges)
}

fn range_error(locale: Locale, token: &str) -> String {
    i18n::err_msg(locale, consts::ERR_NOT_RANGE_FORMAT, &[token])
}

// MARK: - Interactive prompts

/// Answers returned by the reference `interactive()`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InteractiveResult {
    pub urls: Vec<String>,
    pub proxy: Vec<String>,
    pub download_ori: bool,
    pub dir: String,
    pub rename_ori: bool,
    pub make_archive: bool,
    pub jpn_title: bool,
    pub save_tasks: bool,
    pub download_range: Vec<[i64; 2]>,
}

impl InteractiveResult {
    /// Reference `opt.__dict__.update(r)` + `update_config(**r)`.
    pub fn patch(&self) -> serde_json::Map<String, serde_json::Value> {
        let mut patch = serde_json::Map::new();
        patch.insert("proxy".into(), serde_json::json!(self.proxy));
        patch.insert("download_ori".into(), serde_json::json!(self.download_ori));
        patch.insert("dir".into(), serde_json::json!(self.dir));
        patch.insert("rename_ori".into(), serde_json::json!(self.rename_ori));
        patch.insert("make_archive".into(), serde_json::json!(self.make_archive));
        patch.insert("jpn_title".into(), serde_json::json!(self.jpn_title));
        patch.insert("save_tasks".into(), serde_json::json!(self.save_tasks));
        patch.insert(
            "download_range".into(),
            serde_json::json!(self.download_range),
        );
        patch
    }
}

/// Why a prompt stopped before an answer arrived.
#[derive(Debug)]
pub enum PromptError {
    Interrupted,
    Io(std::io::Error),
}

impl std::fmt::Display for PromptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Interrupted => formatter.write_str("interrupted"),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

fn write_prompt(text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut stdout = std::io::stdout();
    stdout.write_all(text.as_bytes())?;
    stdout.flush()
}

/// Reference `_readline`: print the prompt, read one line, `_ or default`.
async fn readline(
    stdin: &mut BufReader<Stdin>,
    prompt_text: &str,
    default: Option<&str>,
    interrupt: &mut crate::SignalStream,
) -> Result<String, PromptError> {
    write_prompt(prompt_text).map_err(PromptError::Io)?;
    let mut buffer = String::new();
    let read = stdin.read_line(&mut buffer);
    tokio::pin!(read);
    tokio::select! {
        result = &mut read => {
            let read = result.map_err(PromptError::Io)?;
            if read == 0 {
                return Err(PromptError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "end of input",
                )));
            }
        }
        _ = interrupt.recv() => return Err(PromptError::Interrupted),
    }
    if buffer.ends_with('\n') {
        buffer.pop();
        if buffer.ends_with('\r') {
            buffer.pop();
        }
    }
    if buffer.is_empty() {
        return Ok(default.unwrap_or("").to_string());
    }
    Ok(buffer)
}

/// Reference `interactive(xeH)`. Returns `None` when the prompt was cancelled
/// before any answer would be applied (Ctrl-C inside the prompt).
pub async fn interactive(
    app: &mut App,
    app_filters: &mut crate::filters::Filters,
    locale: Locale,
    interrupt: &mut crate::SignalStream,
) -> Result<Option<InteractiveResult>, PromptError> {
    let interrupt_shared = interrupt;
    // One buffered reader for the whole prompt sequence: a scripted writer may
    // deliver several answers at once and must not lose the extra lines.
    let mut stdin = BufReader::new(tokio::io::stdin());
    if !app.has_login() {
        let answer = readline(
            &mut stdin,
            &prompt(locale, "PS_LOGIN", &[]),
            None,
            interrupt_shared,
        )
        .await?;
        if answer == "y" {
            let prompt_username = prompt(locale, "PS_USERNAME", &[]);
            let mut username = String::new();
            while username.is_empty() {
                username = readline(&mut stdin, &prompt_username, None, interrupt_shared).await?;
            }
            let prompt_password = prompt(locale, "PS_PASSWD", &[]);
            let mut password = String::new();
            while password.is_empty() {
                password = readline(&mut stdin, &prompt_password, None, interrupt_shared).await?;
            }
            if let Err(error) = app.login_exhentai(app_filters, &username, &password).await {
                let _ = app.logger_mut().error(&error.to_string());
            }
        }
    }
    let prompt_url = prompt(locale, "PS_URL", &[]);
    let mut url = String::new();
    while url.is_empty() {
        url = readline(&mut stdin, &prompt_url, None, interrupt_shared).await?;
    }
    let urls: Vec<String> = url.split(',').map(str::to_string).collect();
    let download_ori_default = if app.cfg.download_ori { "y" } else { "n" };
    let download_ori = readline(
        &mut stdin,
        &prompt(locale, "PS_DOWNLOAD_ORI", &[download_ori_default]),
        Some(download_ori_default),
        interrupt_shared,
    )
    .await?
        == "y";
    let entered_proxy = readline(
        &mut stdin,
        &prompt(locale, "PS_PROXY", &[]),
        None,
        interrupt_shared,
    )
    .await?
    .trim()
    .to_string();
    let proxy = if entered_proxy.is_empty() {
        app.cfg.proxy.clone()
    } else {
        vec![entered_proxy]
    };
    let default_dir = abspath(Path::new(&app.cfg.dir));
    let dir_prompt = prompt(locale, "PS_DOWNLOAD_DIR", &[&default_dir]);
    let dir = readline(
        &mut stdin,
        &dir_prompt,
        Some(&app.cfg.dir),
        interrupt_shared,
    )
    .await?;
    let rename_default = if app.cfg.rename_ori { "y" } else { "n" };
    let rename_ori = readline(
        &mut stdin,
        &prompt(locale, "PS_RENAME_ORI", &[rename_default]),
        Some(rename_default),
        interrupt_shared,
    )
    .await?
        == "y";
    let archive_default = if app.cfg.make_archive { "y" } else { "n" };
    let make_archive = readline(
        &mut stdin,
        &prompt(locale, "PS_MAKE_ARCHIVE", &[archive_default]),
        Some(archive_default),
        interrupt_shared,
    )
    .await?
        == "y";
    let jpn_default = if app.cfg.jpn_title { "y" } else { "n" };
    let jpn_title = readline(
        &mut stdin,
        &prompt(locale, "PS_JPN_TITLE", &[jpn_default]),
        Some(jpn_default),
        interrupt_shared,
    )
    .await?
        == "y";
    let range_prompt = prompt(locale, "PS_DOWNLOAD_RANGE", &[]);
    let download_range = loop {
        let entered = readline(&mut stdin, &range_prompt, None, interrupt_shared).await?;
        if entered.is_empty() {
            break Vec::new();
        }
        match parse_range(&entered, locale) {
            Ok(ranges) => break ranges,
            Err(message) => {
                use std::io::Write;
                let mut stdout = std::io::stdout();
                let _ = writeln!(stdout, "{message}");
                let _ = stdout.flush();
            }
        }
    };
    Ok(Some(InteractiveResult {
        urls,
        proxy,
        download_ori,
        dir,
        rename_ori,
        make_archive,
        jpn_title,
        save_tasks: false,
        download_range,
    }))
}

// MARK: - Command definition

/// Build the reference parser. Every help string comes from the locale files;
/// `(current: ...)` shows the configured value like the reference.
pub fn command(program: &str, defaults: &Config, locale: Locale) -> Command {
    let dir_current = abspath(Path::new(&defaults.dir));
    let log_current = abspath(Path::new(&defaults.log_path));
    let force_current = "None";
    Command::new(program.to_string())
        .about(option_help(locale, "XEH_OPT_DESC", ""))
        .after_help(option_help(locale, "XEH_OPT_EPILOG", ""))
        .disable_help_flag(true)
        .disable_version_flag(true)
        .arg(
            Arg::new("username")
                .short('u')
                .long("username")
                .value_name("USERNAME")
                .num_args(1)
                .help(option_help(locale, "XEH_OPT_u", "")),
        )
        .arg(
            Arg::new("key")
                .short('k')
                .long("key")
                .value_name("KEY")
                .num_args(1)
                .help(option_help(locale, "XEH_OPT_k", "")),
        )
        .arg(
            Arg::new("cookie")
                .short('c')
                .long("cookie")
                .value_name("COOKIE")
                .num_args(1)
                .help(option_help(locale, "XEH_OPT_c", "")),
        )
        .arg(
            Arg::new("interactive")
                .short('i')
                .long("interactive")
                .action(ArgAction::SetTrue)
                .help(option_help(locale, "XEH_OPT_i", &py_repr_bool(false))),
        )
        .arg(
            Arg::new("daemon")
                .long("daemon")
                .action(ArgAction::SetTrue)
                .help(option_help(
                    locale,
                    "XEH_OPT_daemon",
                    &py_repr_bool(defaults.daemon),
                )),
        )
        .arg(
            Arg::new("urls")
                .value_name("url")
                .num_args(0..)
                .action(ArgAction::Append)
                .help(option_help(locale, "XEH_OPT_URLS", "")),
        )
        .arg(
            Arg::new("dir")
                .short('d')
                .long("dir")
                .value_name("DIR")
                .num_args(1)
                .help(option_help(locale, "XEH_OPT_d", &dir_current)),
        )
        .arg(
            Arg::new("download_ori")
                .short('o')
                .long("download-ori")
                .action(ArgAction::SetTrue)
                .help(option_help(
                    locale,
                    "XEH_OPT_o",
                    &py_repr_bool(defaults.download_ori),
                )),
        )
        .arg(
            Arg::new("jpn_title")
                .short('j')
                .long("jpn-title")
                .value_name("BOOL")
                .num_args(1)
                .value_parser(|text: &str| Ok::<bool, String>(py_bool(text)))
                .help(option_help(
                    locale,
                    "XEH_OPT_j",
                    &py_repr_bool(defaults.jpn_title),
                )),
        )
        .arg(
            Arg::new("rename_ori")
                .short('r')
                .long("rename-ori")
                .value_name("BOOL")
                .num_args(1)
                .value_parser(|text: &str| Ok::<bool, String>(py_bool(text)))
                .help(option_help(
                    locale,
                    "XEH_OPT_r",
                    &py_repr_bool(defaults.rename_ori),
                )),
        )
        .arg(
            Arg::new("proxy")
                .short('p')
                .long("proxy")
                .value_name("PROXY")
                .num_args(1)
                .action(ArgAction::Append)
                .help(option_help(
                    locale,
                    "XEH_OPT_p",
                    &py_repr_list(&defaults.proxy),
                )),
        )
        .arg(
            Arg::new("proxy_image")
                .long("proxy-image")
                .action(ArgAction::SetTrue)
                .conflicts_with("proxy_image_only")
                .help(option_help(
                    locale,
                    "XEH_OPT_proxy_image",
                    &py_repr_bool(defaults.proxy_image),
                )),
        )
        .arg(
            Arg::new("proxy_image_only")
                .long("proxy-image-only")
                .action(ArgAction::SetTrue)
                .conflicts_with("proxy_image")
                .help(option_help(
                    locale,
                    "XEH_OPT_proxy_image_only",
                    &py_repr_bool(defaults.proxy_image_only),
                )),
        )
        .arg(
            Arg::new("rpc_interface")
                .long("rpc-interface")
                .value_name("ADDR")
                .num_args(1)
                .help(option_help(
                    locale,
                    "XEH_OPT_rpc_interface",
                    &py_repr(&defaults.rpc_interface),
                )),
        )
        .arg(
            Arg::new("rpc_port")
                .long("rpc-port")
                .value_name("PORT")
                .num_args(1)
                .value_parser(clap::value_parser!(i64))
                .help(option_help(
                    locale,
                    "XEH_OPT_rpc_port",
                    &py_repr_opt_int(defaults.rpc_port),
                )),
        )
        .arg(
            Arg::new("rpc_secret")
                .long("rpc-secret")
                .value_name("...")
                .num_args(1)
                .help(option_help(
                    locale,
                    "XEH_OPT_rpc_secret",
                    &py_repr_opt_str(defaults.rpc_secret.as_deref()),
                )),
        )
        .arg(
            Arg::new("rpc_open_browser")
                .long("rpc-open-browser")
                .value_name("BOOL")
                .num_args(1)
                .value_parser(|text: &str| Ok::<bool, String>(py_bool(text)))
                .help(option_help(
                    locale,
                    "XEH_OPT_rpc_open_browser",
                    &py_repr_bool(defaults.rpc_open_browser),
                )),
        )
        .arg(
            Arg::new("delete_task_files")
                .long("delete-task-files")
                .value_name("BOOL")
                .num_args(1)
                .value_parser(|text: &str| Ok::<bool, String>(py_bool(text)))
                .help(option_help(
                    locale,
                    "XEH_OPT_delete_task_files",
                    &py_repr_bool(defaults.delete_task_files),
                )),
        )
        .arg(
            Arg::new("make_archive")
                .short('a')
                .long("archive")
                .value_name("BOOL")
                .num_args(1)
                .value_parser(|text: &str| Ok::<bool, String>(py_bool(text)))
                .help(option_help(
                    locale,
                    "XEH_OPT_a",
                    &py_repr_bool(defaults.make_archive),
                )),
        )
        .arg(
            Arg::new("download_range")
                .long("download-range")
                .value_name("a-b,c-d,e")
                .num_args(1)
                .value_parser(move |text: &str| parse_range(text, locale))
                .help(option_help(locale, "XEH_OPT_download_range", "")),
        )
        .arg(
            Arg::new("download_thread_cnt")
                .short('t')
                .long("thread")
                .value_name("N")
                .num_args(1)
                .value_parser(clap::value_parser!(i64))
                .help(option_help(
                    locale,
                    "XEH_OPT_t",
                    &defaults.download_thread_cnt.to_string(),
                )),
        )
        .arg(
            Arg::new("download_timeout")
                .long("timeout")
                .value_name("N")
                .num_args(1)
                .value_parser(clap::value_parser!(i64))
                .help(option_help(
                    locale,
                    "XEH_OPT_timeout",
                    &defaults.download_timeout.to_string(),
                )),
        )
        .arg(
            Arg::new("low_speed_threshold")
                .long("low-speed-threshold")
                .value_name("N")
                .num_args(1)
                .value_parser(clap::value_parser!(i64))
                .help(option_help(
                    locale,
                    "XEH_OPT_low_speed",
                    &defaults.low_speed_threshold.to_string(),
                )),
        )
        .arg(
            Arg::new("force")
                .short('f')
                .long("force")
                .action(ArgAction::Count)
                .help(option_help(locale, "XEH_OPT_f", force_current)),
        )
        .arg(
            Arg::new("auto_update")
                .long("auto-update")
                .value_name("{check,download,off}")
                .num_args(1)
                .value_parser(["check", "download", "off"])
                .help(option_help(
                    locale,
                    "XEH_OPT_auto_update",
                    &defaults.auto_update,
                )),
        )
        .arg(
            Arg::new("update_beta_channel")
                .long("update-beta-channel")
                .value_name("BOOL")
                .num_args(1)
                .value_parser(|text: &str| Ok::<bool, String>(py_bool(text)))
                .help(option_help(
                    locale,
                    "XEH_OPT_update_beta_channel",
                    &py_repr_bool(defaults.update_beta_channel),
                )),
        )
        .arg(
            Arg::new("log_path")
                .short('l')
                .long("logpath")
                .value_name("/path/to/eh.log")
                .num_args(1)
                .help(option_help(locale, "XEH_OPT_l", &log_current)),
        )
        .arg(
            Arg::new("verbose")
                .short('v')
                .long("verbose")
                .action(ArgAction::Count)
                .help(option_help(
                    locale,
                    "XEH_OPT_v",
                    &defaults.log_verbose.to_string(),
                )),
        )
        .arg(
            Arg::new("help")
                .short('h')
                .long("help")
                .action(ArgAction::Help)
                .help(option_help(locale, "XEH_OPT_h", "")),
        )
        .arg(
            Arg::new("version")
                .long("version")
                .action(ArgAction::SetTrue)
                .help(option_help(locale, "XEH_OPT_version", "")),
        )
}

/// Reference `os.path.abspath`: resolve against the process cwd without
/// following symlinks and normalize `.`/`..` lexically.
pub fn abspath(path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    lexically_normal(&absolute).to_string_lossy().into_owned()
}

fn lexically_normal(path: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(name) => out.push(name),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(Path::new(std::path::MAIN_SEPARATOR_STR));
    }
    out
}

// MARK: - Parsing

/// Parse a full argv (`argv[0]` is the program name, as in the reference).
pub fn parse_from<I, T>(argv: I, defaults: &Config, locale: Locale) -> Result<Parsed, UsageError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let argv: Vec<OsString> = argv.into_iter().map(Into::into).collect();
    let program = argv
        .first()
        .map(|first| {
            Path::new(first)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| consts::SCRIPT_NAME.to_string())
        })
        .unwrap_or_else(|| consts::SCRIPT_NAME.to_string());
    let command = command(&program, defaults, locale);
    match command.try_get_matches_from(argv) {
        Ok(matches) if matches.get_flag("version") => Ok(Parsed::Version(format!(
            "{} v{}\n",
            consts::SCRIPT_NAME,
            consts::VERSION_DISPLAY
        ))),
        Ok(matches) => Ok(Parsed::Opts(project(&matches, defaults))),
        Err(error) => match error.kind() {
            clap::error::ErrorKind::DisplayHelp => Ok(Parsed::Help(error.to_string())),
            clap::error::ErrorKind::DisplayVersion => Ok(Parsed::Version(error.to_string())),
            _ => Err(UsageError {
                message: error.to_string(),
            }),
        },
    }
}

fn text(matches: &ArgMatches, name: &str) -> Option<String> {
    matches.get_one::<String>(name).cloned()
}

fn flag(matches: &ArgMatches, name: &str, default: bool) -> bool {
    if matches.get_flag(name) {
        true
    } else {
        default
    }
}

fn project(matches: &ArgMatches, defaults: &Config) -> Opts {
    let cli_proxy: Vec<String> = matches
        .get_many::<String>("proxy")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    let mut proxy = defaults.proxy.clone();
    proxy.extend(cli_proxy);
    let force = matches.get_count("force");
    let ignored_errors = if force == 0 {
        None
    } else {
        Some(vec![consts::ERR_QUOTA_EXCEEDED; force as usize])
    };
    let urls: Vec<String> = matches
        .get_many::<String>("urls")
        .map(|values| values.cloned().collect())
        .unwrap_or_default();
    Opts {
        username: text(matches, "username"),
        key: text(matches, "key"),
        cookie: text(matches, "cookie"),
        interactive: matches.get_flag("interactive"),
        daemon: flag(matches, "daemon", defaults.daemon),
        urls,
        // The reference computes the default with `os.path.abspath`, but keeps
        // an explicitly supplied relative value untouched.
        dir: text(matches, "dir").unwrap_or_else(|| abspath(Path::new(&defaults.dir))),
        download_ori: flag(matches, "download_ori", defaults.download_ori),
        jpn_title: matches
            .get_one::<bool>("jpn_title")
            .copied()
            .unwrap_or(defaults.jpn_title),
        rename_ori: matches
            .get_one::<bool>("rename_ori")
            .copied()
            .unwrap_or(defaults.rename_ori),
        proxy,
        proxy_image: flag(matches, "proxy_image", defaults.proxy_image),
        proxy_image_only: flag(matches, "proxy_image_only", defaults.proxy_image_only),
        rpc_interface: text(matches, "rpc_interface")
            .unwrap_or_else(|| defaults.rpc_interface.clone()),
        rpc_port: matches
            .get_one::<i64>("rpc_port")
            .copied()
            .or(defaults.rpc_port),
        rpc_secret: text(matches, "rpc_secret").or_else(|| defaults.rpc_secret.clone()),
        rpc_open_browser: matches
            .get_one::<bool>("rpc_open_browser")
            .copied()
            .unwrap_or(defaults.rpc_open_browser),
        delete_task_files: matches
            .get_one::<bool>("delete_task_files")
            .copied()
            .unwrap_or(defaults.delete_task_files),
        make_archive: matches
            .get_one::<bool>("make_archive")
            .copied()
            .unwrap_or(defaults.make_archive),
        // The CLI default is hard-coded to None even when the file configures
        // a range; the reference namespace never inherits it.
        download_range: matches.get_one::<Vec<[i64; 2]>>("download_range").cloned(),
        download_thread_cnt: matches
            .get_one::<i64>("download_thread_cnt")
            .copied()
            .unwrap_or(defaults.download_thread_cnt),
        download_timeout: matches
            .get_one::<i64>("download_timeout")
            .copied()
            .unwrap_or(defaults.download_timeout),
        low_speed_threshold: matches
            .get_one::<i64>("low_speed_threshold")
            .copied()
            .unwrap_or(defaults.low_speed_threshold),
        ignored_errors,
        auto_update: text(matches, "auto_update").unwrap_or_else(|| defaults.auto_update.clone()),
        update_beta_channel: matches
            .get_one::<bool>("update_beta_channel")
            .copied()
            .unwrap_or(defaults.update_beta_channel),
        log_path: text(matches, "log_path")
            .unwrap_or_else(|| abspath(Path::new(&defaults.log_path))),
        verbose: defaults.log_verbose + i64::from(matches.get_count("verbose")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn parse_with(config: &Config, argv: &[&str]) -> Result<Parsed, UsageError> {
        parse_from(argv.iter().copied(), config, Locale::EnUs)
    }

    fn opts(argv: &[&str]) -> Opts {
        match parse_with(&Config::default(), argv).expect("parse") {
            Parsed::Opts(opts) => opts,
            other => panic!("expected options, got {other:?}"),
        }
    }

    #[test]
    fn cli_defaults_match_the_reference_namespace() {
        let opts = opts(&["xeH"]);
        let cwd = abspath(Path::new("."));
        assert_eq!(opts.dir, cwd);
        assert_eq!(opts.log_path, format!("{cwd}/eh.log"));
        assert_eq!(opts.auto_update, "download");
        assert_eq!(opts.verbose, 2);
        assert!(!opts.daemon && !opts.interactive && !opts.download_ori);
        assert!(opts.jpn_title && !opts.rename_ori);
        assert!(opts.proxy.is_empty());
        assert!(opts.proxy_image && !opts.proxy_image_only);
        assert_eq!(opts.rpc_interface, "localhost");
        assert_eq!(opts.rpc_port, None);
        assert_eq!(opts.rpc_secret, None);
        assert!(opts.rpc_open_browser);
        assert!(!opts.make_archive && !opts.delete_task_files);
        assert_eq!(opts.download_range, None);
        assert_eq!(opts.download_thread_cnt, 5);
        assert_eq!(opts.download_timeout, 10);
        assert_eq!(opts.low_speed_threshold, 10);
        assert_eq!(opts.ignored_errors, None);
        assert!(opts.urls.is_empty());
        assert_eq!(opts.username, None);
        assert_eq!(opts.key, None);
        assert_eq!(opts.cookie, None);
    }

    #[test]
    fn cli_config_overrides_become_the_defaults() {
        let config = Config {
            daemon: true,
            dir: "rel_dl".into(),
            log_path: "rel_eh.log".into(),
            auto_update: "off".into(),
            log_verbose: 4,
            proxy: vec![
                "socks5://127.0.0.1:1080".into(),
                "http://127.0.0.1:3128".into(),
            ],
            proxy_image: false,
            proxy_image_only: true,
            rpc_port: Some(6888),
            rpc_secret: Some("sekrit".into()),
            download_thread_cnt: 9,
            download_timeout: 33,
            low_speed_threshold: 77,
            download_ori: true,
            jpn_title: false,
            rename_ori: true,
            make_archive: true,
            delete_task_files: true,
            update_beta_channel: true,
            rpc_open_browser: false,
            download_range: Some(vec![[1, 5], [8, 8]]),
            ..Config::default()
        };
        let Parsed::Opts(opts) = parse_with(&config, &["xeH"]).expect("parse") else {
            panic!("options")
        };
        let cwd = abspath(Path::new("."));
        assert_eq!(opts.dir, format!("{cwd}/rel_dl"));
        assert_eq!(opts.log_path, format!("{cwd}/rel_eh.log"));
        assert_eq!(opts.auto_update, "off");
        assert_eq!(opts.verbose, 4);
        assert_eq!(
            opts.proxy,
            ["socks5://127.0.0.1:1080", "http://127.0.0.1:3128"]
        );
        assert!(!opts.proxy_image && opts.proxy_image_only);
        assert_eq!(opts.rpc_port, Some(6888));
        assert_eq!(opts.rpc_secret.as_deref(), Some("sekrit"));
        assert_eq!(opts.download_thread_cnt, 9);
        assert_eq!(opts.download_timeout, 33);
        assert_eq!(opts.low_speed_threshold, 77);
        assert!(opts.daemon && opts.download_ori && opts.rename_ori);
        assert!(opts.make_archive && opts.delete_task_files && opts.update_beta_channel);
        assert!(!opts.jpn_title && !opts.rpc_open_browser);
        // The CLI default is hard-coded to None even when the file has a range.
        assert_eq!(opts.download_range, None);
        // `-f` absent keeps the reference namespace `None`, not the file list.
        assert_eq!(opts.ignored_errors, None);
    }

    #[test]
    fn cli_short_and_long_aliases_project_identically() {
        let short = opts(&[
            "xeH",
            "-u",
            "user1",
            "-k",
            "pass1",
            "-c",
            "ipb_member_id=111",
            "-i",
            "-d",
            "dl_x",
            "-o",
            "-j",
            "True",
            "-r",
            "0",
            "-p",
            "socks5://127.0.0.1:1080",
            "--proxy-image-only",
            "--rpc-interface",
            "0.0.0.0",
            "--rpc-port",
            "7000",
            "--rpc-secret",
            "s3",
            "--rpc-open-browser",
            "False",
            "--delete-task-files",
            "1",
            "-a",
            "0",
            "--download-range",
            "1-2,4",
            "-t",
            "3",
            "--timeout",
            "44",
            "--low-speed-threshold",
            "55",
            "-f",
            "--auto-update",
            "check",
            "--update-beta-channel",
            "0",
            "-l",
            "rel.log",
            "-vv",
            "urlA",
            "urlB",
        ]);
        let long = opts(&[
            "xeH",
            "--username",
            "user1",
            "--key",
            "pass1",
            "--cookie",
            "ipb_member_id=111",
            "--interactive",
            "--dir",
            "dl_x",
            "--download-ori",
            "--jpn-title",
            "True",
            "--rename-ori",
            "0",
            "--proxy",
            "socks5://127.0.0.1:1080",
            "--proxy-image-only",
            "--rpc-interface",
            "0.0.0.0",
            "--rpc-port",
            "7000",
            "--rpc-secret",
            "s3",
            "--rpc-open-browser",
            "False",
            "--delete-task-files",
            "1",
            "--archive",
            "0",
            "--download-range",
            "1-2,4",
            "--thread",
            "3",
            "--timeout",
            "44",
            "--low-speed-threshold",
            "55",
            "--force",
            "--auto-update",
            "check",
            "--update-beta-channel",
            "0",
            "--logpath",
            "rel.log",
            "--verbose",
            "--verbose",
            "urlA",
            "urlB",
        ]);
        assert_eq!(short, long);
        assert_eq!(short.dir, "dl_x");
        assert_eq!(short.log_path, "rel.log");
        assert_eq!(short.verbose, 4);
        assert_eq!(short.download_range, Some(vec![[1, 2], [4, 4]]));
        assert_eq!(short.ignored_errors, Some(vec![consts::ERR_QUOTA_EXCEEDED]));
        assert!(short.jpn_title && short.rename_ori && short.make_archive);
        assert_eq!(short.urls, ["urlA", "urlB"]);
    }

    #[test]
    fn cli_bool_options_use_the_reference_truthiness() {
        // `type=bool` on a *string* argument: only the empty string is false.
        assert!(opts(&["xeH", "-j", "False"]).jpn_title);
        assert!(opts(&["xeH", "-j", "0"]).jpn_title);
        assert!(!opts(&["xeH", "-j", ""]).jpn_title);
        assert!(opts(&["xeH", "-j", "True"]).jpn_title);
        let bare = parse_with(&Config::default(), &["xeH", "-j"]);
        let Err(error) = bare else {
            panic!("a bare -j must fail")
        };
        assert!(error.message.contains("value"), "{}", error.message);
        let others = opts(&[
            "xeH",
            "-r",
            "0",
            "-a",
            "False",
            "--delete-task-files",
            "0",
            "--rpc-open-browser",
            "False",
            "--update-beta-channel",
            "False",
        ]);
        assert!(others.rename_ori && others.make_archive);
        assert!(others.delete_task_files && others.rpc_open_browser);
        assert!(others.update_beta_channel);
    }

    #[test]
    fn cli_force_discards_the_file_list_and_repeats() {
        let config = Config {
            ignored_errors: vec![1012, 1009],
            ..Config::default()
        };
        let Parsed::Opts(absent) = parse_with(&config, &["xeH"]).expect("parse") else {
            panic!("options")
        };
        assert_eq!(absent.ignored_errors, None);
        let Parsed::Opts(once) = parse_with(&config, &["xeH", "-f"]).expect("parse") else {
            panic!("options")
        };
        assert_eq!(once.ignored_errors, Some(vec![consts::ERR_QUOTA_EXCEEDED]));
        let Parsed::Opts(twice) = parse_with(&config, &["xeH", "-f", "-f"]).expect("parse") else {
            panic!("options")
        };
        assert_eq!(
            twice.ignored_errors,
            Some(vec![consts::ERR_QUOTA_EXCEEDED, consts::ERR_QUOTA_EXCEEDED])
        );
    }

    #[test]
    fn cli_proxy_appends_to_the_configured_list() {
        let empty = opts(&["xeH", "-p", "a", "-p", "b"]);
        assert_eq!(empty.proxy, ["a", "b"]);
        let config = Config {
            proxy: vec![
                "socks5://127.0.0.1:1080".into(),
                "http://127.0.0.1:3128".into(),
            ],
            ..Config::default()
        };
        let Parsed::Opts(one) = parse_with(&config, &["xeH", "-p", "x"]).expect("parse") else {
            panic!("options")
        };
        assert_eq!(
            one.proxy,
            ["socks5://127.0.0.1:1080", "http://127.0.0.1:3128", "x"]
        );
    }

    #[test]
    fn cli_verbose_counts_on_top_of_the_configured_level() {
        assert_eq!(opts(&["xeH", "-vvv"]).verbose, 5);
        let config = Config {
            log_verbose: 4,
            ..Config::default()
        };
        let Parsed::Opts(one) = parse_with(&config, &["xeH", "-v"]).expect("parse") else {
            panic!("options")
        };
        assert_eq!(one.verbose, 5);
    }

    #[test]
    fn cli_range_grammar_matches_the_reference() {
        assert_eq!(parse_range("3", Locale::EnUs).unwrap(), vec![[3, 3]]);
        assert_eq!(parse_range("1-5", Locale::EnUs).unwrap(), vec![[1, 5]]);
        assert_eq!(
            parse_range("1-2，5，8-9", Locale::EnUs).unwrap(),
            vec![[1, 2], [5, 5], [8, 9]]
        );
        assert_eq!(
            parse_range(" 1 , 4 ", Locale::EnUs).unwrap(),
            vec![[1, 1], [4, 4]]
        );
        assert_eq!(
            parse_range("5,1-3,2", Locale::EnUs).unwrap(),
            vec![[1, 3], [2, 2], [5, 5]]
        );
        assert_eq!(parse_range("9-3", Locale::EnUs).unwrap(), vec![[9, 3]]);
        assert_eq!(parse_range("0", Locale::EnUs).unwrap(), vec![[0, 0]]);
        assert_eq!(
            parse_range("1-5,3-8", Locale::EnUs).unwrap(),
            vec![[1, 5], [3, 8]]
        );
        assert_eq!(parse_range("001-003", Locale::EnUs).unwrap(), vec![[1, 3]]);
        // Arabic-Indic and fullwidth decimal digits (`\d` + `int()`).
        assert_eq!(parse_range("١-٢", Locale::EnUs).unwrap(), vec![[1, 2]]);
        assert_eq!(parse_range("１-２", Locale::EnUs).unwrap(), vec![[1, 2]]);
        for (token, reported) in [
            ("1 - 2", "1 - 2"),
            ("abc", "abc"),
            ("1,,2", ""),
            ("1-", "1-"),
            ("-1", "-1"),
            ("1-2-3", "1-2-3"),
            ("", ""),
            ("   ", ""),
        ] {
            let error = parse_range(token, Locale::EnUs).expect_err(token);
            assert!(
                error.contains(&format!("'{reported}' is not a range format")),
                "{token}: {error}"
            );
        }
    }

    #[test]
    fn cli_range_errors_reach_the_exit_status() {
        let error = parse_with(&Config::default(), &["xeH", "--download-range", "1 - 2"])
            .expect_err("bad range");
        assert!(
            error.message.contains("'1 - 2' is not a range format"),
            "{}",
            error.message
        );
    }

    #[test]
    fn cli_urls_accept_double_dash_and_ordering() {
        assert_eq!(opts(&["xeH", "u1", "u2"]).urls, ["u1", "u2"]);
        assert_eq!(
            opts(&["xeH", "--", "--not-a-flag", "-x"]).urls,
            ["--not-a-flag", "-x"]
        );
        assert_eq!(
            opts(&["xeH", "-u", "bob", "--", "--weird"]).urls,
            ["--weird"]
        );
        let unknown = parse_with(&Config::default(), &["xeH", "--nope"]).expect_err("unknown");
        assert!(unknown.message.contains("--nope"), "{}", unknown.message);
        let missing = parse_with(&Config::default(), &["xeH", "-u"]).expect_err("missing");
        assert!(missing.message.contains("-u"), "{}", missing.message);
    }

    #[test]
    fn cli_value_constraints_are_exit_errors() {
        let mutex = parse_with(
            &Config::default(),
            &["xeH", "--proxy-image", "--proxy-image-only"],
        )
        .expect_err("mutually exclusive");
        assert!(
            mutex.message.contains("--proxy-image-only") && mutex.message.contains("--proxy-image"),
            "{}",
            mutex.message
        );
        for argv in [
            vec!["xeH", "--auto-update", "nope"],
            vec!["xeH", "--timeout", "x"],
            vec!["xeH", "--rpc-port", "abc"],
        ] {
            assert!(
                parse_with(&Config::default(), &argv).is_err(),
                "{argv:?} must fail"
            );
        }
        assert_eq!(
            opts(&["xeH", "--auto-update", "check"]).auto_update,
            "check"
        );
        assert_eq!(opts(&["xeH", "--auto-update", "off"]).auto_update, "off");
    }

    #[test]
    fn cli_interactive_and_daemon_coexist_at_parse_level() {
        let opts = opts(&["xeH", "-i", "--daemon"]);
        assert!(opts.interactive && opts.daemon);
    }

    #[test]
    fn cli_help_and_version_are_reported_separately() {
        match parse_with(&Config::default(), &["xeH", "--version"]).expect("version") {
            Parsed::Version(text) => assert_eq!(text.trim(), "xeHentai v2.023-rss"),
            other => panic!("{other:?}"),
        }
        match parse_with(&Config::default(), &["xeH", "-h"]).expect("help") {
            Parsed::Help(text) => {
                for flag in [
                    "--username",
                    "--key",
                    "--cookie",
                    "--interactive",
                    "--daemon",
                    "--dir",
                    "--download-ori",
                    "--jpn-title",
                    "--rename-ori",
                    "--proxy",
                    "--proxy-image",
                    "--proxy-image-only",
                    "--rpc-interface",
                    "--rpc-port",
                    "--rpc-secret",
                    "--rpc-open-browser",
                    "--delete-task-files",
                    "--archive",
                    "--download-range",
                    "--thread",
                    "--timeout",
                    "--low-speed-threshold",
                    "--force",
                    "--auto-update",
                    "--update-beta-channel",
                    "--logpath",
                    "--verbose",
                    "--help",
                    "--version",
                ] {
                    assert!(text.contains(flag), "help is missing {flag}");
                }
            }
            other => panic!("{other:?}"),
        }
    }
}
