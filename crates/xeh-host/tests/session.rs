//! Session save/load, cookie, legacy and recovery integration over the real VM.
//!
//! Every case opens the actual `app/domain/state.rss` entry through
//! [`TaskControl`], uses a fixture-owned session directory, legacy directory
//! and download root, and inspects the real report and files. The upstream
//! oracle compares the same inputs later in `diff --suite session`; these
//! tests pin the RSS side and the atomic file effects.

#[allow(dead_code)]
#[path = "support.rs"]
mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use xeh_host::{ControlOptions, SessionOptions, TaskControl};

use support::TestRoot;

const GALLERY_A: &str = "https://e-hentai.org/g/123456/0123456789/";
const GALLERY_B: &str = "https://e-hentai.org/g/654321/abcdef0123/";

struct Fixture {
    _root: TestRoot,
    session_dir: PathBuf,
    legacy_dir: PathBuf,
    download_dir: PathBuf,
}

impl Fixture {
    fn create() -> Self {
        let root = TestRoot::create();
        let session_dir = root.path().join("session");
        let legacy_dir = root.path().join("legacy");
        let download_dir = root.path().join("downloads");
        for dir in [&session_dir, &legacy_dir, &download_dir] {
            fs::create_dir(dir).unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
        }
        Self {
            _root: root,
            session_dir,
            legacy_dir,
            download_dir,
        }
    }

    fn options(&self) -> SessionOptions {
        SessionOptions::new(
            self.session_dir.join("h.json"),
            self.legacy_dir.join(".ehentai.cookie"),
            vec![self.download_dir.clone()],
        )
    }

    fn session_path(&self) -> PathBuf {
        self.session_dir.join("h.json")
    }

    fn legacy_path(&self) -> PathBuf {
        self.legacy_dir.join(".ehentai.cookie")
    }
}

struct Harness {
    control: TaskControl,
}

impl Harness {
    fn open(fixture: &Fixture, guids: &[&str]) -> Self {
        let options = ControlOptions::fixture(guids, 0);
        let control = TaskControl::open_with_session(&options, Some(&fixture.options()))
            .unwrap_or_else(|error| panic!("open session control: {error}"));
        Self { control }
    }

    fn command(&mut self, request: Value) -> Value {
        let text = self
            .control
            .invoke(&request.to_string())
            .unwrap_or_else(|error| panic!("command {request} failed: {error}"));
        serde_json::from_str(&text).unwrap_or_else(|error| panic!("report JSON: {error}\n{text}"))
    }

    fn add(&mut self, url: &str) -> Value {
        self.command(json!({"op": "add", "url": url}))
    }

    fn update(&mut self, overlay: Value) -> Value {
        self.command(json!({"op": "update", "overlay": overlay}))
    }

    fn snapshot(&mut self) -> Value {
        self.command(json!({"op": "snapshot"}))
    }

    fn save_session(&mut self) -> Value {
        let text = self
            .control
            .save_session()
            .unwrap_or_else(|error| panic!("save session: {error}"));
        serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("save report JSON: {error}\n{text}"))
    }

    fn load_session(&mut self) -> Value {
        let text = self
            .control
            .load_session()
            .unwrap_or_else(|error| panic!("load session: {error}"));
        serde_json::from_str(&text)
            .unwrap_or_else(|error| panic!("load report JSON: {error}\n{text}"))
    }
}

fn read_json(path: &Path) -> Value {
    let text =
        fs::read_to_string(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("parse {}: {error}\n{text}", path.display()))
}

#[test]
fn save_tasks_false_still_writes_nw_cookie_and_tasks_object() {
    let fixture = Fixture::create();
    let mut harness = Harness::open(&fixture, &["0000000a"]);
    let created = harness.add(GALLERY_A);
    assert_eq!(created["code"], json!(0), "add: {created}");

    let saved = harness.save_session();
    assert_eq!(saved["session"]["code"], json!(0), "save: {saved}");
    assert_eq!(saved["session"]["committed"], json!(true), "save: {saved}");
    assert_eq!(
        saved["session"]["directory_synced"],
        json!(true),
        "save: {saved}"
    );
    let document = read_json(&fixture.session_path());
    assert_eq!(
        document["tasks"],
        json!({}),
        "default save_tasks=false must write an empty tasks object"
    );
    assert_eq!(document["cookies"], json!({"nw": "1"}));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(fixture.session_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600, "session file must be created 0600");
    }

    // A fresh controller loads the same file and reconstructs cookies/header.
    let mut loaded = Harness::open(&fixture, &[]);
    let report = loaded.load_session();
    assert_eq!(report["session"]["code"], json!(0), "load: {report}");
    assert_eq!(
        report["tasks"],
        json!([]),
        "save_tasks=false must not restore the task"
    );
    assert_eq!(report["cookies"], json!([{"name": "nw", "value": "1"}]));
    assert_eq!(report["cookie_header"], json!("nw=1"));
    assert_eq!(report["has_login"], json!(false));
}

fn seed_task_document(fixture: &Fixture, document: Value) {
    let text = serde_json::to_string(&document).unwrap();
    fs::write(fixture.session_path(), text).unwrap();
}

