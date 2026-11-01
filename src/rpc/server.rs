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
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::watch;

use super::{
    FrozenPack, GalleryFail, Response, RpcDispatcher, RpcRequest, ServerError, StaticCache,
    hash_link, jsonrpc_envelope, parse_jsonrpc, parse_thumb_query, read_archive, read_gallery_file,
    thumbnail,
};
use crate::r#const as consts;
use crate::core::App;
use crate::worker::HttpClock;

pub use super::{
    MAX_HEAD_BYTES, MAX_JSONRPC_BODY, STATIC_CACHE_NAME, StaticEntry, ThumbError, cors_404,
    fetch_static, options_response, reason_of,
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
    if let Ok(url) = std::env::var("XEH_TEST_LOGIN_URL") {
        if !url.is_empty() {
            return url;
        }
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
        "GET" => dispatch_get(app, cache, target, secret),
        "POST" => dispatch_post(app, rpc, get_info, target, body, secret, runtime),
        _ => RequestAction::Ready(Response {
            status: 501,
            reason: reason_of(501),
            mime: None,
            body: Vec::new(),
        }),
    }
}

/// What a synchronous dispatch decided: either a complete response or an
/// async follow-up that the connection owner performs without any borrow.
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

fn dispatch_get(
    app: &App,
    cache: &mut StaticCache,
    target: &str,
    secret: Option<&str>,
) -> RequestAction {
    if !path_matches(target) {
        return RequestAction::Ready(cors_404());
    }
    let (path, query) = match target.find('?') {
        Some(index) => (&target[..index], &target[index + 1..]),
        None => (target, ""),
    };
    if path.starts_with("/img/") {
        return RequestAction::Ready(serve_image(app, path, query, secret));
    }
    if path.starts_with("/zip/") {
        return RequestAction::Ready(serve_archive(app, path, secret));
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
        body: serde_json::to_vec(&body).unwrap_or_default(),
    })
}

fn serve_image(app: &App, path: &str, query: &str, secret: Option<&str>) -> Response {
    // `rpc.py:199-231`: segments from the URL path; extra filename ignored.
    // Short paths answer 400 with the default text/html MIME in place.
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() < 5 {
        return Response {
            status: 400,
            reason: reason_of(400),
            mime: Some("text/html".to_string()),
            body: Vec::new(),
        };
    }
    // Signature check (`rpc.py:206-210`): the URL signature must equal the
    // real product signature; mismatches log a warning and answer 403.
    let guid = segments[3];
    let fid = segments[4];
    let presented = segments[2];
    // Query strictness (`rpc.py:200`) runs before the signature check, so
    // malformed queries answer 400 exactly like the reference precedence.
    let (width, height) = match parse_thumb_query(query) {
        Ok(sizes) => sizes,
        Err(_) => {
            return Response {
                status: 400,
                reason: reason_of(400),
                mime: Some("text/html".to_string()),
                body: Vec::new(),
            };
        }
    };
    let expected = hash_link(secret, &format!("{guid}/{fid}"));
    if presented != expected.as_str() {
        // The reference falls through to the shared tail with the default
        // text/html MIME on a hash mismatch.
        return Response {
            status: 403,
            reason: reason_of(403),
            mime: Some("text/html".to_string()),
            body: Vec::new(),
        };
    }
    let (bytes, mime) = match read_gallery_file(app, guid, fid) {
        Err(fail) => {
            // `rpc.py:211` unpacks `(None, None, None)` for an unknown guid
            // and `send_header("Content-Type", None)` coerces to the literal
            // string "None": byte-exact preservation of the observable wire.
            let mime = match fail {
                GalleryFail::NotFound => Some("None".to_string()),
                GalleryFail::BadRequest => Some("text/html".to_string()),
                GalleryFail::Server(_) => None,
            };
            return Response {
                status: fail.status(),
                reason: reason_of(fail.status()),
                mime,
                body: Vec::new(),
            };
        }
        Ok(found) => found,
    };
    let (body, out_mime) = match thumbnail(&bytes, width, height) {
        Ok((resized, mime)) => (resized, mime),
        Err(ThumbError::Passthrough) => (bytes, mime),
        Err(ThumbError::BadRequest(_)) => {
            return Response {
                status: 400,
                reason: reason_of(400),
                mime: None,
                body: Vec::new(),
            };
        }
        Err(ThumbError::Decode(_)) => {
            return Response {
                status: 500,
                reason: reason_of(500),
                mime: None,
                body: Vec::new(),
            };
        }
    };
    Response {
        status: 200,
        reason: reason_of(200),
        mime: Some(out_mime),
        body,
    }
}

