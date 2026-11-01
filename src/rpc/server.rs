//! Native HTTP front for the original RPC module (`xeHentai/rpc.py`).
//!
//! The reference runs `BaseHTTPRequestHandler` (HTTP/1.0, connection close,
//! one trailing `\n` outside `Content-Length`). This server reproduces that
//! framing with a mature parser (`httparse`, already in the tree) over
//! `tokio` TCP: every response carries CORS, an explicit `Content-Length`,
//! the body, then `\n`, and the connection closes. There is no keep-alive,
//! like the reference. Strictness deltas forced by the codec (bounded
//! headers/bodies, rejected chunked or missing lengths) are recorded in
//! `tools/cases.json`, never hidden by a global comparator ignore.
//!
//! Request handling never holds an `App` borrow across an `.await`: network
//! bodies are fully read first, gallery/archive/thumbnail bytes are produced
//! synchronously under short borrows, and the login and static-cache fetches
//! move their state out of the borrow before awaiting.
#![allow(dead_code)]

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::{
    FileStream, FrozenPack, GalleryFail, GallerySnap, IO_CHUNK, MAX_THUMB_ENCODED,
    MAX_ZIP_IMAGE_ENTRY, OpenOutcome, Response, ResponseBody, RpcDispatcher, RpcRequest,
    ServerError, StaticCache, hash_link, jsonrpc_envelope, parse_jsonrpc, parse_thumb_query,
    probe_gallery_file, snapshot_gallery_file, thumbnail_with_limits,
};
use crate::r#const as consts;
use crate::core::App;
use crate::worker::HttpClock;

pub use super::{
    MAX_HEAD_BYTES, MAX_JSONRPC_BODY, STATIC_CACHE_NAME, StaticEntry, cors_404, fetch_static,
    options_response, reason_of,
};

/// Production login endpoint from `core.py:454`, reused by the RPC login.
pub const LOGIN_URL: &str = "https://forums.e-hentai.org/index.php?act=Login&CODE=01";

/// Login POST target. Production always uses [`LOGIN_URL`]; tests may point
/// one controlled local-login fixture at a loopback origin through
/// `XEH_TEST_LOGIN_URL` so the real RPC path, the real App login and real
/// HTTP bytes are exercised without any public network. The variable is only
/// honored in test builds and only names the origin — never credentials, and
/// business logic is never patched.
fn login_endpoint() -> String {
    #[cfg(test)]
    if let Ok(url) = std::env::var("XEH_TEST_LOGIN_URL")
        && !url.is_empty()
    {
        return url;
    }
    LOGIN_URL.to_string()
}

/// Synchronous per-request dispatch used by connection tasks and tests.
/// Gallery/archive/thumbnail/static-cache-hit bytes are produced here under
/// short borrows; only the static-network fetch awaits outside.
///
/// `runtime` is `Some` when the server is mounted on a live runtime: pause
/// and delete then drain in-flight worker retirement instead of mutating
/// task state directly. `None` keeps the direct `App` path for tests and
/// the compatibility probe.
#[allow(clippy::too_many_arguments)]
pub fn dispatch_request(
    app: &mut App,
    rpc: &mut RpcDispatcher,
    cache: &mut StaticCache,
    get_info: &dyn Fn() -> Value,
    request_line: &str,
    body: &[u8],
    secret: Option<&str>,
    runtime: Option<&crate::core::runtime::AppHandle>,
) -> RequestAction {
    let mut parts = request_line.splitn(2, ' ');
    let method = parts.next().unwrap_or("");
    let target = parts.next().unwrap_or("/");
    match method {
        "OPTIONS" => RequestAction::Ready(options_response()),
        "GET" => dispatch_get(cache, target, secret),
        "POST" => dispatch_post(app, rpc, get_info, target, body, secret, runtime),
        _ => RequestAction::Ready(Response {
            status: 501,
            reason: reason_of(501),
            mime: None,
            body: ResponseBody::Inline(Vec::new()),
        }),
    }
}

/// What a synchronous dispatch decided: either a complete response, an
/// async follow-up that the connection owner performs without any borrow, or
/// an owned media request whose snapshot is taken under a short borrow and
/// whose bytes stream with no borrow held.
#[derive(Debug)]
pub enum RequestAction {
    Ready(Response),
    FetchStatic {
        name: String,
    },
    Login {
        id: Value,
        user: String,
        pwd: String,
    },
    /// Runtime-mounted pause: the owner drains in-flight worker retirement
    /// through the same records public drains use (M4), then answers.
    DrainPause {
        id: Value,
        guid: String,
    },
    /// Runtime-mounted delete: workers are reaped before `del_task` runs.
    DrainDelete {
        id: Value,
        guid: String,
    },
    /// Owned media: the snapshot moves out under a short borrow; opening,
    /// validation, decode/build and streaming all run borrow-free.
    Media(MediaRequest),
}

/// One validated media route: the parsed path pieces plus the secret the
/// signature is checked against. No file is opened here.
#[derive(Debug)]
pub struct MediaRequest {
    pub kind: MediaKind,
    pub secret: Option<String>,
}

/// The two media routes, with their parsed path pieces.
#[derive(Debug)]
pub enum MediaKind {
    Image {
        guid: String,
        fid: String,
        query: String,
        presented: String,
    },
    Archive {
        guid: String,
        presented: String,
    },
}

fn path_matches(target: &str) -> bool {
    // `pathre = re.compile("/(?:jsonrpc|img/|zip/|static/|ui/$)")` with
    // `match`: a prefix alternative, `ui/` anchored at the end.
    if let Some(rest) = target.strip_prefix('/') {
        for alternative in ["jsonrpc", "img/", "zip/", "static/"] {
            if rest.starts_with(alternative) {
                return true;
            }
        }
        if rest == "ui/" {
            return true;
        }
    }
    false
}

fn dispatch_get(cache: &mut StaticCache, target: &str, secret: Option<&str>) -> RequestAction {
    if !path_matches(target) {
        return RequestAction::Ready(cors_404());
    }
    let (path, query) = match target.find('?') {
        Some(index) => (&target[..index], &target[index + 1..]),
        None => (target, ""),
    };
    if path.starts_with("/img/") {
        // `rpc.py:199-231`: segments from the URL path; extra filename
        // ignored. Short paths answer 400 with the default text/html MIME
        // in place. Query strictness (`rpc.py:200`) runs before the
        // signature check, exactly like the reference precedence; both run
        // before anything is opened. Dispatch returns the validated route;
        // the owner streams it borrow-free.
        let segments: Vec<&str> = path.split('/').collect();
        if segments.len() < 5 {
            return RequestAction::Ready(Response {
                status: 400,
                reason: reason_of(400),
                mime: Some("text/html".to_string()),
                body: ResponseBody::Inline(Vec::new()),
            });
        }
        if parse_thumb_query(query).is_err() {
            return RequestAction::Ready(Response {
                status: 400,
                reason: reason_of(400),
                mime: Some("text/html".to_string()),
                body: ResponseBody::Inline(Vec::new()),
            });
        }
        let guid = segments[3];
        let fid = segments[4];
        let presented = segments[2];
        // Signature check (`rpc.py:206-210`): mismatches answer 403 with the
        // default MIME and never touch disk.
        if presented != hash_link(secret, &format!("{guid}/{fid}")).as_str() {
            return RequestAction::Ready(Response {
                status: 403,
                reason: reason_of(403),
                mime: Some("text/html".to_string()),
                body: ResponseBody::Inline(Vec::new()),
            });
        }
        return RequestAction::Media(MediaRequest {
            kind: MediaKind::Image {
                guid: guid.to_string(),
                fid: fid.to_string(),
                query: query.to_string(),
                presented: presented.to_string(),
            },
            secret: secret.map(str::to_string),
        });
    }
    if path.starts_with("/zip/") {
        let segments: Vec<&str> = path.split('/').collect();
        if segments.len() < 5 {
            return RequestAction::Ready(Response {
                status: 400,
                reason: reason_of(400),
                mime: Some("text/html".to_string()),
                body: ResponseBody::Inline(Vec::new()),
            });
        }
        let guid = segments[3];
        let presented = segments[2];
        // Signature check (`rpc.py:240-244`): mismatches answer 403. An
        // empty guid can never present a valid signature, so it 403s here
        // exactly like the reference instead of reaching the archive lookup.
        if presented != hash_link(secret, guid).as_str() {
            return RequestAction::Ready(Response {
                status: 403,
                reason: reason_of(403),
                mime: Some("text/html".to_string()),
                body: ResponseBody::Inline(Vec::new()),
            });
        }
        return RequestAction::Media(MediaRequest {
            kind: MediaKind::Archive {
                guid: guid.to_string(),
                presented: presented.to_string(),
            },
            secret: secret.map(str::to_string),
        });
    }
    if target == "/ui/" || path.starts_with("/static/") {
        return static_action(cache, target);
    }
    // `do_GET` fallback (`rpc.py:301-304`): invalid-request JSON at 200 with
    // the default text/html MIME.
    let body = jsonrpc_envelope(&Value::Null, consts::ERR_RPC_INVALID_REQUEST, None, None);
    RequestAction::Ready(Response {
        status: 200,
        reason: reason_of(200),
        mime: Some("text/html".to_string()),
        body: ResponseBody::Inline(serde_json::to_vec(&body).unwrap_or_default()),
    })
}

fn text_html(status: u16) -> Response {
    Response {
        status,
        reason: reason_of(status),
        mime: Some("text/html".to_string()),
        body: ResponseBody::Inline(Vec::new()),
    }
}

fn gallery_fail(fail: GalleryFail) -> Response {
    // `rpc.py:211` unpacks `(None, None, None)` for an unknown guid
    // and `send_header("Content-Type", None)` coerces to the literal
    // string "None": byte-exact preservation of the observable wire.
    let mime = match fail {
        GalleryFail::NotFound => Some("None".to_string()),
        GalleryFail::BadRequest => Some("text/html".to_string()),
        GalleryFail::Server(_) => None,
    };
    Response {
        status: fail.status(),
        reason: reason_of(fail.status()),
        mime,
        body: ResponseBody::Inline(Vec::new()),
    }
}

fn task_fail(error: crate::task::TaskError) -> GalleryFail {
    match &error {
        crate::task::TaskError::UnsafePath { .. } => GalleryFail::NotFound,
        _ => GalleryFail::Server(error.to_string()),
    }
}

/// Owned media entry point: snapshot under a short borrow, then open,
/// validate, decode/build and stream with no borrow held.
pub async fn run_media(
    handle: crate::core::runtime::AppHandle,
    permits: Arc<tokio::sync::Semaphore>,
    request: MediaRequest,
) -> Response {
    match request.kind {
        MediaKind::Image {
            guid,
            fid,
            query,
            presented,
        } => {
            run_image(
                handle,
                permits,
                request.secret.as_deref(),
                &guid,
                &fid,
                &query,
                &presented,
            )
            .await
        }
        MediaKind::Archive { guid, presented } => {
            run_archive(
                handle,
                permits,
                request.secret.as_deref(),
                &guid,
                &presented,
            )
            .await
        }
    }
}

// ---------------------------------------------------------------------------
// Owned heavy media workers (M5 R2 B3.1/B3.3).
// ---------------------------------------------------------------------------

/// Cancellation flag for one media request. Dropping the request future
/// (disconnect, timeout, shutdown) sets the flag through [`CancelOnDrop`];
/// the owned worker checks it between chunks and aborts early. A truly
/// blocked native operation keeps the worker (and its permit) until it is
/// released; cancellation alone never claims cleanup.
type CancelFlag = Arc<std::sync::atomic::AtomicBool>;

struct CancelOnDrop(CancelFlag);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// TEST-ONLY worker gate: hold an owned heavy worker between permit
// acquisition and heavy I/O, deterministically on the worker thread.
// ---------------------------------------------------------------------------

/// TEST-ONLY handle that holds a heavy worker until the test releases it.
/// Armed per gallery-root marker (unique per test task); a no-op unless
/// armed. Production builds contain no gate at all.
#[cfg(test)]
pub(crate) struct MediaGate {
    pub started: tokio::sync::oneshot::Receiver<()>,
    pub release: std::sync::mpsc::Sender<()>,
}

/// One armed TEST-ONLY media gate: the worker start signal plus the test
/// release channel, keyed by gallery-root marker.
#[cfg(test)]
type MediaGateArmed = (
    String,
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

#[cfg(test)]
static MEDIA_GATES: std::sync::Mutex<Vec<MediaGateArmed>> = std::sync::Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn arm_media_worker_gate(marker: &str) -> MediaGate {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    MEDIA_GATES
        .lock()
        .unwrap()
        .push((marker.to_string(), started_tx, release_rx));
    MediaGate {
        started: started_rx,
        release: release_tx,
    }
}

/// Block the worker until the test releases its gate, or until the request
/// is cancelled. Returns true when cancelled (the worker must abort and
/// release its permit through normal return).
#[cfg(test)]
fn media_worker_hold(marker: &str, cancel: &CancelFlag) -> bool {
    use std::sync::atomic::Ordering;
    let channels = {
        let mut gates = MEDIA_GATES.lock().unwrap();
        gates
            .iter()
            .position(|(armed, _, _)| armed == marker)
            .map(|index| gates.remove(index))
    };
    let Some((_, started, release)) = channels else {
        return false;
    };
    let _ = started.send(());
    loop {
        if cancel.load(Ordering::Relaxed) {
            return true;
        }
        match release.recv_timeout(std::time::Duration::from_millis(20)) {
            Ok(()) => return cancel.load(Ordering::Relaxed),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return true,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
}

/// Everything the blocking image worker needs: all owned and `Send`, no
/// `App` borrow, no VM, no file descriptor. The heavy permit moves in with
/// the job and is released only when the worker finishes, even if the
/// awaiting request future is dropped first.
struct ImageJob {
    snap: GallerySnap,
    width: Option<u32>,
    height: Option<u32>,
    parent_id: Option<crate::task::DirIdentity>,
    gate: String,
    cancel: CancelFlag,
}

#[allow(clippy::too_many_arguments)]
async fn run_image(
    handle: crate::core::runtime::AppHandle,
    permits: Arc<tokio::sync::Semaphore>,
    secret: Option<&str>,
    guid: &str,
    fid: &str,
    query: &str,
    presented: &str,
) -> Response {
    // Query strictness (`rpc.py:200`) runs before the signature check, so
    // malformed queries answer 400 exactly like the reference precedence.
    let (width, height) = match parse_thumb_query(query) {
        Ok(sizes) => sizes,
        Err(_) => return text_html(400),
    };
    // Signature check (`rpc.py:206-210`): the URL signature must equal the
    // real product signature; mismatches answer 403 with the default MIME.
    let expected = hash_link(secret, &format!("{guid}/{fid}"));
    if presented != expected.as_str() {
        return text_html(403);
    }
    // Snapshot under a SHORT borrow; released before any disk read. The
    // snapshot carries the gallery root FD identity plus the sibling-parent
    // identity so the worker can bind both opens.
    let snap = handle.borrow_app(|app| snapshot_gallery_file(app, guid, fid));
    let snap = match snap {
        Ok(snap) => snap,
        Err(fail) => return gallery_fail(fail),
    };
    let parent_id = snap.root.parent().and_then(crate::task::dir_identity);
    // The permit is acquired before any file open, read, ZIP decompress,
    // spool write or decode, and moves into the worker: it stays held until
    // the worker actually finishes, never released by dropping the waiter.
    let permit = match Arc::clone(&permits).acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return Response {
                status: 500,
                reason: reason_of(500),
                mime: None,
                body: ResponseBody::Inline(Vec::new()),
            };
        }
    };
    let cancel: CancelFlag = Arc::new(false.into());
    let guard = CancelOnDrop(Arc::clone(&cancel));
    let job = ImageJob {
        gate: snap.root.to_string_lossy().into_owned(),
        snap,
        width,
        height,
        parent_id,
        cancel,
    };
    let joined = tokio::task::spawn_blocking(move || image_worker(job, permit)).await;
    drop(guard);
    match joined {
        Err(_) => Response {
            status: 500,
            reason: reason_of(500),
            mime: None,
            body: ResponseBody::Inline(Vec::new()),
        },
        Ok(response) => response,
    }
}

/// Blocking image worker: confined open, bounded read, ZIP decompress
/// straight to a private anonymous spool, thumbnail decode. All budgets
/// (declared, decoded, CRC) resolve before any response headers exist.
fn image_worker(job: ImageJob, _permit: tokio::sync::OwnedSemaphorePermit) -> Response {
    use std::sync::atomic::Ordering;
    if job.cancel.load(Ordering::Relaxed) {
        return gallery_fail(GalleryFail::Server("cancelled".to_string()));
    }
    #[cfg(test)]
    if media_worker_hold(&job.gate, &job.cancel) {
        return gallery_fail(GalleryFail::Server("cancelled".to_string()));
    }
    match probe_gallery_file(&job.snap) {
        Err(_) => gallery_fail(GalleryFail::NotFound),
        Ok(OpenOutcome::Denied) => gallery_fail(GalleryFail::NotFound),
        Ok(OpenOutcome::Missing) => zip_fallback_worker(&job),
        Ok(OpenOutcome::File(opened)) => {
            if job.width.is_none() && job.height.is_none() {
                // Passthrough streams the owned file; no Vec at all.
                return Response {
                    status: 200,
                    reason: reason_of(200),
                    mime: Some(opened.mime),
                    body: ResponseBody::Stream(FileStream {
                        file: Some(opened.file),
                        len: opened.len,
                        cleanup: None,
                    }),
                };
            }
            // Bounded source read, then decode under the same permit.
            let bytes = {
                use std::io::Read;
                let mut limited = opened.file.take(MAX_THUMB_ENCODED as u64 + 1);
                let mut bytes = Vec::new();
                match limited.read_to_end(&mut bytes) {
                    Ok(_) => bytes,
                    Err(_) => return gallery_fail(GalleryFail::NotFound),
                }
            };
            if bytes.len() > MAX_THUMB_ENCODED {
                return Response {
                    status: 500,
                    reason: reason_of(500),
                    mime: None,
                    body: ResponseBody::Inline(Vec::new()),
                };
            }
            match thumbnail_with_limits(&bytes, job.width, job.height, super::MAX_THUMB_DECODED) {
                Ok((resized, mime)) => Response {
                    status: 200,
                    reason: reason_of(200),
                    mime: Some(mime),
                    body: ResponseBody::Inline(resized),
                },
                Err(super::ThumbError::Passthrough) => Response {
                    status: 200,
                    reason: reason_of(200),
                    mime: Some(opened.mime),
                    body: ResponseBody::Inline(bytes),
                },
                Err(super::ThumbError::BadRequest(_)) => Response {
                    status: 400,
                    reason: reason_of(400),
                    mime: None,
                    body: ResponseBody::Inline(Vec::new()),
                },
                Err(super::ThumbError::Decode(_)) => Response {
                    status: 500,
                    reason: reason_of(500),
                    mime: None,
                    body: ResponseBody::Inline(Vec::new()),
                },
            }
        }
    }
}

/// Reference ZIP fallback (`rpc.py:212-226`) inside the worker: the sibling
/// opens confined, the declared entry size faces the 512 MiB budget, then
/// the entry decompresses DIRECTLY to a private anonymous spool in 64 KiB
/// pieces (never a full 512 MiB Vec) with cooperative cancel checks; the
/// actual byte count and the CRC (surfaced as a read error at entry end)
/// resolve before any headers exist. Thumbnails from ZIP entries read
/// bounded (64 MiB + 1; more would fail the decode budget anyway) and
/// decode under the same permit.
fn zip_fallback_worker(job: &ImageJob) -> Response {
    use std::io::{Read, Seek, Write};
    use std::sync::atomic::Ordering;
    let suffixed = format!("{}.zip", job.snap.root.display());
    let suffixed = Path::new(&suffixed);
    let (parent, leaf) = match (
        suffixed.parent(),
        suffixed.file_name().and_then(|n| n.to_str()),
    ) {
        (Some(parent), Some(leaf)) => (parent, leaf),
        _ => return gallery_fail(GalleryFail::NotFound),
    };
    let file =
        match crate::task::open_confined_regular(parent, job.parent_id, leaf, &job.snap.reserved) {
            Ok(Some(file)) => file,
            Ok(None) | Err(_) => return gallery_fail(GalleryFail::NotFound),
        };
    let mut archive = match zip::ZipArchive::new(file) {
        Ok(archive) => archive,
        Err(_) => return gallery_fail(GalleryFail::NotFound),
    };
    let entry = match archive.by_name(&job.snap.name) {
        Ok(entry) => entry,
        Err(_) => return gallery_fail(GalleryFail::NotFound),
    };
    if entry.size() > MAX_ZIP_IMAGE_ENTRY {
        return gallery_fail(GalleryFail::NotFound);
    }
    if job.width.is_none() && job.height.is_none() {
        // Passthrough: decompress straight to a private anonymous spool
        // OUTSIDE the gallery/archive namespace (never a gallery dot-file),
        // then stream it. The spool FD is owned by the response and dies
        // with it (close/cancel/error/CRC failure); nothing is ever
        // published where `read_dir` could see it.
        let mut spool = match tempfile::tempfile() {
            Ok(spool) => spool,
            Err(_) => return gallery_fail(GalleryFail::Server("spool".to_string())),
        };
        let mut entry = entry;
        let mut chunk = vec![0u8; IO_CHUNK];
        let mut actual: u64 = 0;
        loop {
            if job.cancel.load(Ordering::Relaxed) {
                return gallery_fail(GalleryFail::Server("cancelled".to_string()));
            }
            let n = match entry.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => return gallery_fail(GalleryFail::NotFound),
            };
            actual += n as u64;
            if actual > MAX_ZIP_IMAGE_ENTRY {
                return gallery_fail(GalleryFail::NotFound);
            }
            if spool.write_all(&chunk[..n]).is_err() {
                return gallery_fail(GalleryFail::Server("spool".to_string()));
            }
        }
        if spool.flush().is_err() {
            return gallery_fail(GalleryFail::Server("spool".to_string()));
        }
        let mime = Path::new(&job.snap.name)
            .extension()
            .and_then(|ext| ext.to_str())
            .unwrap_or("")
            .to_string();
        if spool.rewind().is_err() {
            return gallery_fail(GalleryFail::NotFound);
        }
        return Response {
            status: 200,
            reason: reason_of(200),
            mime: Some(super::image_mime(&mime).to_string()),
            body: ResponseBody::Stream(FileStream {
                file: Some(spool),
                len: actual,
                cleanup: None,
            }),
        };
    }
    // Thumbnail from a ZIP entry: bounded read, then decode under the permit.
    let mut limited = entry.take(MAX_THUMB_ENCODED as u64 + 1);
    let mut bytes = Vec::new();
    let read = (|| -> Result<(), GalleryFail> {
        let mut chunk = vec![0u8; IO_CHUNK];
        loop {
            if job.cancel.load(Ordering::Relaxed) {
                return Err(GalleryFail::Server("cancelled".to_string()));
            }
            match limited.read(&mut chunk) {
                Ok(0) => return Ok(()),
                Ok(n) => bytes.extend_from_slice(&chunk[..n]),
                Err(_) => return Err(GalleryFail::NotFound),
            }
            if bytes.len() > MAX_THUMB_ENCODED {
                return Ok(());
            }
        }
    })();
    if read.is_err() {
        return gallery_fail(GalleryFail::NotFound);
    }
    if bytes.len() > MAX_THUMB_ENCODED {
        return Response {
            status: 500,
            reason: reason_of(500),
            mime: None,
            body: ResponseBody::Inline(Vec::new()),
        };
    }
    let mime = Path::new(&job.snap.name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_string();
    let mime = super::image_mime(&mime).to_string();
    match thumbnail_with_limits(&bytes, job.width, job.height, super::MAX_THUMB_DECODED) {
        Ok((resized, mime)) => Response {
            status: 200,
            reason: reason_of(200),
            mime: Some(mime),
            body: ResponseBody::Inline(resized),
        },
        Err(super::ThumbError::Passthrough) => Response {
            status: 200,
            reason: reason_of(200),
            mime: Some(mime),
            body: ResponseBody::Inline(bytes),
        },
        Err(super::ThumbError::BadRequest(_)) => Response {
            status: 400,
            reason: reason_of(400),
            mime: None,
            body: ResponseBody::Inline(Vec::new()),
        },
        Err(super::ThumbError::Decode(_)) => Response {
            status: 500,
            reason: reason_of(500),
            mime: None,
            body: ResponseBody::Inline(Vec::new()),
        },
    }
}

async fn run_archive(
    handle: crate::core::runtime::AppHandle,
    permits: Arc<tokio::sync::Semaphore>,
    secret: Option<&str>,
    guid: &str,
    presented: &str,
) -> Response {
    // Signature check (`rpc.py:240-244`): mismatches answer 403. An empty
    // guid can never present a valid signature, so it 403s here exactly like
    // the reference instead of reaching the archive lookup.
    let expected = hash_link(secret, guid);
    if presented != expected.as_str() {
        return text_html(403);
    }
    // Snapshot under a SHORT borrow; the writer never touches task state.
    // The reserved set and the sibling-parent identity for the final
    // confined open are captured here as well (all owned, Send).
    let snapshot = handle.borrow_app(|app| {
        app.task(guid)
            .ok_or(GalleryFail::NotFound)
            .and_then(|task| task.archive_snapshot().map_err(task_fail))
    });
    let snapshot = match snapshot {
        Ok(snapshot) => snapshot,
        Err(fail) => {
            return Response {
                status: fail.status(),
                reason: reason_of(fail.status()),
                mime: None,
                body: ResponseBody::Inline(Vec::new()),
            };
        }
    };
    let (reserved, parent_id) = handle.borrow_app(|app| {
        let base = app.base_dir().to_path_buf();
        let log_path = {
            let log_path = Path::new(&app.cfg.log_path);
            if log_path.is_absolute() {
                log_path.to_path_buf()
            } else {
                base.join(log_path)
            }
        };
        (
            vec![
                base.join(crate::core::SESSION_FILE),
                log_path,
                base.join(crate::config::CONFIG_FILE),
                base.join(crate::config::LEGACY_CONFIG_FILE),
            ],
            snapshot.arc.parent().and_then(crate::task::dir_identity),
        )
    });
    // Heavy build on the blocking pool behind the heavy permit. The task
    // file lock is taken inside that thread, like the M4 worker path. The
    // permit moves into the worker and comes back with the result, so it
    // stays owned until the worker actually finishes.
    let cancel: CancelFlag = Arc::new(false.into());
    let guard = CancelOnDrop(Arc::clone(&cancel));
    let permit = match Arc::clone(&permits).acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            return Response {
                status: 500,
                reason: reason_of(500),
                mime: None,
                body: ResponseBody::Inline(Vec::new()),
            };
        }
    };
    let prepared = tokio::task::spawn_blocking(move || {
        let outcome = crate::task::prepare_archive_snapshot(&snapshot, false);
        (permit, outcome)
    })
    .await;
    let (permit, prepared) = match prepared {
        Err(_) => {
            return Response {
                status: 500,
                reason: reason_of(500),
                mime: None,
                body: ResponseBody::Inline(Vec::new()),
            };
        }
        Ok(done) => done,
    };
    let prepared = match prepared {
        Err(error) => {
            let fail = task_fail(error);
            return Response {
                status: fail.status(),
                reason: reason_of(fail.status()),
                mime: None,
                body: ResponseBody::Inline(Vec::new()),
            };
        }
        Ok(prepared) => prepared,
    };
    use crate::task::PreparedArchive;
    let path = match prepared {
        PreparedArchive::Existing(path) => path,
        PreparedArchive::Staged(stage) => {
            // Re-validate the exact task before publishing, then commit the
            // bounded namespace step on the owner thread.
            let live = handle.borrow_app(|app| app.task(guid).is_some());
            if !live {
                crate::task::discard_prepared(PreparedArchive::Staged(stage));
                return Response {
                    status: 404,
                    reason: reason_of(404),
                    mime: None,
                    body: ResponseBody::Inline(Vec::new()),
                };
            }
            match crate::task::commit_prepared(PreparedArchive::Staged(stage)) {
                Err(error) => {
                    let fail = task_fail(error);
                    return Response {
                        status: fail.status(),
                        reason: reason_of(fail.status()),
                        mime: None,
                        body: ResponseBody::Inline(Vec::new()),
                    };
                }
                Ok(committed) => committed.path,
            }
        }
    };
    // The committed sibling opens through the anchored confined open on
    // the blocking pool, never a raw check-then-reopen: the permit covers
    // the final open too and stays owned until the open actually finishes.
    let opened = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let (parent, leaf) = match (
            path.parent(),
            path.file_name().and_then(|name| name.to_str()),
        ) {
            (Some(parent), Some(leaf)) => (parent.to_path_buf(), leaf.to_string()),
            _ => return Err(GalleryFail::NotFound),
        };
        let file = match crate::task::open_confined_regular(&parent, parent_id, &leaf, &reserved) {
            Ok(Some(file)) => file,
            Ok(None) | Err(_) => return Err(GalleryFail::NotFound),
        };
        let len = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok((file, len))
    })
    .await;
    drop(guard);
    let (file, len) = match opened {
        Ok(Ok(found)) => found,
        _ => {
            return Response {
                status: 404,
                reason: reason_of(404),
                mime: None,
                body: ResponseBody::Inline(Vec::new()),
            };
        }
    };
    Response {
        status: 200,
        reason: reason_of(200),
        mime: Some("application/zip".to_string()),
        body: ResponseBody::Stream(FileStream {
            file: Some(file),
            len,
            cleanup: None,
        }),
    }
}

