//! Real task-control behavior through the RSS control entry.
//!
//! Every command in this suite is one owner invocation of
//! `app/domain/entry.rss::dispatch` with the previous control root; the same
//! `TaskControl` (one VM, one root value) is reused for the whole scenario, so
//! the table, configuration, queue, and epochs really persist across commands.
//! No test rebuilds an empty store or asserts a hand-written state machine in
//! Rust.

#[allow(dead_code)]
#[path = "support.rs"]
mod support;

use serde_json::{json, Value};

use xeh_host::{ControlOptions, TaskControl};

const GALLERY_A: &str = "https://e-hentai.org/g/123456/0123456789/";
const GALLERY_B: &str = "https://e-hentai.org/g/654321/abcdef0123/";
const GALLERY_C: &str = "https://e-hentai.org/g/777777/8888888888/";
const EXHENTAI: &str = "https://exhentai.org/g/111111/2222222222/";

const TASK_KEYS: usize = 14;

fn default_task_baseline() -> Value {
    json!({
        "dir": ".",
        "download_ori": false,
        "download_thread_cnt": 5,
        "scan_thread_cnt": 1,
        "proxy_image": true,
        "proxy_image_only": false,
        "ignored_errors": [],
        "low_speed_threshold": 10,
        "rename_ori": false,
        "make_archive": false,
        "delete_task_files": false,
        "jpn_title": true,
        "download_range": null,
        "download_timeout": 10,
    })
}

struct Harness {
    control: TaskControl,
}

impl Harness {
    fn fixture(guids: &[&str]) -> Self {
        let control = TaskControl::open(&ControlOptions::fixture(guids, 0))
            .unwrap_or_else(|error| panic!("open task control: {error}"));
        Self { control }
    }

    fn production() -> Self {
        let control = TaskControl::open_production()
            .unwrap_or_else(|error| panic!("open production task control: {error}"));
        Self { control }
    }

    fn command(&mut self, request: Value) -> Value {
        let text = self
            .control
            .invoke(&request.to_string())
            .unwrap_or_else(|error| panic!("command {request} failed: {error}"));
        serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("report is not JSON: {error}\n{text}"))
    }

    fn add(&mut self, url: &str) -> Value {
        self.command(json!({"op": "add", "url": url}))
    }

    fn add_with(&mut self, url: &str, overrides: Value) -> Value {
        self.command(json!({"op": "add", "url": url, "overrides": overrides}))
    }

    fn guid_of(&mut self, url: &str) -> String {
        let report = self.add(url);
        assert_eq!(report["code"], json!(0), "add failed: {report}");
        report["value"].as_str().expect("guid").to_string()
    }

    fn del(&mut self, guid: &str) -> Value {
        self.command(json!({"op": "del", "guid": guid}))
    }

    fn pause(&mut self, guid: &str) -> Value {
        self.command(json!({"op": "pause", "guid": guid}))
    }

    fn resume(&mut self, guid: &str) -> Value {
        self.command(json!({"op": "resume", "guid": guid}))
    }

    fn update(&mut self, overlay: Value) -> Value {
        self.command(json!({"op": "update", "overlay": overlay}))
    }

    fn seed_state(&mut self, guid: &str, state: i64) -> Value {
        self.command(json!({"op": "seed_state", "guid": guid, "state": state}))
    }

    fn seed_meta(&mut self, guid: &str, meta: Value) -> Value {
        self.command(json!({"op": "seed_meta", "guid": guid, "meta": meta}))
    }

    fn set_login(&mut self, value: bool) -> Value {
        self.command(json!({"op": "set_login", "value": value}))
    }

    fn prepare(&mut self, guid: &str) -> Value {
        self.command(json!({"op": "prepare", "guid": guid}))
    }

    fn event(&mut self, ticket: &Value, kind: &str, code: Option<i64>) -> Value {
        let mut request = json!({"op": "event", "ticket": ticket, "kind": kind});
        if let Some(code) = code {
            request["code"] = json!(code);
        }
        self.command(request)
    }

    fn snapshot(&mut self) -> Value {
        self.command(json!({"op": "snapshot"}))
    }
}