#[test]
fn load_restores_task_fields_guid_and_ready_queue() {
    let fixture = Fixture::create();
    seed_task_document(
        &fixture,
        json!({
            "tasks": {
                "outer-key": {
                    "url": GALLERY_B,
                    "failcode": 0,
                    "state": 1,
                    "guid": "abcd1234",
                    "config": {"dir": fixture.download_dir.to_string_lossy(), "extra": true, "nested": [1, null, "x"]},
                    "meta": {"title": "T", "total": 3},
                    "reload_map": {},
                    "duplicate_map": {},
                    "renamed_map": {},
                    "img_q": null,
                    "page_q": null
                }
            },
            "cookies": {"nw": "1", "ipb_member_id": "42", "ipb_pass_hash": "hash"}
        }),
    );
    let mut harness = Harness::open(&fixture, &[]);
    let report = harness.load_session();
    assert_eq!(report["session"]["code"], json!(0), "load: {report}");
    let tasks = report["tasks"].as_array().expect("tasks array");
    assert_eq!(tasks.len(), 1, "load report: {report}");
    let task = &tasks[0];
    assert_eq!(
        task["guid"],
        json!("abcd1234"),
        "inner guid wins over the outer key"
    );
    assert_eq!(task["url"], json!(GALLERY_B));
    assert_eq!(task["gid"], json!("654321"));
    assert_eq!(task["sethash"], json!("abcdef0123"));
    assert_eq!(task["config"]["extra"], json!(true));
    assert_eq!(task["config"]["nested"], json!([1, null, "x"]));
    assert_eq!(report["queue"], json!(["abcd1234"]));
    assert_eq!(
        report["cookie_header"],
        json!("nw=1;ipb_member_id=42;ipb_pass_hash=hash")
    );
    assert_eq!(report["has_login"], json!(true));
}

#[allow(clippy::too_many_arguments)]
fn task_record(
    url: &str,
    guid: &str,
    state: i64,
    config: Value,
    meta: Value,
    reload_map: Value,
    duplicate_map: Value,
    renamed_map: Value,
    img_q: Value,
    page_q: Value,
) -> Value {
    json!({
        "url": url,
        "failcode": 0,
        "state": state,
        "guid": guid,
        "config": config,
        "meta": meta,
        "reload_map": reload_map,
        "duplicate_map": duplicate_map,
        "renamed_map": renamed_map,
        "img_q": img_q,
        "page_q": page_q
    })
}

#[test]
fn save_true_round_trips_tasks_queues_and_extra_config() {
    let fixture = Fixture::create();
    let mut harness = Harness::open(&fixture, &["0000000a"]);
    assert_eq!(harness.add(GALLERY_A)["code"], json!(0));
    assert_eq!(
        harness.update(json!({"save_tasks": true}))["code"],
        json!(0)
    );
    assert_eq!(
        harness.update(json!({"dir": fixture.download_dir.to_string_lossy()}))["code"],
        json!(0)
    );
    let seeded = harness.command(json!({
        "op": "seed_queue",
        "guid": "0000000a",
        "img_q": ["https://e-hentai.org/i/1", "https://e-hentai.org/i/2"],
        "page_q": ["https://e-hentai.org/s/aaaaaaaaaa/123456-1"]
    }));
    assert_eq!(seeded["ok"], json!(true), "seed queue: {seeded}");
    let saved = harness.save_session();
    assert_eq!(saved["session"]["code"], json!(0), "save: {saved}");
    let document = read_json(&fixture.session_path());
    let record = &document["tasks"]["0000000a"];
    assert_eq!(record["guid"], json!("0000000a"));
    assert_eq!(
        record["img_q"],
        json!(["https://e-hentai.org/i/1", "https://e-hentai.org/i/2"])
    );
    assert_eq!(
        record["page_q"],
        json!(["https://e-hentai.org/s/aaaaaaaaaa/123456-1"])
    );

    // The loaded record keeps the inner guid, the queue order, and the
    // process-local queue text as the public to_dict shape.
    let mut loaded = Harness::open(&fixture, &[]);
    let report = loaded.load_session();
    assert_eq!(report["session"]["code"], json!(0), "load: {report}");
    let task = &report["tasks"][0];
    assert_eq!(task["guid"], json!("0000000a"));
    assert_eq!(task["img_q"], record["img_q"]);
    assert_eq!(task["page_q"], record["page_q"]);
    assert_eq!(report["queue"], json!(["0000000a"]));
    let raw = fs::read_to_string(fixture.session_path()).unwrap();
    assert!(
        !raw.contains("\"owner\""),
        "the controller owner nonce must never enter h.json: {raw}"
    );
}

