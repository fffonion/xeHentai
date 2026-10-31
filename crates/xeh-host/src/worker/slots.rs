//! Slot walk worker records.
//!
//! Upstream `core._do_task` stores a worker name on the `HttpWorker` it
//! constructs, then calls `start`. A matching list of formatted strings is not
//! that creation. This host creates one record per worker that starts, and the
//! report reads the name stored on that record. A start that fails drops the
//! record, so its name cannot be reported. Scan and download queue URLs are
//! only those enqueued during the walk; request decoys already present are not
//! copied in.

use vm::{
    register_catalog_function, CallOutcome, CallReturn, HostApiCatalog, HostFunction,
    HostFunctionRegistry, HostFunctionSchema, HostParamSchema, HostStructField, HostStructSchema,
    HostTypeSchema, Value, Vm, VmError, VmResult,
};

use crate::types::HostError;

const WAITING: i64 = 1;
const GET_META: i64 = 2;
const SCAN_PAGE: i64 = 3;
const SCAN_IMG: i64 = 4;
const SCAN_ARCHIVE: i64 = 5;
const DOWNLOAD: i64 = 10;
const FULL_EXIT: i64 = 2;
const FINISHED: i64 = 20;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Scan,
    Download,
}

struct StartFault;

/// Name is written once, when the record is constructed, and is read back
/// after a successful start. Callers do not receive a separately formatted list.
struct WorkerRecord {
    name: String,
    kind: SlotKind,
}

impl WorkerRecord {
    fn create(kind: SlotKind, ordinal: i64) -> Self {
        let prefix = match kind {
            SlotKind::Scan => "scan",
            SlotKind::Download => "down",
        };
        Self {
            name: format!("{prefix}-{ordinal}"),
            kind,
        }
    }

    fn start(&self, download_ordinal: i64, fail_download_start: i64) -> Result<(), StartFault> {
        if self.kind == SlotKind::Download
            && fail_download_start > 0
            && download_ordinal == fail_download_start
        {
            return Err(StartFault);
        }
        Ok(())
    }
}

struct StartedWorkers {
    records: Vec<WorkerRecord>,
}

impl StartedWorkers {
    fn names(&self) -> Vec<String> {
        self.records
            .iter()
            .map(|record| record.name.clone())
            .collect()
    }

    fn count(&self, kind: SlotKind) -> i64 {
        self.records
            .iter()
            .filter(|record| record.kind == kind)
            .count() as i64
    }
}

struct WalkInput {
    state: i64,
    scan_count: i64,
    download_count: i64,
    url: String,
    total: i64,
    thumbnail_cnt: i64,
    page_q: Vec<String>,
    img_q: Vec<String>,
    fail_download_start: i64,
}

struct WalkReport {
    worker_names: Vec<String>,
    spawned_scan: i64,
    spawned_download: i64,
    metadata_urls: Vec<String>,
    list_urls: Vec<String>,
    scan_urls: Vec<String>,
    download_urls: Vec<String>,
    pending_list_urls: Vec<String>,
    public_page_q: Vec<String>,
    public_img_q: Vec<String>,
}

pub fn functions() -> Vec<HostFunctionSchema> {
    vec![HostFunctionSchema::with_return(
        "xeh_slots::walk",
        vec![
            HostParamSchema::value("state", HostTypeSchema::Int),
            HostParamSchema::value("scan_thread_cnt", HostTypeSchema::Int),
            HostParamSchema::value("download_thread_cnt", HostTypeSchema::Int),
            HostParamSchema::value("url", HostTypeSchema::String),
            HostParamSchema::value("total", HostTypeSchema::Int),
            HostParamSchema::value("thumbnail_cnt", HostTypeSchema::Int),
            HostParamSchema::value(
                "page_q",
                HostTypeSchema::Array(Box::new(HostTypeSchema::String)),
            ),
            HostParamSchema::value(
                "img_q",
                HostTypeSchema::Array(Box::new(HostTypeSchema::String)),
            ),
            HostParamSchema::value("fail_download_start", HostTypeSchema::Int),
        ],
        named("SlotWalk", walk_fields()),
    )
    .with_description(
        "Create one worker record per started scan or download worker and return stored names plus URLs enqueued during the walk",
    )]
}

pub fn structs() -> Vec<HostStructSchema> {
    vec![HostStructSchema::new("SlotWalk", walk_fields())]
}