fn serve_archive(app: &App, path: &str, secret: Option<&str>) -> Response {
    let segments: Vec<&str> = path.split('/').collect();
    if segments.len() < 5 {
        return Response {
            status: 400,
            reason: reason_of(400),
            mime: Some("text/html".to_string()),
            body: Vec::new(),
        };
    }
    // Signature check (`rpc.py:240-244`): the URL signature must equal the
    // real product signature; mismatches log a warning and answer 403.
    // An empty guid can never present a valid signature, so it 403s here
    // exactly like the reference instead of reaching the archive lookup.
    let guid = segments[3];
    let presented = segments[2];
    let expected = hash_link(secret, guid);
    if presented != expected.as_str() {
        // Same shared-tail default as the image path: text/html.
        return Response {
            status: 403,
            reason: reason_of(403),
            mime: Some("text/html".to_string()),
            body: Vec::new(),
        };
    }
    match read_archive(app, guid) {
        Err(fail) => Response {
            status: fail.status(),
            reason: reason_of(fail.status()),
            mime: None,
            body: Vec::new(),
        },
        Ok(bytes) => Response {
            status: 200,
            reason: reason_of(200),
            mime: Some("application/zip".to_string()),
            body: bytes,
        },
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
            body: entry.body,
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
                body: serde_json::to_vec(&envelope).unwrap_or_default(),
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
                body: serde_json::to_vec(&envelope).unwrap_or_default(),
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
        body: serde_json::to_vec(&envelope).unwrap_or_default(),
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
        body: serde_json::to_vec(&envelope).unwrap_or_default(),
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
                body: serde_json::to_vec(&envelope).unwrap_or_default(),
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
            body: serde_json::to_vec(&envelope).unwrap_or_default(),
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
            body: serde_json::to_vec(&envelope).unwrap_or_default(),
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
/// (VM ownership) and runs on the single `LocalSet` thread.
pub struct SharedRpc {
    pub rpc: RpcDispatcher,
    pub cache: StaticCache,
    pub pack: Option<FrozenPack>,
    pub login_filters: crate::filters::Filters,
    pub secret: Option<String>,
    pub static_origin: String,
    pub clock: Arc<dyn HttpClock>,
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

/// Read one HTTP request head plus optional body from the stream.
pub async fn read_request<S>(
    stream: &mut S,
) -> Result<(String, Vec<(String, String)>, Vec<u8>), u16>
where
    S: AsyncRead + Unpin,
{
    let mut head = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        if head.len() > MAX_HEAD_BYTES {
            return Err(413);
        }
        let n = stream.read(&mut buf).await.map_err(|_| 400u16)?;
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
            let n = stream.read(&mut buf).await.map_err(|_| 400u16)?;
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
                            body: entry.body,
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
                                    body: bytes.to_vec(),
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
                                        body: serde_json::to_vec(&body).unwrap_or_default(),
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
                        body,
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
                            body: entry.body,
                        },
                        None => match shared_ref.pack.as_ref().and_then(|pack| pack.get(&name)) {
                            Some((bytes, mime)) => Response {
                                status: 200,
                                reason: reason_of(200),
                                mime: Some(mime.to_string()),
                                body: bytes.to_vec(),
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
                                    body: serde_json::to_vec(&body).unwrap_or_default(),
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
    };
    let bytes = response.bytes(true);
    stream.write_all(&bytes).await?;
    stream.shutdown().await?;
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
        body: serde_json::to_vec(&envelope).unwrap_or_default(),
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
        body: serde_json::to_vec(&envelope).unwrap_or_default(),
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
        body: serde_json::to_vec(&outcome.envelope).unwrap_or_default(),
    }
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
pub async fn serve_until_closed(
    listener: TcpListener,
    mut shutdown: watch::Receiver<bool>,
    handle: crate::core::runtime::AppHandle,
    shared: Rc<RefCell<SharedRpc>>,
    static_origin: String,
) -> Result<(), ServerError> {
    use tokio::task::JoinSet;
    let mut owned = JoinSet::new();
    loop {
        tokio::select! {
            incoming = listener.accept() => {
                let (mut stream, _) = incoming?;
                let handle = handle.clone();
                let shared = Rc::clone(&shared);
                let static_origin = static_origin.clone();
                owned.spawn_local(async move {
                    let _ = serve_one(&mut stream, handle, shared, static_origin).await;
                });
            }
            _ = shutdown.changed() => {
                if *shutdown.borrow() {
                    break;
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
    use crate::rpc::{check_pixels, refuse_unsafe_name};
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
        // Serving the signed URL returns the exact file bytes.
        let image = dispatch_get(
            &app,
            &mut StaticCache::load(
                Path::new("/nonexistent-webui-cache.json"),
                Arc::new(crate::worker::RealClock),
            ),
            first,
            None,
        );
        match image {
            RequestAction::Ready(response) => {
                assert_eq!(response.status, 200);
                assert_eq!(response.mime.as_deref(), Some("image/jpeg"));
                assert_eq!(response.body, png);
            }
            other => panic!("expected ready image, got {other:?}"),
        }
        // A tampered signature is refused without touching disk.
        let tampered = first.replacen(sign, "00000000", 1);
        let refused = dispatch_get(
            &app,
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
        let served = dispatch_get(
            &app,
            &mut StaticCache::load(
                &cache_path.join("webui-cache.json"),
                Arc::new(crate::worker::RealClock),
            ),
            first,
            Some("s3cret"),
        );
        match served {
            RequestAction::Ready(response) => {
                assert_eq!(response.status, 200);
                assert_eq!(response.body, png);
            }
            other => panic!("expected a signed image, got {other:?}"),
        }
        // The same URL is not valid on a server without that secret.
        let unsigned = dispatch_get(
            &app,
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
        fn ready(action: RequestAction) -> Response {
            match action {
                RequestAction::Ready(response) => response,
                other => panic!("expected a ready gallery response, got {other:?}"),
            }
        }
        let mut app = test_app();
        let png = png_bytes(8, 8);
        let guid = task_with_image(&mut app, "1.jpg", &png);
        let cache_dir = tempfile::tempdir().expect("cache").keep();
        // Mismatched and empty signatures answer 403 without touching disk.
        let image = dispatch_get(
            &app,
            &mut StaticCache::load(
                Path::new("/nonexistent-webui-cache.json"),
                Arc::new(crate::worker::RealClock),
            ),
            "/img/00000000/guid1/1",
            None,
        );
        assert_eq!(status_of(image), 403);
        let archive = dispatch_get(
            &app,
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
        let served = ready(dispatch_get(
            &app,
            &mut StaticCache::load(
                &cache_dir.join("webui-cache.json"),
                Arc::new(crate::worker::RealClock),
            ),
            &image_path,
            None,
        ));
        assert_eq!(served.status, 200);
        assert_eq!(served.mime.as_deref(), Some("image/jpeg"));
        assert_eq!(served.body, png);
        // A valid /zip signature serves the built archive bytes.
        let zip_path = format!("/zip/{}/{guid}/gallery.zip", hash_link(None, &guid));
        let archived = ready(dispatch_get(
            &app,
            &mut StaticCache::load(
                &cache_dir.join("webui-cache.json"),
                Arc::new(crate::worker::RealClock),
            ),
            &zip_path,
            None,
        ));
        assert_eq!(archived.status, 200);
        assert_eq!(archived.mime.as_deref(), Some("application/zip"));
        assert!(archived.body.starts_with(b"PK"));
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
        let envelope: Value = serde_json::from_slice(&response.body).expect("envelope");
        (response.status, envelope)
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

    struct LoginFixture {
        _server: std::thread::JoinHandle<()>,
        url: String,
        requests: std::sync::Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>,
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
        // Only width: height scales proportionally.
        let (wide, _) = thumbnail(&png, Some(50), None).expect("thumb");
        assert_eq!(image_dims(&wide), (50, 25));
        // Only height.
        let (tall, _) = thumbnail(&png, None, Some(25)).expect("thumb");
        assert_eq!(image_dims(&tall), (50, 25));
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
        let app = test_app();
        let cache_dir = tempfile::tempdir().expect("cache").keep();
        let cache_path = cache_dir.join("webui-cache.json");
        let before = open_fds();
        for index in 0..64 {
            let mut cache = StaticCache::load(&cache_path, Arc::new(crate::worker::RealClock));
            let _ = dispatch_get(
                &app,
                &mut cache,
                &format!("/img/00000000/guid{index}/1"),
                None,
            );
            let mut cache = StaticCache::load(&cache_path, Arc::new(crate::worker::RealClock));
            let _ = dispatch_get(&app, &mut cache, "/zip/00000000/none/gallery.zip", None);
        }
        let after = open_fds();
        assert!(after <= before + 4, "descriptor leak: {before} -> {after}");
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
        let reloaded = StaticCache::load(&path, Arc::clone(&clock) as Arc<dyn HttpClock>);
        assert!(reloaded.fresh("/").is_some());
        let broken = StaticCache::load(
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
}