#[test]
fn load_merges_over_existing_tasks_without_clearing_and_keeps_document_order() {
    let fixture = Fixture::create();
    let mut harness = Harness::open(&fixture, &["0000000a", "0000000b"]);
    assert_eq!(harness.add(GALLERY_A)["code"], json!(0), "existing add");
    let first = task_record(
        GALLERY_B,
        "0000000a",
        3,
        json!({"dir": fixture.download_dir.to_string_lossy(), "kept": true}),
        json!({}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    let second = task_record(
        GALLERY_A,
        "0000000c",
        1,
        json!({"extra_flag": "kept"}),
        json!({}),
        json!({}),
        json!({}),
        json!({}),
        json!([]),
        json!([]),
    );
    seed_raw(
        &fixture,
        &format!("{{\"tasks\":{{\"zzz-outer\":{first},\"aaa-outer\":{second}}},\"cookies\":{{\"nw\":\"1\"}}}}"),
    );
    let report = harness.load_session();
    assert_eq!(report["session"]["code"], json!(0), "load: {report}");
    let tasks = report["tasks"].as_array().expect("tasks");
    assert_eq!(tasks.len(), 2, "load must merge, not clear: {report}");
    assert_eq!(
        tasks[0]["guid"],
        json!("0000000a"),
        "existing position first"
    );
    assert_eq!(
        tasks[0]["url"],
        json!(GALLERY_B),
        "same guid is overwritten by the loaded record"
    );
    assert_eq!(tasks[0]["state"], json!(3));
    assert_eq!(tasks[0]["config"]["kept"], json!(true));
    assert_eq!(tasks[1]["guid"], json!("0000000c"));
    assert_eq!(tasks[1]["config"]["extra_flag"], json!("kept"));
    // The pre-existing ready entry is kept, then every loaded entry is queued
    // in document order; no de-duplication happens.
    assert_eq!(report["queue"], json!(["0000000a", "0000000a", "0000000c"]));
}

fn seed_raw(fixture: &Fixture, text: &str) {
    fs::write(fixture.session_path(), text).unwrap();
}

#[test]
fn load_preserves_the_document_member_order_when_the_raw_file_is_reversed() {
    let fixture = Fixture::create();
    let first = task_record(
        GALLERY_A,
        "order001",
        1,
        json!({}),
        json!({}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    let second = task_record(
        GALLERY_B,
        "order002",
        1,
        json!({}),
        json!({}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    // Reverse lexical order: `zzz` must stay before `aaa` exactly as the file
    // wrote it. serde_json's own object writer sorts keys, so this case needs
    // literal document text.
    seed_raw(
        &fixture,
        &format!(
            "{{\"tasks\":{{\"zzz-outer\":{first},\"aaa-outer\":{second}}},\"cookies\":{{\"zzz\":\"9\",\"nw\":\"7\",\"aaa\":\"1\"}}}}"
        ),
    );
    let mut harness = Harness::open(&fixture, &[]);
    let report = harness.load_session();
    assert_eq!(report["session"]["code"], json!(0), "load: {report}");
    let tasks = report["tasks"].as_array().expect("tasks");
    assert_eq!(tasks[0]["guid"], json!("order001"));
    assert_eq!(tasks[1]["guid"], json!("order002"));
    assert_eq!(
        report["cookies"],
        json!([
            {"name": "nw", "value": "7"},
            {"name": "zzz", "value": "9"},
            {"name": "aaa", "value": "1"}
        ])
    );
}

#[test]
fn duplicate_guids_and_null_empty_queue_values_follow_upstream_shapes() {
    let fixture = Fixture::create();
    let document = json!({
        "tasks": {
            "first": task_record(
                GALLERY_A, "dupe0001", 1, json!({}), json!({}), json!({}), json!({}), json!({}), json!(null), json!([])
            ),
            "second": task_record(
                GALLERY_B, "dupe0001", 1, json!({}), json!({}), json!({}), json!({}), json!({}), json!([]), json!(null)
            )
        },
        "cookies": {"nw": "1"}
    });
    seed_task_document(&fixture, document);
    let mut harness = Harness::open(&fixture, &[]);
    let report = harness.load_session();
    assert_eq!(report["session"]["code"], json!(0), "load: {report}");
    let tasks = report["tasks"].as_array().expect("tasks");
    assert_eq!(
        tasks.len(),
        1,
        "duplicate inner guid overwrites the table entry"
    );
    assert_eq!(tasks[0]["url"], json!(GALLERY_B));
    assert!(
        tasks[0].get("img_q").is_none(),
        "empty/null queues stay absent in to_dict"
    );
    assert!(tasks[0].get("page_q").is_none());
    assert_eq!(report["queue"], json!(["dupe0001", "dupe0001"]));
}

#[test]
fn numbers_and_renamed_maps_keep_string_keys_and_extra_task_config() {
    let fixture = Fixture::create();
    let document = json!({
        "tasks": {
            "k": task_record(
                GALLERY_A, "num00001", 1,
                json!({"dir": fixture.download_dir.to_string_lossy(), "download_range": null, "custom_zero": 0, "custom_null": null, "custom_range": [[1, 4]]}),
                json!({"total": 2, "title": "n", "filelist": {"aa": [1, "01.jpg"]}}),
                json!({"1": ["https://e-hentai.org/s/aaaaaaaaaa/1-1", "renamed.jpg"]}),
                json!({"1": [2, 3]}),
                json!({"1": "renamed.jpg", "2": "other.jpg"}),
                json!(null), json!(null)
            )
        },
        "cookies": {"nw": "1"}
    });
    seed_task_document(&fixture, document);
    let mut harness = Harness::open(&fixture, &[]);
    let report = harness.load_session();
    assert_eq!(report["session"]["code"], json!(0), "load: {report}");
    let task = &report["tasks"][0];
    assert_eq!(task["config"]["custom_zero"], json!(0));
    assert_eq!(task["config"]["custom_null"], json!(null));
    assert_eq!(task["config"]["custom_range"], json!([[1, 4]]));
    assert_eq!(task["renamed_map"]["1"], json!("renamed.jpg"));
    assert_eq!(task["duplicate_map"]["1"], json!([2, 3]));
    assert_eq!(
        task["reload_map"]["1"][0],
        json!("https://e-hentai.org/s/aaaaaaaaaa/1-1")
    );
}

#[test]
fn scan_rules_cover_marker_zip_zero_range_and_state_compare() {
    let fixture = Fixture::create();
    let root = fixture.download_dir.to_string_lossy().to_string();

    let gallery = fixture.download_dir.join("gallery");
    fs::create_dir(&gallery).unwrap();
    fs::write(gallery.join("1.jpg"), b"payload").unwrap();
    fs::write(gallery.join("2.jpg"), b"").unwrap();

    let marked = fixture.download_dir.join("marked");
    fs::create_dir(&marked).unwrap();
    fs::write(marked.join(".xehdone"), b"done\n").unwrap();

    fs::write(fixture.download_dir.join("zipped.zip"), b"zip").unwrap();

    let ranged = fixture.download_dir.join("ranged");
    fs::create_dir(&ranged).unwrap();
    fs::write(ranged.join("2.jpg"), b"two").unwrap();
    fs::write(ranged.join("3.jpg"), b"three").unwrap();

    let complete = fixture.download_dir.join("complete");
    fs::create_dir(&complete).unwrap();
    for fid in 1..=2 {
        fs::write(complete.join(format!("{fid}.jpg")), b"x").unwrap();
    }

    let document = json!({
        "tasks": {
            "scan0001": task_record(
                GALLERY_A, "scan0001", 10,
                json!({"dir": root, "download_range": null}),
                json!({"total": 3, "title": "gallery", "filelist": {}, "finished": 0}),
                json!({}), json!({}), json!({}), json!(null), json!(["https://e-hentai.org/s/aaaaaaaaaa/1-1"])
            ),
            "mark0001": task_record(
                GALLERY_A, "mark0001", 10, json!({"dir": root, "download_range": null}), json!({"total": 2, "title": "marked", "filelist": {}, "finished": 0}),
                json!({}), json!({}), json!({}), json!(null), json!(null)
            ),
            "zip00001": task_record(
                GALLERY_A, "zip00001", 10, json!({"dir": root, "download_range": null}), json!({"total": 1, "title": "zipped", "filelist": {}, "finished": 0}),
                json!({}), json!({}), json!({}), json!(null), json!(null)
            ),
            "range001": task_record(
                GALLERY_A, "range001", 10,
                json!({"dir": root, "download_range": [[2, 3]]}),
                json!({"total": 4, "title": "ranged", "filelist": {}, "finished": 0}),
                json!({}), json!({}), json!({}), json!(null), json!(null)
            ),
            "full0001": task_record(
                GALLERY_A, "full0001", 10, json!({"dir": root, "download_range": null}), json!({"total": 2, "title": "complete", "filelist": {}, "finished": 0}),
                json!({}), json!({}), json!({}), json!(null), json!(null)
            )
        },
        "cookies": {"nw": "1"}
    });
    seed_task_document(&fixture, document);
    let mut harness = Harness::open(&fixture, &[]);
    let report = harness.load_session();
    assert_eq!(report["session"]["code"], json!(0), "load: {report}");
    let tasks = report["tasks"].as_array().expect("tasks");
    let by_guid = |guid: &str| {
        tasks
            .iter()
            .find(|task| task["guid"] == json!(guid))
            .unwrap_or_else(|| panic!("missing {guid}: {report}"))
    };
    assert_eq!(
        by_guid("scan0001")["meta"]["finished"],
        json!(1),
        "nonzero kept, zero removed"
    );
    assert_eq!(
        by_guid("scan0001")["state"],
        json!(4),
        "download + truthy page_q becomes scan_img"
    );
    assert!(
        !gallery.join("2.jpg").exists(),
        "zero-byte file must be removed"
    );
    assert_eq!(
        by_guid("mark0001")["meta"]["finished"],
        json!(2),
        ".xehdone marks every id done"
    );
    assert_eq!(
        by_guid("mark0001")["state"],
        json!(10),
        "scan never assigns the finished state"
    );
    assert_eq!(
        by_guid("zip00001")["meta"]["finished"],
        json!(1),
        "archive marks every id done"
    );
    assert_eq!(
        by_guid("range001")["meta"]["finished"],
        json!(4),
        "range marks out-of-range ids done"
    );
    assert_eq!(by_guid("full0001")["meta"]["finished"], json!(2));
    assert_eq!(
        by_guid("full0001")["state"],
        json!(10),
        "the upstream `state == finished` comparison does not assign 20"
    );
}

#[test]
fn cookie_merge_order_and_legacy_import_follow_upstream_text_rules() {
    let fixture = Fixture::create();
    seed_raw(
        &fixture,
        "{\"tasks\":{},\"cookies\":{\"zzz\":\"9\",\"nw\":\"7\",\"aaa\":\"1\"}}",
    );
    let mut harness = Harness::open(&fixture, &[]);
    let report = harness.load_session();
    assert_eq!(report["session"]["code"], json!(0), "load: {report}");
    assert_eq!(
        report["cookies"],
        json!([
            {"name": "nw", "value": "7"},
            {"name": "zzz", "value": "9"},
            {"name": "aaa", "value": "1"}
        ]),
        "existing nw keeps its constructor position; new names append in document order"
    );
    assert_eq!(report["cookie_header"], json!("nw=7;zzz=9;aaa=1"));
    assert_eq!(report["has_login"], json!(false));

    // A valid 1.x cookie is stripped as one string, split on ',', and neither
    // part is trimmed again.
    fs::write(fixture.legacy_path(), " 11 ,synthetic-hash \n").unwrap();
    let mut legacy = Harness::open(&fixture, &[]);
    let report = legacy.load_session();
    assert_eq!(report["session"]["code"], json!(0), "legacy load: {report}");
    assert_eq!(
        report["cookie_header"],
        json!("nw=7;zzz=9;aaa=1;ipb_member_id=11 ;ipb_pass_hash=synthetic-hash")
    );
    assert_eq!(report["has_login"], json!(true));

    // A session that already carries both login keys suppresses the legacy
    // import entirely.
    seed_raw(
        &fixture,
        "{\"tasks\":{},\"cookies\":{\"nw\":\"7\",\"ipb_member_id\":\"5\",\"ipb_pass_hash\":\"H\"}}",
    );
    fs::write(fixture.legacy_path(), "other,values\n").unwrap();
    let mut logged = Harness::open(&fixture, &[]);
    let report = logged.load_session();
    assert_eq!(report["has_login"], json!(true));
    assert_eq!(
        report["cookie_header"],
        json!("nw=7;ipb_member_id=5;ipb_pass_hash=H"),
        "legacy values must not overwrite an already logged-in session"
    );

    // Malformed legacy (three parts) is silently ignored; no header appears
    // when there is no session document to merge.
    fs::remove_file(fixture.session_path()).unwrap();
    fs::write(fixture.legacy_path(), "a,b,c").unwrap();
    let mut malformed = Harness::open(&fixture, &[]);
    let report = malformed.load_session();
    assert_eq!(
        report["session"]["code"],
        json!(0),
        "malformed legacy is not a failure: {report}"
    );
    assert_eq!(report["cookie_header"], json!(null));
    assert_eq!(report["has_login"], json!(false));

    // Exactly two empty values are accepted and set both keys.
    fs::write(fixture.legacy_path(), ",").unwrap();
    let mut empty = Harness::open(&fixture, &[]);
    let report = empty.load_session();
    assert_eq!(report["has_login"], json!(true));
    assert_eq!(
        report["cookie_header"],
        json!("nw=1;ipb_member_id=;ipb_pass_hash=")
    );
}

#[test]
fn session_failures_are_typed_keep_the_file_and_leave_the_vm_usable() {
    let fixture = Fixture::create();
    fs::write(fixture.session_path(), "{\"tasks\": {}, \"cookies\": {}").unwrap();
    let original = fs::read(fixture.session_path()).unwrap();
    let mut harness = Harness::open(&fixture, &[]);
    let report = harness.load_session();
    assert_eq!(report["session"]["code"], json!(1103), "bad JSON: {report}");
    assert_eq!(report["session"]["error"], json!("session:parse"));
    assert_eq!(
        fs::read(fixture.session_path()).unwrap(),
        original,
        "a parse failure keeps the file bytes"
    );
    let snapshot = harness.snapshot();
    assert_eq!(
        snapshot["ok"],
        json!(true),
        "the same VM remains usable: {snapshot}"
    );

    fs::write(fixture.session_path(), "{\"cookies\": {}}").unwrap();
    let report = harness.load_session();
    assert_eq!(
        report["session"]["code"],
        json!(null),
        "missing tasks: {report}"
    );
    assert_eq!(report["session"]["error"], json!("session:missing_tasks"));

    fs::write(fixture.session_path(), "{\"tasks\": [], \"cookies\": {}}").unwrap();
    let report = harness.load_session();
    assert_eq!(
        report["session"]["code"],
        json!(null),
        "tasks type: {report}"
    );
    assert_eq!(report["session"]["error"], json!("session:structure"));
    assert_eq!(
        harness.snapshot()["tasks"],
        json!([]),
        "no half-applied tasks"
    );

    fs::write(
        fixture.session_path(),
        "{\"tasks\": {\"a\": {\"url\": \"https://e-hentai.org/g/1/aaaaaaaaaa/\"}}, \"cookies\": {}}",
    )
    .unwrap();
    let report = harness.load_session();
    assert_eq!(
        report["session"]["error"],
        json!("session:missing_guid"),
        "{report}"
    );

    fs::write(
        fixture.session_path(),
        format!(
            "{{\"tasks\": {{\"a\": {}}}, \"cookies\": {{}}}}",
            task_record(
                GALLERY_A,
                "outside1",
                10,
                json!({"dir": "/definitely-not-a-trusted-root", "download_range": null}),
                json!({"total": 1, "title": "x", "filelist": {}}),
                json!({}),
                json!({}),
                json!({}),
                json!(null),
                json!(null)
            )
        ),
    )
    .unwrap();
    let report = harness.load_session();
    assert_eq!(
        report["session"]["code"],
        json!(null),
        "outside root: {report}"
    );
    assert_eq!(report["session"]["error"], json!("session:host_failure"));
    assert_eq!(
        harness.snapshot()["tasks"],
        json!([]),
        "outside-root scan must not publish a task"
    );
}

#[test]
fn production_set_login_is_not_an_authentication_entry() {
    let control = TaskControl::open_production().expect("production control");
    let mut control = control;
    let report = control
        .invoke(&json!({"op": "set_login", "value": true}).to_string())
        .expect("set_login request");
    let report: Value = serde_json::from_str(&report).unwrap();
    assert_eq!(report["ok"], json!(false), "production set_login: {report}");
    assert_eq!(report["error"], json!("set_login_not_allowed"));
    assert_eq!(report["has_login"], json!(false));
}

#[test]
fn production_reports_do_not_publish_cookie_values() {
    let mut control = TaskControl::open_production().expect("production control");
    assert!(
        !control.has_session(),
        "a plain production control has no disk session binding"
    );
    let report = control
        .invoke(&json!({"op": "snapshot"}).to_string())
        .unwrap();
    let report: Value = serde_json::from_str(&report).unwrap();
    assert_eq!(report["cookies"], json!(null));
    assert_eq!(report["cookie_header"], json!(null));
    assert_eq!(report["has_login"], json!(false));
}

#[test]
fn restoring_a_task_rejects_the_ticket_prepared_before_the_load() {
    let fixture = Fixture::create();
    let mut harness = Harness::open(&fixture, &["0000000a"]);
    assert_eq!(harness.add(GALLERY_A)["code"], json!(0));
    let ticket = ticket_of(&harness.command(json!({"op": "prepare", "guid": "0000000a"})));
    let document = json!({
        "tasks": {
            "a": task_record(GALLERY_A, "0000000a", 1, json!({}), json!({}), json!({}), json!({}), json!({}), json!(null), json!(null))
        },
        "cookies": {"nw": "1"}
    });
    seed_task_document(&fixture, document);
    let loaded = harness.load_session();
    assert_eq!(loaded["session"]["code"], json!(0), "restore: {loaded}");
    let rejected = harness.command(json!({"op": "event", "ticket": ticket, "kind": "finished"}));
    assert_eq!(
        rejected["event"]["reason"],
        json!("stale_epoch"),
        "{rejected}"
    );
    assert_eq!(harness.snapshot()["tasks"][0]["state"], json!(1));
    assert!(harness.snapshot()["completions"].get("0000000a").is_none());
}

#[test]
fn a_ticket_from_another_controller_is_rejected_by_owner_even_with_the_same_epoch() {
    let fixture = Fixture::create();
    let mut first = Harness::open(&fixture, &["0000000a"]);
    assert_eq!(first.add(GALLERY_A)["code"], json!(0));
    let ticket = ticket_of(&first.command(json!({"op": "prepare", "guid": "0000000a"})));
    assert!(
        ticket.get("owner").is_some(),
        "tickets carry the controller owner: {ticket}"
    );

    let mut second = Harness::open(&fixture, &["0000000a"]);
    assert_eq!(second.add(GALLERY_A)["code"], json!(0));
    let own = ticket_of(&second.command(json!({"op": "prepare", "guid": "0000000a"})));
    assert_ne!(
        ticket["owner"], own["owner"],
        "controllers have distinct owners"
    );
    assert_eq!(
        ticket["epoch"], own["epoch"],
        "same planned epoch makes the owner check load-bearing"
    );
    let rejected = second.command(json!({"op": "event", "ticket": ticket, "kind": "finished"}));
    assert_eq!(
        rejected["event"]["reason"],
        json!("stale_owner"),
        "{rejected}"
    );
    assert_eq!(second.snapshot()["tasks"][0]["state"], json!(1));
}

fn ticket_of(report: &Value) -> Value {
    let text = report["value"].as_str().expect("ticket text");
    serde_json::from_str(text).expect("ticket JSON")
}

#[test]
fn load_prechecks_the_whole_epoch_budget_before_publishing_tasks() {
    let fixture = Fixture::create();
    let a = task_record(
        GALLERY_A,
        "budget01",
        1,
        json!({}),
        json!({}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    let b = task_record(
        GALLERY_A,
        "budget02",
        1,
        json!({}),
        json!({}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    seed_raw(
        &fixture,
        &format!("{{\"tasks\":{{\"a\":{a},\"b\":{b}}},\"cookies\":{{\"nw\":\"1\"}}}}"),
    );
    let options = ControlOptions::fixture(&[], 9007199254740990);
    let mut control = TaskControl::open_with_session(&options, Some(&fixture.options()))
        .expect("open near-max control");
    let text = control.load_session().expect("load report");
    let report: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(
        report["session"]["error"],
        json!("session:epoch_exhausted"),
        "{report}"
    );
    assert_eq!(report["tasks"], json!([]));
    assert_eq!(report["queue"], json!([]));
    let snapshot = control
        .invoke(&json!({"op": "snapshot"}).to_string())
        .unwrap();
    let snapshot: Value = serde_json::from_str(&snapshot).unwrap();
    assert_eq!(snapshot["tasks"], json!([]), "no half-applied batch");
}

#[test]
fn save_failure_keeps_the_previous_session_bytes() {
    let fixture = Fixture::create();
    let old = b"{\"tasks\":{},\"cookies\":{\"nw\":\"1\"}}\n";
    fs::write(fixture.session_path(), old).unwrap();
    let mut harness = Harness::open(&fixture, &[]);
    let mut permissions = fs::metadata(fixture.session_dir.clone())
        .unwrap()
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o500);
    }
    fs::set_permissions(fixture.session_dir.clone(), permissions).unwrap();
    let saved = harness.save_session();
    // Restore write permission immediately so the fixture guard can clean up.
    let mut permissions = fs::metadata(fixture.session_dir.clone())
        .unwrap()
        .permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o700);
    }
    fs::set_permissions(fixture.session_dir.clone(), permissions).unwrap();
    assert_eq!(
        saved["session"]["code"],
        json!(1103),
        "save failure: {saved}"
    );
    assert_eq!(saved["session"]["committed"], json!(false));
    assert_eq!(
        fs::read(fixture.session_path()).unwrap(),
        old,
        "precommit failure keeps the old bytes"
    );
    let saved_again = harness.save_session();
    assert_eq!(
        saved_again["session"]["code"],
        json!(0),
        "retry: {saved_again}"
    );
    let document = read_json(&fixture.session_path());
    assert_eq!(document["tasks"], json!({}));
}

fn atomic_child_target_dir() -> PathBuf {
    std::env::temp_dir().join(format!("xeh-atomic-child-{}", std::process::id()))
}

#[test]
fn atomic_writer_child_entry() {
    let Some(target) = std::env::var_os("XEH_ATOMIC_CHILD_TARGET") else {
        return;
    };
    let target = PathBuf::from(target);
    let contents = std::env::var("XEH_ATOMIC_CHILD_CONTENTS").expect("child contents");
    let parent = target.parent().expect("child target parent").to_path_buf();
    let state = xeh_host::os::fs::FsState::new(vec![parent], None, None);
    let outcome =
        xeh_host::os::fs::write_atomic_with_hook(&state, &target, &contents, Some(3), || {
            use std::io::Write;
            println!("READY");
            std::io::stdout().flush().expect("flush ready");
            let mut line = String::new();
            let _ = std::io::stdin().read_line(&mut line);
        })
        .expect("child atomic write");
    if !outcome.committed() {
        std::process::exit(4);
    }
}

#[test]
fn interrupted_atomic_write_leaves_the_old_session_loadable() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let _ = fs::remove_dir_all(atomic_child_target_dir());
    fs::create_dir_all(atomic_child_target_dir()).unwrap();
    let target = atomic_child_target_dir().join("h.json");
    let old = b"{\"tasks\":{},\"cookies\":{\"nw\":\"1\"}}\n";
    fs::write(&target, old).unwrap();
    let new_contents = "{\"tasks\":{},\"cookies\":{\"nw\":\"1\",\"ipb_member_id\":\"7\"}}";

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "atomic_writer_child_entry", "--nocapture"])
        .env("XEH_ATOMIC_CHILD_TARGET", &target)
        .env("XEH_ATOMIC_CHILD_CONTENTS", new_contents)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn atomic child");
    let stdout = child.stdout.take().expect("child stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut ready = false;
    for _attempt in 0..4 {
        let read = reader.read_line(&mut line).expect("read child readiness");
        if read == 0 {
            break;
        }
        if line.trim() == "READY" {
            ready = true;
            break;
        }
        line.clear();
    }
    assert!(
        ready,
        "child did not reach the partial-write checkpoint: {line:?}"
    );
    assert_eq!(
        fs::read(&target).unwrap(),
        old,
        "old target is untouched before the rename"
    );
    let staging: Vec<PathBuf> = fs::read_dir(atomic_child_target_dir())
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().starts_with(".h.json.xeh-"))
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(staging.len(), 1, "one partial staging file: {staging:?}");
    assert_eq!(
        fs::read(&staging[0]).unwrap(),
        b"{\"t",
        "only the first three bytes reached the staging file"
    );

    child.kill().expect("kill only this test child");
    let status = child.wait().expect("wait for the killed child");
    assert!(
        !status.success(),
        "the paused child must be terminated, got {status}"
    );
    assert_eq!(
        fs::read(&target).unwrap(),
        old,
        "a killed writer leaves the old file byte-identical"
    );

    // A fresh controller still loads the old bytes, then a normal save
    // replaces them.
    let fixture = Fixture::create();
    fs::copy(&target, fixture.session_path()).unwrap();
    let mut harness = Harness::open(&fixture, &[]);
    let loaded = harness.load_session();
    assert_eq!(
        loaded["session"]["code"],
        json!(0),
        "old file loads: {loaded}"
    );
    assert_eq!(loaded["cookies"], json!([{"name": "nw", "value": "1"}]));
    let saved = harness.save_session();
    assert_eq!(
        saved["session"]["code"],
        json!(0),
        "normal save after interruption: {saved}"
    );
    let document = read_json(&fixture.session_path());
    assert_eq!(document["cookies"]["nw"], json!("1"));
    let _ = fs::remove_dir_all(atomic_child_target_dir());
}

/// Serves exactly `bodies.len()` connections, records each request path, and
/// answers with the mapped payload. A client that tries an extra request gets
/// a refused connection after the expected two, so the test cannot silently
/// accept a download for an already-finished id.
fn serve_paths(
    bodies: Vec<(String, Vec<u8>)>,
) -> (u16, Arc<Mutex<Vec<String>>>, std::thread::JoinHandle<()>) {
    use std::io::{Read, Write};
    use std::net::TcpListener;

    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let handle = std::thread::spawn(move || {
        for (path, body) in bodies {
            let (mut stream, _) = listener.accept().expect("accept fixture request");
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).to_string();
            let requested = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("")
                .to_string();
            log.lock().unwrap().push(requested.clone());
            assert_eq!(requested, path, "unexpected request path");
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    (port, seen, handle)
}

#[test]
fn remaining_downloads_run_through_the_native_worker_and_rescan_cleanly() {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use vm::HttpConfig;
    use xeh_host::{HeaderEntry, JobKey, PoolConfig, TransferRequest, TransportHooks, WorkerPool};

    let fixture = Fixture::create();
    let root = fixture.download_dir.clone();
    let partial = root.join("partial");
    fs::create_dir(&partial).unwrap();
    fs::write(partial.join("01.jpg"), b"already-downloaded").unwrap();

    let hash_one = "0123456789";
    let full_one = "0123456789abcdef0123456789abcdef01234567";
    let hash_two = "abcdef0123";
    let full_two = "abcdef0123456789abcdef0123456789abcdef01";
    let payload_one = b"image-eleven-bytes".to_vec();
    let payload_two = b"image-twelve-bytes".to_vec();
    let payloads = Arc::new(Mutex::new(vec![
        (format!("/{full_one}-11-1-1-jpg"), payload_one.clone()),
        (format!("/{full_two}-12-1-1-jpg"), payload_two.clone()),
    ]));
    let (port, seen, server) = {
        let payloads = payloads.lock().unwrap().clone();
        serve_paths(payloads)
    };

    let image_one = format!("http://127.0.0.1:{port}/{full_one}-11-1-1-jpg");
    let image_two = format!("http://127.0.0.1:{port}/{full_two}-12-1-1-jpg");
    let page_one = format!("https://e-hentai.org/s/{hash_one}/999999-11");
    let page_two = format!("https://e-hentai.org/s/{hash_two}/999999-12");
    let document = format!(
        "{{\"tasks\":{{\"partial\":{{\"url\":\"https://e-hentai.org/g/999999/ffffffffff/\",\"guid\":\"partial1\",\"state\":10,\"failcode\":0,\"config\":{{\"dir\":\"{}\",\"save_tasks\":true,\"download_range\":null}},\"meta\":{{\"total\":12,\"title\":\"partial\",\"finished\":0,\"filelist\":{{}}}},\"reload_map\":{{\"{hash_one}\":[\"https://e-hentai.org/s/{hash_one}/999999-11\",\"renamed.jpg\"],\"{hash_two}\":[\"https://e-hentai.org/s/{hash_two}/999999-12\",\"renamed2.jpg\"]}},\"duplicate_map\":{{}},\"renamed_map\":{{}},\"img_q\":[\"{image_one}\",\"{image_two}\"],\"page_q\":[\"{page_one}\",\"{page_two}\"]}}}},\"cookies\":{{\"nw\":\"1\"}}}}",
        root.display()
    );
    seed_raw(&fixture, &document);
    let mut harness = Harness::open(&fixture, &[]);
    let loaded = harness.load_session();
    assert_eq!(loaded["session"]["code"], json!(0), "load: {loaded}");
    let task = &loaded["tasks"][0];
    assert_eq!(task["guid"], json!("partial1"));
    assert_eq!(task["meta"]["finished"], json!(1), "fid 01 already on disk");
    assert_eq!(
        task["state"],
        json!(4),
        "download + page_q becomes scan_img"
    );

    let plan_report = harness.command(json!({"op": "remaining_plan", "guid": "partial1"}));
    assert_eq!(plan_report["ok"], json!(true), "plan: {plan_report}");
    let plan: Value =
        serde_json::from_str(plan_report["value"].as_str().expect("plan text")).unwrap();
    let items = plan["items"].as_array().expect("plan items");
    assert_eq!(items.len(), 2, "only fids 11 and 12 remain: {plan}");
    assert_eq!(items[0]["fid"], json!(11));
    assert_eq!(items[1]["fid"], json!(12));
    assert_eq!(
        items[0]["file"],
        json!(partial.join("11.jpg").to_string_lossy())
    );
    assert_eq!(
        items[0]["staging"],
        json!(partial.join("11.jpg.xehpart").to_string_lossy())
    );

    let http = HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(20),
        stream_idle_timeout: Duration::from_secs(5),
        max_stream_duration: Duration::from_secs(20),
        ..HttpConfig::default()
    };
    let pool = WorkerPool::start(PoolConfig {
        workers: 2,
        job_capacity: 4,
        event_capacity: 8,
        http,
        root: root.clone(),
        chunk_bytes: 4096,
        stream_limit: HttpConfig::default().max_stream_total_bytes,
        writer_fault: None,
        commit_gate: None,
        link_gate: None,
        commit_hold: None,
        hooks: TransportHooks::default(),
        fail_worker_once: false,
        submit_hold: None,
    })
    .expect("worker pool");
    pool.wait_ready(Duration::from_secs(5)).expect("pool ready");
    let lease = pool.register(1, 1).expect("register");
    for (index, item) in items.iter().enumerate() {
        let staging = PathBuf::from(item["staging"].as_str().unwrap());
        let relative = staging
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        pool.submit(lease.submission(TransferRequest {
            key: JobKey {
                task_id: 1,
                epoch: 1,
                job_id: index as i64 + 1,
            },
            url: item["url"].as_str().unwrap().to_string(),
            headers: Vec::<HeaderEntry>::new(),
            relative_path: relative,
        }))
        .expect("submit download");
    }
    let terminals = pool
        .wait_terminals(2, Duration::from_secs(30))
        .expect("terminals");
    for terminal in &terminals {
        assert_eq!(
            terminal.terminal_status.as_deref(),
            Some("completed"),
            "{terminal:?}"
        );
    }
    let fs_state = xeh_host::os::fs::FsState::new(vec![root.clone()], None, None);
    for item in items {
        let staging = PathBuf::from(item["staging"].as_str().unwrap());
        let file = PathBuf::from(item["file"].as_str().unwrap());
        let renamed = xeh_host::os::fs::rename_atomic(&fs_state, &staging, &file).expect("rename");
        assert!(renamed.committed(), "rename failed: {:?}", renamed.error());
        assert!(renamed.directory_synced());
    }
    assert_eq!(fs::read(partial.join("11.jpg")).unwrap(), payload_one);
    assert_eq!(fs::read(partial.join("12.jpg")).unwrap(), payload_two);
    assert!(!partial.join("11.jpg.xehpart").exists());
    assert!(!partial.join("12.jpg.xehpart").exists());
    let requested = seen.lock().unwrap().clone();
    assert_eq!(
        requested.len(),
        2,
        "no request for the finished fid: {requested:?}"
    );
    assert!(
        requested.iter().all(|path| !path.contains("-01-")),
        "finished fid was requested: {requested:?}"
    );
    pool.shutdown().expect("shutdown");
    server.join().expect("server");

    // The reloaded owner observes the same disk layout and stops asking for
    // the now-finished pages.
    let mut rescanned = Harness::open(&fixture, &[]);
    let report = rescanned.load_session();
    assert_eq!(report["session"]["code"], json!(0), "rescan: {report}");
    let task = &report["tasks"][0];
    assert_eq!(task["guid"], json!("partial1"));
    assert_eq!(task["meta"]["finished"], json!(3));
    assert_eq!(task["state"], json!(4));
    let empty_plan = rescanned.command(json!({"op": "remaining_plan", "guid": "partial1"}));
    assert_eq!(empty_plan["ok"], json!(true), "empty plan: {empty_plan}");
    let empty: Value = serde_json::from_str(empty_plan["value"].as_str().unwrap()).unwrap();
    assert_eq!(empty["items"], json!([]));

    let updated = rescanned.update(json!({"save_tasks": true}));
    assert_eq!(updated["code"], json!(0), "save_tasks update: {updated}");
    let saved = rescanned.save_session();
    assert_eq!(saved["session"]["code"], json!(0), "save: {saved}");
    let mut third = Harness::open(&fixture, &[]);
    let report = third.load_session();
    assert_eq!(report["session"]["code"], json!(0), "third load: {report}");
    assert_eq!(report["tasks"][0]["guid"], json!("partial1"));
    assert_eq!(report["tasks"][0]["meta"]["finished"], json!(3));
    assert_eq!(report["queue"], json!(["partial1"]));
    assert_eq!(report["cookie_header"], json!("nw=1"));
}