fn static_action(cache: &mut StaticCache, target: &str) -> RequestAction {
    let name = if target == "/ui/" { "/" } else { target };
    // Fresh cache wins without touching the network (`rpc.py:259-266`).
    if let Some(entry) = cache.fresh(name) {
        let entry = entry.clone();
        return RequestAction::Ready(Response {
            status: 200,
            reason: reason_of(200),
            mime: Some(entry.mime),
            body: ResponseBody::Inline(entry.body),
        });
    }
    // A stale entry triggers a synchronous-equivalent origin revalidation:
    // the async owner fetches, then serves 200/304/fallback exactly like the
    // reference (`rpc.py:267-299`) without holding any borrow over I/O.
    RequestAction::FetchStatic {
        name: name.to_string(),
    }
}

fn dispatch_post(
    app: &mut App,
    rpc: &mut RpcDispatcher,
    get_info: &dyn Fn() -> Value,
    target: &str,
    body: &[u8],
    secret: Option<&str>,
    runtime: Option<&crate::core::runtime::AppHandle>,
) -> RequestAction {
    // `do_POST` carries the same `path_filter` as `do_GET` (`rpc.py:322`):
    // only the jsonrpc/img/zip/static/ui routes serve the API.
    if !path_matches(target) {
        return RequestAction::Ready(cors_404());
    }
    let req = match parse_jsonrpc(body) {
        Ok(req) => req,
        Err((status, code)) => {
            let envelope = jsonrpc_envelope(&Value::Null, code, None, None);
            return RequestAction::Ready(Response {
                status,
                reason: reason_of(status),
                mime: Some("application/json-rpc".to_string()),
                body: ResponseBody::Inline(serde_json::to_vec(&envelope).unwrap_or_default()),
            });
        }
    };
    // One normalized, singly-authenticated path: the RSS selector maps every
    // accepted wire spelling (camelCase and snake_case) to one canonical
    // method, the optional first token is authenticated and consumed once,
    // and login/drain/ordinary calls all run prepared. Lookup runs before
    // auth, like the reference (`rpc.py:343-370`).
    let prepared = match super::prepare_call(rpc, &req, secret) {
        Ok(prepared) => prepared,
        Err(outcome) => {
            let envelope = jsonrpc_envelope(&req.id, outcome.code, outcome.result, outcome.message);
            return RequestAction::Ready(Response {
                status: outcome.status,
                reason: reason_of(outcome.status),
                mime: Some("application/json-rpc".to_string()),
                body: ResponseBody::Inline(serde_json::to_vec(&envelope).unwrap_or_default()),
            });
        }
    };
    // The server secret still reaches `execute_prepared` so getImage signs
    // URLs with it; the token itself was already consumed above.
    if prepared.method == "login_exhentai" {
        return dispatch_login(rpc, &req, &prepared);
    }
    if runtime.is_some() && (prepared.method == "pause_task" || prepared.method == "del_task") {
        return dispatch_drain(rpc, &req, &prepared);
    }
    let outcome = super::execute_prepared(
        app,
        rpc,
        &prepared.method,
        &prepared.args,
        &req.kwargs,
        secret,
        Some(get_info()),
    );
    let envelope = jsonrpc_envelope(&req.id, outcome.code, outcome.result, outcome.message);
    RequestAction::Ready(Response {
        status: outcome.status,
        reason: reason_of(outcome.status),
        mime: Some("application/json-rpc".to_string()),
        body: ResponseBody::Inline(serde_json::to_vec(&envelope).unwrap_or_default()),
    })
}

/// Runtime-mounted pause/delete: check arity and guid types synchronously,
/// then let the connection owner drain worker retirement before mutating.
/// The canonical RSS method selects the drain; the raw wire spelling is only
/// kept for the reference-shaped error text.
fn dispatch_drain(
    rpc: &mut RpcDispatcher,
    req: &RpcRequest,
    prepared: &super::PreparedCall,
) -> RequestAction {
    let arity = match rpc.guid_arity(prepared.args.len() as i64, req.kwargs.len() as i64) {
        Ok(arity) => arity,
        Err(error) => return ready_exec_error(&req.id, error.to_string()),
    };
    if arity.outcome != "ok" {
        return ready_exec_error(
            &req.id,
            format!("{}() has invalid arguments", req.method_raw),
        );
    }
    let Some(guid) = prepared.args.first().and_then(|value| value.as_str()) else {
        return ready_exec_error(
            &req.id,
            format!("{}() argument 'guid' must be a string", req.method_raw),
        );
    };
    if prepared.method == "pause_task" {
        RequestAction::DrainPause {
            id: req.id.clone(),
            guid: guid.to_string(),
        }
    } else {
        RequestAction::DrainDelete {
            id: req.id.clone(),
            guid: guid.to_string(),
        }
    }
}

fn ready_exec_error(id: &Value, message: String) -> RequestAction {
    let envelope = jsonrpc_envelope(id, consts::ERR_RPC_EXEC_ERROR, None, Some(message));
    RequestAction::Ready(Response {
        status: 500,
        reason: reason_of(500),
        mime: Some("application/json-rpc".to_string()),
        body: ResponseBody::Inline(serde_json::to_vec(&envelope).unwrap_or_default()),
    })
}

fn dispatch_login(
    rpc: &mut RpcDispatcher,
    req: &RpcRequest,
    prepared: &super::PreparedCall,
) -> RequestAction {
    // The token was already authenticated and consumed by `prepare_call`;
    // these are the real credentials, never a second USERNAME check.
    let arity = match rpc.login_arity(prepared.args.len() as i64, req.kwargs.len() as i64) {
        Ok(arity) => arity,
        Err(error) => {
            let envelope = jsonrpc_envelope(
                &req.id,
                consts::ERR_RPC_EXEC_ERROR,
                None,
                Some(error.to_string()),
            );
            return RequestAction::Ready(Response {
                status: 500,
                reason: reason_of(500),
                mime: Some("application/json-rpc".to_string()),
                body: ResponseBody::Inline(serde_json::to_vec(&envelope).unwrap_or_default()),
            });
        }
    };
    if arity.outcome != "ok" {
        let envelope = jsonrpc_envelope(
            &req.id,
            consts::ERR_RPC_EXEC_ERROR,
            None,
            Some(format!("login_exhentai() {}", arity.outcome)),
        );
        return RequestAction::Ready(Response {
            status: 500,
            reason: reason_of(500),
            mime: Some("application/json-rpc".to_string()),
            body: ResponseBody::Inline(serde_json::to_vec(&envelope).unwrap_or_default()),
        });
    }
    let user = prepared
        .args
        .first()
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let pwd = prepared
        .args
        .get(1)
        .and_then(|value| value.as_str())
        .unwrap_or("");
    // Non-string credentials: the reference passes them to `requests`, which
    // raises `TypeError` outside the RPC catch and drops the connection; the
    // candidate answers 500 instead of inventing form encoding.
    if prepared
        .args
        .first()
        .is_some_and(|value| !value.is_string())
        || prepared.args.get(1).is_some_and(|value| !value.is_string())
    {
        let envelope = jsonrpc_envelope(
            &req.id,
            consts::ERR_RPC_EXEC_ERROR,
            None,
            Some("login_exhentai() credentials must be strings".to_string()),
        );
        return RequestAction::Ready(Response {
            status: 500,
            reason: reason_of(500),
            mime: Some("application/json-rpc".to_string()),
            body: ResponseBody::Inline(serde_json::to_vec(&envelope).unwrap_or_default()),
        });
    }
    RequestAction::Login {
        id: req.id.clone(),
        user: user.to_string(),
        pwd: pwd.to_string(),
    }
}

/// Per-connection owned dispatch: one dispatcher VM, one login VM and the
/// shared static cache handle. Everything here is `!Send` by construction
/// (VM ownership) and runs on the single `LocalSet` thread. The heavy-media
/// permit semaphore is `Send` and shared server-wide through this handle.
pub struct SharedRpc {
    pub rpc: RpcDispatcher,
    pub cache: StaticCache,
    pub pack: Option<FrozenPack>,
    pub login_filters: crate::filters::Filters,
    pub secret: Option<String>,
    pub static_origin: String,
    pub clock: Arc<dyn HttpClock>,
    pub media_permits: Arc<tokio::sync::Semaphore>,
}

impl SharedRpc {
    pub fn new(
        resource_root: &Path,
        cache_dir: &Path,
        clock: Arc<dyn HttpClock>,
        secret: Option<String>,
        static_origin: String,
    ) -> Result<Self, ServerError> {
        let rpc = RpcDispatcher::load(resource_root)?;
        let login_filters = crate::filters::Filters::load(resource_root)
            .map_err(|error| ServerError::Shape(error.to_string()))?;
        let cache = StaticCache::load(&cache_dir.join(STATIC_CACHE_NAME), Arc::clone(&clock));
        let pack = FrozenPack::load(resource_root);
        Ok(Self {
            rpc,
            cache,
            pack,
            login_filters,
            secret,
            static_origin,
            clock,
            media_permits: Arc::new(tokio::sync::Semaphore::new(super::MAX_HEAVY_WORKERS)),
        })
    }

    pub fn request(
        &mut self,
        app: &mut App,
        get_info: &dyn Fn() -> Value,
        request_line: &str,
        body: &[u8],
        runtime: Option<&crate::core::runtime::AppHandle>,
    ) -> RequestAction {
        let secret = self.secret.clone();
        dispatch_request(
            app,
            &mut self.rpc,
            &mut self.cache,
            get_info,
            request_line,
            body,
            secret.as_deref(),
            runtime,
        )
    }
}

/// Read one HTTP request head plus optional body from the stream. Every
/// read carries a per-operation idle bound: a client that goes quiet for
/// longer than [`super::READ_IDLE_SECS`] is dropped instead of holding a
/// connection slot forever. Continuous transfers never hit the bound.
pub async fn read_request<S>(
    stream: &mut S,
) -> Result<(String, Vec<(String, String)>, Vec<u8>), u16>
where
    S: AsyncRead + Unpin,
{
    let idle = std::time::Duration::from_secs(super::READ_IDLE_SECS);
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if head.len() > MAX_HEAD_BYTES {
            return Err(413);
        }
        let n = tokio::time::timeout(idle, stream.read(&mut buf))
            .await
            .map_err(|_| 400u16)?
            .map_err(|_| 400u16)?;
        if n == 0 {
            return Err(400);
        }
        head.extend_from_slice(&buf[..n]);
        if head.windows(4).any(|w| w == b"\r\n\r\n") {
            break;
        }
    }
    let mut headers = [httparse::EMPTY_HEADER; 64];
    let mut parsed = httparse::Request::new(&mut headers);
    let offset = parsed.parse(&head).map_err(|_| 400u16)?;
    if offset.is_partial() {
        return Err(400);
    }
    let method = parsed.method.unwrap_or("").to_string();
    let path = parsed.path.unwrap_or("/").to_string();
    let mut header_list = Vec::new();
    for header in parsed.headers.iter() {
        header_list.push((
            header.name.to_ascii_lowercase(),
            String::from_utf8_lossy(header.value).into_owned(),
        ));
    }
    let body_start = offset.unwrap();
    let mut body = head[body_start..].to_vec();
    let content_length = header_list
        .iter()
        .find(|(name, _)| name == "content-length")
        .and_then(|(_, value)| value.trim().parse::<usize>().ok());
    if method == "POST" {
        let Some(length) = content_length else {
            // The reference indexes `Content-Length` unconditionally and
            // drops the connection; the candidate answers 400.
            return Err(400);
        };
        if length > MAX_JSONRPC_BODY {
            return Err(413);
        }
        while body.len() < length {
            let n = tokio::time::timeout(idle, stream.read(&mut buf))
                .await
                .map_err(|_| 400u16)?
                .map_err(|_| 400u16)?;
            if n == 0 {
                return Err(400);
            }
            body.extend_from_slice(&buf[..n]);
            if body.len() > MAX_JSONRPC_BODY {
                return Err(413);
            }
        }
        body.truncate(length);
    }
    Ok((format!("{method} {path}"), header_list, body))
}

/// Serve one connection: real bytes in, real bytes out. Async follow-ups
/// (static fetch, login) run here with no `App` borrow held. `handle` is the
/// runtime App handle: `get_info` is snapshotted before any mutable borrow
/// (no reentrant borrow), and pause/delete drain worker retirement.
pub async fn serve_one<S>(
    stream: &mut S,
    handle: crate::core::runtime::AppHandle,
    shared: Rc<RefCell<SharedRpc>>,
    static_origin: String,
) -> Result<(), ServerError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (request_line, _headers, body) = match read_request(stream).await {
        Ok(ok) => ok,
        Err(_) => return Ok(()),
    };
    let info = handle.get_info();
    let action = {
        let mut shared_ref = shared.borrow_mut();
        handle.borrow_app_mut(|app| {
            shared_ref.request(app, &|| info.clone(), &request_line, &body, Some(&handle))
        })
    };
    let response = match action {
        RequestAction::Ready(response) => response,
        RequestAction::FetchStatic { name } => {
            let cached = shared.borrow().cache.stale(&name).cloned();
            match fetch_static(&static_origin, &name, cached.as_ref()).await {
                Ok(None) => {
                    // 304: the origin confirms our stale entry; re-stamp it.
                    let mut shared_ref = shared.borrow_mut();
                    shared_ref.cache.refresh_index(&name);
                    match shared_ref.cache.stale(&name).cloned() {
                        Some(entry) => Response {
                            status: 200,
                            reason: reason_of(200),
                            mime: Some(entry.mime),
                            body: ResponseBody::Inline(entry.body),
                        },
                        None => {
                            // 304 with no local entry left: pack, else the
                            // reference body. Unreachable in practice (304
                            // implies we sent If-Modified-Since).
                            match shared_ref.pack.as_ref().and_then(|pack| pack.get(&name)) {
                                Some((bytes, mime)) => Response {
                                    status: 200,
                                    reason: reason_of(200),
                                    mime: Some(mime.to_string()),
                                    body: ResponseBody::Inline(bytes.to_vec()),
                                },
                                None => {
                                    let body = jsonrpc_envelope(
                                        &Value::Null,
                                        consts::ERR_RPC_INVALID_REQUEST,
                                        None,
                                        None,
                                    );
                                    Response {
                                        status: 200,
                                        reason: reason_of(200),
                                        mime: Some("text/html".to_string()),
                                        body: ResponseBody::Inline(
                                            serde_json::to_vec(&body).unwrap_or_default(),
                                        ),
                                    }
                                }
                            }
                        }
                    }
                }
                Ok(Some((body, mime, last_modified))) => {
                    let mut shared_ref = shared.borrow_mut();
                    let now = shared_ref.clock.now();
                    let clear = name == "/";
                    shared_ref.cache.store(
                        &name,
                        StaticEntry {
                            body: body.clone(),
                            mime: mime.clone(),
                            fetched: now,
                            last_modified,
                        },
                        clear,
                    );
                    Response {
                        status: 200,
                        reason: reason_of(200),
                        mime: Some(mime),
                        body: ResponseBody::Inline(body),
                    }
                }
                Err(_) => {
                    // Origin fetch failed: stale fallback, else the frozen
                    // offline pack, else the reference invalid-request body
                    // (`rpc.py:274-299`).
                    let shared_ref = shared.borrow();
                    match shared_ref.cache.stale(&name).cloned() {
                        Some(entry) => Response {
                            status: 200,
                            reason: reason_of(200),
                            mime: Some(entry.mime),
                            body: ResponseBody::Inline(entry.body),
                        },
                        None => match shared_ref.pack.as_ref().and_then(|pack| pack.get(&name)) {
                            Some((bytes, mime)) => Response {
                                status: 200,
                                reason: reason_of(200),
                                mime: Some(mime.to_string()),
                                body: ResponseBody::Inline(bytes.to_vec()),
                            },
                            None => {
                                let body = jsonrpc_envelope(
                                    &Value::Null,
                                    consts::ERR_RPC_INVALID_REQUEST,
                                    None,
                                    None,
                                );
                                Response {
                                    status: 200,
                                    reason: reason_of(200),
                                    mime: Some("text/html".to_string()),
                                    body: ResponseBody::Inline(
                                        serde_json::to_vec(&body).unwrap_or_default(),
                                    ),
                                }
                            }
                        },
                    }
                }
            }
        }
        RequestAction::Login { id, user, pwd } => {
            run_login(handle.clone(), shared.clone(), id, user, pwd).await
        }
        RequestAction::DrainPause { id, guid } => {
            // Read-only pre-check mirrors `App::pause_task` refusals; the
            // drain itself re-validates before mutating.
            let refusal = handle.borrow_app(|app| match app.task(&guid) {
                None => Some(consts::ERR_TASK_NOT_FOUND),
                Some(task)
                    if matches!(
                        task.state,
                        consts::TASK_STATE_PAUSED
                            | consts::TASK_STATE_FINISHED
                            | consts::TASK_STATE_FAILED
                    ) =>
                {
                    Some(consts::ERR_TASK_CANNOT_PAUSE)
                }
                _ => None,
            });
            match refusal {
                Some(code) => method_error_envelope(id, code),
                None => match handle.pause_task_and_wait(&guid).await {
                    Err(error) => drain_envelope(id, Err(error)),
                    Ok(_) => drain_envelope(id, Ok((0, None))),
                },
            }
        }
        RequestAction::DrainDelete { id, guid } => {
            let refusal = handle.borrow_app(|app| match app.task(&guid) {
                None => Some(consts::ERR_TASK_NOT_FOUND),
                Some(task)
                    if consts::TASK_STATE_PAUSED < task.state
                        && task.state < consts::TASK_STATE_FINISHED =>
                {
                    Some(consts::ERR_DELETE_RUNNING_TASK)
                }
                _ => None,
            });
            match refusal {
                Some(code) => method_error_envelope(id, code),
                None => match handle.delete_task_and_wait(&guid).await {
                    Err(error) => drain_envelope(id, Err(error)),
                    Ok(_) => drain_envelope(id, Ok((0, None))),
                },
            }
        }
        RequestAction::Media(media) => {
            // The snapshot moves out under a short borrow; opening,
            // validation, decode/build and streaming all run borrow-free.
            let permits = Arc::clone(&shared.borrow().media_permits);
            run_media(handle.clone(), permits, media).await
        }
    };
    write_response(stream, response, true).await?;
    Ok(())
}