fn records(report: &Value) -> Vec<Value> {
    report["tasks"].as_array().cloned().unwrap_or_default()
}

fn record(report: &Value, guid: &str) -> Value {
    records(report)
        .into_iter()
        .find(|task| task["guid"] == json!(guid))
        .unwrap_or_else(|| panic!("task record {guid} is absent from {report}"))
}

fn guids(report: &Value) -> Vec<String> {
    records(report)
        .iter()
        .map(|task| task["guid"].as_str().unwrap_or_default().to_string())
        .collect()
}

fn state_of(report: &Value, guid: &str) -> i64 {
    record(report, guid)["state"].as_i64().expect("state")
}

#[test]
fn first_task_keeps_its_configuration_across_a_global_update() {
    let mut harness = Harness::fixture(&["0000000a", "0000000b"]);
    let overrides = json!({
        "dir": "task-dir",
        "download_thread_cnt": 0,
        "jpn_title": false,
        "download_range": [[2, 4]],
        "download_timeout": 2.5,
        "custom_flag": true,
        "custom_empty": "",
        "custom_zero": 0,
        "custom_null": null,
        "custom_object": {"nested": [1, 2]},
    });
    let created = harness.add_with(GALLERY_A, overrides.clone());
    assert_eq!(created["code"], json!(0), "add: {created}");
    let guid = created["value"].as_str().expect("guid").to_string();
    assert_eq!(guid, "0000000a");
    assert_eq!(created["queue"], json!(["0000000a"]));

    let mut expected = default_task_baseline();
    for (key, value) in overrides.as_object().expect("overrides") {
        expected[key] = value.clone();
    }
    let extra_fields = overrides
        .as_object()
        .expect("overrides")
        .keys()
        .filter(|key| default_task_baseline().get(key).is_none())
        .count();
    assert_eq!(extra_fields, 5);
    assert_eq!(
        expected.as_object().expect("expected").len(),
        TASK_KEYS + extra_fields,
        "baseline plus the extra task fields"
    );

    let before = harness.snapshot();
    let before_task = record(&before, &guid);
    assert_eq!(before_task["config"], expected, "task configuration");
    assert_eq!(
        before_task["config"].as_object().expect("config").len(),
        TASK_KEYS + extra_fields
    );
    assert_eq!(before_task["state"], json!(1));
    assert_eq!(before_task["failcode"], json!(0));
    assert_eq!(before_task["meta"], json!({}));
    assert_eq!(before_task["url"], json!(GALLERY_A));
    assert_eq!(before_task["gid"], json!("123456"));
    assert_eq!(before_task["sethash"], json!("0123456789"));
    assert!(
        before_task.get("img_q").is_none() && before_task.get("page_q").is_none(),
        "initial queues stay absent like the upstream record: {before_task}"
    );

    // Global update on the same owner: the existing task keeps its own
    // configuration, the store configuration changes, and the queue is not
    // touched.
    let updated = harness.update(json!({
        "dir": "after-update",
        "download_thread_cnt": 2,
        "ignored_errors": [1009],
    }));
    assert_eq!(updated["ok"], json!(true), "update: {updated}");
    assert_eq!(updated["config"]["dir"], json!("after-update"));
    assert_eq!(updated["config"]["download_thread_cnt"], json!(2));
    assert_eq!(updated["config"]["ignored_errors"], json!([1009]));
    assert_eq!(
        updated["presence"],
        json!({"proxy": false, "dir": true, "ignored_errors": true})
    );
    assert_eq!(updated["queue"], json!(["0000000a"]));

    let after = harness.snapshot();
    let after_task = record(&after, &guid);
    assert_eq!(
        after_task["config"], before_task["config"],
        "the stored task configuration must not follow the global update"
    );
    assert_eq!(after_task["state"], json!(1));

    // A later task uses the updated global configuration and its own overrides.
    let second = harness.add_with(GALLERY_B, json!({"scan_thread_cnt": 3}));
    assert_eq!(second["code"], json!(0), "second add: {second}");
    let second_guid = second["value"].as_str().expect("guid").to_string();
    assert_eq!(second_guid, "0000000b");
    let after_second = harness.snapshot();
    let second_config = record(&after_second, &second_guid)["config"].clone();
    assert_eq!(second_config["dir"], json!("after-update"));
    assert_eq!(second_config["download_thread_cnt"], json!(2));
    assert_eq!(second_config["scan_thread_cnt"], json!(3));
    assert_eq!(second_config["ignored_errors"], json!([1009]));
    assert_eq!(
        second_config.as_object().expect("config").len(),
        TASK_KEYS,
        "no override means exactly the 14 baseline keys"
    );
    assert_eq!(guids(&after_second), vec!["0000000a", "0000000b"]);
    assert_eq!(after_second["queue"], json!(["0000000a", "0000000b"]));
}