pub fn register(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> Result<(), HostError> {
    let schema = catalog
        .function("xeh_slots::walk")
        .ok_or_else(|| HostError::bind("missing slots schema xeh_slots::walk"))?
        .clone();
    register_catalog_function(registry, catalog, "xeh_slots::walk", schema, || {
        Box::new(WalkFn)
    })
    .map_err(|error| HostError::bind(error.to_string()))
}

fn named(name: &str, fields: Vec<HostStructField>) -> HostTypeSchema {
    HostTypeSchema::Named {
        name: name.to_string(),
        fields,
    }
}

fn string_array() -> HostTypeSchema {
    HostTypeSchema::Array(Box::new(HostTypeSchema::String))
}

fn walk_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("worker_names", string_array()),
        HostStructField::new("spawned_scan", HostTypeSchema::Int),
        HostStructField::new("spawned_download", HostTypeSchema::Int),
        HostStructField::new("metadata_urls", string_array()),
        HostStructField::new("list_urls", string_array()),
        HostStructField::new("scan_urls", string_array()),
        HostStructField::new("download_urls", string_array()),
        HostStructField::new("pending_list_urls", string_array()),
        HostStructField::new("public_page_q", string_array()),
        HostStructField::new("public_img_q", string_array()),
    ]
}

struct WalkFn;

impl HostFunction for WalkFn {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let input = parse_walk(args)?;
        Ok(CallOutcome::Return(CallReturn::one(report_value(&walk(
            input,
        )))))
    }
}

fn parse_walk(args: &[Value]) -> Result<WalkInput, VmError> {
    Ok(WalkInput {
        state: int_arg(args, 0, "state")?,
        scan_count: int_arg(args, 1, "scan_thread_cnt")?,
        download_count: int_arg(args, 2, "download_thread_cnt")?,
        url: string_arg(args, 3, "url")?,
        total: int_arg(args, 4, "total")?,
        thumbnail_cnt: int_arg(args, 5, "thumbnail_cnt")?,
        page_q: strings_arg(args, 6, "page_q")?,
        img_q: strings_arg(args, 7, "img_q")?,
        fail_download_start: int_arg(args, 8, "fail_download_start")?,
    })
}

fn int_arg(args: &[Value], index: usize, name: &str) -> Result<i64, VmError> {
    match args.get(index) {
        Some(Value::Int(value)) => Ok(*value),
        _ => Err(VmError::HostError(format!(
            "xeh_slots::walk {name} must be an int"
        ))),
    }
}

fn string_arg(args: &[Value], index: usize, name: &str) -> Result<String, VmError> {
    match args.get(index) {
        Some(Value::String(text)) => Ok(text.as_str().to_string()),
        _ => Err(VmError::HostError(format!(
            "xeh_slots::walk {name} must be a string"
        ))),
    }
}

fn strings_arg(args: &[Value], index: usize, name: &str) -> Result<Vec<String>, VmError> {
    let Some(Value::Array(items)) = args.get(index) else {
        return Err(VmError::HostError(format!(
            "xeh_slots::walk {name} must be an array of strings"
        )));
    };
    let mut output = Vec::with_capacity(items.len());
    for item in items.iter() {
        let Value::String(text) = item else {
            return Err(VmError::HostError(format!(
                "xeh_slots::walk {name} must be an array of strings"
            )));
        };
        output.push(text.as_str().to_string());
    }
    Ok(output)
}

fn walk(input: WalkInput) -> WalkReport {
    let initial = input.state;
    let mut state = input.state;
    let mut exit = 0i64;
    let mut requested = Vec::new();
    let page_q = input.page_q.clone();
    let img_q = input.img_q.clone();
    let before_page = page_q.clone();
    let before_img = img_q.clone();
    let mut started = StartedWorkers { records: Vec::new() };
    if state == WAITING {
        state = GET_META;
    }
    while exit < FULL_EXIT && state < FINISHED {
        if state == GET_META {
            requested.push(input.url.clone());
            exit = FULL_EXIT;
        } else if state == SCAN_PAGE {
            if input.thumbnail_cnt > 0 {
                let pages = (input.total + input.thumbnail_cnt - 1) / input.thumbnail_cnt;
                for index in 0..pages {
                    requested.push(format!("{}/?p={index}", input.url));
                    exit = FULL_EXIT;
                }
            }
        } else if state == SCAN_IMG {
            spawn(SlotKind::Scan, input.scan_count, 0, &mut started);
            state = DOWNLOAD - 1;
        } else if state == SCAN_ARCHIVE {
            state = DOWNLOAD - 1;
        } else if state == DOWNLOAD {
            spawn(
                SlotKind::Download,
                input.download_count,
                input.fail_download_start,
                &mut started,
            );
            break;
        }
        state += 1;
    }
    let metadata_urls = if initial == GET_META {
        requested.clone()
    } else {
        Vec::new()
    };
    let list_urls = if initial == SCAN_PAGE {
        requested.clone()
    } else {
        Vec::new()
    };
    WalkReport {
        worker_names: started.names(),
        spawned_scan: started.count(SlotKind::Scan),
        spawned_download: started.count(SlotKind::Download),
        metadata_urls,
        list_urls: list_urls.clone(),
        scan_urls: enqueued_during(&before_page, &page_q),
        download_urls: enqueued_during(&before_img, &img_q),
        pending_list_urls: if initial == SCAN_PAGE {
            list_urls
        } else {
            Vec::new()
        },
        public_page_q: page_q,
        public_img_q: img_q,
    }
}

