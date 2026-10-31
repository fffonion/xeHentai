//! Slot walk threads.
//!
//! Upstream `core._do_task` constructs an `HttpWorker` bound to `page_q` or
//! `img_q` and to the success and failure callbacks, then calls `start`. This
//! host does the same with `std::thread::Builder::spawn`. The thread body holds
//! that queue and those callbacks and runs them when the queue has an item.
//! A reported name is read from `JoinHandle::thread().name()` only after spawn
//! returns `Ok`. A spawn error has no handle, so it contributes no name and
//! stops further starts of that kind. Every handle is joined before the report
//! is read. Scan and download queue URLs are only those enqueued during the
//! walk; request decoys already present are not copied in.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use vm::{
    CallOutcome, CallReturn, HostApiCatalog, HostFunction, HostFunctionRegistry,
    HostFunctionSchema, HostParamSchema, HostStructField, HostStructSchema, HostTypeSchema, Value,
    Vm, VmError, VmResult, register_catalog_function,
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

struct LiveWorkers {
    names: Vec<String>,
    handles: Vec<JoinHandle<()>>,
}

impl LiveWorkers {
    fn join(&mut self) {
        for handle in self.handles.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for LiveWorkers {
    fn drop(&mut self) {
        self.join();
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
        ],
        named("SlotWalk", walk_fields()),
    )
    .with_description(
        "Spawn one named thread per scan or download worker. The thread holds its queue and callbacks and runs them when the queue has an item",
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
    walk_started(input, start_named)
}

struct WorkerStart {
    name: String,
    kind: SlotKind,
    queues: SharedQueues,
    callbacks: WorkerCallbacks,
}

fn start_named(job: WorkerStart) -> std::io::Result<JoinHandle<()>> {
    let WorkerStart {
        name,
        kind,
        queues,
        callbacks,
    } = job;
    std::thread::Builder::new()
        .name(name)
        .spawn(move || run_worker(kind, queues, callbacks))
}

fn walk_started(
    input: WalkInput,
    mut start: impl FnMut(WorkerStart) -> std::io::Result<JoinHandle<()>>,
) -> WalkReport {
    let initial = input.state;
    let mut state = input.state;
    let mut exit = 0i64;
    let mut requested = Vec::new();
    let queues = Arc::new(Mutex::new(SlotQueues {
        page: input.page_q.iter().cloned().collect(),
        image: input.img_q.iter().cloned().collect(),
    }));
    let before_page = input.page_q.clone();
    let before_img = input.img_q.clone();
    let mut live = LiveWorkers {
        names: Vec::new(),
        handles: Vec::new(),
    };
    let mut spawned_scan = 0i64;
    let mut spawned_download = 0i64;
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
            spawned_scan = spawn_workers(
                SlotKind::Scan,
                input.scan_count,
                &queues,
                &mut live,
                &mut start,
            );
            state = DOWNLOAD - 1;
        } else if state == SCAN_ARCHIVE {
            state = DOWNLOAD - 1;
        } else if state == DOWNLOAD {
            spawned_download = spawn_workers(
                SlotKind::Download,
                input.download_count,
                &queues,
                &mut live,
                &mut start,
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
    live.join();
    let (page_q, img_q) = {
        let guard = queues.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (
            guard.page.iter().cloned().collect::<Vec<_>>(),
            guard.image.iter().cloned().collect::<Vec<_>>(),
        )
    };
    WalkReport {
        worker_names: live.names.clone(),
        spawned_scan,
        spawned_download,
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

fn worker_name(kind: SlotKind, ordinal: i64) -> String {
    let prefix = match kind {
        SlotKind::Scan => "scan",
        SlotKind::Download => "down",
    };
    format!("{prefix}-{ordinal}")
}

fn spawn_workers(
    kind: SlotKind,
    count: i64,
    queues: &SharedQueues,
    live: &mut LiveWorkers,
    start: &mut impl FnMut(WorkerStart) -> std::io::Result<JoinHandle<()>>,
) -> i64 {
    let mut spawned = 0i64;
    for index in 0..count.max(0) {
        let requested = worker_name(kind, index + 1);
        match start(WorkerStart {
            name: requested,
            kind,
            queues: Arc::clone(queues),
            callbacks: default_callbacks(),
        }) {
            Ok(handle) => {
                let Some(reported) = handle.thread().name() else {
                    live.handles.push(handle);
                    break;
                };
                live.names.push(reported.to_string());
                live.handles.push(handle);
                spawned += 1;
            }
            Err(_) => break,
        }
    }
    spawned
}

type SharedQueues = Arc<Mutex<SlotQueues>>;

struct SlotQueues {
    page: VecDeque<String>,
    image: VecDeque<String>,
}

#[derive(Clone, Copy)]
struct WorkerCallbacks {
    scan_success: fn(&str, &mut SlotQueues),
    scan_failure: fn(&str, &mut SlotQueues),
    download_success: fn(&str, &mut SlotQueues),
    download_failure: fn(&str, &mut SlotQueues),
}

fn default_callbacks() -> WorkerCallbacks {
    WorkerCallbacks {
        scan_success: enqueue_scanned_image,
        scan_failure: ignore_callback,
        download_success: acknowledge_download,
        download_failure: ignore_callback,
    }
}

fn run_worker(kind: SlotKind, queues: SharedQueues, callbacks: WorkerCallbacks) {
    let mut queues = queues
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match kind {
        SlotKind::Scan => {
            let Some(page) = queues.page.pop_front() else {
                return;
            };
            if image_from_page(&page).is_some() {
                (callbacks.scan_success)(&page, &mut queues);
            } else {
                (callbacks.scan_failure)(&page, &mut queues);
            }
        }
        SlotKind::Download => {
            let Some(image) = queues.image.front().cloned() else {
                return;
            };
            if image.is_empty() {
                (callbacks.download_failure)(&image, &mut queues);
            } else {
                (callbacks.download_success)(&image, &mut queues);
            }
        }
    }
}

fn enqueue_scanned_image(page: &str, queues: &mut SlotQueues) {
    let Some(image) = image_from_page(page) else {
        return;
    };
    if queues.image.iter().any(|item| item == &image) {
        return;
    }
    queues.image.push_back(image);
}

fn acknowledge_download(image: &str, _queues: &mut SlotQueues) {
    let _ = image;
}

fn ignore_callback(item: &str, _queues: &mut SlotQueues) {
    let _ = item;
}

fn image_from_page(page_url: &str) -> Option<String> {
    let bytes = page_url.as_bytes();
    let mut index = 0;
    while index + 14 < bytes.len() {
        if bytes[index] == b'/'
            && bytes.get(index + 11) == Some(&b'/')
            && bytes[index + 1..index + 11]
                .iter()
                .all(|byte| byte.is_ascii_hexdigit() && byte.is_ascii_lowercase())
        {
            let rest = &page_url[index + 12..];
            if let Some(gid_end) = rest.find(|ch: char| !ch.is_ascii_digit()) {
                if gid_end > 0 && rest.as_bytes().get(gid_end) == Some(&b'-') {
                    let fid = &rest[gid_end + 1..];
                    let fid_len = fid
                        .chars()
                        .take_while(|ch| ch.is_ascii_digit())
                        .count();
                    if fid_len > 0 {
                        let digest = page_url[index + 1..index + 11].repeat(4);
                        let gid = &rest[..gid_end];
                        let fid = &fid[..fid_len];
                        return Some(format!(
                            "https://cdn.example/h/{digest}-{gid}-{fid}-1-jpg"
                        ));
                    }
                }
            }
        }
        index += 1;
    }
    None
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
        (
            Value::string("spawned_scan"),
            Value::Int(report.spawned_scan),
        ),
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

    fn input(state: i64) -> WalkInput {
        WalkInput {
            state,
            scan_count: 2,
            download_count: 3,
            url: "https://e-hentai.org/g/9/aaaaaaaaaa/".to_string(),
            total: 40,
            thumbnail_cnt: 20,
            page_q: vec!["https://decoy.example/page-not-enqueued".to_string()],
            img_q: vec!["https://decoy.example/image-not-enqueued".to_string()],
        }
    }

    #[test]
    fn failed_third_download_spawn_returns_only_live_handle_names() {
        let report = walk_started(input(9), |job| {
            let name = job.name.clone();
            let mut builder = std::thread::Builder::new().name(format!("live-{name}"));
            if name == "down-3" {
                builder = builder.stack_size(usize::MAX);
            }
            builder.spawn(move || run_worker(job.kind, job.queues, job.callbacks))
        });
        assert_eq!(report.worker_names, ["live-down-1", "live-down-2"]);
        assert!(
            !report
                .worker_names
                .iter()
                .any(|name| name.contains("down-3"))
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
    fn spawn_error_leaves_no_handle_to_name() {
        let failed = std::thread::Builder::new()
            .name("down-3".to_string())
            .stack_size(usize::MAX)
            .spawn(|| {
                let _ = std::thread::current().name();
            });
        assert!(failed.is_err());
    }

    #[test]
    fn live_handle_names_follow_workers_that_spawned() {
        let report = walk(input(4));
        assert_eq!(
            report.worker_names,
            ["scan-1", "scan-2", "down-1", "down-2", "down-3"]
        );
        assert_eq!(report.spawned_scan, 2);
        assert_eq!(report.spawned_download, 3);
        assert!(report.scan_urls.is_empty());
        assert!(report.download_urls.is_empty());
    }

    #[test]
    fn scan_callback_enqueues_an_image_url_that_is_not_the_request_queue() {
        let mut sample = input(4);
        sample.scan_count = 1;
        sample.download_count = 1;
        sample.page_q = vec!["https://e-hentai.org/s/bbbbbbbbbb/9-4".to_string()];
        sample.img_q =
            vec!["https://cdn.example/h/0123456789abcdef0123456789abcdef01234567-100-10-10-jpg"
                .to_string()];
        let report = walk(sample);
        let image = "https://cdn.example/h/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-9-4-1-jpg";
        assert_eq!(report.download_urls, [image]);
        assert_ne!(
            report.download_urls,
            ["https://cdn.example/h/0123456789abcdef0123456789abcdef01234567-100-10-10-jpg"]
        );
        assert_eq!(
            report.public_img_q,
            [
                "https://cdn.example/h/0123456789abcdef0123456789abcdef01234567-100-10-10-jpg",
                image,
            ]
        );
        assert!(report.public_page_q.is_empty());
    }
}