fn ticket_of(report: &Value) -> Value {
    let text = report["value"]
        .as_str()
        .unwrap_or_else(|| panic!("ticket: {report}"));
    serde_json::from_str(text).unwrap_or_else(|error| panic!("ticket JSON: {error}\n{text}"))
}

#[test]
fn mutation_matrix_matches_upstream_rules() {
    let mut harness = Harness::fixture(&["00000001", "00000002"]);
    let guid = harness.guid_of(GALLERY_A);

    // Waiting tasks cannot be deleted and cannot be resumed; pause accepts
    // them and resume brings them back to waiting.
    let paused = harness.pause(&guid);
    assert_eq!(paused["code"], json!(0), "pause: {paused}");
    assert_eq!(state_of(&paused, &guid), 0);
    assert_eq!(harness.pause(&guid)["code"], json!(1106));
    let resumed = harness.resume(&guid);
    assert_eq!(resumed["code"], json!(0), "resume: {resumed}");
    assert_eq!(state_of(&resumed, &guid), 1);
    assert_eq!(resumed["queue"], json!(["00000001", "00000001"]));
    assert_eq!(harness.del(&guid)["code"], json!(1105));

    // Every public active state rejects delete and resume, pause accepts.
    for state in [2, 3, 4, 5, 10, 19] {
        let seeded = harness.seed_state(&guid, state);
        assert_eq!(seeded["ok"], json!(true), "seed {state}: {seeded}");
        assert_eq!(
            harness.del(&guid)["code"],
            json!(1105),
            "delete state {state}"
        );
        assert_eq!(
            harness.resume(&guid)["code"],
            json!(1107),
            "resume state {state}"
        );
        let paused = harness.pause(&guid);
        assert_eq!(paused["code"], json!(0), "pause state {state}");
        assert_eq!(state_of(&paused, &guid), 0);
        harness.seed_state(&guid, 1);
    }

    // Terminal states: pause is refused; resume keeps finished at 20 and
    // enqueues it, while failed becomes waiting.
    harness.seed_state(&guid, 20);
    assert_eq!(harness.pause(&guid)["code"], json!(1106));
    let finished_resume = harness.resume(&guid);
    assert_eq!(finished_resume["code"], json!(0), "finished resume");
    assert_eq!(state_of(&finished_resume, &guid), 20);
    harness.seed_state(&guid, -1);
    assert_eq!(harness.pause(&guid)["code"], json!(1106));
    let failed_resume = harness.resume(&guid);
    assert_eq!(failed_resume["code"], json!(0), "failed resume");
    assert_eq!(state_of(&failed_resume, &guid), 1);

    // Paused, failed, and finished records can be deleted; the queued guid
    // stays in the queue like the upstream Queue, and the table entry goes.
    harness.seed_state(&guid, 0);
    let deleted = harness.del(&guid);
    assert_eq!(deleted["code"], json!(0), "delete paused: {deleted}");
    assert_eq!(deleted["value"], json!(""));
    assert!(records(&deleted)
        .iter()
        .all(|task| task["guid"] != json!(guid)));
    assert_eq!(harness.del(&guid)["code"], json!(1101));

    let second = harness.guid_of(GALLERY_B);
    harness.seed_state(&second, -1);
    assert_eq!(harness.del(&second)["code"], json!(0));

    // Unknown guid keeps the original error code and a null value.
    for report in [
        harness.del("ffffffff"),
        harness.pause("ffffffff"),
        harness.resume("ffffffff"),
    ] {
        assert_eq!(report["code"], json!(1101), "unknown guid: {report}");
        assert_eq!(report["value"], Value::Null);
    }
}

