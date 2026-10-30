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
        Self::open_options(fixture, guids, fixture.options())
    }

    fn open_options(fixture: &Fixture, guids: &[&str], options: SessionOptions) -> Self {
        let _ = fixture;
        let options_control = ControlOptions::fixture(guids, 0);
        let control = TaskControl::open_with_session(&options_control, Some(&options))
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
fn scan_budget_rejects_negative_and_cumulative_totals_before_mutation() {
    let negative_fixture = Fixture::create();
    fs::create_dir_all(negative_fixture.download_dir.join("partial")).unwrap();
    fs::write(negative_fixture.download_dir.join("partial/1.jpg"), b"").unwrap();
    let negative = task_record(
        GALLERY_A,
        "budget01",
        10,
        json!({"dir": negative_fixture.download_dir.to_string_lossy(), "download_range": null}),
        json!({"total": -1, "title": "partial", "filelist": {}}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    seed_raw(
        &negative_fixture,
        &format!("{{\"tasks\":{{\"a\":{negative}}},\"cookies\":{{\"nw\":\"1\"}}}}"),
    );
    let small = negative_fixture
        .options()
        .with_limits(5, xeh_host::SESSION_READ_LIMIT_BYTES);
    let mut negative_harness = Harness::open_options(&negative_fixture, &[], small);
    let negative_before = negative_harness.snapshot();
    let negative_report = negative_harness.load_session();
    assert_eq!(
        negative_report["session"]["error"],
        json!("session:scan_budget")
    );
    assert_eq!(negative_report["session"]["code"], json!(null));
    let negative_snapshot = negative_harness.snapshot();
    assert_eq!(
        negative_snapshot, negative_before,
        "negative total leaves Root unchanged"
    );
    assert_eq!(negative_snapshot["tasks"], json!([]));
    assert_eq!(negative_snapshot["queue"], json!([]));
    assert_eq!(
        negative_snapshot["cookies"],
        json!([{"name": "nw", "value": "1"}])
    );
    assert_eq!(
        fs::read(negative_fixture.download_dir.join("partial/1.jpg")).unwrap(),
        b"",
        "negative total must not reach zero-file cleanup"
    );
    assert_eq!(negative_harness.control.resource_count(), 0);

    let cumulative_fixture = Fixture::create();
    fs::create_dir_all(cumulative_fixture.download_dir.join("partial")).unwrap();
    fs::write(cumulative_fixture.download_dir.join("partial/1.jpg"), b"").unwrap();
    let first = task_record(
        GALLERY_A,
        "budget02",
        10,
        json!({"dir": cumulative_fixture.download_dir.to_string_lossy(), "download_range": null}),
        json!({"total": 3, "title": "partial", "filelist": {}}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    let second = task_record(
        GALLERY_B,
        "budget03",
        10,
        json!({"dir": cumulative_fixture.download_dir.to_string_lossy(), "download_range": null}),
        json!({"total": 3, "title": "partial", "filelist": {}}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    seed_raw(
        &cumulative_fixture,
        &format!("{{\"tasks\":{{\"a\":{first},\"b\":{second}}},\"cookies\":{{\"nw\":\"1\"}}}}"),
    );
    let small = cumulative_fixture
        .options()
        .with_limits(5, xeh_host::SESSION_READ_LIMIT_BYTES);
    let mut cumulative_harness = Harness::open_options(&cumulative_fixture, &[], small);
    let cumulative_before = cumulative_harness.snapshot();
    let cumulative_report = cumulative_harness.load_session();
    assert_eq!(
        cumulative_report["session"]["error"],
        json!("session:scan_budget")
    );
    assert_eq!(cumulative_report["session"]["code"], json!(null));
    let cumulative_snapshot = cumulative_harness.snapshot();
    assert_eq!(
        cumulative_snapshot, cumulative_before,
        "cumulative refusal leaves Root unchanged"
    );
    assert_eq!(cumulative_snapshot["tasks"], json!([]));
    assert_eq!(cumulative_snapshot["queue"], json!([]));
    assert_eq!(
        fs::read(cumulative_fixture.download_dir.join("partial/1.jpg")).unwrap(),
        b"",
        "aggregate refusal must precede the first task scan"
    );
    assert_eq!(cumulative_harness.control.resource_count(), 0);

    let exact_fixture = Fixture::create();
    fs::create_dir_all(exact_fixture.download_dir.join("partial")).unwrap();
    fs::write(exact_fixture.download_dir.join("partial/1.jpg"), b"").unwrap();
    let first = task_record(
        GALLERY_A,
        "budget04",
        10,
        json!({"dir": exact_fixture.download_dir.to_string_lossy(), "download_range": null}),
        json!({"total": 3, "title": "partial", "filelist": {}}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    let second = task_record(
        GALLERY_B,
        "budget05",
        10,
        json!({"dir": exact_fixture.download_dir.to_string_lossy(), "download_range": null}),
        json!({"total": 3, "title": "partial", "filelist": {}}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    seed_raw(
        &exact_fixture,
        &format!("{{\"tasks\":{{\"a\":{first},\"b\":{second}}},\"cookies\":{{\"nw\":\"1\"}}}}"),
    );
    let exact = exact_fixture
        .options()
        .with_limits(6, xeh_host::SESSION_READ_LIMIT_BYTES);
    let mut exact_harness = Harness::open_options(&exact_fixture, &[], exact);
    let exact_report = exact_harness.load_session();
    assert_eq!(exact_report["session"]["code"], json!(0), "{exact_report}");
    assert_eq!(exact_report["tasks"].as_array().unwrap().len(), 2);
    assert!(!exact_fixture.download_dir.join("partial/1.jpg").exists());
    assert_eq!(exact_harness.control.resource_count(), 0);
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
    let state = xeh_host::os::fs::FsState::new(Vec::new(), Some(target.clone()), None);
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

#[test]
fn scan_budget_precheck_refuses_before_scan_and_keeps_disk_and_state() {
    let fixture = Fixture::create();
    fs::create_dir_all(fixture.download_dir.join("partial")).unwrap();
    fs::write(fixture.download_dir.join("partial/01.jpg"), b"").unwrap();
    let record = task_record(
        GALLERY_A,
        "budget01",
        10,
        json!({"dir": fixture.download_dir.to_string_lossy(), "download_range": null}),
        json!({"total": 12, "title": "partial", "filelist": {}}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    seed_raw(
        &fixture,
        &format!("{{\"tasks\":{{\"a\":{record}}},\"cookies\":{{\"nw\":\"1\"}}}}"),
    );
    let small = fixture
        .options()
        .with_limits(10, xeh_host::SESSION_READ_LIMIT_BYTES);
    let mut harness = Harness::open_options(&fixture, &[], small);
    let report = harness.load_session();
    assert_eq!(
        report["session"]["error"],
        json!("session:scan_budget"),
        "over-budget scan: {report}"
    );
    assert_eq!(report["session"]["code"], json!(null));
    assert!(
        fixture.download_dir.join("partial/01.jpg").exists(),
        "no zero-file deletion happens before the budget decision"
    );
    let snapshot = harness.snapshot();
    assert_eq!(snapshot["tasks"], json!([]));
    assert_eq!(snapshot["queue"], json!([]));
    assert_eq!(snapshot["cookies"], json!([{"name": "nw", "value": "1"}]));
    assert_eq!(harness.control.resource_count(), 0);

    // A huge total is refused by the same precheck without walking fids.
    let huge = task_record(
        GALLERY_A,
        "budget02",
        10,
        json!({"dir": fixture.download_dir.to_string_lossy(), "download_range": null}),
        json!({"total": 9223372036854775807i64, "title": "partial", "filelist": {}}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    seed_raw(
        &fixture,
        &format!("{{\"tasks\":{{\"b\":{huge}}},\"cookies\":{{\"nw\":\"1\"}}}}"),
    );
    let report = harness.load_session();
    assert_eq!(report["session"]["error"], json!("session:scan_budget"));
    assert!(fixture.download_dir.join("partial/01.jpg").exists());

    // The same document at the exact budget boundary scans normally.
    let record = task_record(
        GALLERY_A,
        "budget03",
        10,
        json!({"dir": fixture.download_dir.to_string_lossy(), "download_range": null}),
        json!({"total": 12, "title": "partial", "filelist": {}}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    seed_raw(
        &fixture,
        &format!("{{\"tasks\":{{\"c\":{record}}},\"cookies\":{{\"nw\":\"1\"}}}}"),
    );
    let exact = fixture
        .options()
        .with_limits(12, xeh_host::SESSION_READ_LIMIT_BYTES);
    let mut harness = Harness::open_options(&fixture, &[], exact);
    let report = harness.load_session();
    assert_eq!(
        report["session"]["code"],
        json!(0),
        "exact budget: {report}"
    );
    assert_eq!(report["tasks"][0]["guid"], json!("budget03"));
    assert!(!fixture.download_dir.join("partial/01.jpg").exists());
    assert_eq!(report["tasks"][0]["meta"]["finished"], json!(0));
}

#[test]
fn plain_control_without_session_has_no_file_authority() {
    let mut control = TaskControl::open(&ControlOptions::fixture(&[], 0)).expect("plain control");
    let report = control.load_session().expect("load report");
    let report: Value = serde_json::from_str(&report).unwrap();
    assert_eq!(report["session"]["error"], json!("session:host_failure"));
    assert_eq!(report["tasks"], json!([]));
    let saved = control.save_session().expect("save report");
    let saved: Value = serde_json::from_str(&saved).unwrap();
    assert_eq!(saved["session"]["error"], json!("session:host_failure"));
    assert_eq!(
        saved["session"]["code"],
        json!(null),
        "no session target means no write capability: {saved}"
    );
}

// ---- R1: separated file authority ------------------------------------------

fn session_document_of_bytes(size: usize) -> String {
    let head = "{\"tasks\":{},\"cookies\":{\"nw\":\"";
    let tail = "\"}}";
    assert!(size > head.len() + tail.len());
    let fill = size - head.len() - tail.len();
    format!("{head}{}{tail}", "a".repeat(fill))
}

fn session_document_of_bytes_with(size: usize, filler: &str) -> String {
    let head = "{\"tasks\":{},\"cookies\":{\"nw\":\"";
    let tail = "\"}}";
    let room = size - head.len() - tail.len();
    let fill = room / filler.len();
    assert_eq!(
        fill * filler.len(),
        room,
        "filler must tile the byte budget"
    );
    format!("{head}{}{tail}", filler.repeat(fill))
}

#[test]
fn session_and_legacy_parents_do_not_grant_download_authority() {
    let fixture = Fixture::create();
    for base in [&fixture.session_dir, &fixture.legacy_dir] {
        fs::create_dir_all(base.join("partial")).unwrap();
        fs::write(base.join("partial/01.jpg"), b"").unwrap();
    }
    for (label, dir) in [
        ("session", fixture.session_dir.clone()),
        ("legacy", fixture.legacy_dir.clone()),
    ] {
        let record = task_record(
            GALLERY_A,
            "deny0001",
            10,
            json!({"dir": dir.to_string_lossy(), "download_range": null}),
            json!({"total": 12, "title": "partial", "filelist": {}}),
            json!({}),
            json!({}),
            json!({}),
            json!(null),
            json!(null),
        );
        seed_raw(
            &fixture,
            &format!("{{\"tasks\":{{\"a\":{record}}},\"cookies\":{{\"nw\":\"1\"}}}}"),
        );
        let mut harness = Harness::open(&fixture, &[]);
        let report = harness.load_session();
        assert_eq!(
            report["session"]["error"],
            json!("session:host_failure"),
            "{label} parent must not be a download root: {report}"
        );
        let message = report["session"]["value"].as_str().unwrap_or("");
        assert!(
            message.contains(xeh_host::ROOT_DENIED_TAG),
            "{label} refusal must carry the stable root-denied tag: {report}"
        );
        let snapshot = harness.snapshot();
        assert_eq!(snapshot["tasks"], json!([]), "{label}: no task published");
        assert_eq!(snapshot["queue"], json!([]), "{label}: no queue member");
        assert_eq!(
            snapshot["cookies"],
            json!([{"name": "nw", "value": "1"}]),
            "{label}: cookies unchanged"
        );
        assert_eq!(
            fs::read(dir.join("partial/01.jpg")).unwrap(),
            b"",
            "{label}: the zero file must still exist"
        );
    }

    // A real download root keeps the zero-file cleanup and publishes the task.
    fs::create_dir_all(fixture.download_dir.join("partial")).unwrap();
    fs::write(fixture.download_dir.join("partial/01.jpg"), b"").unwrap();
    let record = task_record(
        GALLERY_A,
        "allow001",
        10,
        json!({"dir": fixture.download_dir.to_string_lossy(), "download_range": null}),
        json!({"total": 12, "title": "partial", "filelist": {}}),
        json!({}),
        json!({}),
        json!({}),
        json!(null),
        json!(null),
    );
    seed_raw(
        &fixture,
        &format!("{{\"tasks\":{{\"a\":{record}}},\"cookies\":{{\"nw\":\"1\"}}}}"),
    );
    let mut harness = Harness::open(&fixture, &[]);
    let report = harness.load_session();
    assert_eq!(report["session"]["code"], json!(0), "allow: {report}");
    assert_eq!(report["tasks"][0]["guid"], json!("allow001"));
    assert_eq!(report["tasks"][0]["meta"]["finished"], json!(0));
    assert!(
        !fixture.download_dir.join("partial/01.jpg").exists(),
        "a trusted download root still removes the zero file"
    );
}

#[test]
fn overlapping_download_root_cannot_mutate_reserved_session_files() {
    let fixture = Fixture::create();
    fs::write(fixture.session_path(), b"").unwrap();
    fs::write(fixture.legacy_path(), b"11,hash\n").unwrap();
    fs::write(fixture.download_dir.join("zero.jpg"), b"").unwrap();
    let state = xeh_host::os::fs::FsState::new(
        vec![fixture.session_dir.clone()],
        Some(fixture.session_path()),
        Some(fixture.legacy_path()),
    );
    let removed = xeh_host::os::fs::remove_zero_file(&state, &fixture.session_path());
    assert!(
        removed.is_err(),
        "the session file is reserved even inside a download root"
    );
    assert!(fixture.session_path().exists());
    let staged = fixture.session_dir.join("staged.jpg");
    fs::write(&staged, b"staged").unwrap();
    let renamed = xeh_host::os::fs::rename_atomic(&state, &staged, &fixture.session_path());
    assert!(renamed.is_err(), "rename over h.json must be refused");
    assert_eq!(fs::read(fixture.session_path()).unwrap(), b"");
    let renamed_away = xeh_host::os::fs::rename_atomic(&state, &fixture.session_path(), &staged);
    assert!(
        renamed_away.is_err(),
        "renaming h.json away must be refused"
    );
    assert!(fixture.session_path().exists());
    let legacy_write = xeh_host::os::fs::write_atomic(&state, &fixture.legacy_path(), "x");
    assert!(legacy_write.is_err(), "legacy cookies are read-only");
    assert_eq!(fs::read(fixture.legacy_path()).unwrap(), b"11,hash\n");
    let session_write = xeh_host::os::fs::write_atomic(&state, &fixture.session_path(), "{}");
    let outcome = session_write.expect("session target is writable");
    assert!(outcome.committed(), "{:?}", outcome.error());
    let download_state =
        xeh_host::os::fs::FsState::new(vec![fixture.download_dir.clone()], None, None);
    let zero =
        xeh_host::os::fs::remove_zero_file(&download_state, &fixture.download_dir.join("zero.jpg"))
            .expect("ordinary zero file");
    assert!(zero.removed());
}

#[cfg(unix)]
#[test]
fn zero_byte_symlink_removes_the_link_and_keeps_the_target() {
    use std::os::unix::fs::symlink;

    let fixture = Fixture::create();
    let state = xeh_host::os::fs::FsState::new(vec![fixture.download_dir.clone()], None, None);
    let target = fixture.download_dir.join("target.jpg");
    fs::write(&target, b"").unwrap();
    let link = fixture.download_dir.join("link.jpg");
    symlink(&target, &link).unwrap();
    let removed = xeh_host::os::fs::remove_zero_file(&state, &link).expect("symlink zero file");
    assert!(removed.removed(), "{:?}", removed.error());
    assert!(link.symlink_metadata().is_err(), "the link is gone");
    assert!(target.exists(), "the target stays");
    assert_eq!(fs::metadata(&target).unwrap().len(), 0, "target untouched");

    // A link to a non-empty target reports the followed size and removes
    // nothing, exactly like upstream os.stat + os.remove.
    let payload = fixture.download_dir.join("payload.jpg");
    fs::write(&payload, b"payload").unwrap();
    let payload_link = fixture.download_dir.join("payload-link.jpg");
    symlink(&payload, &payload_link).unwrap();
    let kept = xeh_host::os::fs::remove_zero_file(&state, &payload_link).expect("payload link");
    assert!(!kept.removed(), "a non-zero target is not removed");
    assert!(payload_link.symlink_metadata().is_ok(), "the link stays");
    assert_eq!(fs::read(&payload).unwrap(), b"payload");

    let plain = fixture.download_dir.join("plain.jpg");
    fs::write(&plain, b"").unwrap();
    let removed = xeh_host::os::fs::remove_zero_file(&state, &plain).expect("plain zero file");
    assert!(removed.removed());
    assert!(!plain.exists());
}

#[cfg(unix)]
#[test]
fn final_symlink_target_requires_download_root_for_bound_stat_and_mutations() {
    use std::os::unix::fs::symlink;

    let target_payloads: [&[u8]; 2] = [b"", b"outside payload"];
    for (index, payload) in target_payloads.into_iter().enumerate() {
        let fixture = Fixture::create();
        let target = fixture._root.path().join(format!("outside-{index}.bin"));
        fs::write(&target, payload).unwrap();
        let partial = fixture.download_dir.join("partial");
        fs::create_dir_all(&partial).unwrap();
        let link = partial.join("1.jpg");
        symlink(&target, &link).unwrap();

        let record = task_record(
            GALLERY_A,
            "link0001",
            10,
            json!({"dir": fixture.download_dir.to_string_lossy(), "download_range": null}),
            json!({"total": 1, "title": "partial", "filelist": {}}),
            json!({}),
            json!({}),
            json!({}),
            json!(null),
            json!(null),
        );
        seed_raw(
            &fixture,
            &format!("{{\"tasks\":{{\"a\":{record}}},\"cookies\":{{\"nw\":\"1\"}}}}"),
        );
        let mut harness = Harness::open(&fixture, &[]);
        let report = harness.load_session();
        assert!(
            fs::symlink_metadata(&link).is_ok(),
            "a refused stat must preserve the zero-byte link"
        );
        assert_eq!(
            report["session"]["error"],
            json!("session:host_failure"),
            "bound xeh_fs::stat must deny external target: {report}"
        );
        assert!(
            report["session"]["value"]
                .as_str()
                .unwrap_or("")
                .contains(xeh_host::ROOT_DENIED_TAG),
            "stat denial must carry the root-denied tag: {report}"
        );
        let snapshot = harness.snapshot();
        assert_eq!(snapshot["tasks"], json!([]));
        assert_eq!(snapshot["queue"], json!([]));
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&target).unwrap(), payload);

        let state = xeh_host::os::fs::FsState::new(vec![fixture.download_dir.clone()], None, None);
        let removed = xeh_host::os::fs::remove_zero_file(&state, &link);
        let remove_error = match removed {
            Err(error) => format!("{error:?}"),
            Ok(_) => panic!("zero-file mutation accepted an external target"),
        };
        assert!(
            remove_error.contains(xeh_host::ROOT_DENIED_TAG),
            "zero-file denial must be root-denied: {remove_error}"
        );
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&target).unwrap(), payload);

        let moved = fixture.download_dir.join("moved.jpg");
        let moved_result = xeh_host::os::fs::rename_atomic(&state, &link, &moved);
        let moved_error = match moved_result {
            Err(error) => format!("{error:?}"),
            Ok(_) => panic!("rename accepted an external symlink source"),
        };
        assert!(
            moved_error.contains(xeh_host::ROOT_DENIED_TAG),
            "source denial must be root-denied: {moved_error}"
        );
        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(
            !moved.exists(),
            "denied source rename creates no destination"
        );
        assert_eq!(fs::read(&target).unwrap(), payload);

        let source = fixture.download_dir.join("source.jpg");
        fs::write(&source, b"source bytes").unwrap();
        let destination_link = fixture.download_dir.join("destination-link.jpg");
        symlink(&target, &destination_link).unwrap();
        let destination_result =
            xeh_host::os::fs::rename_atomic(&state, &source, &destination_link);
        let destination_error = match destination_result {
            Err(error) => format!("{error:?}"),
            Ok(_) => panic!("rename accepted an external symlink destination"),
        };
        assert!(
            destination_error.contains(xeh_host::ROOT_DENIED_TAG),
            "destination denial must be root-denied: {destination_error}"
        );
        assert_eq!(fs::read(&source).unwrap(), b"source bytes");
        assert!(fs::symlink_metadata(&destination_link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read(&target).unwrap(), payload);
    }
}

#[test]
fn session_byte_budget_bounds_reads_and_writes_and_keeps_old_bytes() {
    let budget = 64usize;
    assert_eq!(xeh_host::SESSION_READ_LIMIT_BYTES, 16 * 1024 * 1024);
    let fixture = Fixture::create();
    let options = fixture.options().with_limits(100_000, budget);
    let exact = session_document_of_bytes(budget);
    assert_eq!(exact.len(), budget);
    fs::write(fixture.session_path(), exact.as_bytes()).unwrap();
    let mut harness = Harness::open_options(&fixture, &[], options.clone());
    let report = harness.load_session();
    assert_eq!(
        report["session"]["code"],
        json!(0),
        "exact budget: {report}"
    );

    let over = session_document_of_bytes(budget + 1);
    fs::write(fixture.session_path(), over.as_bytes()).unwrap();
    let report = harness.load_session();
    assert_eq!(
        report["session"]["error"],
        json!("session:read_limit"),
        "one byte over the read budget: {report}"
    );
    assert_eq!(
        fs::read(fixture.session_path()).unwrap(),
        over.as_bytes(),
        "over-limit load keeps the file bytes"
    );

    // UTF-8 byte length, not the character count, is the bound: sixteen 'é'
    // characters are thirty-two bytes and exactly fill the same budget.
    let unicode = session_document_of_bytes_with(budget, "é");
    assert_eq!(unicode.len(), budget);
    fs::write(fixture.session_path(), unicode.as_bytes()).unwrap();
    let report = harness.load_session();
    assert_eq!(
        report["session"]["code"],
        json!(0),
        "unicode exact: {report}"
    );
    let unicode_over = session_document_of_bytes_with(budget + 1, "éa");
    assert_eq!(unicode_over.len(), budget + 1);
    fs::write(fixture.session_path(), unicode_over.as_bytes()).unwrap();
    let report = harness.load_session();
    assert_eq!(
        report["session"]["error"],
        json!("session:read_limit"),
        "unicode over the byte budget: {report}"
    );

    // The same VM stays usable after both failures.
    fs::write(fixture.session_path(), exact.as_bytes()).unwrap();
    let report = harness.load_session();
    assert_eq!(report["session"]["code"], json!(0), "recovery: {report}");
    assert_eq!(report["pending"], json!(null));
    assert_eq!(harness.control.resource_count(), 0);

    // A save whose complete UTF-8 document exceeds the budget is refused
    // before staging: committed=false and the old bytes are exact.
    let updated = harness.update(json!({"save_tasks": true}));
    assert_eq!(updated["code"], json!(0), "save_tasks: {updated}");
    let added = harness.add(GALLERY_A);
    assert_eq!(added["code"], json!(0), "add: {added}");
    let saved = harness.save_session();
    assert_eq!(saved["session"]["code"], json!(1103), "over save: {saved}");
    assert_eq!(saved["session"]["committed"], json!(false));
    assert_eq!(
        fs::read(fixture.session_path()).unwrap(),
        exact.as_bytes(),
        "refused save keeps the previous document"
    );
    let staging: Vec<PathBuf> = fs::read_dir(&fixture.session_dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().starts_with(".h.json.xeh-"))
                .unwrap_or(false)
        })
        .collect();
    assert!(
        staging.is_empty(),
        "the byte-budget refusal happens before staging: {staging:?}"
    );
    assert_eq!(harness.control.resource_count(), 0);
    let shrunk = harness.update(json!({"save_tasks": false}));
    assert_eq!(shrunk["code"], json!(0), "save_tasks false: {shrunk}");
    let saved = harness.save_session();
    assert_eq!(saved["session"]["code"], json!(0), "small save: {saved}");
    assert_eq!(fs::read(fixture.session_path()).unwrap(), exact.as_bytes());
    assert_eq!(harness.control.resource_count(), 0);
}

#[test]
fn default_session_budget_refuses_sixteen_mib_plus_one_and_recovers() {
    let limit = xeh_host::SESSION_READ_LIMIT_BYTES;
    let fixture = Fixture::create();
    let too_big = vec![b'a'; limit + 1];
    fs::write(fixture.session_path(), &too_big).unwrap();
    let mut harness = Harness::open(&fixture, &[]);
    let report = harness.load_session();
    assert_eq!(
        report["session"]["error"],
        json!("session:read_limit"),
        "default read budget: {report}"
    );
    assert_eq!(
        fs::read(fixture.session_path()).unwrap(),
        too_big,
        "the oversized file is not truncated"
    );

    let small = b"{\"tasks\":{},\"cookies\":{\"nw\":\"1\"}}";
    fs::write(fixture.session_path(), small).unwrap();
    let report = harness.load_session();
    assert_eq!(report["session"]["code"], json!(0), "small load: {report}");
    assert_eq!(
        harness.update(json!({"save_tasks": true}))["code"],
        json!(0)
    );
    let added = harness.add(GALLERY_A);
    let guid = added["value"].as_str().unwrap().to_string();
    let huge = "x".repeat(limit + 1);
    let seeded = harness.command(json!({
        "op": "seed_meta",
        "guid": guid,
        "meta": {"title": huge, "total": 1, "filelist": {}}
    }));
    assert_eq!(seeded["ok"], json!(true), "seed meta failed");
    let saved = harness.save_session();
    assert_eq!(saved["session"]["code"], json!(1103), "default save");
    assert_eq!(saved["session"]["committed"], json!(false));
    assert_eq!(
        fs::read(fixture.session_path()).unwrap(),
        small,
        "refused save keeps the previous document"
    );
    let seeded = harness.command(json!({
        "op": "seed_meta",
        "guid": guid,
        "meta": {"title": "small", "total": 1, "filelist": {}}
    }));
    assert_eq!(seeded["ok"], json!(true), "small meta: {seeded}");
    let saved = harness.save_session();
    assert_eq!(saved["session"]["code"], json!(0), "small save: {saved}");
    assert_eq!(harness.control.resource_count(), 0);
    assert_eq!(
        harness.command(json!({"op": "snapshot"}))["pending"],
        json!(null)
    );
}

#[test]
fn save_then_load_in_the_same_vm_refreshes_the_exact_read_root() {
    let fixture = Fixture::create();
    let mut harness = Harness::open(&fixture, &[]);
    let first = harness.load_session();
    assert_eq!(
        first["session"]["code"],
        json!(0),
        "absent session: {first}"
    );
    let saved = harness.save_session();
    assert_eq!(saved["session"]["code"], json!(0), "save: {saved}");
    assert!(fixture.session_path().is_file());
    let loaded = harness.load_session();
    assert_eq!(
        loaded["session"]["code"],
        json!(0),
        "same-VM load after save: {loaded}"
    );
    assert_eq!(loaded["cookie_header"], json!("nw=1"));
    assert_eq!(harness.control.resource_count(), 0);
}