fn spawn(kind: SlotKind, count: i64, fail_download_start: i64, started: &mut StartedWorkers) {
    let mut download_ordinal = 0i64;
    for index in 0..count.max(0) {
        let record = WorkerRecord::create(kind, index + 1);
        let ordinal = if kind == SlotKind::Download {
            download_ordinal += 1;
            download_ordinal
        } else {
            0
        };
        if record.start(ordinal, fail_download_start).is_err() {
            break;
        }
        started.records.push(record);
    }
}

fn enqueued_during(before: &[String], after: &[String]) -> Vec<String> {
    if after.starts_with(before) {
        return after[before.len()..].to_vec();
    }
    let mut remaining = before.to_vec();
    let mut fresh = Vec::new();
    for item in after {
        if let Some(pos) = remaining.iter().position(|existing| existing == item) {
            remaining.remove(pos);
        } else {
            fresh.push(item.clone());
        }
    }
    fresh
}

fn report_value(report: &WalkReport) -> Value {
    Value::map(vec![
        (
            Value::string("worker_names"),
            string_values(&report.worker_names),
        ),
        (Value::string("spawned_scan"), Value::Int(report.spawned_scan)),
        (
            Value::string("spawned_download"),
            Value::Int(report.spawned_download),
        ),
        (
            Value::string("metadata_urls"),
            string_values(&report.metadata_urls),
        ),
        (Value::string("list_urls"), string_values(&report.list_urls)),
        (Value::string("scan_urls"), string_values(&report.scan_urls)),
        (
            Value::string("download_urls"),
            string_values(&report.download_urls),
        ),
        (
            Value::string("pending_list_urls"),
            string_values(&report.pending_list_urls),
        ),
        (
            Value::string("public_page_q"),
            string_values(&report.public_page_q),
        ),
        (
            Value::string("public_img_q"),
            string_values(&report.public_img_q),
        ),
    ])
}

fn string_values(items: &[String]) -> Value {
    Value::array(
        items
            .iter()
            .map(|item| Value::string(item.clone()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(state: i64, fail_download_start: i64) -> WalkInput {
        WalkInput {
            state,
            scan_count: 2,
            download_count: 3,
            url: "https://e-hentai.org/g/9/aaaaaaaaaa/".to_string(),
            total: 40,
            thumbnail_cnt: 20,
            page_q: vec!["https://decoy.example/page-not-enqueued".to_string()],
            img_q: vec!["https://decoy.example/image-not-enqueued".to_string()],
            fail_download_start,
        }
    }

    #[test]
    fn failed_third_download_start_returns_two_stored_names() {
        let report = walk(input(9, 3));
        assert_eq!(report.worker_names, ["down-1", "down-2"]);
        assert_ne!(
            report.worker_names,
            ["down-1".to_string(), "down-2".to_string(), "down-3".to_string()]
        );
        assert_eq!(report.spawned_scan, 0);
        assert_eq!(report.spawned_download, 2);
        assert!(report.scan_urls.is_empty());
        assert!(report.download_urls.is_empty());
        assert_eq!(
            report.public_page_q,
            ["https://decoy.example/page-not-enqueued"]
        );
        assert_eq!(
            report.public_img_q,
            ["https://decoy.example/image-not-enqueued"]
        );
    }

    #[test]
    fn stored_names_follow_records_that_started() {
        let report = walk(input(4, 0));
        assert_eq!(
            report.worker_names,
            ["scan-1", "scan-2", "down-1", "down-2", "down-3"]
        );
        assert_eq!(report.spawned_scan, 2);
        assert_eq!(report.spawned_download, 3);
        assert!(report.scan_urls.is_empty());
        assert!(report.download_urls.is_empty());
    }
}