#[test]
fn guid_url_and_login_contract() {
    let mut harness = Harness::fixture(&["aaaaaaaa", "bbbbbbbb", "cccccccc", "dddddddd"]);
    let first = harness.guid_of(GALLERY_A);
    assert_eq!(first, "aaaaaaaa");
    // The same URL creates a second, independent task: no URL dedupe.
    let second = harness.guid_of(GALLERY_A);
    assert_eq!(second, "bbbbbbbb");
    assert_ne!(first, second);
    let snapshot = harness.snapshot();
    assert_eq!(snapshot["queue"], json!(["aaaaaaaa", "bbbbbbbb"]));
    assert_eq!(guids(&snapshot), vec!["aaaaaaaa", "bbbbbbbb"]);

    // A URL the gallery pattern rejects is stored as failed before returning.
    let bad = harness.add("");
    assert_eq!(bad["code"], json!(1000), "empty url: {bad}");
    assert_eq!(bad["value"], Value::Null);
    assert_eq!(state_of(&bad, "cccccccc"), -1);
    assert_eq!(record(&bad, "cccccccc")["failcode"], json!(1000));
    assert_eq!(bad["queue"], json!(["aaaaaaaa", "bbbbbbbb"]));
    let also_bad = harness.add("https://example.com/g/1/x/");
    assert_eq!(also_bad["code"], json!(1000));

    // Logged-out exhentai fails with 1001 and keeps the failed record.
    let exhentai = harness.add(EXHENTAI);
    assert_eq!(exhentai["code"], json!(1001), "exhentai logged out");
    assert_eq!(exhentai["value"], Value::Null);
    assert_eq!(state_of(&exhentai, "dddddddd"), -1);

    harness.set_login(true);
    let logged_in = harness.add(EXHENTAI);
    assert_eq!(logged_in["code"], json!(0), "exhentai logged in");
    assert_eq!(logged_in["queue"].as_array().map(Vec::len), Some(3));

    // download_ori without login only warns: the table task is still created
    // and queued.
    harness.set_login(false);
    let warned = harness.add_with(GALLERY_C, json!({"download_ori": true}));
    assert_eq!(warned["code"], json!(0), "download_ori warning: {warned}");
    assert_eq!(warned["warnings"], json!(["download_ori_requires_login"]));
    assert_eq!(warned["queue"].as_array().map(Vec::len), Some(4));
}