/// Envelope for a drained pause/delete: method errors stay HTTP 200 with an
/// error envelope, exactly like the direct path.
fn method_error_envelope(id: Value, code: i64) -> Response {
    let envelope = jsonrpc_envelope(&id, code, None, None);
    Response {
        status: 200,
        reason: reason_of(200),
        mime: Some("application/json-rpc".to_string()),
        body: ResponseBody::Inline(serde_json::to_vec(&envelope).unwrap_or_default()),
    }
}

fn drain_envelope(
    id: Value,
    outcome: Result<(i64, Option<String>), crate::core::AppError>,
) -> Response {
    let (code, result, message) = match outcome {
        Err(error) => (consts::ERR_RPC_EXEC_ERROR, None, Some(error.to_string())),
        Ok((code, _)) if code != 0 => (code, None, None),
        Ok(_) => (0, Some(Value::String(String::new())), None),
    };
    let envelope = jsonrpc_envelope(&id, code, result, message);
    let status = if code == consts::ERR_RPC_EXEC_ERROR {
        500
    } else {
        200
    };
    Response {
        status,
        reason: reason_of(status),
        mime: Some("application/json-rpc".to_string()),
        body: ResponseBody::Inline(serde_json::to_vec(&envelope).unwrap_or_default()),
    }
}

/// Async login flow reusing the real endpoint and the real RSS login filter.
/// The session leaves the `App` borrow before the network await and returns
/// before the outcome is applied; nothing is held across the await.
async fn run_login(
    handle: crate::core::runtime::AppHandle,
    shared: Rc<RefCell<SharedRpc>>,
    id: Value,
    user: String,
    pwd: String,
) -> Response {
    struct Outcome {
        envelope: Value,
    }
    let outcome: Outcome = {
        // Synchronous prologue: already-logged-in short-circuits exactly like
        // `App::login_exhentai` (`core.py:443-444` returns None there, which
        // drops the reference connection; the candidate answers success).
        let already = handle.borrow_app(|app| app.has_login());
        if already {
            Outcome {
                envelope: jsonrpc_envelope(&id, 0, Some(Value::Bool(true)), None),
            }
        } else {
            // Move one session out of the borrow, then await without it.
            let mut session = handle.borrow_app_mut(|app| {
                let policy =
                    crate::util::get_proxy_policy(app.cfg.proxy_image_only, app.cfg.proxy_image);
                app.take_http_session(policy)
            });
            let data = vec![
                ("UserName".to_string(), user.clone()),
                ("returntype".to_string(), "8".to_string()),
                ("CookieDate".to_string(), "1".to_string()),
                ("b".to_string(), "d".to_string()),
                ("bt".to_string(), "pone".to_string()),
                ("PassWord".to_string(), pwd.clone()),
            ];
            let fetched = session
                .request("POST", &login_endpoint(), Some(data), false)
                .await;
            // Restore the session before touching App state again.
            let applied = handle.borrow_app_mut(|app| {
                app.restore_http_session(session);
                match fetched {
                    Err(error) => jsonrpc_envelope(
                        &id,
                        consts::ERR_RPC_EXEC_ERROR,
                        None,
                        Some(error.to_string()),
                    ),
                    Ok(outcome) => {
                        let view =
                            crate::core::App::page_view_for_rpc(&outcome.response, false, false);
                        let mut shared_ref = shared.borrow_mut();
                        match shared_ref.login_filters.login_outcome(&view) {
                            Err(error) => jsonrpc_envelope(
                                &id,
                                consts::ERR_RPC_EXEC_ERROR,
                                None,
                                Some(error.to_string()),
                            ),
                            Ok(login) => {
                                if let Some(cookies) = login.cookies {
                                    let cookie_text = cookies
                                        .iter()
                                        .map(|(name, value)| format!("{name}={value}"))
                                        .collect::<Vec<_>>()
                                        .join("; ");
                                    app.set_cookie(&cookie_text);
                                    let _ = app.save_session();
                                    jsonrpc_envelope(
                                        &id,
                                        0,
                                        Some(Value::Bool(app.has_login())),
                                        None,
                                    )
                                } else if let Some(message) = login.fail_message {
                                    let _ = app.logger_mut().warning(&message);
                                    jsonrpc_envelope(
                                        &id,
                                        consts::ERR_TASK_NOT_FOUND,
                                        None,
                                        Some(message),
                                    )
                                } else if let Some(kind) = login.exception {
                                    jsonrpc_envelope(
                                        &id,
                                        consts::ERR_RPC_EXEC_ERROR,
                                        None,
                                        Some(format!("login raised {kind}")),
                                    )
                                } else {
                                    jsonrpc_envelope(
                                        &id,
                                        consts::ERR_RPC_EXEC_ERROR,
                                        None,
                                        Some("login produced no outcome".to_string()),
                                    )
                                }
                            }
                        }
                    }
                }
            });
            Outcome { envelope: applied }
        }
    };
    Response {
        status: 200,
        reason: reason_of(200),
        mime: Some("application/json-rpc".to_string()),
        body: ResponseBody::Inline(serde_json::to_vec(&outcome.envelope).unwrap_or_default()),
    }
}

/// Write one response with the exact reference framing: headers carrying the
/// real `Content-Length`, the body, then `\\n` outside the length. Inline
/// bodies go out in one bounded write; streams move in [`IO_CHUNK`] pieces
/// with backpressure, so a slow client holds one buffer and one file handle.
/// Every write carries a per-operation stall bound; legit large files stream
/// continuously and never hit it.
async fn write_response<S>(
    stream: &mut S,
    response: Response,
    cors: bool,
) -> Result<(), ServerError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let idle = std::time::Duration::from_secs(super::WRITE_IDLE_SECS);
    let head = response.head_bytes(cors);
    tokio::time::timeout(idle, stream.write_all(&head))
        .await
        .map_err(|_| ServerError::Io("response head write stalled".to_string()))?
        .map_err(ServerError::from)?;
    match response.body {
        ResponseBody::Inline(bytes) => {
            tokio::time::timeout(idle, stream.write_all(&bytes))
                .await
                .map_err(|_| ServerError::Io("response body write stalled".to_string()))?
                .map_err(ServerError::from)?;
        }
        ResponseBody::Stream(mut stream_body) => {
            let Some(std_file) = stream_body.file.take() else {
                return Err(ServerError::Io("media stream has no file".to_string()));
            };
            let mut file = tokio::fs::File::from_std(std_file);
            let mut remaining = stream_body.len;
            let mut chunk = vec![0u8; super::IO_CHUNK];
            use tokio::io::AsyncReadExt;
            while remaining > 0 {
                let want = (remaining as usize).min(chunk.len());
                file.read_exact(&mut chunk[..want])
                    .await
                    .map_err(|_| ServerError::Io("media stream ended early".to_string()))?;
                tokio::time::timeout(idle, stream.write_all(&chunk[..want]))
                    .await
                    .map_err(|_| ServerError::Io("media stream write stalled".to_string()))?
                    .map_err(ServerError::from)?;
                remaining -= want as u64;
            }
            // The spool cleanup (if any) drops here with the stream body.
        }
    }
    // `rpc.py:319/396`: the newline sits outside Content-Length.
    tokio::time::timeout(idle, stream.write_all(b"\n"))
        .await
        .map_err(|_| ServerError::Io("response tail write stalled".to_string()))?
        .map_err(ServerError::from)?;
    stream.shutdown().await?;
    Ok(())
}

/// Bind an owned listener. Bind errors are real: the server is never marked
/// started without a listener.
pub async fn bind(interface: &str, port: u16) -> Result<TcpListener, std::io::Error> {
    TcpListener::bind((interface, port)).await
}