#[test]
fn colliding_guid_keeps_the_older_record() {
    let mut harness = Harness::fixture(&["eeeeeeee", "eeeeeeee", "eeeeeeee"]);
    let guid = harness.guid_of(GALLERY_A);
    assert_eq!(guid, "eeeeeeee");

    // An active record is returned unchanged: no replacement, no new queue
    // entry, and the original URL/configuration survive.
    let before = harness.snapshot();
    let original = record(&before, &guid);
    let collision = harness.add_with(GALLERY_B, json!({"dir": "ignored"}));
    assert_eq!(collision["code"], json!(0), "collision: {collision}");
    assert_eq!(collision["value"], json!(guid));
    assert_eq!(collision["queue"], json!([guid]));
    let after = harness.snapshot();
    assert_eq!(record(&after, &guid)["url"], json!(GALLERY_A));
    assert_eq!(record(&after, &guid)["config"], original["config"]);
    assert_eq!(record(&after, &guid)["state"], json!(1));
    assert_eq!(guids(&after), vec![guid.clone()]);

    // A terminal record becomes waiting and is not enqueued, matching the
    // upstream early return. Its queues are left alone because `cleanup` sees
    // the new waiting state. The resurrection advances the epoch, so a ticket
    // prepared for the old incarnation cannot finish the new one.
    let old_ticket = ticket_of(&harness.prepare(&guid));
    harness.seed_state(&guid, 20);
    let revived = harness.add(GALLERY_C);
    assert_eq!(revived["code"], json!(0), "terminal collision");
    assert_eq!(revived["value"], json!(guid));
    assert_eq!(revived["queue"], json!([guid]));
    assert_eq!(state_of(&revived, &guid), 1);
    assert_eq!(record(&revived, &guid)["url"], json!(GALLERY_A));
    let stale = harness.event(&old_ticket, "finished", None);
    assert_eq!(stale["event"]["reason"], json!("stale_epoch"));
    assert_eq!(state_of(&stale, &guid), 1);
}

#[test]
fn epochs_reject_stale_tickets_and_duplicate_completions() {
    let mut harness = Harness::fixture(&["11111111", "22222222", "33333333", "44444444"]);

    // A completion accepted once cannot run twice.
    let guid = harness.guid_of(GALLERY_A);
    let ticket = ticket_of(&harness.prepare(&guid));
    assert_eq!(ticket["guid"], json!(guid));
    assert_eq!(ticket["kind"], json!("run"));
    let finished = harness.event(&ticket, "finished", None);
    assert_eq!(finished["event"], json!({"accepted": true, "reason": ""}));
    assert_eq!(state_of(&finished, &guid), 20);
    assert_eq!(finished["completions"][&guid], json!(1));
    let duplicate = harness.event(&ticket, "finished", None);
    assert_eq!(duplicate["event"]["accepted"], json!(false));
    assert_eq!(duplicate["event"]["reason"], json!("not_running"));
    assert_eq!(state_of(&duplicate, &guid), 20);
    assert_eq!(duplicate["completions"][&guid], json!(1));

    // Pause invalidates the ticket prepared before it.
    let second = harness.guid_of(GALLERY_B);
    let before_pause = ticket_of(&harness.prepare(&second));
    let paused = harness.pause(&second);
    assert_eq!(paused["code"], json!(0));
    let stale = harness.event(&before_pause, "finished", None);
    assert_eq!(stale["event"]["reason"], json!("stale_epoch"));
    assert_eq!(state_of(&stale, &second), 0);
    assert!(stale["completions"].get(&second).is_none());

    // Resume starts a new incarnation with a newer epoch.
    assert_eq!(harness.resume(&second)["code"], json!(0));
    let resumed_ticket = ticket_of(&harness.prepare(&second));
    let old_epoch = before_pause["epoch"].as_i64().expect("epoch");
    let new_epoch = resumed_ticket["epoch"].as_i64().expect("epoch");
    assert!(new_epoch > old_epoch, "epochs must advance");
    assert_eq!(
        harness.event(&before_pause, "finished", None)["event"]["reason"],
        json!("stale_epoch")
    );
    let accepted = harness.event(&resumed_ticket, "finished", None);
    assert_eq!(accepted["event"]["accepted"], json!(true));
    assert_eq!(state_of(&accepted, &second), 20);

    // Delete retires the incarnation: an old ticket cannot reopen the task,
    // and a same-guid resurrection gets a newer epoch.
    let third = harness.guid_of(GALLERY_C);
    let before_delete = ticket_of(&harness.prepare(&third));
    assert!(before_delete["epoch"].as_i64().is_some());
    harness.seed_state(&third, 20);
    assert_eq!(harness.del(&third)["code"], json!(0));
    let gone = harness.event(&before_delete, "finished", None);
    assert_eq!(gone["event"]["reason"], json!("unknown_task"));
    assert!(records(&gone)
        .iter()
        .all(|task| task["guid"] != json!(third)));
}

#[test]
fn finished_resume_keeps_20_and_rejects_the_old_ticket() {
    let mut harness = Harness::fixture(&["55555555"]);
    let guid = harness.guid_of(GALLERY_A);
    let ticket = ticket_of(&harness.prepare(&guid));
    harness.event(&ticket, "finished", None);

    let resumed = harness.resume(&guid);
    assert_eq!(resumed["code"], json!(0), "finished resume");
    assert_eq!(state_of(&resumed, &guid), 20, "finished stays 20");
    assert_eq!(
        resumed["queue"],
        json!([guid, guid]),
        "finished is enqueued"
    );
    let stale = harness.event(&ticket, "finished", None);
    assert_eq!(stale["event"]["reason"], json!("stale_epoch"));
    assert_eq!(stale["completions"][&guid], json!(1));
    let not_running = harness.prepare(&guid);
    assert_eq!(not_running["ok"], json!(false));
    assert_eq!(not_running["error"], json!("not_running"));
}

#[test]
fn epoch_overflow_fails_closed() {
    let control = TaskControl::open(&ControlOptions::fixture(&["66666666"], 9007199254740991))
        .expect("open at the ceiling");
    let mut harness = Harness { control };
    let overflow = harness.add(GALLERY_A);
    assert_eq!(overflow["ok"], json!(false));
    assert_eq!(overflow["error"], json!("epoch_exhausted"));
    assert_eq!(overflow["code"], Value::Null);
    assert!(records(&overflow).is_empty());
    assert_eq!(overflow["queue"], json!([]));
}

#[test]
fn delete_with_a_physical_effect_stays_pending() {
    let mut harness = Harness::fixture(&["77777777", "88888888"]);
    let guid = harness.add_with(GALLERY_A, json!({"delete_task_files": true}))["value"]
        .as_str()
        .expect("guid")
        .to_string();
    harness.seed_meta(&guid, json!({"title": "Synthetic Title"}));
    // Only terminal records can be deleted upstream; the physical effect is
    // requested only when delete_task_files and a title are both present.
    harness.seed_state(&guid, 20);

    let pending = harness.del(&guid);
    assert_eq!(pending["ok"], json!(false), "pending: {pending}");
    assert_eq!(pending["error"], json!("pending_effect"));
    assert_eq!(pending["code"], Value::Null);
    assert_eq!(pending["pending"]["kind"], json!("delete_task_files"));
    assert_eq!(pending["pending"]["directory"], json!("."));
    assert_eq!(pending["pending"]["title"], json!("Synthetic Title"));
    // The record is retained: no removal is reported before the file owner
    // performs it.
    assert_eq!(record(&pending, &guid)["state"], json!(20));

    // Without a title the upstream cleanup has no file effect, so the delete
    // is complete.
    harness.seed_meta(&guid, json!({}));
    let deleted = harness.del(&guid);
    assert_eq!(deleted["code"], json!(0), "plain delete: {deleted}");
    assert_eq!(deleted["value"], json!(""));

    // delete_task_files without a title stays a plain delete too.
    let second = harness.add_with(GALLERY_B, json!({"delete_task_files": true}))["value"]
        .as_str()
        .expect("guid")
        .to_string();
    harness.seed_state(&second, 20);
    assert_eq!(harness.del(&second)["code"], json!(0));
}

#[test]
fn production_guids_and_seed_boundary() {
    let mut harness = Harness::production();
    let first = harness.add(GALLERY_A);
    let second = harness.add(GALLERY_B);
    for report in [&first, &second] {
        assert_eq!(report["code"], json!(0));
        let guid = report["value"].as_str().expect("guid");
        assert_eq!(guid.len(), 8, "guid length: {guid}");
        assert!(
            guid.chars()
                .all(|character| character.is_ascii_hexdigit() && !character.is_ascii_uppercase()),
            "guid must be lowercase hex: {guid}"
        );
    }
    assert_ne!(first["value"], second["value"]);

    let seeded = harness.seed_state(first["value"].as_str().expect("guid"), 20);
    assert_eq!(seeded["ok"], json!(false));
    assert_eq!(seeded["error"], json!("seed_not_allowed"));
}