/// Accept loop with owned shutdown: closing the sender drains the listener
/// and every connection task it owns before returning. All connection tasks
/// run `spawn_local` on this thread, so the `!Send` App/dispatcher borrows
/// never cross threads.
///
/// Connection accounting is explicit and bounded: at most
/// [`super::MAX_ACTIVE_CONN`] live connections; past that the loop defers
/// (the listener backlog holds newcomers) until a slot frees. Finished
/// connection records are reaped as they complete, so sequential requests
/// never grow the owner set.
pub async fn serve_until_closed(
    listener: TcpListener,
    mut shutdown: watch::Receiver<bool>,
    handle: crate::core::runtime::AppHandle,
    shared: Rc<RefCell<SharedRpc>>,
    static_origin: String,
) -> Result<(), ServerError> {
    use tokio::task::JoinSet;
    let mut owned = JoinSet::new();
    let mut active: usize = 0;
    loop {
        if active == 0 {
            tokio::select! {
                incoming = listener.accept() => {
                    let (mut stream, _) = incoming?;
                    let handle = handle.clone();
                    let shared = Rc::clone(&shared);
                    let static_origin = static_origin.clone();
                    owned.spawn_local(async move {
                        let _ = serve_one(&mut stream, handle, shared, static_origin).await;
                    });
                    active += 1;
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
            }
        } else if active >= super::MAX_ACTIVE_CONN {
            // Deferred: no new accept until a connection record is reaped.
            tokio::select! {
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                finished = owned.join_next() => {
                    if finished.is_some() {
                        active = active.saturating_sub(1);
                    }
                }
            }
        } else {
            tokio::select! {
                incoming = listener.accept() => {
                    let (mut stream, _) = incoming?;
                    let handle = handle.clone();
                    let shared = Rc::clone(&shared);
                    let static_origin = static_origin.clone();
                    owned.spawn_local(async move {
                        let _ = serve_one(&mut stream, handle, shared, static_origin).await;
                    });
                    active += 1;
                }
                _ = shutdown.changed() => {
                    if *shutdown.borrow() {
                        break;
                    }
                }
                finished = owned.join_next() => {
                    if finished.is_some() {
                        active = active.saturating_sub(1);
                    }
                }
            }
        }
    }
    while owned.join_next().await.is_some() {}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::{
        MAX_THUMB_DECODED, ThumbError, check_pixels, read_archive, read_gallery_file,
        refuse_unsafe_name, thumbnail, thumbnail_with_limits,
    };
    use serde_json::Map as JsonMap;

    pub(crate) fn test_app() -> App {
        let root = tempfile::tempdir().expect("root");
        let root_path = root.keep();
        let config = crate::config::Config::default();
        let logger = crate::util::logger::Logger::with_output(Box::new(Vec::new()), false);
        App::new(config, logger, &root_path, &root_path).expect("app")
    }

    pub(crate) fn dispatcher() -> RpcDispatcher {
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(super::super::RPC_RSS),
        )
        .expect("handlers.rss must exist");
        RpcDispatcher::from_source(&source).expect("handlers.rss must compile")
    }

    fn call(
        app: &mut App,
        rpc: &mut RpcDispatcher,
        method: &str,
        args: Vec<Value>,
        kwargs: JsonMap<String, Value>,
    ) -> (u16, Value) {
        let req = RpcRequest {
            method_raw: method.to_string(),
            id: Value::from(1),
            args,
            kwargs,
        };
        let outcome = super::super::execute(app, rpc, &req, None, Some(Value::Null));
        let envelope = jsonrpc_envelope(&req.id, outcome.code, outcome.result, outcome.message);
        (outcome.status, envelope)
    }

    #[test]
    fn wire_envelope_reports_parse_and_unknown_method() {
        assert_eq!(
            parse_jsonrpc(b"{oops").unwrap_err(),
            (400, consts::ERR_RPC_PARSE_ERROR)
        );
        assert_eq!(
            parse_jsonrpc(b"{\"method\":null,\"id\":1}").unwrap_err(),
            (400, consts::ERR_RPC_INVALID_REQUEST)
        );
        assert_eq!(
            parse_jsonrpc(b"{\"method\":\"xeH.getInfo\"}").unwrap_err(),
            (400, consts::ERR_RPC_INVALID_REQUEST)
        );
        assert!(parse_jsonrpc(b"{\"method\":\"xeH.getInfo\",\"id\":0}").is_ok());
        // Non-pair params are an invalid request, not a dropped connection.
        assert_eq!(
            parse_jsonrpc(b"{\"method\":\"xeH.getInfo\",\"id\":1,\"params\":{}}").unwrap_err(),
            (400, consts::ERR_RPC_INVALID_REQUEST)
        );
    }

    #[test]
    fn get_config_hides_rpc_keys_and_reports_arity() {
        let mut app = test_app();
        let mut rpc = dispatcher();
        let (status, envelope) = call(&mut app, &mut rpc, "xeH.getConfig", vec![], JsonMap::new());
        assert_eq!(status, 200);
        let result = envelope.get("result").expect("result");
        assert!(result.get("dir").is_some());
        assert!(result.get("rpc_port").is_none());
        assert!(result.get("rpc_secret").is_none());
        assert!(result.get("rpc_interface").is_none());
        assert!(result.get("rpc_open_browser").is_none());
        let (status, envelope) = call(
            &mut app,
            &mut rpc,
            "xeH.getConfig",
            vec![Value::from(1)],
            JsonMap::new(),
        );
        assert_eq!(status, 500);
        assert!(envelope.get("error").is_some());
    }

    #[test]
    fn update_config_without_proxy_is_a_read() {
        let mut app = test_app();
        let mut rpc = dispatcher();
        let before = app.cfg.dir.clone();
        let mut kwargs = JsonMap::new();
        kwargs.insert("dir".to_string(), Value::String("changed".to_string()));
        let (status, envelope) = call(&mut app, &mut rpc, "xeH.updateConfig", vec![], kwargs);
        assert_eq!(status, 200);
        // No proxy in the payload: no-op, the directory is untouched.
        assert_eq!(app.cfg.dir, before);
        assert_eq!(
            envelope
                .get("result")
                .and_then(|r| r.get("dir"))
                .and_then(Value::as_str),
            Some(before.as_str())
        );
    }

    #[test]
    fn unknown_method_is_404_and_unauthorized_is_403() {
        let mut app = test_app();
        let mut rpc = dispatcher();
        let req = RpcRequest {
            method_raw: "xeH.nope".to_string(),
            id: Value::from(1),
            args: vec![],
            kwargs: JsonMap::new(),
        };
        let outcome = super::super::execute(&mut app, &mut rpc, &req, None, None);
        assert_eq!(outcome.status, 404);
        let req = RpcRequest {
            method_raw: "xeH._private".to_string(),
            id: Value::from(1),
            args: vec![],
            kwargs: JsonMap::new(),
        };
        let outcome = super::super::execute(&mut app, &mut rpc, &req, None, None);
        assert_eq!(outcome.status, 404);
        let req = RpcRequest {
            method_raw: "xeH.getConfig".to_string(),
            id: Value::from(7),
            args: vec![],
            kwargs: JsonMap::new(),
        };
        let outcome = super::super::execute(&mut app, &mut rpc, &req, Some("s3cret"), None);
        assert_eq!(outcome.status, 403);
        let envelope = jsonrpc_envelope(&req.id, outcome.code, outcome.result, outcome.message);
        assert_eq!(envelope.get("id"), Some(&Value::from(7)));
    }

    #[test]
    fn thumbnail_query_parsing_matches_reference_strictness() {
        assert_eq!(parse_thumb_query(""), Ok((None, None)));
        assert_eq!(parse_thumb_query("w=100"), Ok((Some(100), None)));
        assert_eq!(parse_thumb_query("w=100&h=50"), Ok((Some(100), Some(50))));
        assert!(parse_thumb_query("w").is_err());
        assert!(parse_thumb_query("w=1=2").is_err());
        assert!(parse_thumb_query("w=abc").is_err());
        assert!(parse_thumb_query("w=0").is_err());
        assert!(parse_thumb_query("w=-3").is_err());
    }

    #[test]
    fn gallery_names_reject_traversal() {
        assert!(refuse_unsafe_name("1.jpg").is_ok());
        assert!(refuse_unsafe_name("../x.jpg").is_err());
        assert!(refuse_unsafe_name("/abs.jpg").is_err());
        assert!(refuse_unsafe_name("..").is_err());
        assert!(refuse_unsafe_name("a\\b.jpg").is_err());
    }

    #[test]
    fn pixel_bound_rejects_huge_dimensions() {
        assert!(check_pixels(100, 100).is_ok());
        assert!(check_pixels(100_000, 100_000).is_err());
    }

    #[test]
    fn get_image_returns_real_signed_urls_and_serves_them() {
        use crate::rpc::hash_link;
        let mut app = test_app();
        let mut rpc = dispatcher();
        let png = png_bytes(16, 16);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        // task_with_image maps fid 5 to the file and sets total 3; this
        // test needs the plain fidpad layout over nine fids instead.
        {
            let task = app.task_mut(&guid).expect("task");
            task.meta.insert("total".to_string(), Value::from(9));
            use crate::task::FidKey;
            task.renamed_map
                .shift_remove(&FidKey::Text("5".to_string()));
        }
        let (status, envelope) = call(
            &mut app,
            &mut rpc,
            "xeH.getImage",
            vec![Value::String(guid.clone())],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
        let urls = envelope
            .get("result")
            .and_then(Value::as_array)
            .expect("urls");
        assert_eq!(urls.len(), 9);
        let first = urls[0].as_str().expect("url");
        assert!(first.starts_with("/img/"), "{first}");
        assert!(first.ends_with("/1.jpg"), "{first}");
        // The embedded signature is the real product signature.
        let sign = first.split('/').nth(2).expect("sign");
        assert_eq!(sign, hash_link(None, &format!("{guid}/1")));
        // A tampered signature is refused without touching disk.
        let tampered = first.replacen(sign, "00000000", 1);
        let refused = dispatch_get(
            &mut StaticCache::load(
                Path::new("/nonexistent-webui-cache.json"),
                Arc::new(crate::worker::RealClock),
            ),
            &tampered,
            None,
        );
        match refused {
            RequestAction::Ready(response) => assert_eq!(response.status, 403),
            other => panic!("expected ready refusal, got {other:?}"),
        }
        // Serving the signed URL returns the exact file bytes.
        let (status, mime, bytes) = serve_media_bytes(app, None, first);
        assert_eq!(status, 200);
        assert_eq!(mime.as_deref(), Some("image/jpeg"));
        assert_eq!(bytes, png);
    }

    #[test]
    fn get_image_signs_urls_with_the_configured_secret() {
        use crate::rpc::hash_link;
        let mut app = test_app();
        let mut rpc = dispatcher();
        let png = png_bytes(16, 16);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let req = RpcRequest {
            method_raw: "xeH.getImage".to_string(),
            id: Value::from(1),
            args: vec![
                Value::String("token:s3cret".to_string()),
                Value::String(guid.clone()),
            ],
            kwargs: JsonMap::new(),
        };
        let outcome = super::super::execute(&mut app, &mut rpc, &req, Some("s3cret"), None);
        assert_eq!(outcome.status, 200);
        let urls = outcome.result.expect("urls");
        let first = urls[0].as_str().expect("url");
        let sign = first.split('/').nth(2).expect("sign");
        assert_eq!(sign, hash_link(Some("s3cret"), &format!("{guid}/1")));
        assert_ne!(sign, hash_link(None, &format!("{guid}/1")));
        let cache_path = tempfile::tempdir().expect("cache").keep();
        // The same URL is not valid on a server without that secret.
        let unsigned = dispatch_get(
            &mut StaticCache::load(
                &cache_path.join("webui-cache.json"),
                Arc::new(crate::worker::RealClock),
            ),
            first,
            None,
        );
        match unsigned {
            RequestAction::Ready(response) => assert_eq!(response.status, 403),
            other => panic!("expected a refusal, got {other:?}"),
        }
        let (status, _, bytes) = serve_media_bytes(app, Some("s3cret".to_string()), first);
        assert_eq!(status, 200);
        assert_eq!(bytes, png);
    }

    #[test]
    fn signed_gallery_paths_validate_real_signatures() {
        use crate::rpc::hash_link;
        fn status_of(action: RequestAction) -> u16 {
            match action {
                RequestAction::Ready(response) => response.status,
                other => panic!("expected a ready gallery response, got {other:?}"),
            }
        }
        let mut app = test_app();
        let png = png_bytes(8, 8);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        // Mismatched and empty signatures answer 403 without touching disk.
        let image = dispatch_get(
            &mut StaticCache::load(
                Path::new("/nonexistent-webui-cache.json"),
                Arc::new(crate::worker::RealClock),
            ),
            "/img/00000000/guid1/1",
            None,
        );
        assert_eq!(status_of(image), 403);
        let archive = dispatch_get(
            &mut StaticCache::load(
                Path::new("/nonexistent-webui-cache.json"),
                Arc::new(crate::worker::RealClock),
            ),
            "/zip/00000000/guid1/gallery.zip",
            None,
        );
        assert_eq!(status_of(archive), 403);
        // The real product signature is accepted: md5(secret-or-empty +
        // "-xehentai-" + uri), first 8 hex, exactly like rpc.py:79-83.
        assert_eq!(hash_link(None, "guid1/1"), hash_link(None, "guid1/1"));
        assert_eq!(hash_link(None, "guid1/1").len(), 8);
        assert_ne!(hash_link(None, "guid1/1"), hash_link(None, "guid1/2"));
        assert_ne!(hash_link(Some("s"), "guid1/1"), hash_link(None, "guid1/1"));
        // A valid /img signature serves the exact file bytes.
        let image_path = format!("/img/{}/{guid}/1", hash_link(None, &format!("{guid}/1")));
        // A valid /img signature serves the exact file bytes.
        let (status, mime, bytes) = serve_media_bytes(app, None, &image_path);
        assert_eq!(status, 200);
        assert_eq!(mime.as_deref(), Some("image/jpeg"));
        assert_eq!(bytes, png);
        // A valid /zip signature streams the built archive bytes.
        let mut app = test_app();
        let zip_guid = task_with_image(&mut app, "1.jpg", &png_bytes(8, 8));
        let zip_path = format!("/zip/{}/{zip_guid}/gallery.zip", hash_link(None, &zip_guid));
        let (status, mime, bytes) = serve_media_bytes(app, None, &zip_path);
        assert_eq!(status, 200);
        assert_eq!(mime.as_deref(), Some("application/zip"));
        assert!(bytes.starts_with(b"PK"));
    }

    // --- task lifecycle through the real dispatcher -------------------------

    fn add_task_with_url(app: &mut App, rpc: &mut RpcDispatcher, url: &str) -> (u16, Value) {
        let mut kwargs = JsonMap::new();
        kwargs.insert("dir".to_string(), Value::String("downloads".to_string()));
        call(
            app,
            rpc,
            "xeH.addTask",
            vec![Value::String(url.to_string())],
            kwargs,
        )
    }

    #[test]
    fn task_lifecycle_codes_match_reference() {
        let mut app = test_app();
        let mut rpc = dispatcher();
        let (status, envelope) =
            add_task_with_url(&mut app, &mut rpc, "http://e-hentai.org/g/123/abcdef0123/");
        assert_eq!(status, 200);
        let guid = envelope
            .get("result")
            .and_then(Value::as_str)
            .expect("guid")
            .to_string();
        assert!(!guid.is_empty());
        // Pause twice: second refuses with TASK_STATE 1106.
        let (status, envelope) = call(
            &mut app,
            &mut rpc,
            "xeH.pauseTask",
            vec![Value::String(guid.clone())],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
        assert!(envelope.get("result").is_some());
        let (status, envelope) = call(
            &mut app,
            &mut rpc,
            "xeH.pauseTask",
            vec![Value::String(guid.clone())],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
        assert_eq!(
            envelope
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64),
            Some(consts::ERR_TASK_CANNOT_PAUSE)
        );
        // Resume a paused task, then refuse a second resume (1107).
        let (status, _) = call(
            &mut app,
            &mut rpc,
            "xeH.resumeTask",
            vec![Value::String(guid.clone())],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
        let (status, envelope) = call(
            &mut app,
            &mut rpc,
            "xeH.resumeTask",
            vec![Value::String(guid.clone())],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
        assert_eq!(
            envelope
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64),
            Some(consts::ERR_TASK_CANNOT_RESUME)
        );
        // Delete a running (waiting) task refuses with 1105.
        let (status, envelope) = call(
            &mut app,
            &mut rpc,
            "xeH.delTask",
            vec![Value::String(guid.clone())],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
        assert_eq!(
            envelope
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64),
            Some(consts::ERR_DELETE_RUNNING_TASK)
        );
        // Unknown guid answers 1101 at HTTP 200, like the reference.
        let (status, envelope) = call(
            &mut app,
            &mut rpc,
            "xeH.delTask",
            vec![Value::String("nonexistent".to_string())],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
        assert_eq!(
            envelope
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64),
            Some(consts::ERR_TASK_NOT_FOUND)
        );
    }

    #[test]
    fn set_cookie_and_session_roundtrip() {
        let mut app = test_app();
        let mut rpc = dispatcher();
        assert!(!app.has_login());
        let (status, envelope) = call(
            &mut app,
            &mut rpc,
            "xeH.setCookie",
            vec![Value::String(
                "ipb_member_id=42; ipb_pass_hash=39d0c523".to_string(),
            )],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
        assert!(envelope.get("result").is_some());
        assert!(app.has_login());
        assert_eq!(app.cookie("ipb_member_id"), Some("42"));
        let (status, _) = call(
            &mut app,
            &mut rpc,
            "xeH.saveSession",
            vec![],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
        assert!(app.base_dir().join("h.json").is_file());
        let (status, _) = call(
            &mut app,
            &mut rpc,
            "xeH.loadSession",
            vec![],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
    }

    #[test]
    fn update_config_proxy_nodir_isolated_applies_and_returns_config() {
        // Fresh app, distinct proxy plus another config change, no earlier
        // proxy case: the narrow missing-dir repair applies the requested
        // effects, keeps the current dir and answers 200 with the real
        // post-update getConfig view.
        let mut app = test_app();
        let mut rpc = dispatcher();
        let before_dir = app.cfg.dir.clone();
        let before_tree = task_tree(app.base_dir());
        let mut kwargs = JsonMap::new();
        kwargs.insert(
            "proxy".to_string(),
            Value::Array(vec![Value::String("socks5://127.0.0.1:1081".to_string())]),
        );
        kwargs.insert("download_thread_cnt".to_string(), Value::from(9));
        let (status, envelope) = call(&mut app, &mut rpc, "xeH.updateConfig", vec![], kwargs);
        assert_eq!(status, 200);
        let result = envelope.get("result").expect("result");
        assert_eq!(
            result.get("proxy"),
            Some(&Value::Array(vec![Value::String(
                "socks5://127.0.0.1:1081".to_string()
            )]))
        );
        assert_eq!(result.get("download_thread_cnt"), Some(&Value::from(9)));
        // Same effects land in the App: pool learns the distinct member,
        // the other change sticks, the directory is untouched.
        assert_eq!(app.proxy_count(), 1);
        let members: Vec<String> = app
            .proxy_pool()
            .expect("pool")
            .lock()
            .expect("pool")
            .proxies
            .keys()
            .cloned()
            .collect();
        assert_eq!(members, vec!["socks5://127.0.0.1:1081".to_string()]);
        assert_eq!(app.cfg.dir, before_dir);
        assert_eq!(app.cfg.download_thread_cnt, 9);
        assert_eq!(task_tree(app.base_dir()), before_tree);
    }

    #[test]
    fn update_config_proxy_empty_isolated_keeps_pool_semantics() {
        // Empty proxy with the dir omitted: the cfg change still applies,
        // the absent pool stays absent, and the view answers 200.
        let mut app = test_app();
        let mut rpc = dispatcher();
        let mut kwargs = JsonMap::new();
        kwargs.insert("proxy".to_string(), Value::Array(vec![]));
        kwargs.insert("download_thread_cnt".to_string(), Value::from(9));
        let (status, envelope) = call(&mut app, &mut rpc, "xeH.updateConfig", vec![], kwargs);
        assert_eq!(status, 200);
        assert_eq!(
            envelope
                .get("result")
                .and_then(|result| result.get("proxy")),
            Some(&Value::Array(vec![]))
        );
        assert_eq!(app.proxy_count(), 0);
        assert!(app.proxy_pool().is_none());
        assert_eq!(app.cfg.download_thread_cnt, 9);
    }

    #[test]
    fn update_config_proxy_existing_pool_is_additive() {
        // A pre-seeded pool gains the distinct member; both survive with
        // insertion order and the other change applies on top.
        let mut app = test_app();
        let mut rpc = dispatcher();
        let mut seed = JsonMap::new();
        seed.insert(
            "proxy".to_string(),
            Value::Array(vec![Value::String("socks5://127.0.0.1:1079".to_string())]),
        );
        seed.insert("dir".to_string(), Value::String(app.cfg.dir.clone()));
        let (status, _) = call(&mut app, &mut rpc, "xeH.updateConfig", vec![], seed);
        assert_eq!(status, 200);
        assert_eq!(app.proxy_count(), 1);
        let mut kwargs = JsonMap::new();
        kwargs.insert(
            "proxy".to_string(),
            Value::Array(vec![Value::String("socks5://127.0.0.1:1081".to_string())]),
        );
        kwargs.insert("download_thread_cnt".to_string(), Value::from(9));
        let (status, _) = call(&mut app, &mut rpc, "xeH.updateConfig", vec![], kwargs);
        assert_eq!(status, 200);
        assert_eq!(app.proxy_count(), 2);
        let members: Vec<String> = app
            .proxy_pool()
            .expect("pool")
            .lock()
            .expect("pool")
            .proxies
            .keys()
            .cloned()
            .collect();
        assert_eq!(
            members,
            vec![
                "socks5://127.0.0.1:1079".to_string(),
                "socks5://127.0.0.1:1081".to_string()
            ]
        );
        assert_eq!(app.cfg.download_thread_cnt, 9);
    }

    #[test]
    fn update_config_with_proxy_applies_and_dir_without_proxy_is_noop() {
        let mut app = test_app();
        let mut rpc = dispatcher();
        // Proxy present: the core update runs (pool learns the entry) and the
        // public view is returned.
        let mut kwargs = JsonMap::new();
        kwargs.insert(
            "proxy".to_string(),
            Value::Array(vec![Value::String("socks5://127.0.0.1:1080".to_string())]),
        );
        let (status, envelope) = call(&mut app, &mut rpc, "xeH.updateConfig", vec![], kwargs);
        assert_eq!(status, 200);
        assert!(envelope.get("result").is_some());
        assert_eq!(app.proxy_count(), 1);
        // Unknown keys are rejected instead of being silently stored: the
        // reference `dict.update` kept them; the typed candidate refuses.
        let mut kwargs = JsonMap::new();
        kwargs.insert(
            "proxy".to_string(),
            Value::Array(vec![Value::String("socks5://127.0.0.1:1080".to_string())]),
        );
        kwargs.insert("bogus_key".to_string(), Value::from(1));
        let (status, envelope) = call(&mut app, &mut rpc, "xeH.updateConfig", vec![], kwargs);
        assert_eq!(status, 500);
        assert!(envelope.get("error").is_some());
    }

    #[test]
    fn list_tasks_levels_negation_and_invalid() {
        let mut app = test_app();
        let mut rpc = dispatcher();
        let (status, envelope) =
            add_task_with_url(&mut app, &mut rpc, "http://e-hentai.org/g/123/abcdef0123/");
        assert_eq!(status, 200);
        let guid = envelope
            .get("result")
            .and_then(Value::as_str)
            .expect("guid")
            .to_string();
        // Fresh tasks wait; move it to the download state the default
        // level selects, like a running worker would. A second waiting task
        // proves the negation selector.
        app.task_mut(&guid).expect("task").state = consts::TASK_STATE_DOWNLOAD;
        let (status, _) =
            add_task_with_url(&mut app, &mut rpc, "http://e-hentai.org/g/456/abcdef0123/");
        assert_eq!(status, 200);
        // Default level lists the waiting task with hidden fields dropped.
        let (status, envelope) = call(&mut app, &mut rpc, "xeH.listTasks", vec![], JsonMap::new());
        assert_eq!(status, 200);
        let tasks = envelope
            .get("result")
            .and_then(Value::as_array)
            .expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(
            tasks[0].get("guid").and_then(Value::as_str),
            Some(guid.as_str())
        );
        for hidden in [
            "reload_map",
            "duplicate_map",
            "renamed_map",
            "logger",
            "img_q",
            "page_q",
        ] {
            assert!(tasks[0].get(hidden).is_none(), "{hidden}");
        }
        // Negation selects everything but the level.
        let (status, envelope) = call(
            &mut app,
            &mut rpc,
            "xeH.listTasks",
            vec![Value::String("!download".to_string())],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
        assert_eq!(
            envelope
                .get("result")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        let mut kwargs = JsonMap::new();
        kwargs.insert("level".to_string(), Value::String("finished".to_string()));
        let (status, envelope) = call(&mut app, &mut rpc, "xeH.listTasks", vec![], kwargs);
        assert_eq!(status, 200);
        assert_eq!(
            envelope
                .get("result")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );
        // Invalid level answers 1104 at HTTP 200.
        let (status, envelope) = call(
            &mut app,
            &mut rpc,
            "xeH.listTasks",
            vec![Value::String("bogus".to_string())],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
        assert_eq!(
            envelope
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64),
            Some(consts::ERR_TASK_LEVEL_UNDEF)
        );
    }

    #[test]
    fn get_image_reports_missing_tasks_and_bad_ranges() {
        let mut app = test_app();
        let mut rpc = dispatcher();
        let (status, envelope) = call(
            &mut app,
            &mut rpc,
            "xeH.getImage",
            vec![Value::String("nope".to_string())],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
        assert_eq!(
            envelope
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64),
            Some(consts::ERR_TASK_NOT_FOUND)
        );
        let (status, envelope) =
            add_task_with_url(&mut app, &mut rpc, "http://e-hentai.org/g/123/abcdef0123/");
        assert_eq!(status, 200);
        let guid = envelope
            .get("result")
            .and_then(Value::as_str)
            .expect("guid")
            .to_string();
        // A fresh task has no `total` yet: 500 instead of a dropped
        // connection (reference KeyError).
        let (status, _) = call(
            &mut app,
            &mut rpc,
            "xeH.getImage",
            vec![Value::String(guid.clone())],
            JsonMap::new(),
        );
        assert_eq!(status, 500);
        // A bad range is a 500 before any signing decision.
        let (status, _) = call(
            &mut app,
            &mut rpc,
            "xeH.getImage",
            vec![
                Value::String(guid.clone()),
                Value::String("abc".to_string()),
            ],
            JsonMap::new(),
        );
        assert_eq!(status, 500);
    }

    // --- real wire bytes over an in-memory stream ----------------------------

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    }

    fn post_bytes(path: &str, body: &[u8]) -> Vec<u8> {
        let mut out = format!(
            "POST {path} HTTP/1.1\r\nHost: x\r\nContent-Type: application/json-rpc\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        out.extend_from_slice(body);
        out
    }

    fn roundtrip(request: &[u8], secret: Option<String>) -> Vec<u8> {
        block_on(async move {
            let (mut client, mut server) = tokio::io::duplex(256 * 1024);
            {
                use tokio::io::AsyncWriteExt;
                client.write_all(request).await.expect("write");
            }
            let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let cache_dir = tempfile::tempdir().expect("cache").keep();
            let shared = Rc::new(RefCell::new(
                SharedRpc::new(
                    manifest,
                    &cache_dir,
                    Arc::new(crate::worker::RealClock),
                    secret,
                    "http://127.0.0.1:9".to_string(),
                )
                .expect("shared"),
            ));
            // A live handle without a spawned task loop: direct App ops and
            // the dispatch path behave exactly like the mounted server.
            let runtime = crate::core::Runtime::new(test_app(), manifest.to_path_buf());
            let handle = runtime.handle();
            serve_one(
                &mut server,
                handle,
                shared,
                "http://127.0.0.1:9".to_string(),
            )
            .await
            .expect("serve");
            drop(server);
            {
                use tokio::io::AsyncReadExt;
                let mut out = Vec::new();
                client.read_to_end(&mut out).await.expect("read");
                out
            }
        })
    }

    fn split_response(raw: &[u8]) -> (String, Vec<(String, String)>, Vec<u8>) {
        let text = String::from_utf8_lossy(raw);
        let head_end = text.find("\r\n\r\n").expect("head");
        let head = &text[..head_end];
        let mut lines = head.split("\r\n");
        let status = lines.next().expect("status").to_string();
        let mut headers = Vec::new();
        for line in lines {
            let (name, value) = line.split_once(':').expect("header");
            headers.push((name.trim().to_lowercase(), value.trim().to_string()));
        }
        let body_start = head_end + 4;
        let declared: usize = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .expect("content-length")
            .1
            .parse()
            .expect("length");
        let body = raw[body_start..body_start + declared].to_vec();
        // The reference newline sits outside Content-Length.
        assert_eq!(raw[body_start + declared], b'\n', "{status}");
        (status, headers, body)
    }

    #[test]
    fn post_roundtrip_preserves_framing_id_and_cors() {
        let body = br#"{"method":"xeH.getInfo","id":41,"params":[[],{}]}"#;
        let raw = roundtrip(&post_bytes("/jsonrpc", body), None);
        let (status, headers, body) = split_response(&raw);
        assert!(status.contains("200"), "{status}");
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name == "access-control-allow-origin")
                .map(|(_, value)| value.as_str()),
            Some("*")
        );
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name == "content-type")
                .map(|(_, value)| value.as_str()),
            Some("application/json-rpc")
        );
        let envelope: Value = serde_json::from_slice(&body).expect("envelope");
        assert_eq!(envelope.get("id"), Some(&Value::from(41)));
        assert_eq!(
            envelope
                .get("result")
                .and_then(|info| info.get("version"))
                .and_then(Value::as_str),
            Some(crate::r#const::VERSION_DISPLAY)
        );
    }

    #[test]
    fn post_auth_roundtrip_exact_literal_only() {
        let body = br#"{"method":"xeH.getConfig","id":1,"params":[[],{}]}"#;
        // Missing token with a configured secret: 403.
        let (status, _, _) = {
            let raw = roundtrip(&post_bytes("/jsonrpc", body), Some("s3cret".to_string()));
            let (status, _, _) = split_response(&raw);
            (status.clone(), (), ())
        };
        assert!(status.contains("403"), "{status}");
        // Substring token (accepted by the reference regex) is refused.
        let body = br#"{"method":"xeH.getConfig","id":1,"params":[["xx token:s3cret yy"],{}]}"#;
        let raw = roundtrip(&post_bytes("/jsonrpc", body), Some("s3cret".to_string()));
        let (status, _, body) = split_response(&raw);
        assert!(status.contains("403"), "{status}");
        let envelope: Value = serde_json::from_slice(&body).expect("envelope");
        assert_eq!(
            envelope
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_i64),
            Some(consts::ERR_RPC_UNAUTHORIZED)
        );
        // Exact literal token passes and is consumed positionally.
        let body = br#"{"method":"xeH.getConfig","id":2,"params":[["token:s3cret"],{}]}"#;
        let raw = roundtrip(&post_bytes("/jsonrpc", body), Some("s3cret".to_string()));
        let (status, _, body) = split_response(&raw);
        assert!(status.contains("200"), "{status}");
        let envelope: Value = serde_json::from_slice(&body).expect("envelope");
        assert_eq!(envelope.get("id"), Some(&Value::from(2)));
        assert!(envelope.get("result").is_some());
    }

    #[test]
    fn get_and_options_roundtrips() {
        let raw = roundtrip(b"OPTIONS * HTTP/1.0\r\nHost: x\r\n\r\n", None);
        let (status, headers, _) = split_response(&raw);
        assert!(status.contains("200"), "{status}");
        assert!(
            headers
                .iter()
                .any(|(name, _)| name == "access-control-allow-origin")
        );
        // Unknown GET paths answer 404 with CORS and no content type.
        let raw = roundtrip(b"GET /nope HTTP/1.0\r\nHost: x\r\n\r\n", None);
        let (status, headers, body) = split_response(&raw);
        assert!(status.contains("404"), "{status}");
        assert!(
            headers
                .iter()
                .any(|(name, _)| name == "access-control-allow-origin")
        );
        assert!(
            !headers.iter().any(|(name, _)| name == "content-type"),
            "{headers:?}"
        );
        assert!(body.is_empty());
        // GET /ui/ with no cache and no reachable origin serves the frozen
        // offline pack (recorded delta from the reference invalid-request
        // JSON): the real built index of the fixed WebUI revision.
        let raw = roundtrip(b"GET /ui/ HTTP/1.0\r\nHost: x\r\n\r\n", None);
        let (status, headers, body) = split_response(&raw);
        assert!(status.contains("200"), "{status}");
        assert_eq!(
            headers
                .iter()
                .find(|(name, _)| name == "content-type")
                .map(|(_, value)| value.as_str()),
            Some("text/html")
        );
        let text = String::from_utf8_lossy(&body).into_owned();
        assert!(text.contains("<title"), "{text}");
        assert!(text.contains("/static/js/app"), "{text}");
    }

    #[test]
    fn frozen_pack_serves_assets_and_rejects_traversal() {
        use crate::rpc::FrozenPack;
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let pack = FrozenPack::load(manifest).expect("frozen pack must exist");
        let (index, mime) = pack.get("/").expect("index");
        assert_eq!(mime, "text/html");
        assert!(index.starts_with(b"<!DOCTYPE html>"));
        let (app_js, mime) = pack
            .get("/static/js/app.b712f314a32fe45ff793.js")
            .expect("app js");
        assert_eq!(mime, "application/javascript");
        assert!(!app_js.is_empty());
        let (css, mime) = pack
            .get("/static/css/app.6027935ff9f1e3bed9935438c6be1f38.css")
            .expect("css");
        assert_eq!(mime, "text/css");
        assert!(!css.is_empty());
        assert!(pack.get("/../secret").is_none());
        assert!(pack.get("/static/../../x").is_none());
        assert!(pack.get("/static/js/nope.js").is_none());
        assert!(FrozenPack::load(std::path::Path::new("/nonexistent-root")).is_none());
    }

    #[test]
    fn malformed_posts_answer_400_not_dropped() {
        for body in [
            &br#"{"method":"xeH.getInfo","id":1,"params":{}}"#[..],
            br#"{"method":"xeH.getInfo"}"#,
            b"not json",
        ] {
            let raw = roundtrip(&post_bytes("/jsonrpc", body), None);
            let (status, _, body) = split_response(&raw);
            assert!(status.contains("400"), "{status}");
            let envelope: Value = serde_json::from_slice(&body).expect("envelope");
            assert!(envelope.get("error").is_some());
        }
        // Non-string method: the reference drops the connection (uncaught
        // TypeError); the candidate answers 500.
        let raw = roundtrip(&post_bytes("/jsonrpc", br#"{"method":5,"id":1}"#), None);
        let (status, _, _) = split_response(&raw);
        assert!(status.contains("500"), "{status}");
    }

    // --- runtime-mounted pause/delete drain worker retirement --------------

    fn roundtrip_handle(request: &[u8], secret: Option<String>, app: App) -> Vec<u8> {
        block_on(async move {
            let (mut client, mut server) = tokio::io::duplex(256 * 1024);
            {
                use tokio::io::AsyncWriteExt;
                client.write_all(request).await.expect("write");
            }
            let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let cache_dir = tempfile::tempdir().expect("cache").keep();
            let shared = Rc::new(RefCell::new(
                SharedRpc::new(
                    manifest,
                    &cache_dir,
                    Arc::new(crate::worker::RealClock),
                    secret,
                    "http://127.0.0.1:9".to_string(),
                )
                .expect("shared"),
            ));
            let runtime = crate::core::Runtime::new(app, manifest.to_path_buf());
            let handle = runtime.handle();
            serve_one(
                &mut server,
                handle,
                shared,
                "http://127.0.0.1:9".to_string(),
            )
            .await
            .expect("serve");
            drop(server);
            {
                use tokio::io::AsyncReadExt;
                let mut out = Vec::new();
                client.read_to_end(&mut out).await.expect("read");
                out
            }
        })
    }

    #[test]
    fn runtime_mounted_pause_and_delete_drain() {
        let mut app = test_app();
        let mut rpc = dispatcher();
        let (status, envelope) =
            add_task_with_url(&mut app, &mut rpc, "http://e-hentai.org/g/123/abcdef0123/");
        assert_eq!(status, 200);
        let guid = envelope
            .get("result")
            .and_then(Value::as_str)
            .expect("guid")
            .to_string();
        // Runtime-mounted pause drains through the M4 retirement path.
        let body =
            format!("{{\"method\":\"xeH.pauseTask\",\"id\":9,\"params\":[[\"{guid}\"],{{}}]}}");
        let raw = roundtrip_handle(&post_bytes("/jsonrpc", body.as_bytes()), None, app);
        let (status, _, body) = split_response(&raw);
        assert!(status.contains("200"), "{status}");
        let envelope: Value = serde_json::from_slice(&body).expect("envelope");
        assert_eq!(envelope.get("result"), Some(&Value::String(String::new())));
    }

    // --- B1+B4: one normalized, singly-authenticated request path -----------

    fn task_tree(root: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let mut names: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
                .expect("list")
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .collect();
            names.sort();
            for path in names {
                let rel = path
                    .strip_prefix(root)
                    .expect("rel")
                    .to_string_lossy()
                    .replace('\\', "/");
                if path.is_dir() && !path.is_symlink() {
                    out.push(format!("{rel}/"));
                    stack.push(path);
                } else {
                    // Volatile operational files (log/session/cache) are
                    // routine set_logfile output, not directory creation.
                    let name = path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .unwrap_or("");
                    if ["h.json", "eh.log", "webui.gz", "webui-cache.json"].contains(&name) {
                        continue;
                    }
                    out.push(rel);
                }
            }
        }
        out.sort();
        out
    }

    fn dispatch_sync(
        app: &mut App,
        rpc: &mut RpcDispatcher,
        body: &[u8],
        secret: Option<&str>,
        runtime: Option<&crate::core::runtime::AppHandle>,
    ) -> RequestAction {
        let cache_dir = tempfile::tempdir().expect("cache").keep();
        let mut cache = crate::rpc::StaticCache::load(
            &cache_dir.join(crate::rpc::STATIC_CACHE_NAME),
            Arc::new(crate::worker::RealClock),
        );
        let info = Value::Null;
        dispatch_request(
            app,
            rpc,
            &mut cache,
            &|| info.clone(),
            "POST /jsonrpc",
            body,
            secret,
            runtime,
        )
    }

    fn ready_envelope(action: RequestAction) -> (u16, Value) {
        let RequestAction::Ready(response) = action else {
            panic!("expected a ready response, got {action:?}");
        };
        let envelope: Value =
            serde_json::from_slice(&response_body_bytes(&response)).expect("envelope");
        (response.status, envelope)
    }

    fn response_body_bytes(response: &Response) -> Vec<u8> {
        match &response.body {
            ResponseBody::Inline(bytes) => bytes.clone(),
            ResponseBody::Stream(_) => panic!("expected inline test response"),
        }
    }

    /// Drive a GET target through dispatch plus the owned media path and
    /// collect the exact wire bytes the client would receive.
    fn serve_media_bytes(
        app: App,
        secret: Option<String>,
        target: &str,
    ) -> (u16, Option<String>, Vec<u8>) {
        block_on(async {
            let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let runtime = crate::core::Runtime::new(app, manifest.to_path_buf());
            let handle = runtime.handle();
            let cache_dir = tempfile::tempdir().expect("cache").keep();
            let shared = Rc::new(RefCell::new(
                SharedRpc::new(
                    manifest,
                    &cache_dir,
                    Arc::new(crate::worker::RealClock),
                    secret.clone(),
                    "http://127.0.0.1:9".to_string(),
                )
                .expect("shared"),
            ));
            let mut cache = StaticCache::load(
                &cache_dir.join(crate::rpc::STATIC_CACHE_NAME),
                Arc::new(crate::worker::RealClock),
            );
            let action = dispatch_get(&mut cache, target, secret.as_deref());
            let response = match action {
                RequestAction::Media(media) => {
                    let permits = Arc::clone(&shared.borrow().media_permits);
                    run_media(handle, permits, media).await
                }
                RequestAction::Ready(response) => response,
                other => panic!("unexpected media action: {other:?}"),
            };
            let status = response.status;
            let mime = response.mime.clone();
            let bytes = match response.body {
                ResponseBody::Inline(bytes) => bytes,
                ResponseBody::Stream(mut stream) => {
                    use std::io::Read;
                    let mut file = stream.file.take().expect("stream file");
                    let mut out = Vec::new();
                    file.read_to_end(&mut out).expect("stream read");
                    out
                }
            };
            (status, mime, bytes)
        })
    }

    #[test]
    fn secret_login_authenticates_once_and_prepares_credentials() {
        let mut app = test_app();
        let mut rpc = dispatcher();
        // RED on 21fc8f7: the second USERNAME check in dispatch_login pops
        // the secret first, then rejects the real username with 403.
        let body =
            br#"{"method":"xeH.loginExhentai","id":7,"params":[["token:s3cret","u","p"],{}]}"#;
        match dispatch_sync(&mut app, &mut rpc, body, Some("s3cret"), None) {
            RequestAction::Login { id, user, pwd } => {
                assert_eq!(id, Value::from(7));
                assert_eq!(user, "u");
                assert_eq!(pwd, "p");
            }
            other => panic!("secret login must prepare a login action, got {other:?}"),
        }
        // Ordinary usernames (never tokens) still reach the login path.
        let body = br#"{"method":"xeH.loginExhentai","id":8,"params":[["token:s3cret","alice","wonderland"],{}]}"#;
        match dispatch_sync(&mut app, &mut rpc, body, Some("s3cret"), None) {
            RequestAction::Login { user, pwd, .. } => {
                assert_eq!(user, "alice");
                assert_eq!(pwd, "wonderland");
            }
            other => panic!("ordinary login must prepare a login action, got {other:?}"),
        }
        // Open server keeps the stray-token pop without authentication.
        let body = br#"{"method":"xeH.loginExhentai","id":9,"params":[["token:xyz","u","p"],{}]}"#;
        match dispatch_sync(&mut app, &mut rpc, body, None, None) {
            RequestAction::Login { user, pwd, .. } => {
                assert_eq!(user, "u");
                assert_eq!(pwd, "p");
            }
            other => panic!("open-server login must pop the stray token, got {other:?}"),
        }
    }

    #[test]
    fn login_arity_and_type_errors_survive_single_auth() {
        let mut app = test_app();
        let mut rpc = dispatcher();
        // Wrong or missing token with a configured secret: 403, unchanged.
        for body in [
            &br#"{"method":"xeH.loginExhentai","id":1,"params":[["bogus","u","p"],{}]}"#[..],
            br#"{"method":"xeH.loginExhentai","id":2,"params":[[],{}]}"#,
        ] {
            let (status, envelope) = ready_envelope(dispatch_sync(
                &mut app,
                &mut rpc,
                body,
                Some("s3cret"),
                None,
            ));
            assert_eq!(status, 403, "{envelope}");
        }
        // Non-string credentials: 500 type error, never a second 403.
        let body = br#"{"method":"xeH.loginExhentai","id":3,"params":[["token:s3cret",5,"p"],{}]}"#;
        let (status, envelope) = ready_envelope(dispatch_sync(
            &mut app,
            &mut rpc,
            body,
            Some("s3cret"),
            None,
        ));
        assert_eq!(status, 500, "{envelope}");
        // Keyword arguments: the reference TypeError surfaces as 500.
        let body = br#"{"method":"xeH.loginExhentai","id":4,"params":[["token:s3cret","u","p"],{"name":"u"}]}"#;
        let (status, envelope) = ready_envelope(dispatch_sync(
            &mut app,
            &mut rpc,
            body,
            Some("s3cret"),
            None,
        ));
        assert_eq!(status, 500, "{envelope}");
        // Missing password: 500 missing-argument error.
        let body = br#"{"method":"xeH.loginExhentai","id":5,"params":[["token:s3cret","u"],{}]}"#;
        let (status, envelope) = ready_envelope(dispatch_sync(
            &mut app,
            &mut rpc,
            body,
            Some("s3cret"),
            None,
        ));
        assert_eq!(status, 500, "{envelope}");
    }

    #[test]
    fn snake_pause_and_delete_take_the_runtime_drain_path() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let runtime = crate::core::Runtime::new(test_app(), manifest.to_path_buf());
        let handle = runtime.handle();
        let mut rpc = dispatcher();
        // RED on 21fc8f7: only raw camelCase names drain; the snake_case
        // forms the RSS selector also accepts fall into sync App mutation.
        for method in ["xeH.pause_task", "xeH.pauseTask"] {
            let body = format!(r#"{{"method":"{method}","id":3,"params":[["nope"],{{}}]}}"#);
            let action = handle.borrow_app_mut(|app| {
                dispatch_sync(app, &mut rpc, body.as_bytes(), None, Some(&handle))
            });
            assert!(
                matches!(action, RequestAction::DrainPause { .. }),
                "{method} must drain, got {action:?}"
            );
        }
        for method in ["xeH.del_task", "xeH.delTask"] {
            let body = format!(r#"{{"method":"{method}","id":3,"params":[["nope"],{{}}]}}"#);
            let action = handle.borrow_app_mut(|app| {
                dispatch_sync(app, &mut rpc, body.as_bytes(), None, Some(&handle))
            });
            assert!(
                matches!(action, RequestAction::DrainDelete { .. }),
                "{method} must drain, got {action:?}"
            );
        }
    }

    #[test]
    fn snake_pause_and_delete_drain_over_wire() {
        let mut app = test_app();
        let mut rpc = dispatcher();
        let (status, envelope) =
            add_task_with_url(&mut app, &mut rpc, "http://e-hentai.org/g/123/abcdef0123/");
        assert_eq!(status, 200);
        let guid = envelope
            .get("result")
            .and_then(Value::as_str)
            .expect("guid")
            .to_string();
        // Snake pause drains through the same M4 retirement path as camel.
        let body = format!(r#"{{"method":"xeH.pause_task","id":10,"params":[["{guid}"],{{}}]}}"#);
        let raw = roundtrip_handle(&post_bytes("/jsonrpc", body.as_bytes()), None, app);
        let (status, _, body) = split_response(&raw);
        assert!(status.contains("200"), "{status}");
        let envelope: Value = serde_json::from_slice(&body).expect("envelope");
        assert_eq!(envelope.get("result"), Some(&Value::String(String::new())));
    }

    #[test]
    fn snake_delete_running_is_refused_over_wire() {
        // A running task refuses deletion with the real code, in both
        // spellings; nothing is pretended drained.
        for method in ["xeH.del_task", "xeH.delTask"] {
            let mut running = test_app();
            let mut rpc = dispatcher();
            let (_, envelope) = add_task_with_url(
                &mut running,
                &mut rpc,
                "http://e-hentai.org/g/123/abcdef0123/",
            );
            let guid = envelope
                .get("result")
                .and_then(Value::as_str)
                .expect("guid")
                .to_string();
            running.task_mut(&guid).expect("task").state = consts::TASK_STATE_DOWNLOAD;
            let body = format!(r#"{{"method":"{method}","id":11,"params":[["{guid}"],{{}}]}}"#);
            let raw = roundtrip_handle(&post_bytes("/jsonrpc", body.as_bytes()), None, running);
            let (status, _, body) = split_response(&raw);
            assert!(status.contains("200"), "{status}");
            let envelope: Value = serde_json::from_slice(&body).expect("envelope");
            assert_eq!(
                envelope
                    .get("error")
                    .and_then(|error| error.get("code"))
                    .and_then(Value::as_i64),
                Some(consts::ERR_DELETE_RUNNING_TASK),
                "{method}"
            );
        }
    }

    type LoginCaptures = std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>;

    struct LoginFixture {
        _server: std::thread::JoinHandle<()>,
        url: String,
        requests: LoginCaptures,
    }

    /// Loopback login origin on a plain OS thread (it must outlive the
    /// one-shot test runtimes): captures the real HTTP request bytes the
    /// native session sends and answers with a synthetic logged-in cookie
    /// pair. Only external I/O origin is faked; no handler, filter or App
    /// business logic is patched. Credential values stay in memory and are
    /// never written to retained logs.
    fn loopback_login() -> LoginFixture {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback");
        let port = listener.local_addr().expect("addr").port();
        let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = std::sync::Arc::clone(&requests);
        let server = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else {
                    return;
                };
                let seen = std::sync::Arc::clone(&seen);
                std::thread::spawn(move || {
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(15)))
                        .ok();
                    let mut head = Vec::new();
                    let mut byte = [0u8; 1];
                    loop {
                        match stream.read(&mut byte) {
                            Ok(1) => {
                                head.extend_from_slice(&byte);
                                if head.len() > 65536 {
                                    return;
                                }
                                if head.ends_with(b"\r\n\r\n") {
                                    break;
                                }
                            }
                            _ => return,
                        }
                    }
                    let text = String::from_utf8_lossy(&head).into_owned();
                    let mut lines = text.split("\r\n");
                    let request_line = lines.next().unwrap_or("").to_string();
                    let mut length = 0usize;
                    for line in lines {
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                    let mut body = vec![0u8; length.min(1 << 20)];
                    let mut read = 0usize;
                    while read < body.len() {
                        match stream.read(&mut body[read..]) {
                            Ok(0) => break,
                            Ok(n) => read += n,
                            Err(_) => break,
                        }
                    }
                    body.truncate(read);
                    seen.lock().expect("capture").push((request_line, body));
                    let reply = b"HTTP/1.1 200 OK\r\nSet-Cookie: ipb_member_id=4242; path=/\r\nSet-Cookie: ipb_pass_hash=deadbeef00; path=/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
                    let _ = stream.write_all(reply);
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                });
            }
        });
        LoginFixture {
            _server: server,
            url: format!("http://127.0.0.1:{port}/index.php?act=Login&CODE=01"),
            requests,
        }
    }

    #[test]
    fn secret_login_runs_real_loopback_login_and_keeps_session() {
        let fixture = loopback_login();
        // Synthetic fixture credentials only; redacted from retained logs.
        let user = "fixture-user";
        let pwd = "fixture-pass-9";
        unsafe {
            std::env::set_var("XEH_TEST_LOGIN_URL", &fixture.url);
        }
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let runtime = crate::core::Runtime::new(test_app(), manifest.to_path_buf());
        let handle = runtime.handle();
        let login = |id: i64| {
            let body = format!(
                r#"{{"method":"xeH.loginExhentai","id":{id},"params":[["token:s3cret","{user}","{pwd}"],{{}}]}}"#
            );
            block_on(async {
                let (mut client, mut server) = tokio::io::duplex(256 * 1024);
                {
                    use tokio::io::AsyncWriteExt;
                    client
                        .write_all(&post_bytes("/jsonrpc", body.as_bytes()))
                        .await
                        .expect("write");
                }
                let cache_dir = tempfile::tempdir().expect("cache").keep();
                let shared = Rc::new(RefCell::new(
                    SharedRpc::new(
                        manifest,
                        &cache_dir,
                        Arc::new(crate::worker::RealClock),
                        Some("s3cret".to_string()),
                        "http://127.0.0.1:9".to_string(),
                    )
                    .expect("shared"),
                ));
                serve_one(
                    &mut server,
                    handle.clone(),
                    shared,
                    "http://127.0.0.1:9".to_string(),
                )
                .await
                .expect("serve");
                drop(server);
                let mut out = Vec::new();
                {
                    use tokio::io::AsyncReadExt;
                    client.read_to_end(&mut out).await.expect("read");
                }
                out
            })
        };
        // First login: real RPC POST with the secret, real App login against
        // the loopback origin, real HTTP bytes and Set-Cookie capture.
        let raw = login(7);
        let (status, _, body) = split_response(&raw);
        assert!(status.contains("200"), "{status}");
        let envelope: Value = serde_json::from_slice(&body).expect("envelope");
        assert_eq!(envelope.get("result"), Some(&Value::Bool(true)));
        let captured = fixture.requests.lock().expect("capture");
        assert_eq!(captured.len(), 1, "one real HTTP login POST");
        assert!(captured[0].0.starts_with("POST "), "{}", captured[0].0);
        let sent = String::from_utf8_lossy(&captured[0].1).into_owned();
        for field in ["UserName=", "PassWord=", "returntype=8", "CookieDate=1"] {
            assert!(sent.contains(field), "login form must carry {field}");
        }
        drop(captured);
        // Same App now carries the session: login flag, Cookie header and no
        // second network hit on the follow-up request.
        assert!(handle.borrow_app(|app| app.has_login()));
        let cookie = handle.borrow_app(|app| app.cookie_header().unwrap_or_default().to_string());
        assert!(cookie.contains("ipb_member_id=4242"), "{cookie}");
        assert!(cookie.contains("ipb_pass_hash=deadbeef00"), "{cookie}");
        let raw = login(8);
        let (status, _, body) = split_response(&raw);
        assert!(status.contains("200"), "{status}");
        let envelope: Value = serde_json::from_slice(&body).expect("envelope");
        assert_eq!(envelope.get("result"), Some(&Value::Bool(true)));
        assert_eq!(
            fixture.requests.lock().expect("capture").len(),
            1,
            "logged-in login must not hit the network again"
        );
        unsafe {
            std::env::remove_var("XEH_TEST_LOGIN_URL");
        }
    }

    // --- native thumbnails on synthetic images ------------------------------

    fn png_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 128])
        });
        let mut out = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        image::ImageEncoder::write_image(
            encoder,
            &image,
            width,
            height,
            image::ExtendedColorType::Rgb8,
        )
        .expect("encode");
        out
    }

    fn jpeg_bytes(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, 64])
        });
        let mut out = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new(&mut out);
        encoder.encode_image(&image).expect("encode");
        out
    }

    fn image_dims(data: &[u8]) -> (u32, u32) {
        let image = image::load_from_memory(data).expect("decode");
        (image.width(), image.height())
    }

    fn encoded_bytes(format: image::ImageFormat) -> Vec<u8> {
        let (width, height) = (80u32, 40u32);
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([((x * 3) % 256) as u8, ((y * 5) % 256) as u8, 90])
        });
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(image)
            .write_to(&mut out, format)
            .expect("encode");
        out.into_inner()
    }

    fn mean_rgb(data: &[u8]) -> (f64, f64, f64) {
        let image = image::load_from_memory(data).expect("decode").to_rgb8();
        let count = (image.width() * image.height()) as f64;
        let mut sums = (0.0f64, 0.0f64, 0.0f64);
        for pixel in image.pixels() {
            sums.0 += f64::from(pixel[0]);
            sums.1 += f64::from(pixel[1]);
            sums.2 += f64::from(pixel[2]);
        }
        (sums.0 / count, sums.1 / count, sums.2 / count)
    }

    #[test]
    fn thumbnails_fit_never_upscale_and_keep_format() {
        let png = png_bytes(100, 50);
        // Both bounds: fit inside the box preserving aspect.
        let (small, mime) = thumbnail(&png, Some(20), Some(20)).expect("thumb");
        assert_eq!(mime, "image/png");
        assert_eq!(image_dims(&small), (20, 10));
        // Only width: the missing axis copies the given one (square box),
        // then the box fits the source preserving aspect.
        let (wide, _) = thumbnail(&png, Some(50), None).expect("thumb");
        assert_eq!(image_dims(&wide), (50, 25));
        // Only height: square box from the height, fitted to the source.
        // The old axis-only scale answered (50, 25) here; the Pillow oracle
        // (`rpc.py:85-105` box semantics) answers (25, 13).
        let (tall, _) = thumbnail(&png, None, Some(25)).expect("thumb");
        assert_eq!(image_dims(&tall), (25, 13));
        // Larger-than-source box never upscales: original bytes back.
        let (same, _) = thumbnail(&png, Some(1000), Some(1000)).expect("thumb");
        assert_eq!(same, png);
        // No bounds: passthrough signal, serve original.
        assert_eq!(thumbnail(&png, None, None), Err(ThumbError::Passthrough));
        // JPEG keeps its format and MIME.
        let jpeg = jpeg_bytes(80, 40);
        let (small_jpeg, mime) = thumbnail(&jpeg, Some(20), Some(20)).expect("thumb");
        assert_eq!(mime, "image/jpeg");
        assert_eq!(image_dims(&small_jpeg), (20, 10));
        // Garbage is a decode failure, not a silent original.
        assert!(matches!(
            thumbnail(b"not an image", Some(10), Some(10)),
            Err(ThumbError::Decode(_))
        ));
        // All five reference formats survive the resize with their MIME and
        // decoded colour content (encoders/resamplers differ byte-wise, so
        // equivalence is documented as mean-channel proximity).
        for (format, mime) in [
            (image::ImageFormat::Jpeg, "image/jpeg"),
            (image::ImageFormat::Png, "image/png"),
            (image::ImageFormat::Gif, "image/gif"),
            (image::ImageFormat::WebP, "image/webp"),
            (image::ImageFormat::Bmp, "image/bmp"),
        ] {
            let source = encoded_bytes(format);
            let (resized, got_mime) = thumbnail(&source, Some(20), Some(20))
                .unwrap_or_else(|error| panic!("{format:?} thumbnail failed: {error:?}"));
            assert_eq!(got_mime, mime, "{format:?}");
            assert_eq!(image_dims(&resized), (20, 10), "{format:?}");
            let before = mean_rgb(&source);
            let after = mean_rgb(&resized);
            for (channel, (left, right)) in [
                (before.0, after.0),
                (before.1, after.1),
                (before.2, after.2),
            ]
            .into_iter()
            .enumerate()
            {
                assert!(
                    (left - right).abs() < 12.0,
                    "{format:?} channel {channel}: {left} != {right}"
                );
            }
        }
    }

    // --- gallery reads from a real task -------------------------------------

    fn task_with_image(app: &mut App, name: &str, bytes: &[u8]) -> String {
        use crate::task::{FidKey, Task, TaskConfig};
        let root = app.base_dir().to_path_buf();
        let mut config = TaskConfig::from_app(&app.cfg, &JsonMap::new()).expect("config");
        config.dir = "downloads".to_string();
        let mut task = Task::new(
            "http://e-hentai.org/g/123/abcdef0123/",
            config,
            format!("guid-{name}"),
            &root,
        )
        .expect("task");
        task.meta.insert(
            "title".to_string(),
            Value::String("gallery-title".to_string()),
        );
        task.meta.insert("total".to_string(), Value::from(3));
        let dir = task.get_fpath().expect("fpath");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join(name), bytes).expect("image");
        let guid = task.guid.clone();
        task.renamed_map
            .insert(FidKey::Text("5".to_string()), name.to_string());
        app.insert_task_for_test(task);
        guid
    }

    #[test]
    fn gallery_reads_serve_files_and_reject_bad_fids() {
        let mut app = test_app();
        let png = png_bytes(8, 8);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let (bytes, mime) = read_gallery_file(&app, &guid, "1").expect("file");
        assert_eq!(bytes, png);
        assert_eq!(mime, "image/jpeg");
        // Renamed lookup honors both int and text keys.
        let (renamed, _) = read_gallery_file(&app, &guid, "5").expect("renamed");
        assert_eq!(renamed, png);
        assert!(read_gallery_file(&app, &guid, "nope").is_err());
        assert!(read_gallery_file(&app, "missing", "1").is_err());
    }

    #[test]
    fn gallery_reads_fall_back_to_sibling_zip() {
        let mut app = test_app();
        let png = png_bytes(8, 8);
        let guid = task_with_image(&mut app, "2.jpg", &png);
        let task = app.task(&guid).expect("task");
        let dir = task.get_fpath().expect("fpath");
        // Build the sibling archive the reference would read from, then drop
        // the loose file.
        let zip_name = format!("{}.zip", dir.display());
        let zip_path = Path::new(&zip_name);
        {
            let file = std::fs::File::create(zip_path).expect("zip");
            let mut archive = zip::ZipWriter::new(file);
            archive
                .start_file("2.jpg", zip::write::SimpleFileOptions::default())
                .expect("entry");
            {
                use std::io::Write;
                archive.write_all(&png).expect("bytes");
            }
            archive.finish().expect("finish");
        }
        std::fs::remove_file(dir.join("2.jpg")).expect("remove");
        let (bytes, mime) = read_gallery_file(&app, &guid, "2").expect("zip fallback");
        assert_eq!(bytes, png);
        assert_eq!(mime, "image/jpeg");
        // Nothing anywhere: 404.
        assert!(read_gallery_file(&app, &guid, "3").is_err());
    }

    #[test]
    fn archive_builds_on_demand_and_reuses() {
        let mut app = test_app();
        let png = png_bytes(8, 8);
        let guid = task_with_image(&mut app, "9.jpg", &png);
        let first = read_archive(&app, &guid).expect("archive");
        assert!(first.starts_with(b"PK"));
        let second = read_archive(&app, &guid).expect("archive again");
        assert_eq!(first, second);
        let cursor = std::io::Cursor::new(&second);
        let mut archive = zip::ZipArchive::new(cursor).expect("open");
        assert!(archive.by_name("9.jpg").is_ok());
    }

    #[test]
    fn zip_error_path_keeps_later_reads_healthy() {
        let mut app = test_app();
        let png = png_bytes(8, 8);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let dir = app.task(&guid).expect("task").get_fpath().expect("fpath");
        let zip_path = std::path::PathBuf::from(format!("{}.zip", dir.display()));
        std::fs::write(&zip_path, b"not a zip archive").expect("corrupt zip");
        // fid 2 has no loose file: the reference falls back to the sibling
        // ZIP, which is corrupt here, so the request fails cleanly.
        assert!(read_gallery_file(&app, &guid, "2").is_err());
        // The corrupt archive must not poison a healthy read of the loose
        // file or a later archive attempt.
        let (bytes, mime) = read_gallery_file(&app, &guid, "1").expect("healthy read");
        assert_eq!(bytes, png);
        assert_eq!(mime, "image/jpeg");
        let _ = read_archive(&app, &guid);
        let (again, _) = read_gallery_file(&app, &guid, "1").expect("healthy reuse");
        assert_eq!(again, png);
        std::fs::remove_file(&zip_path).expect("cleanup");
    }

    #[test]
    fn rejected_gallery_requests_do_not_leak_descriptors() {
        fn open_fds() -> usize {
            std::fs::read_dir("/proc/self/fd")
                .map(|dir| dir.count())
                .unwrap_or(0)
        }
        let cache_dir = tempfile::tempdir().expect("cache").keep();
        let cache_path = cache_dir.join("webui-cache.json");
        let before = open_fds();
        for index in 0..64 {
            let mut cache = StaticCache::load(&cache_path, Arc::new(crate::worker::RealClock));
            let _ = dispatch_get(&mut cache, &format!("/img/00000000/guid{index}/1"), None);
            let mut cache = StaticCache::load(&cache_path, Arc::new(crate::worker::RealClock));
            let _ = dispatch_get(&mut cache, "/zip/00000000/none/gallery.zip", None);
        }
        let after = open_fds();
        assert!(after <= before + 4, "descriptor leak: {before} -> {after}");
    }

    // --- B3 RED: owned IO, confined targets, bounded buffering ----------------
    // These fail on the unfixed media path (whole-file Vec under the App
    // borrow, unconfined symlink targets, unbounded decode/cache/zip reads)
    // and pass once snapshots stream with enforced budgets.

    fn make_fifo(path: &std::path::Path) {
        let status = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .expect("mkfifo");
        assert!(status.success(), "mkfifo failed");
    }

    /// Minimal BMP header claiming huge dimensions with no pixel data:
    /// header-only bytes that must fail the pixel gate before any full
    /// decode allocation.
    fn bmp_claiming_dims(width: i32, height: i32) -> Vec<u8> {
        let mut out = vec![b'B', b'M'];
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&width.to_le_bytes());
        out.extend_from_slice(&height.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(&[0u8; 24]);
        out
    }

    #[test]
    fn gallery_symlink_escape_and_reserved_are_denied() {
        let mut app = test_app();
        let png = png_bytes(8, 8);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let dir = app.task(&guid).expect("task").get_fpath().expect("fpath");
        // Symlink pointing outside the owned gallery root.
        let outside = tempfile::tempdir().expect("outside").keep();
        std::fs::write(outside.join("evil.jpg"), &png).expect("evil");
        std::fs::remove_file(dir.join("1.jpg")).expect("remove");
        std::os::unix::fs::symlink(outside.join("evil.jpg"), dir.join("1.jpg")).expect("link");
        // RED: served through the link. Must be denied before any read.
        assert!(
            read_gallery_file(&app, &guid, "1").is_err(),
            "symlink outside the gallery was admitted"
        );
        // Symlink pointing at reserved App persistence.
        let reserved = app.base_dir().join("eh.log");
        std::fs::write(&reserved, b"log bytes").expect("log");
        std::fs::remove_file(dir.join("1.jpg")).expect("remove link");
        std::os::unix::fs::symlink(&reserved, dir.join("1.jpg")).expect("link");
        assert!(
            read_gallery_file(&app, &guid, "1").is_err(),
            "reserved symlink target was admitted"
        );
    }

    #[test]
    fn gallery_sibling_archive_symlink_is_denied() {
        let mut app = test_app();
        let png = png_bytes(8, 8);
        let guid = task_with_image(&mut app, "2.jpg", &png);
        let dir = app.task(&guid).expect("task").get_fpath().expect("fpath");
        let outside = tempfile::tempdir().expect("outside").keep();
        let evil_zip = outside.join("evil.zip");
        {
            let file = std::fs::File::create(&evil_zip).expect("zip");
            let mut archive = zip::ZipWriter::new(file);
            archive
                .start_file("2.jpg", zip::write::SimpleFileOptions::default())
                .expect("entry");
            {
                use std::io::Write;
                archive.write_all(&png).expect("bytes");
            }
            archive.finish().expect("finish");
        }
        std::fs::remove_file(dir.join("2.jpg")).expect("remove");
        let sibling = std::path::PathBuf::from(format!("{}.zip", dir.display()));
        std::os::unix::fs::symlink(&evil_zip, &sibling).expect("link");
        // RED: read through the archive symlink. Sibling archives need the
        // exact owned authority, never an arbitrary link target.
        assert!(
            read_gallery_file(&app, &guid, "2").is_err(),
            "sibling archive symlink was admitted"
        );
    }

    #[test]
    fn gallery_dispatch_releases_before_reads() {
        let mut app = test_app();
        let png = png_bytes(8, 8);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let dir = app.task(&guid).expect("task").get_fpath().expect("fpath");
        std::fs::remove_file(dir.join("1.jpg")).expect("remove");
        make_fifo(&dir.join("1.jpg"));
        // A FIFO with no writer blocks any read forever. Dispatch must come
        // back without reading; RED blocks here and the timeout below fires.
        let (tx, rx) = std::sync::mpsc::channel();
        let sign = hash_link(None, &format!("{guid}/1"));
        let target = format!("/img/{sign}/{guid}/1");
        std::thread::spawn(move || {
            let cache_dir = tempfile::tempdir().expect("cache").keep();
            let mut cache = StaticCache::load(
                &cache_dir.join(crate::rpc::STATIC_CACHE_NAME),
                Arc::new(crate::worker::RealClock),
            );
            let action = dispatch_get(&mut cache, &target, None);
            let _ = tx.send(action);
        });
        rx.recv_timeout(std::time::Duration::from_secs(5))
            .expect("dispatch blocked on a media read");
    }

    #[test]
    fn gallery_thumbnail_input_budgets_fire_before_decode() {
        // Huge claimed dimensions, header-only bytes: the pixel gate must
        // fire before any full decode allocation.
        let evil = bmp_claiming_dims(20_000, 20_000);
        let error = thumbnail(&evil, Some(20), Some(20)).expect_err("must refuse");
        let ThumbError::Decode(message) = error else {
            panic!("wrong failure shape: {error:?}");
        };
        assert!(
            message.contains("pixel bound"),
            "dimension gate must fire first, got: {message}"
        );
        // 65 MiB of encoded bytes: the dedicated encoded-input budget must
        // fire before parsing.
        let mut big = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        big.resize(65 * 1024 * 1024, 0);
        let error = thumbnail(&big, Some(20), Some(20)).expect_err("must refuse");
        let ThumbError::Decode(message) = error else {
            panic!("wrong failure shape: {error:?}");
        };
        assert!(
            message.contains("encoded"),
            "encoded-input gate must fire first, got: {message}"
        );
    }

    #[test]
    fn static_cache_bounds_entries_and_total() {
        let dir = tempfile::tempdir().expect("dir").keep();
        let path = dir.join("webui-cache.json");
        let mut cache = StaticCache::load(&path, Arc::new(crate::worker::RealClock));
        for index in 0..65 {
            cache.store(
                &format!("/static/a{index}.js"),
                StaticEntry {
                    body: b"x".to_vec(),
                    mime: "application/javascript".to_string(),
                    fetched: 1.0,
                    last_modified: String::new(),
                },
                false,
            );
        }
        // RED: all 65 kept. Only the newest 64 may survive.
        assert!(
            cache.stale("/static/a0.js").is_none(),
            "oldest entry must be evicted past 64 entries"
        );
        // Five 8 MiB entries exceed the 32 MiB total: oldest-first
        // eviction keeps only the newest within budget.
        let mut cache = StaticCache::load(
            &dir.join("webui-cache2.json"),
            Arc::new(crate::worker::RealClock),
        );
        for name in ["big-a", "big-b", "big-c", "big-d", "big-e"] {
            cache.store(
                name,
                StaticEntry {
                    body: vec![0u8; 8 * 1024 * 1024],
                    mime: "application/octet-stream".to_string(),
                    fetched: 1.0,
                    last_modified: String::new(),
                },
                false,
            );
        }
        assert!(
            cache.stale("big-a").is_none() && cache.stale("big-e").is_some(),
            "total must stay within 32 MiB with oldest-first eviction"
        );
    }

    #[test]
    fn static_fetch_refuses_oversize_before_buffering() {
        block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let origin = format!("http://127.0.0.1:{port}");
            tokio::spawn(async move {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0u8; 4096];
                let _ = stream.read(&mut buf).await;
                let head = b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 9437184\r\nConnection: close\r\n\r\n";
                stream.write_all(head).await.expect("head");
                stream
                    .write_all(&vec![0u8; 9 * 1024 * 1024])
                    .await
                    .expect("body");
            });
            // RED: the whole 9 MiB body is buffered and returned.
            assert!(
                fetch_static(&origin, "/big", None).await.is_err(),
                "oversize static asset must be refused at the 8 MiB bound"
            );
        });
    }

    #[test]
    fn static_persisted_cache_load_enforces_bounds() {
        let dir = tempfile::tempdir().expect("dir").keep();
        // One 9 MiB entry: skipped on load, never cached.
        let huge_b64 = "A".repeat(9 * 1024 * 1024 / 3 * 4);
        let text = format!(
            "{{\"v\":1,\"entries\":{{\"big\":{{\"body_b64\":\"{huge_b64}\",\"mime\":\"application/octet-stream\",\"fetched\":100.0,\"last_modified\":\"x\"}}}}}}"
        );
        let path = dir.join("webui-cache.json");
        std::fs::write(&path, text).expect("cache file");
        let cache = StaticCache::load(&path, Arc::new(crate::worker::RealClock));
        // RED: the oversize entry loads.
        assert!(
            cache.stale("big").is_none(),
            "persisted entry past the 8 MiB bound must be skipped"
        );
        // 200 persisted entries: truncated to the newest 64.
        let mut entries = String::from("{\"v\":1,\"entries\":{");
        for index in 0..200 {
            if index > 0 {
                entries.push(',');
            }
            entries.push_str(&format!(
                "\"e{index}\":{{\"body_b64\":\"eA==\",\"mime\":\"text/plain\",\"fetched\":100.0,\"last_modified\":\"x\"}}"
            ));
        }
        entries.push_str("}}");
        std::fs::write(&path, entries).expect("cache file");
        let cache = StaticCache::load(&path, Arc::new(crate::worker::RealClock));
        let mut kept = 0;
        for index in 0..200 {
            if cache.stale(&format!("e{index}")).is_some() {
                kept += 1;
            }
        }
        assert!(
            kept <= 64,
            "persisted entries must truncate to 64, kept {kept}"
        );
    }

    #[test]
    fn gallery_zip_entry_budget_refuses_before_allocation() {
        let mut app = test_app();
        let png = png_bytes(8, 8);
        let guid = task_with_image(&mut app, "9.jpg", &png);
        let dir = app.task(&guid).expect("task").get_fpath().expect("fpath");
        std::fs::remove_file(dir.join("9.jpg")).expect("remove loose file");
        // Declared AND actual 513 MiB of zeros: past the dedicated 512 MiB
        // ZIP-image budget. RED buffers the whole entry and succeeds.
        let sibling = std::path::PathBuf::from(format!("{}.zip", dir.display()));
        {
            let file = std::fs::File::create(&sibling).expect("zip");
            let mut archive = zip::ZipWriter::new(file);
            archive
                .start_file(
                    "9.jpg",
                    zip::write::SimpleFileOptions::default()
                        .compression_method(zip::CompressionMethod::Stored),
                )
                .expect("entry");
            {
                use std::io::Write;
                archive
                    .write_all(&vec![0u8; 513 * 1024 * 1024])
                    .expect("bytes");
            }
            archive.finish().expect("finish");
        }
        assert!(
            read_gallery_file(&app, &guid, "9").is_err(),
            "ZIP entry past the 512 MiB budget must be refused before allocation"
        );
        std::fs::remove_file(&sibling).expect("cleanup");
    }

    // --- B3 GREEN: streaming, caps, owned lifecycle -------------------------

    fn pattern_bytes(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    fn pattern_spot(bytes: &[u8], index: usize) {
        assert_eq!(
            bytes[index],
            (index % 251) as u8,
            "pattern mismatch at {index}"
        );
    }

    fn spool_leftovers(dir: &std::path::Path) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.filter_map(|entry| entry.ok()) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(".xehentai-spool-") {
                    out.push(name);
                }
            }
        }
        out
    }

    /// Concurrent GET driver: the server and the client run together so a
    /// multi-chunk stream never wedges on a full duplex buffer.
    fn get_wire_concurrent(app: App, target: &str) -> Vec<u8> {
        block_on(async {
            let (mut client, mut server) = tokio::io::duplex(64 * 1024);
            let request = format!("GET {target} HTTP/1.0\r\nHost: x\r\n\r\n");
            let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let cache_dir = tempfile::tempdir().expect("cache").keep();
            let shared = Rc::new(RefCell::new(
                SharedRpc::new(
                    manifest,
                    &cache_dir,
                    Arc::new(crate::worker::RealClock),
                    None,
                    "http://127.0.0.1:9".to_string(),
                )
                .expect("shared"),
            ));
            let runtime = crate::core::Runtime::new(app, manifest.to_path_buf());
            let handle = runtime.handle();
            let local = tokio::task::LocalSet::new();
            let (served, read) = local
                .run_until(async {
                    tokio::join!(
                        serve_one(
                            &mut server,
                            handle,
                            shared,
                            "http://127.0.0.1:9".to_string(),
                        ),
                        async {
                            use tokio::io::AsyncWriteExt;
                            client.write_all(request.as_bytes()).await.expect("head");
                            let mut out = Vec::new();
                            {
                                use tokio::io::AsyncReadExt;
                                client.read_to_end(&mut out).await.expect("read");
                            }
                            out
                        }
                    )
                })
                .await;
            served.expect("serve");
            read
        })
    }

    #[test]
    fn gallery_stream_large_file_chunks_pattern() {
        // 70 MiB of known pattern through the owned streaming path: exact
        // length, first/last bytes and spot checks, no whole-asset Vec, no
        // spool leftovers for a direct file.
        let mut app = test_app();
        let big = pattern_bytes(70 * 1024 * 1024);
        let guid = task_with_image(&mut app, "big.jpg", &big);
        let dir = app.task(&guid).expect("task").get_fpath().expect("fpath");
        let (status, mime, bytes) = serve_media_bytes(
            app,
            None,
            &format!("/img/{}/{guid}/5", hash_link(None, &format!("{guid}/5"))),
        );
        assert_eq!(status, 200);
        assert_eq!(mime.as_deref(), Some("image/jpeg"));
        assert_eq!(bytes.len(), big.len());
        pattern_spot(&bytes, 0);
        pattern_spot(&bytes, 1);
        pattern_spot(&bytes, 250);
        pattern_spot(&bytes, 251);
        pattern_spot(&bytes, 1_000_000);
        pattern_spot(&bytes, bytes.len() - 1);
        assert!(spool_leftovers(&dir).is_empty());
    }

    #[test]
    fn gallery_stream_wire_framing() {
        // 8 MiB over real socket bytes: headers carry the exact length, the
        // body streams in chunks, the newline sits outside Content-Length.
        let mut app = test_app();
        let big = pattern_bytes(8 * 1024 * 1024);
        let guid = task_with_image(&mut app, "big.jpg", &big);
        let target = format!("/img/{}/{guid}/5", hash_link(None, &format!("{guid}/5")));
        let raw = get_wire_concurrent(app, &target);
        let (status, headers, body) = split_response(&raw);
        assert!(status.contains("200"), "{status}");
        let declared: usize = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .expect("content-length")
            .1
            .parse()
            .expect("length");
        assert_eq!(declared, big.len());
        assert_eq!(body.len(), big.len());
        pattern_spot(&body, 0);
        pattern_spot(&body, 123_456);
        pattern_spot(&body, body.len() - 1);
    }

    #[test]
    fn gallery_stream_large_archive_entries() {
        // A large valid on-demand archive streams with its real entries,
        // comment and STORE layout; the client checks entries and spots.
        let mut app = test_app();
        let big = pattern_bytes(4 * 1024 * 1024);
        let guid = task_with_image(&mut app, "big.jpg", &big);
        let target = format!("/zip/{}/{guid}/gallery.zip", hash_link(None, &guid));
        let (status, mime, bytes) = serve_media_bytes(app, None, &target);
        assert_eq!(status, 200);
        assert_eq!(mime.as_deref(), Some("application/zip"));
        let cursor = std::io::Cursor::new(&bytes);
        let mut archive = zip::ZipArchive::new(cursor).expect("open");
        assert_eq!(archive.len(), 1);
        let names: Vec<String> = (0..archive.len())
            .map(|index| archive.by_index(index).expect("entry").name().to_string())
            .collect();
        assert_eq!(names, vec!["big.jpg".to_string()]);
        assert!(archive.comment().starts_with(b"xeHentai Archiver v"));
        let mut entry = archive.by_name("big.jpg").expect("entry");
        assert_eq!(entry.compression(), zip::CompressionMethod::Stored);
        let mut content = Vec::new();
        {
            use std::io::Read;
            entry.read_to_end(&mut content).expect("read");
        }
        assert_eq!(content.len(), big.len());
        pattern_spot(&content, 0);
        pattern_spot(&content, 999_999);
        pattern_spot(&content, content.len() - 1);
    }

    #[test]
    fn gallery_stream_spool_cleanup() {
        // ZIP-fallback passthrough spools to a task-owned file, streams it,
        // and unlinks it: bytes served, nothing retained.
        let mut app = test_app();
        let pattern = pattern_bytes(2048);
        let guid = task_with_image(&mut app, "9.jpg", &pattern);
        let dir = app.task(&guid).expect("task").get_fpath().expect("fpath");
        std::fs::remove_file(dir.join("9.jpg")).expect("remove loose");
        let sibling = std::path::PathBuf::from(format!("{}.zip", dir.display()));
        {
            let file = std::fs::File::create(&sibling).expect("zip");
            let mut archive = zip::ZipWriter::new(file);
            archive
                .start_file("9.jpg", zip::write::SimpleFileOptions::default())
                .expect("entry");
            {
                use std::io::Write;
                archive.write_all(&pattern).expect("bytes");
            }
            archive.finish().expect("finish");
        }
        let (status, mime, bytes) = serve_media_bytes(
            app,
            None,
            &format!("/img/{}/{guid}/9", hash_link(None, &format!("{guid}/9"))),
        );
        assert_eq!(status, 200);
        assert_eq!(mime.as_deref(), Some("image/jpeg"));
        assert_eq!(bytes, pattern);
        assert!(spool_leftovers(&dir).is_empty());
        std::fs::remove_file(&sibling).expect("cleanup");
    }

    #[test]
    fn media_spool_guard_unlinks_on_drop() {
        use crate::rpc::FileStream;
        let dir = tempfile::tempdir().expect("dir").keep();
        let path = dir.join("spool-outlives-nothing");
        std::fs::write(&path, b"x").expect("spool");
        {
            let file = std::fs::File::open(&path).expect("open");
            let stream = FileStream {
                file: Some(file),
                len: 1,
                cleanup: Some(path.clone()),
            };
            drop(stream);
        }
        assert!(
            !path.exists(),
            "spool file must be unlinked with its stream"
        );
    }

    #[test]
    fn media_permits_gate_heavy_decode() {
        // The shared heavy permit semaphore gates decode end to end: with
        // both permits held the owned media path waits; freed, it serves.
        let mut app = test_app();
        let png = png_bytes(8, 8);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let target = format!(
            "/img/{}/{guid}/1?w=4&h=4",
            hash_link(None, &format!("{guid}/1"))
        );
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cache_dir = tempfile::tempdir().expect("cache").keep();
        let shared = Rc::new(RefCell::new(
            SharedRpc::new(
                manifest,
                &cache_dir,
                Arc::new(crate::worker::RealClock),
                None,
                "http://127.0.0.1:9".to_string(),
            )
            .expect("shared"),
        ));
        let permits = Arc::clone(&shared.borrow().media_permits);
        let first = block_on(permits.clone().acquire_owned()).expect("permit");
        let second = block_on(permits.clone().acquire_owned()).expect("permit");
        assert_eq!(permits.available_permits(), 0);
        let gated = {
            let runtime = crate::core::Runtime::new(app, manifest.to_path_buf());
            let handle = runtime.handle();
            let mut cache = StaticCache::load(
                &cache_dir.join(crate::rpc::STATIC_CACHE_NAME),
                Arc::new(crate::worker::RealClock),
            );
            let action = dispatch_get(&mut cache, &target, None);
            let RequestAction::Media(media) = action else {
                panic!("expected media, got {action:?}");
            };
            block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    run_media(handle, permits.clone(), media),
                )
                .await
            })
        };
        assert!(
            gated.is_err(),
            "heavy decode must wait while both permits are held"
        );
        drop(first);
        drop(second);
        let mut app = test_app();
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let target = format!(
            "/img/{}/{guid}/1?w=4&h=4",
            hash_link(None, &format!("{guid}/1"))
        );
        let (status, mime, bytes) = serve_media_bytes(app, None, &target);
        assert_eq!(status, 200);
        // Thumbnails keep the source format and MIME, not the filename ext.
        assert_eq!(mime.as_deref(), Some("image/png"));
        let (width, height) = image_dims(&bytes);
        assert_eq!((width, height), (4, 4));
    }

    #[test]
    fn accept_loop_caps_active_connections_and_reaps() {
        block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let listener = bind("127.0.0.1", 0).await.expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let cache_dir = tempfile::tempdir().expect("cache").keep();
            let shared = Rc::new(RefCell::new(
                SharedRpc::new(
                    manifest,
                    &cache_dir,
                    Arc::new(crate::worker::RealClock),
                    None,
                    "http://127.0.0.1:9".to_string(),
                )
                .expect("shared"),
            ));
            let runtime = crate::core::Runtime::new(test_app(), manifest.to_path_buf());
            let handle = runtime.handle();
            let local = tokio::task::LocalSet::new();
            let _ = local
                .run_until(async {
                    let server = serve_until_closed(
                        listener,
                        shutdown_rx,
                        handle,
                        Rc::clone(&shared),
                        "http://127.0.0.1:9".to_string(),
                    );
                    let driver = async {
                        // Fill every slot with a quiet connection.
                        let mut idle = Vec::new();
                        for _ in 0..crate::rpc::MAX_ACTIVE_CONN {
                            idle.push(
                                tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                                    .await
                                    .expect("connect"),
                            );
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                        // Past the cap the 65th connection waits: no task,
                        // no bytes, until a slot frees.
                        let mut extra = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                            .await
                            .expect("connect");
                        extra
                            .write_all(b"GET /ui/ HTTP/1.0\r\nHost: x\r\n\r\n")
                            .await
                            .expect("head");
                        let mut probe = vec![0u8; 1024];
                        let idle_read = tokio::time::timeout(
                            std::time::Duration::from_millis(300),
                            extra.read(&mut probe),
                        )
                        .await;
                        assert!(idle_read.is_err(), "65th connection must wait past the cap");
                        // Free one slot: the waiter is served for real.
                        idle.pop();
                        let mut out = Vec::new();
                        tokio::time::timeout(
                            std::time::Duration::from_secs(5),
                            extra.read_to_end(&mut out),
                        )
                        .await
                        .expect("served after reap")
                        .expect("read");
                        let text = String::from_utf8_lossy(&out).into_owned();
                        assert!(text.starts_with("HTTP/1.0 200 OK"), "{text}");
                        drop(idle);
                        let _ = shutdown_tx.send(true);
                    };
                    let (server_out, _) = tokio::join!(server, driver);
                    server_out.expect("drain");
                })
                .await;
        });
    }

    #[test]
    fn gallery_disconnect_during_stream_drains_cleanly() {
        // A client that vanishes mid-stream cannot strand owned work: the
        // stream ends, the next request still serves, shutdown still drains.
        block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut app = test_app();
            let big = pattern_bytes(20 * 1024 * 1024);
            let guid = task_with_image(&mut app, "big.jpg", &big);
            let target = format!("/img/{}/{guid}/5", hash_link(None, &format!("{guid}/5")));
            let listener = bind("127.0.0.1", 0).await.expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let cache_dir = tempfile::tempdir().expect("cache").keep();
            let shared = Rc::new(RefCell::new(
                SharedRpc::new(
                    manifest,
                    &cache_dir,
                    Arc::new(crate::worker::RealClock),
                    None,
                    "http://127.0.0.1:9".to_string(),
                )
                .expect("shared"),
            ));
            let runtime = crate::core::Runtime::new(app, manifest.to_path_buf());
            let handle = runtime.handle();
            let local = tokio::task::LocalSet::new();
            let _ = local
                .run_until(async {
                    let server = serve_until_closed(
                        listener,
                        shutdown_rx,
                        handle,
                        Rc::clone(&shared),
                        "http://127.0.0.1:9".to_string(),
                    );
                    let driver = async {
                        let mut quitter =
                            tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                                .await
                                .expect("connect");
                        quitter
                            .write_all(
                                format!("GET {target} HTTP/1.0\r\nHost: x\r\n\r\n").as_bytes(),
                            )
                            .await
                            .expect("head");
                        let mut first = vec![0u8; 1024];
                        quitter.read_exact(&mut first).await.expect("first bytes");
                        drop(quitter);
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                        let mut next = tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                            .await
                            .expect("connect");
                        next.write_all(b"GET /ui/ HTTP/1.0\r\nHost: x\r\n\r\n")
                            .await
                            .expect("head");
                        let mut out = Vec::new();
                        next.read_to_end(&mut out).await.expect("read");
                        let text = String::from_utf8_lossy(&out).into_owned();
                        assert!(text.starts_with("HTTP/1.0 200 OK"), "{text}");
                        let _ = shutdown_tx.send(true);
                    };
                    let (server_out, _) = tokio::join!(server, driver);
                    server_out.expect("drain");
                })
                .await;
        });
    }

    #[test]
    fn gallery_responsive_during_media_stream() {
        // A slow media stream holds no App borrow: getInfo is served while
        // the chunks are still in flight on the same owner thread.
        block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut app = test_app();
            let big = pattern_bytes(4 * 1024 * 1024);
            let guid = task_with_image(&mut app, "big.jpg", &big);
            let target = format!("/img/{}/{guid}/5", hash_link(None, &format!("{guid}/5")));
            let (mut media_client, mut media_server) = tokio::io::duplex(64 * 1024);
            let (mut info_client, mut info_server) = tokio::io::duplex(64 * 1024);
            let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let cache_dir = tempfile::tempdir().expect("cache").keep();
            let shared = Rc::new(RefCell::new(
                SharedRpc::new(
                    manifest,
                    &cache_dir,
                    Arc::new(crate::worker::RealClock),
                    None,
                    "http://127.0.0.1:9".to_string(),
                )
                .expect("shared"),
            ));
            let runtime = crate::core::Runtime::new(app, manifest.to_path_buf());
            let handle = runtime.handle();
            let local = tokio::task::LocalSet::new();
            let body = br#"{"method":"xeH.getInfo","id":41,"params":[[],{}]}"#;
            let info_request = post_bytes("/jsonrpc", body);
            let (media_raw, info_raw, media_served, info_served) = local
                .run_until(async {
                    tokio::join!(
                        async {
                            media_client
                                .write_all(
                                    format!("GET {target} HTTP/1.0\r\nHost: x\r\n\r\n").as_bytes(),
                                )
                                .await
                                .expect("head");
                            // Slow reader: the stream stays in flight.
                            let mut out = Vec::new();
                            let mut chunk = vec![0u8; 1024];
                            loop {
                                match media_client.read(&mut chunk).await {
                                    Ok(0) => break,
                                    Ok(n) => {
                                        out.extend_from_slice(&chunk[..n]);
                                        tokio::task::yield_now().await;
                                    }
                                    Err(error) => panic!("read: {error}"),
                                }
                            }
                            out
                        },
                        async {
                            info_client.write_all(&info_request).await.expect("head");
                            let mut out = Vec::new();
                            info_client.read_to_end(&mut out).await.expect("read");
                            out
                        },
                        // Both serves run concurrently on the owner thread:
                        // getInfo must complete while chunks are in flight.
                        serve_one(
                            &mut media_server,
                            handle.clone(),
                            Rc::clone(&shared),
                            "http://127.0.0.1:9".to_string(),
                        ),
                        serve_one(
                            &mut info_server,
                            handle,
                            shared,
                            "http://127.0.0.1:9".to_string(),
                        )
                    )
                })
                .await;
            media_served.expect("media");
            info_served.expect("info");
            let (status, _, media) = split_response(&media_raw);
            assert!(status.contains("200"), "{status}");
            assert_eq!(media.len(), big.len());
            pattern_spot(&media, 77);
            let (status, _, info) = split_response(&info_raw);
            assert!(status.contains("200"), "{status}");
            let envelope: Value = serde_json::from_slice(&info).expect("envelope");
            assert!(envelope.get("result").is_some());
        });
    }

    // --- static cache: TTL, 304, stale fallback, persistence -----------------

    #[test]
    fn static_cache_ttl_stale_persist_and_legacy_refusal() {
        use crate::worker::StepClock;
        let dir = tempfile::tempdir().expect("dir").keep();
        let clock = Arc::new(StepClock::new(1_000_000.0, 0.0));
        let path = dir.join("webui-cache.json");
        let mut cache = StaticCache::load(&path, Arc::clone(&clock) as Arc<dyn HttpClock>);
        assert!(cache.fresh("/").is_none());
        cache.store(
            "/",
            StaticEntry {
                body: b"<html>ui</html>".to_vec(),
                mime: "text/html".to_string(),
                fetched: 1_000_000.0,
                last_modified: "Wed, 01 Jan 2025 00:00:00 GMT".to_string(),
            },
            false,
        );
        cache.store(
            "/static/app.js",
            StaticEntry {
                body: b"console.log(1)".to_vec(),
                mime: "application/javascript".to_string(),
                fetched: 1_000_000.0,
                last_modified: "Wed, 01 Jan 2025 00:00:00 GMT".to_string(),
            },
            false,
        );
        assert!(cache.fresh("/").is_some());
        // Past the TTL everything is stale but still served as fallback.
        clock.set_now(1_000_000.0 + crate::rpc::STATIC_TTL + 1.0);
        assert!(cache.fresh("/").is_none());
        assert!(cache.stale("/static/app.js").is_some());
        // A refreshed index re-stamps every sibling entry.
        cache.refresh_index("/");
        assert!(cache.fresh("/static/app.js").is_some());
        // A fresh index body invalidates the hashed asset keys instead.
        cache.store(
            "/",
            StaticEntry {
                body: b"<html>ui-v2</html>".to_vec(),
                mime: "text/html".to_string(),
                fetched: 1_000_000.0 + crate::rpc::STATIC_TTL + 1.0,
                last_modified: "Thu, 02 Jan 2025 00:00:00 GMT".to_string(),
            },
            true,
        );
        assert!(cache.fresh("/static/app.js").is_none());
        // Restart: the JSON cache reloads; a legacy pickle never loads.
        std::fs::write(dir.join("webui.gz"), b"not a pickle").expect("legacy");
        let mut reloaded = StaticCache::load(&path, Arc::clone(&clock) as Arc<dyn HttpClock>);
        assert!(reloaded.fresh("/").is_some());
        let mut broken = StaticCache::load(
            &dir.join("missing.json"),
            Arc::clone(&clock) as Arc<dyn HttpClock>,
        );
        assert!(broken.fresh("/").is_none());
    }

    #[test]
    fn static_fetch_honors_304_and_reports_origin_errors() {
        block_on(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let origin = format!("http://127.0.0.1:{port}");
            tokio::spawn(async move {
                for _ in 0..3 {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        return;
                    };
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = vec![0u8; 4096];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let lowered = request.to_lowercase();
                    if lowered.contains("if-modified-since: wed, 01 jan 2025") {
                        stream
                            .write_all(b"HTTP/1.1 304 Not Modified\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await
                            .expect("304");
                    } else if request.starts_with("GET /missing") {
                        stream
                            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                            .await
                            .expect("404");
                    } else {
                        stream
                            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nLast-Modified: Wed, 01 Jan 2025 00:00:00 GMT\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world")
                            .await
                            .expect("200");
                    }
                }
            });
            let cached = StaticEntry {
                body: b"old".to_vec(),
                mime: "text/html".to_string(),
                fetched: 0.0,
                last_modified: "Wed, 01 Jan 2025 00:00:00 GMT".to_string(),
            };
            // 304 with a cached entry: no new bytes.
            let none = fetch_static(&origin, "/", Some(&cached))
                .await
                .expect("304");
            assert!(none.is_none());
            // 200 without cache: body, MIME and origin timestamp.
            let some = fetch_static(&origin, "/", None).await.expect("200");
            let (body, mime, last_modified) = some.expect("bytes");
            assert_eq!(body, b"hello world");
            assert_eq!(mime, "text/html");
            assert_eq!(last_modified, "Wed, 01 Jan 2025 00:00:00 GMT");
            // Origin errors surface instead of silent fallbacks.
            assert!(fetch_static(&origin, "/missing", None).await.is_err());
        });
    }

    // --- owned listener: real bind errors, real accept, owned drain ----------

    #[test]
    fn owned_listener_binds_accepts_and_drains() {
        block_on(async {
            let listener = bind("127.0.0.1", 0).await.expect("bind");
            // An occupied port is a real bind error, never a silent start.
            let port = listener.local_addr().expect("addr").port();
            assert!(bind("127.0.0.1", port).await.is_err());
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let cache_dir = tempfile::tempdir().expect("cache").keep();
            let shared = Rc::new(RefCell::new(
                SharedRpc::new(
                    manifest,
                    &cache_dir,
                    Arc::new(crate::worker::RealClock),
                    None,
                    "http://127.0.0.1:9".to_string(),
                )
                .expect("shared"),
            ));
            let runtime = crate::core::Runtime::new(test_app(), manifest.to_path_buf());
            let handle = runtime.handle();
            let local = tokio::task::LocalSet::new();
            // The server future and the driver future run concurrently on the
            // same set; the driver sends shutdown once its byte assertion is
            // captured, so the accept loop drains instead of hanging.
            let driver_out = local
                .run_until(async {
                    let server = serve_until_closed(
                        listener,
                        shutdown_rx,
                        handle,
                        Rc::clone(&shared),
                        "http://127.0.0.1:9".to_string(),
                    );
                    let driver = async {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        let mut stream =
                            tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                                .await
                                .expect("connect");
                        let body = br#"{"method":"xeH.getInfo","id":"tcp","params":[[],{}]}"#;
                        let request = format!(
                            "POST /jsonrpc HTTP/1.0\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
                            body.len()
                        );
                        stream.write_all(request.as_bytes()).await.expect("head");
                        stream.write_all(&body[..]).await.expect("body");
                        let mut out = Vec::new();
                        stream.read_to_end(&mut out).await.expect("read");
                        let _ = shutdown_tx.send(true);
                        out
                    };
                    let (server_out, driver_out) = tokio::join!(server, driver);
                    server_out.expect("drain");
                    driver_out
                })
                .await;
            let text = String::from_utf8_lossy(&driver_out).to_string();
            assert!(text.starts_with("HTTP/1.0 200 OK"), "{text}");
            assert!(text.contains("\"id\":\"tcp\""), "{text}");
        });
    }

    #[test]
    fn abandoned_partial_request_is_contained_and_fresh_requests_succeed() {
        block_on(async {
            let listener = bind("127.0.0.1", 0).await.expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let (shutdown_tx, shutdown_rx) = watch::channel(false);
            let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
            let cache_dir = tempfile::tempdir().expect("cache").keep();
            let shared = Rc::new(RefCell::new(
                SharedRpc::new(
                    manifest,
                    &cache_dir,
                    Arc::new(crate::worker::RealClock),
                    None,
                    "http://127.0.0.1:9".to_string(),
                )
                .expect("shared"),
            ));
            let runtime = crate::core::Runtime::new(test_app(), manifest.to_path_buf());
            let handle = runtime.handle();
            let local = tokio::task::LocalSet::new();
            let out = local
                .run_until(async {
                    let server = serve_until_closed(
                        listener,
                        shutdown_rx,
                        handle,
                        Rc::clone(&shared),
                        "http://127.0.0.1:9".to_string(),
                    );
                    let driver = async {
                        use tokio::io::{AsyncReadExt, AsyncWriteExt};
                        // A client that promises 100 bytes and disconnects
                        // after five must not wedge or leak the connection.
                        let mut abandoned =
                            tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                                .await
                                .expect("connect");
                        abandoned
                            .write_all(
                                b"POST /jsonrpc HTTP/1.0\r\nHost: x\r\nContent-Length: 100\r\n\r\nhalf!",
                            )
                            .await
                            .expect("partial body");
                        drop(abandoned);
                        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
                        // The same listener still serves a fresh request.
                        let mut fresh =
                            tokio::net::TcpStream::connect(format!("127.0.0.1:{port}"))
                                .await
                                .expect("reconnect");
                        fresh
                            .write_all(b"GET /ui/ HTTP/1.0\r\nHost: x\r\n\r\n")
                            .await
                            .expect("fresh head");
                        let mut response = Vec::new();
                        fresh.read_to_end(&mut response).await.expect("fresh read");
                        let _ = shutdown_tx.send(true);
                        response
                    };
                    let (served, response) = tokio::join!(server, driver);
                    served.expect("server drains owned connection tasks");
                    response
                })
                .await;
            let text = String::from_utf8_lossy(&out);
            assert!(text.starts_with("HTTP/1.0 200 OK"), "{text}");
            assert!(
                text.to_lowercase().contains("content-type: text/html"),
                "{text}"
            );
        });
    }

    // --- RSS mutation changes the wire ---------------------------------------

    #[test]
    fn rss_mutant_changes_list_tasks_selection() {
        let mut app = test_app();
        let mut rpc = dispatcher();
        let (status, envelope) =
            add_task_with_url(&mut app, &mut rpc, "http://e-hentai.org/g/123/abcdef0123/");
        assert_eq!(status, 200);
        let guid = envelope
            .get("result")
            .and_then(Value::as_str)
            .expect("guid")
            .to_string();
        app.task_mut(&guid).expect("task").state = consts::TASK_STATE_DOWNLOAD;
        // Baseline: the waiting task is selected by the default level.
        let (status, envelope) = call(&mut app, &mut rpc, "xeH.listTasks", vec![], JsonMap::new());
        assert_eq!(status, 200);
        assert_eq!(
            envelope
                .get("result")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(1)
        );
        // Mutant: remap the default level to finished. The same state now
        // selects nothing, proving the RSS level selector owns the wire.
        let source = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(super::super::RPC_RSS),
        )
        .expect("handlers.rss must exist");
        let mutant = source.replace(
            "let mut name = \"download\";",
            "let mut name = \"finished\";",
        );
        assert_ne!(mutant, source);
        let mut rpc_mutant = RpcDispatcher::from_source(&mutant).expect("mutant compiles");
        let (status, envelope) = call(
            &mut app,
            &mut rpc_mutant,
            "xeH.listTasks",
            vec![],
            JsonMap::new(),
        );
        assert_eq!(status, 200);
        assert_eq!(
            envelope
                .get("result")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(0)
        );
    }

    #[test]
    fn rss_mutant_changes_drain_routing() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let runtime = crate::core::Runtime::new(test_app(), manifest.to_path_buf());
        let handle = runtime.handle();
        let source = std::fs::read_to_string(manifest.join(super::super::RPC_RSS))
            .expect("handlers.rss must exist");
        // Mutant: drop pause_task from the RSS allowlist. The same wire
        // spelling no longer drains — it 404s — proving the RSS selector
        // (not a Rust alias list) owns the pause/delete route.
        let mutant = source.replace("|| snake == \"pause_task\" ||", "||");
        assert_ne!(mutant, source);
        let mut rpc_mutant = RpcDispatcher::from_source(&mutant).expect("mutant compiles");
        let body = br#"{"method":"xeH.pauseTask","id":3,"params":[["nope"],{}]}"#;
        let action = handle
            .borrow_app_mut(|app| dispatch_sync(app, &mut rpc_mutant, body, None, Some(&handle)));
        let (status, envelope) = ready_envelope(action);
        assert_eq!(status, 404, "{envelope}");
        // Control: the login route is untouched by this mutant.
        let body = br#"{"method":"xeH.loginExhentai","id":7,"params":[["u","p"],{}]}"#;
        let action = handle
            .borrow_app_mut(|app| dispatch_sync(app, &mut rpc_mutant, body, None, Some(&handle)));
        assert!(
            matches!(action, RequestAction::Login { .. }),
            "login route must survive the pause mutant, got {action:?}"
        );
    }

    // --- M5 R2 B3 RED: owned heavy I/O, race-safe opens, private spool ---
    // These capture the four remaining B3 defects on the candidate: they
    // FAIL here (RED) and flip to GREEN with the minimal fix, unchanged.

    fn task_with_zip(app: &mut App, name: &str, bytes: &[u8]) -> String {
        task_with_zip_titled(app, "gallery-title", name, bytes)
    }

    fn task_with_image_named(app: &mut App, title: &str, name: &str, bytes: &[u8]) -> String {
        use crate::task::{FidKey, Task, TaskConfig};
        let root = app.base_dir().to_path_buf();
        let mut config = TaskConfig::from_app(&app.cfg, &JsonMap::new()).expect("config");
        config.dir = "downloads".to_string();
        let mut task = Task::new(
            "http://e-hentai.org/g/123/abcdef0123/",
            config,
            format!("guid-{title}-{name}"),
            &root,
        )
        .expect("task");
        task.meta
            .insert("title".to_string(), Value::String(title.to_string()));
        task.meta.insert("total".to_string(), Value::from(3));
        let dir = task.get_fpath().expect("fpath");
        std::fs::create_dir_all(&dir).expect("dir");
        std::fs::write(dir.join(name), bytes).expect("image");
        let guid = task.guid.clone();
        task.renamed_map
            .insert(FidKey::Text("5".to_string()), name.to_string());
        app.insert_task_for_test(task);
        guid
    }

    fn task_with_zip_titled(app: &mut App, title: &str, name: &str, bytes: &[u8]) -> String {
        use crate::task::{FidKey, Task, TaskConfig};
        let root = app.base_dir().to_path_buf();
        let mut config = TaskConfig::from_app(&app.cfg, &JsonMap::new()).expect("config");
        config.dir = "downloads".to_string();
        let mut task = Task::new(
            "http://e-hentai.org/g/123/abcdef0123/",
            config,
            format!("guid-{title}-{name}"),
            &root,
        )
        .expect("task");
        task.meta
            .insert("title".to_string(), Value::String(title.to_string()));
        task.meta.insert("total".to_string(), Value::from(3));
        let dir = task.get_fpath().expect("fpath");
        std::fs::create_dir_all(&dir).expect("dir");
        // No loose file: only the sibling `<gallery>.zip` with one entry,
        // so the reference ZIP fallback path serves it.
        let zip_path = dir.with_extension("zip");
        let file = std::fs::File::create(&zip_path).expect("zip");
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file(name, zip::write::SimpleFileOptions::default())
            .expect("entry");
        use std::io::Write;
        writer.write_all(bytes).expect("bytes");
        writer.finish().expect("finish");
        let guid = task.guid.clone();
        task.renamed_map
            .insert(FidKey::Text("5".to_string()), name.to_string());
        app.insert_task_for_test(task);
        guid
    }

    fn media_action(target: &str) -> MediaRequest {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let cache_dir = tempfile::tempdir().expect("cache").keep();
        let mut cache = StaticCache::load(
            &cache_dir.join(crate::rpc::STATIC_CACHE_NAME),
            Arc::new(crate::worker::RealClock),
        );
        let _ = manifest;
        match dispatch_get(&mut cache, target, None) {
            RequestAction::Media(media) => media,
            other => panic!("expected media, got {other:?}"),
        }
    }

    fn thread_rchar() -> u64 {
        let tid = rustix::thread::gettid();
        let text =
            std::fs::read_to_string(format!("/proc/self/task/{}/io", tid.as_raw_nonzero().get()))
                .expect("task io");
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("rchar: ") {
                return rest.trim().parse().expect("rchar");
            }
        }
        panic!("no rchar in task io");
    }

    #[test]
    fn b3_1_zip_fallback_waits_for_heavy_permit() {
        // With both heavy permits held, the ZIP fallback must stay pending:
        // file open, ZIP decompress and spool all run under the owned
        // permit on the blocking pool. The candidate answers inline with
        // no permit at all, so this is RED until the offload lands.
        let mut app = test_app();
        let png = png_bytes(32, 32);
        let guid = task_with_zip(&mut app, "1.jpg", &png);
        let target = format!("/img/{}/{guid}/1", hash_link(None, &format!("{guid}/1")));
        let media = media_action(&target);
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let runtime = crate::core::Runtime::new(app, manifest.to_path_buf());
        let handle = runtime.handle();
        let permits = Arc::new(tokio::sync::Semaphore::new(super::super::MAX_HEAVY_WORKERS));
        block_on(async {
            let first = permits.clone().acquire_owned().await.expect("permit");
            let second = permits.clone().acquire_owned().await.expect("permit");
            assert_eq!(permits.available_permits(), 0);
            let gated = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                run_media(handle.clone(), Arc::clone(&permits), media),
            )
            .await;
            assert!(
                gated.is_err(),
                "ZIP fallback must wait for the heavy permit instead of answering inline"
            );
            drop(first);
            drop(second);
        });
        // Released permits serve the exact entry bytes, 200 with the entry
        // MIME.
        let media = media_action(&target);
        let (status, mime, bytes) = block_on(async {
            let response = run_media(handle, Arc::clone(&permits), media).await;
            let status = response.status;
            let mime = response.mime.clone();
            let body = match response.body {
                ResponseBody::Inline(body) => body,
                ResponseBody::Stream(mut stream) => {
                    use std::io::Read;
                    let mut file = stream.file.take().expect("stream file");
                    let mut out = Vec::new();
                    file.read_to_end(&mut out).expect("stream read");
                    out
                }
            };
            (status, mime, body)
        });
        assert_eq!(status, 200);
        assert_eq!(mime.as_deref(), Some("image/jpeg"));
        assert_eq!(bytes, png);
    }

    fn png_noise_bytes(width: u32, height: u32) -> Vec<u8> {
        // High-entropy pattern: compresses poorly, so the encoded source
        // is tens of KiB and any same-thread source read dwarfs the
        // /proc self-read noise (~a few hundred bytes).
        let image = image::RgbImage::from_fn(width, height, |x, y| {
            image::Rgb([
                ((x * 73 + y * 149) % 256) as u8,
                ((x * 131 + y * 71) % 256) as u8,
                ((x * 37 + y * 211) % 256) as u8,
            ])
        });
        let mut out = Vec::new();
        let encoder = image::codecs::png::PngEncoder::new(&mut out);
        image::ImageEncoder::write_image(
            encoder,
            &image,
            width,
            height,
            image::ExtendedColorType::Rgb8,
        )
        .expect("encode");
        out
    }

    #[test]
    fn b3_1_thumb_source_read_holds_permit() {
        // The thumbnail source read must happen under the owned heavy
        // permit: with both permits held, the serving thread must perform
        // NO source read (per-thread rchar unchanged apart from the tiny
        // /proc self-read) while the request stays pending. The candidate
        // reads the whole file inline before ever touching a permit, so
        // per-thread rchar grows by the file size: RED.
        let mut app = test_app();
        let png = png_noise_bytes(256, 256);
        assert!(png.len() > 16 * 1024, "fixture must dwarf self-read noise");
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let target = format!(
            "/img/{}/{guid}/1?w=16&h=16",
            hash_link(None, &format!("{guid}/1"))
        );
        let media = media_action(&target);
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let runtime = crate::core::Runtime::new(app, manifest.to_path_buf());
        let handle = runtime.handle();
        let permits = Arc::new(tokio::sync::Semaphore::new(super::super::MAX_HEAVY_WORKERS));
        block_on(async {
            let first = permits.clone().acquire_owned().await.expect("permit");
            let second = permits.clone().acquire_owned().await.expect("permit");
            assert_eq!(permits.available_permits(), 0);
            let before = thread_rchar();
            let gated = tokio::time::timeout(
                std::time::Duration::from_millis(200),
                run_media(handle.clone(), Arc::clone(&permits), media),
            )
            .await;
            let delta = thread_rchar().saturating_sub(before);
            assert!(
                gated.is_err(),
                "thumbnail request must wait for the heavy permit"
            );
            assert!(
                delta < 1024,
                "no source byte may be read before the heavy permit is held (read {delta}B)"
            );
            drop(first);
            drop(second);
        });
    }

    #[test]
    fn b3_2_preopen_swap_is_denied_before_read() {
        use crate::rpc::{probe_gallery_file, read_zip_fallback, snapshot_gallery_file};
        use crate::task::{PreopenSwap, arm_preopen_swap};
        // (a) Regular file swapped for an outside symlink between check and
        // open: no outside byte may be served. The candidate opens the swap
        // and returns the outside bytes: RED.
        let mut app = test_app();
        let png = png_bytes(16, 16);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let outside = app.base_dir().join("outside.jpg");
        std::fs::write(&outside, b"OUTSIDE-UNAUTHORIZED-BYTES").expect("outside");
        let snap = snapshot_gallery_file(&app, &guid, "1").expect("snap");
        arm_preopen_swap(
            &snap.root,
            &snap.name,
            PreopenSwap::LeafLink(outside.clone()),
        );
        match probe_gallery_file(&snap) {
            Ok(crate::rpc::OpenOutcome::Denied) => {}
            Ok(crate::rpc::OpenOutcome::Missing) => {
                panic!("swapped outside link must be denied, not treated as missing")
            }
            Ok(crate::rpc::OpenOutcome::File(mut opened)) => {
                use std::io::Read;
                let mut bytes = Vec::new();
                opened.file.read_to_end(&mut bytes).expect("read");
                panic!(
                    "swapped outside link served {} unauthorized bytes",
                    bytes.len()
                );
            }
            Err(error) => panic!("swapped outside link must be denied, got {error:?}"),
        }
        // (b) Swapped for a reserved persistence symlink: denied as well.
        let mut app = test_app();
        let png = png_bytes(16, 16);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let reserved = app.base_dir().join(crate::core::SESSION_FILE);
        std::fs::write(&reserved, b"RESERVED-SECRET").expect("reserved");
        let snap = snapshot_gallery_file(&app, &guid, "1").expect("snap");
        arm_preopen_swap(
            &snap.root,
            &snap.name,
            PreopenSwap::LeafLink(reserved.clone()),
        );
        match probe_gallery_file(&snap) {
            Ok(crate::rpc::OpenOutcome::Denied) => {}
            Ok(_) => panic!("swapped reserved link must be denied"),
            Err(error) => panic!("swapped reserved link must be denied, got {error:?}"),
        }
        // (c) Sibling archive swapped for an outside zip: the fallback must
        // refuse instead of serving the foreign entry. RED on the candidate.
        let mut app = test_app();
        let png = png_bytes(16, 16);
        let guid = task_with_zip(&mut app, "1.jpg", &png);
        let evil = app.base_dir().join("evil.zip");
        {
            let file = std::fs::File::create(&evil).expect("evil");
            let mut writer = zip::ZipWriter::new(file);
            writer
                .start_file("1.jpg", zip::write::SimpleFileOptions::default())
                .expect("entry");
            use std::io::Write;
            writer.write_all(b"EVIL-ENTRY-BYTES").expect("bytes");
            writer.finish().expect("finish");
        }
        let snap = snapshot_gallery_file(&app, &guid, "1").expect("snap");
        let parent = snap.root.parent().expect("parent").to_path_buf();
        let zip_leaf = format!(
            "{}.zip",
            snap.root.file_name().expect("name").to_string_lossy()
        );
        arm_preopen_swap(&parent, &zip_leaf, PreopenSwap::LeafLink(evil));
        match read_zip_fallback(&snap) {
            Err(_) => {}
            Ok((bytes, _)) => panic!(
                "swapped sibling archive served {} foreign bytes",
                bytes.len()
            ),
        }
        // (d) Gallery root itself swapped for a link to a hostile dir: the
        // opened bytes must not serve. RED on the candidate.
        let mut app = test_app();
        let png = png_bytes(16, 16);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let snap = snapshot_gallery_file(&app, &guid, "1").expect("snap");
        let hostile = app.base_dir().join("hostile-gallery");
        std::fs::create_dir_all(&hostile).expect("hostile");
        std::fs::write(hostile.join(&snap.name), b"HOSTILE-BYTES").expect("hostile file");
        arm_preopen_swap(&snap.root, &snap.name, PreopenSwap::RootLink(hostile));
        match probe_gallery_file(&snap) {
            Ok(crate::rpc::OpenOutcome::Denied) => {}
            Ok(crate::rpc::OpenOutcome::File(mut opened)) => {
                use std::io::Read;
                let mut bytes = Vec::new();
                opened.file.read_to_end(&mut bytes).expect("read");
                panic!("swapped gallery root served {} hostile bytes", bytes.len());
            }
            Ok(crate::rpc::OpenOutcome::Missing) => {
                panic!("swapped gallery root must be denied, not missing")
            }
            Err(error) => panic!("swapped gallery root must be denied, got {error:?}"),
        }
        // (e) Control: a legitimate inside-root alias still serves, and a
        // normal regular file is unaffected.
        let mut app = test_app();
        let png = png_bytes(16, 16);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let snap = snapshot_gallery_file(&app, &guid, "1").expect("snap");
        let real = snap.root.join("real.jpg");
        std::fs::rename(snap.root.join(&snap.name), &real).expect("move");
        #[cfg(unix)]
        std::os::unix::fs::symlink("real.jpg", snap.root.join(&snap.name)).expect("alias");
        match probe_gallery_file(&snap) {
            Ok(crate::rpc::OpenOutcome::File(mut opened)) => {
                use std::io::Read;
                let mut bytes = Vec::new();
                opened.file.read_to_end(&mut bytes).expect("read");
                assert_eq!(bytes, png);
            }
            _ => panic!("inside-root alias must serve"),
        }
        let mut app = test_app();
        let png = png_bytes(16, 16);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let snap = snapshot_gallery_file(&app, &guid, "1").expect("snap");
        match probe_gallery_file(&snap) {
            Ok(crate::rpc::OpenOutcome::File(mut opened)) => {
                use std::io::Read;
                let mut bytes = Vec::new();
                opened.file.read_to_end(&mut bytes).expect("read");
                assert_eq!(bytes, png);
            }
            _ => panic!("regular file must serve"),
        }
    }

    fn gallery_names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read_dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        names.sort();
        names
    }

    #[test]
    fn b3_3_spool_lives_outside_gallery_and_archive() {
        use crate::rpc::{read_archive, snapshot_gallery_file};
        // A live ZIP-fallback response must never stage inside the gallery:
        // while it is held, the gallery listing and a concurrent archive
        // must show exactly the real images, no spool. The candidate spools
        // `.xehentai-spool-*` INTO the gallery, so the archive picks it up:
        // RED. After the fix the spool is a private anonymous file.
        let mut app = test_app();
        let png1 = png_bytes(24, 24);
        let png2 = png_bytes(12, 12);
        let guid = task_with_image(&mut app, "1.jpg", &png1);
        let snap2 = snapshot_gallery_file(&app, &guid, "2").expect("snap2");
        let dir = snap2.root.clone();
        let zip_path = dir.with_extension("zip");
        {
            let file = std::fs::File::create(&zip_path).expect("zip");
            let mut writer = zip::ZipWriter::new(file);
            writer
                .start_file(&snap2.name, zip::write::SimpleFileOptions::default())
                .expect("entry");
            use std::io::Write;
            writer.write_all(&png2).expect("bytes");
            writer.finish().expect("finish");
        }
        let target = format!("/img/{}/{guid}/2", hash_link(None, &format!("{guid}/2")));
        let media = media_action(&target);
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let runtime = crate::core::Runtime::new(app, manifest.to_path_buf());
        let handle = runtime.handle();
        let permits = Arc::new(tokio::sync::Semaphore::new(super::super::MAX_HEAVY_WORKERS));
        let response =
            block_on(async { run_media(handle.clone(), Arc::clone(&permits), media).await });
        assert_eq!(response.status, 200);
        // Hold the live response: no spool may appear in the gallery.
        let names = gallery_names(&dir);
        assert!(
            !names
                .iter()
                .any(|name| name.starts_with(".xehentai-spool-")),
            "live spool response leaked into the gallery: {names:?}"
        );
        assert_eq!(names, vec!["1.jpg".to_string()]);
        // A concurrent archive over the same gallery is exact: only the
        // real image with its bytes, plus the reference comment, and no
        // spool/config/session/private artifacts. The fallback fixture
        // sibling would shadow a fresh build through the reference early
        // return, so only it is removed (the live spool itself is never
        // touched before the probe); the archive below is then built fresh
        // while the spool response is still held.
        std::fs::remove_file(&zip_path).expect("remove fixture sibling");
        let archive = handle.borrow_app(|app| read_archive(app, &guid).expect("archive"));
        let mut reader = zip::ZipArchive::new(std::io::Cursor::new(archive)).expect("archive zip");
        let mut entry_names: Vec<String> = (0..reader.len())
            .map(|index| reader.by_index(index).expect("index").name().to_string())
            .collect();
        entry_names.sort();
        assert_eq!(entry_names, vec!["1.jpg".to_string()]);
        let comment = std::str::from_utf8(reader.comment()).expect("comment");
        assert!(
            comment.contains("xeHentai Archiver") && comment.contains("gallery-title"),
            "archive comment must carry the reference archiver mark, got: {comment}"
        );
        let mut entry = reader.by_name("1.jpg").expect("entry");
        assert_eq!(entry.name(), "1.jpg");
        let mut contents = Vec::new();
        use std::io::Read;
        entry.read_to_end(&mut contents).expect("contents");
        assert_eq!(contents, png1);
        drop(entry);
        drop(reader);
        // Retire the live response: the gallery stays exact and a fresh
        // request still serves (healthy reuse, no abort-before-probe).
        drop(response);
        assert_eq!(gallery_names(&dir), vec!["1.jpg".to_string()]);
        let media = media_action(&format!(
            "/img/{}/{guid}/1",
            hash_link(None, &format!("{guid}/1"))
        ));
        let (status, _, _) = serve_media_bytes_like(handle, permits, media);
        assert_eq!(status, 200);
    }

    fn serve_media_bytes_like(
        handle: crate::core::runtime::AppHandle,
        permits: Arc<tokio::sync::Semaphore>,
        media: MediaRequest,
    ) -> (u16, Option<String>, Vec<u8>) {
        block_on(async {
            let response = run_media(handle, permits, media).await;
            let status = response.status;
            let mime = response.mime.clone();
            let bytes = match response.body {
                ResponseBody::Inline(body) => body,
                ResponseBody::Stream(mut stream) => {
                    use std::io::Read;
                    let mut file = stream.file.take().expect("stream file");
                    let mut out = Vec::new();
                    file.read_to_end(&mut out).expect("stream read");
                    out
                }
            };
            (status, mime, bytes)
        })
    }

    /// One oracle row: name, source dims, optional w/h, expected output dims.
    type OracleRow = (&'static str, u32, u32, Option<u32>, Option<u32>, (u32, u32));

    #[test]
    fn b3_4_thumbnail_box_matches_pil_oracle() {
        // Single-axis bounds form a SQUARE box from the given axis
        // (`rpc.py:85-105`: `size=(w or h, h or w)`), then PIL fits inside
        // preserving aspect with floor/ceil aspect-best rounding (ties go to
        // floor), never upscaling. Table captured from Pillow 13.0.0
        // (`evidence-m5-r2/oracle_thumb.py`). The candidate scales by the
        // only axis given, so landscape-height-only and portrait-width-only
        // (and odd ratios) diverge: RED.
        let cases: &[OracleRow] = &[
            ("landscape-both", 100, 50, Some(20), Some(20), (20, 10)),
            ("landscape-onlyw", 100, 50, Some(50), None, (50, 25)),
            ("landscape-onlyh", 100, 50, None, Some(25), (25, 13)),
            ("portrait-both", 50, 100, Some(20), Some(20), (10, 20)),
            ("portrait-onlyw", 50, 100, Some(25), None, (12, 25)),
            ("portrait-onlyh", 50, 100, None, Some(50), (25, 50)),
            ("square-onlyw", 64, 64, Some(32), None, (32, 32)),
            ("square-onlyh", 64, 64, None, Some(32), (32, 32)),
            ("square-both", 64, 64, Some(20), Some(20), (20, 20)),
            ("odd-landscape-onlyh", 101, 51, None, Some(25), (25, 13)),
            ("odd-portrait-onlyw", 51, 101, Some(25), None, (13, 25)),
            ("even-landscape-onlyw", 200, 100, Some(60), None, (60, 30)),
            ("even-portrait-onlyh", 100, 200, None, Some(60), (30, 60)),
            ("rect-box", 100, 50, Some(30), Some(20), (30, 15)),
            ("rect-box-portrait", 50, 100, Some(20), Some(30), (15, 30)),
        ];
        for (name, src_w, src_h, width, height, expected) in cases {
            let source = png_bytes(*src_w, *src_h);
            let (resized, mime) = thumbnail(&source, *width, *height)
                .unwrap_or_else(|error| panic!("{name} thumbnail failed: {error:?}"));
            assert_eq!(mime, "image/png", "{name}");
            assert_eq!(image_dims(&resized), *expected, "{name}");
        }
        // Never upscale: a box at or beyond the source serves the original
        // bytes, and equal-size is a passthrough of the source dimensions.
        for (name, width, height) in [
            ("no-upscale-both", Some(1000), Some(1000)),
            ("no-upscale-onlyw", Some(200), None),
            ("no-upscale-onlyh", None, Some(200)),
            ("same-size", Some(100), Some(50)),
        ] {
            let source = png_bytes(100, 50);
            let (resized, _) = thumbnail(&source, width, height)
                .unwrap_or_else(|error| panic!("{name} thumbnail failed: {error:?}"));
            assert_eq!(image_dims(&resized), (100, 50), "{name}");
        }
    }

    fn png16_bytes(width: u32, height: u32) -> Vec<u8> {
        let image =
            image::ImageBuffer::<image::Rgba<u16>, Vec<u16>>::from_fn(width, height, |x, y| {
                image::Rgba([(x % 512) as u16 * 128, (y % 512) as u16 * 128, 1000, 60000])
            });
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba16(image)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode");
        out.into_inner()
    }

    #[test]
    fn b3_4_decoder_limit_fires_before_pixels() {
        // The 256 MiB production budget is wired into the actual reader,
        // not just checked against the header estimate: a valid 16-bit
        // image whose header estimate (w*h*4) still passes a tiny budget
        // but whose real decoder need exceeds it must fail with the real
        // LIBRARY limit before any pixel allocation. The same bytes pass
        // under the production budget through the shared path.
        assert_eq!(
            crate::rpc::production_thumb_limits().max_alloc,
            Some(MAX_THUMB_DECODED)
        );
        assert_eq!(MAX_THUMB_DECODED, 256 * 1024 * 1024);
        let source = png16_bytes(32, 32);
        let (claimed_w, claimed_h) = image_dims(&source);
        assert_eq!((claimed_w, claimed_h), (32, 32));
        let estimate = claimed_w as u64 * claimed_h as u64 * 4;
        // 16-bit RGBA decodes to 8 bytes/px: pick a budget the header
        // estimate passes but the real 8 KiB need exceeds.
        let tiny = estimate + 1024;
        assert!(tiny < claimed_w as u64 * claimed_h as u64 * 8);
        match thumbnail_with_limits(&source, Some(16), Some(16), tiny) {
            Err(crate::rpc::ThumbError::Decode(message)) => assert!(
                message.contains("decoder limit"),
                "tiny budget must fail at the real library limit, got: {message}"
            ),
            other => panic!("tiny budget must fail at the library limit, got {other:?}"),
        }
        // Production budget serves the same bytes through the shared path.
        let (resized, mime) = thumbnail(&source, Some(16), Some(16)).expect("production");
        assert_eq!(mime, "image/png");
        assert_eq!(image_dims(&resized), (16, 16));
        // Production is byte-identical to the explicit 256 MiB path: the
        // configured value is the one actually wired into the reader.
        let (wired, _) =
            thumbnail_with_limits(&source, Some(16), Some(16), MAX_THUMB_DECODED).expect("wired");
        assert_eq!(wired, resized);
    }

    #[test]
    fn b3_1_permits_gate_workers_peer_progress_and_cancel() {
        // Two held workers consume the 2-permit cap: a third request stays
        // pending. Same-source getInfo still answers (no App borrow is
        // held, the LocalSet stays responsive). Cancelling a waiter while
        // its worker is still held does NOT release the permit early: the
        // permit stays owned until the native worker actually finishes.
        // Release then joins everything with FD/buffer baselines intact.
        use crate::rpc::snapshot_gallery_file;
        let mut app = test_app();
        let png_a = png_bytes(24, 24);
        let png_b = png_bytes(32, 32);
        let png_c = png_bytes(16, 16);
        let guid_a = task_with_zip_titled(&mut app, "gallery-a", "1.jpg", &png_a);
        let guid_b = task_with_zip_titled(&mut app, "gallery-b", "1.jpg", &png_b);
        let guid_c = task_with_image_named(&mut app, "gallery-c", "1.jpg", &png_c);
        let root_a = snapshot_gallery_file(&app, &guid_a, "1")
            .expect("snap")
            .root
            .to_string_lossy()
            .into_owned();
        let root_b = snapshot_gallery_file(&app, &guid_b, "1")
            .expect("snap")
            .root
            .to_string_lossy()
            .into_owned();
        let target_a = format!(
            "/img/{}/{guid_a}/1",
            hash_link(None, &format!("{guid_a}/1"))
        );
        let target_b = format!(
            "/img/{}/{guid_b}/1",
            hash_link(None, &format!("{guid_b}/1"))
        );
        let target_c = format!(
            "/img/{}/{guid_c}/1",
            hash_link(None, &format!("{guid_c}/1"))
        );
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let runtime = crate::core::Runtime::new(app, manifest.to_path_buf());
        let handle = runtime.handle();
        let permits = Arc::new(tokio::sync::Semaphore::new(super::super::MAX_HEAVY_WORKERS));
        let gate_a = arm_media_worker_gate(&root_a);
        let gate_b = arm_media_worker_gate(&root_b);
        let local = tokio::task::LocalSet::new();
        block_on(local.run_until(async {
            let media_a = media_action(&target_a);
            let media_b = media_action(&target_b);
            let media_c = media_action(&target_c);
            let mut held_a =
                tokio::task::spawn_local(run_media(handle.clone(), Arc::clone(&permits), media_a));
            tokio::time::timeout(std::time::Duration::from_secs(10), gate_a.started)
                .await
                .expect("worker A started")
                .expect("worker A holds its permit");
            assert_eq!(permits.available_permits(), 1);
            let held_b =
                tokio::task::spawn_local(run_media(handle.clone(), Arc::clone(&permits), media_b));
            tokio::time::timeout(std::time::Duration::from_secs(10), gate_b.started)
                .await
                .expect("worker B started")
                .expect("worker B holds its permit");
            assert_eq!(permits.available_permits(), 0);
            // The cap holds: a third request stays pending, and the same
            // source still answers getInfo (short borrows, responsive
            // LocalSet) while both workers are held.
            let mut waiting_c =
                tokio::task::spawn_local(run_media(handle.clone(), Arc::clone(&permits), media_c));
            let pending =
                tokio::time::timeout(std::time::Duration::from_millis(200), &mut waiting_c).await;
            assert!(
                pending.is_err(),
                "third request must wait for the 2-permit cap"
            );
            let (status, envelope) = handle.borrow_app_mut(|app| {
                let mut rpc = dispatcher();
                call(app, &mut rpc, "xeH.getInfo", vec![], JsonMap::new())
            });
            assert_eq!(status, 200);
            assert!(
                envelope.get("result").is_some(),
                "getInfo must progress while workers are held: {envelope}"
            );
            // Cancel waiter A while its worker is still held: the permit
            // stays owned (available still 0) until the native worker
            // actually finishes; the worker then exits early on the flag.
            held_a.abort();
            let aborted = (&mut held_a).await;
            assert!(aborted.is_err(), "aborted waiter joins as cancelled");
            // The worker notices the cancel and releases its permit without
            // any gate release; the pending third request takes it.
            let done_c = tokio::time::timeout(std::time::Duration::from_secs(10), &mut waiting_c)
                .await
                .expect("request C must take the freed permit")
                .expect("request C joins");
            assert_eq!(done_c.status, 200);
            assert_eq!(permits.available_permits(), 1);
            gate_b.release.send(()).expect("release B");
            let done_b = held_b.await.expect("request B joins");
            assert_eq!(done_b.status, 200);
            assert_eq!(permits.available_permits(), 2);
            match done_b.body {
                ResponseBody::Stream(mut stream) => {
                    use std::io::Read;
                    let mut file = stream.file.take().expect("stream file");
                    let mut out = Vec::new();
                    file.read_to_end(&mut out).expect("stream read");
                    assert_eq!(out, png_b);
                }
                other => panic!("request B must stream, got {other:?}"),
            }
            match done_c.body {
                ResponseBody::Stream(mut stream) => {
                    use std::io::Read;
                    let mut file = stream.file.take().expect("stream file");
                    let mut out = Vec::new();
                    file.read_to_end(&mut out).expect("stream read");
                    assert_eq!(out, png_c);
                }
                other => panic!("request C must stream, got {other:?}"),
            }
        }));
    }

    #[test]
    fn b3_1_shutdown_waits_for_worker_release() {
        // A held heavy worker pins its connection: with the response still
        // unbuilt (budgets/CRC unresolved), the client sees no bytes, a
        // shutdown stays pending, and only the worker release retires the
        // connection with the exact bytes and a clean close.
        use crate::rpc::snapshot_gallery_file;
        let mut app = test_app();
        let png = png_bytes(40, 20);
        let guid = task_with_zip_titled(&mut app, "gallery-z", "1.jpg", &png);
        let root = snapshot_gallery_file(&app, &guid, "1").expect("snap").root;
        let dir = root.clone();
        let gate = arm_media_worker_gate(&root.to_string_lossy());
        let target = format!("/img/{}/{guid}/1", hash_link(None, &format!("{guid}/1")));
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let runtime = crate::core::Runtime::new(app, manifest.to_path_buf());
        let handle = runtime.handle();
        let cache_dir = tempfile::tempdir().expect("cache").keep();
        let shared = Rc::new(RefCell::new(
            SharedRpc::new(
                manifest,
                &cache_dir,
                Arc::new(crate::worker::RealClock),
                None,
                "http://127.0.0.1:9".to_string(),
            )
            .expect("shared"),
        ));
        let local = tokio::task::LocalSet::new();
        block_on(local.run_until(async {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("listener");
            let addr = listener.local_addr().expect("addr");
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            let mut serve = tokio::task::spawn_local(serve_until_closed(
                listener,
                shutdown_rx,
                handle.clone(),
                Rc::clone(&shared),
                "http://127.0.0.1:9".to_string(),
            ));
            let mut client = tokio::net::TcpStream::connect(addr).await.expect("client");
            {
                use tokio::io::AsyncWriteExt;
                client
                    .write_all(
                        format!("GET {target} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
                            .as_bytes(),
                    )
                    .await
                    .expect("request");
            }
            tokio::time::timeout(std::time::Duration::from_secs(10), gate.started)
                .await
                .expect("image worker started")
                .expect("image worker holds its permit");
            // No byte leaves before the worker finishes: headers only exist
            // after the entry budgets and CRC resolve.
            {
                use tokio::io::AsyncReadExt;
                let mut probe = [0u8; 1];
                let quiet = tokio::time::timeout(
                    std::time::Duration::from_millis(300),
                    client.read_exact(&mut probe),
                )
                .await;
                assert!(quiet.is_err(), "held worker must not emit headers yet");
            }
            // Shutdown stays pending while the worker is held.
            shutdown_tx.send(true).expect("shutdown");
            let draining =
                tokio::time::timeout(std::time::Duration::from_millis(300), &mut serve).await;
            assert!(
                draining.is_err(),
                "shutdown must wait for the worker release"
            );
            gate.release.send(()).expect("release worker");
            serve.await.expect("serve joins").expect("serve ok");
            // The retired connection carries the exact entry bytes and the
            // reference trailing newline outside Content-Length.
            let mut raw = Vec::new();
            {
                use tokio::io::AsyncReadExt;
                tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    client.read_to_end(&mut raw),
                )
                .await
                .expect("read")
                .expect("bytes");
            }
            let text = String::from_utf8_lossy(&raw);
            let head_end = text.find("\r\n\r\n").expect("head");
            assert!(text.starts_with("HTTP/1.0 200 OK"), "{text}");
            let declared: usize = text[..head_end]
                .split("\r\n")
                .find_map(|line| {
                    line.strip_prefix("Content-Length: ")
                        .or_else(|| line.strip_prefix("content-length: "))
                })
                .expect("content-length")
                .trim()
                .parse()
                .expect("length");
            assert_eq!(declared, png.len());
            let body = &raw[head_end + 4..head_end + 4 + declared];
            assert_eq!(body, png.as_slice());
            assert_eq!(raw[head_end + 4 + declared], b'\n');
            // Baselines: the gallery is exact, the permits are all back, and
            // a fresh request still serves.
            assert_eq!(gallery_names(&dir), Vec::<String>::new());
            assert_eq!(shared.borrow().media_permits.available_permits(), 2);
            let media = media_action(&target);
            let fresh = run_media(handle, Arc::new(tokio::sync::Semaphore::new(2)), media).await;
            assert_eq!(fresh.status, 200);
        }));
    }
}