#[test]
fn recreated_guid_rejects_the_old_ticket_and_count() {
    let mut harness = Harness::fixture(&["99999999", "99999999"]);
    let guid = harness.guid_of(GALLERY_A);
    let old_ticket = ticket_of(&harness.prepare(&guid));
    harness.event(&old_ticket, "finished", None);

    let deleted = harness.del(&guid);
    assert_eq!(deleted["code"], json!(0), "delete: {deleted}");
    assert!(deleted["completions"].get(&guid).is_none());

    // The same GUID is a new incarnation: it gets a newer epoch, the old
    // ticket cannot complete it, and the completion count starts at zero.
    let recreated = harness.add(GALLERY_B);
    assert_eq!(recreated["code"], json!(0), "recreate: {recreated}");
    assert_eq!(recreated["value"], json!(guid));
    let new_ticket = ticket_of(&harness.prepare(&guid));
    let old_epoch = old_ticket["epoch"].as_i64().expect("epoch");
    let new_epoch = new_ticket["epoch"].as_i64().expect("epoch");
    assert!(new_epoch > old_epoch, "new incarnation epoch must advance");

    let stale = harness.event(&old_ticket, "finished", None);
    assert_eq!(stale["event"]["reason"], json!("stale_epoch"));
    assert_eq!(state_of(&stale, &guid), 1);
    assert!(stale["completions"].get(&guid).is_none());

    let accepted = harness.event(&new_ticket, "finished", None);
    assert_eq!(accepted["event"]["accepted"], json!(true));
    assert_eq!(accepted["completions"][&guid], json!(1));
}

#[test]
fn core_has_no_uuid_source_and_the_primitive_is_scoped_to_task_control() {
    // The pinned Core catalog has no UUID or random function; the task control
    // catalog adds exactly one named typed primitive for OS entropy.
    let standard = xeh_host::standard_catalog();
    for name in ["uuid", "uuid::v4", "random", "random::bytes"] {
        assert!(
            standard.function(name).is_none(),
            "unexpected Core function {name}"
        );
    }
    assert!(standard.function("xeh_system::uuid_v4").is_none());
    let task = xeh_host::task_catalog();
    assert!(task.function("xeh_system::uuid_v4").is_some());
}

/// CPython 3.14.7 `str.isspace()` over the full Unicode domain: 29 characters.
const PYTHON_WHITESPACE: [char; 29] = [
    '\u{0009}', '\u{000A}', '\u{000B}', '\u{000C}', '\u{000D}', '\u{001C}', '\u{001D}', '\u{001E}',
    '\u{001F}', '\u{0020}', '\u{0085}', '\u{00A0}', '\u{1680}', '\u{2000}', '\u{2001}', '\u{2002}',
    '\u{2003}', '\u{2004}', '\u{2005}', '\u{2006}', '\u{2007}', '\u{2008}', '\u{2009}', '\u{200A}',
    '\u{2028}', '\u{2029}', '\u{202F}', '\u{205F}', '\u{3000}',
];

fn queued(report: &Value, guid: &str) -> bool {
    report["queue"]
        .as_array()
        .map(|queue| queue.iter().any(|item| item == &json!(guid)))
        .unwrap_or(false)
}

#[test]
fn add_strips_python_unicode_whitespace_around_gallery_urls() {
    // One owner VM for the whole matrix: every case is a real dispatch/add on
    // the same control root, not a call to the trim helper.
    let planned: Vec<String> = (0..256).map(|index| format!("{index:08x}")).collect();
    let borrowed: Vec<&str> = planned.iter().map(String::as_str).collect();
    let mut harness = Harness {
        control: TaskControl::open(&ControlOptions::fixture(&borrowed, 0))
            .expect("open one fixture for the whitespace matrix"),
    };

    for whitespace in PYTHON_WHITESPACE {
        for (position, url) in [
            ("leading", format!("{whitespace}{GALLERY_A}")),
            ("trailing", format!("{GALLERY_A}{whitespace}")),
            (
                "both",
                format!("{whitespace}{whitespace}{GALLERY_A}{whitespace}{whitespace}"),
            ),
        ] {
            let label = format!("{position} U+{:04X}", whitespace as u32);
            let report = harness.add(&url);
            assert_eq!(report["code"], json!(0), "{label}: {report}");
            let guid = report["value"]
                .as_str()
                .unwrap_or_else(|| panic!("{label}: missing guid in {report}"));
            let task = record(&report, guid);
            assert_eq!(task["url"], json!(GALLERY_A), "{label}: {task}");
            assert_eq!(task["gid"], json!("123456"), "{label}: {task}");
            assert_eq!(task["sethash"], json!("0123456789"), "{label}: {task}");
            assert_eq!(task["state"], json!(1), "{label}: {task}");
            assert!(queued(&report, guid), "{label}: {report}");
        }
    }

    // Empty and all-whitespace inputs strip to an empty URL: upstream still
    // records the task and reports (1000, null) from the gallery check.
    let all_whitespace: String = PYTHON_WHITESPACE.iter().collect();
    for (label, url) in [("empty", String::new()), ("all-whitespace", all_whitespace)] {
        let report = harness.add(&url);
        assert_eq!(report["code"], json!(1000), "{label}: {report}");
        assert_eq!(report["value"], Value::Null, "{label}: {report}");
        let task = records(&report)
            .pop()
            .unwrap_or_else(|| panic!("{label}: task was not recorded"));
        assert_eq!(task["url"], json!(""), "{label}: {task}");
        assert_eq!(task["state"], json!(-1), "{label}: {task}");
        assert_eq!(task["failcode"], json!(1000), "{label}: {task}");
        let guid = task["guid"].as_str().expect("guid");
        assert!(!queued(&report, guid), "{label}: {report}");
    }

    // A URL without surrounding whitespace is unchanged.
    let plain = harness.add(GALLERY_A);
    assert_eq!(plain["code"], json!(0), "plain: {plain}");
    let plain_guid = plain["value"].as_str().expect("guid");
    assert_eq!(record(&plain, plain_guid)["url"], json!(GALLERY_A));

    // Inner whitespace is data, not trimming: the URL is stored byte-for-byte
    // and the gallery regex keeps it recognized.
    let inner = "https://e-hentai.org/g/123456/012 3456789/";
    let inner_report = harness.add(inner);
    assert_eq!(inner_report["code"], json!(0), "inner: {inner_report}");
    let inner_guid = inner_report["value"].as_str().expect("guid");
    let inner_task = record(&inner_report, inner_guid);
    assert_eq!(inner_task["url"], json!(inner), "{inner_task}");
    assert_eq!(inner_task["gid"], json!("123456"), "{inner_task}");
    assert_eq!(inner_task["sethash"], json!("012 3456789"), "{inner_task}");

    // The adjacent g.e-hentai host with the same Unicode wrapping (Q7).
    let g_e_hentai = "https://g.e-hentai.org/g/123456/0123456789/";
    let neighbor = format!("\u{3000}{g_e_hentai}\u{3000}");
    let neighbor_report = harness.add(&neighbor);
    assert_eq!(neighbor_report["code"], json!(0), "g.e: {neighbor_report}");
    let neighbor_guid = neighbor_report["value"].as_str().expect("guid");
    let neighbor_task = record(&neighbor_report, neighbor_guid);
    assert_eq!(neighbor_task["url"], json!(g_e_hentai), "{neighbor_task}");
    assert_eq!(neighbor_task["gid"], json!("123456"));
    assert_eq!(neighbor_task["sethash"], json!("0123456789"));
}
