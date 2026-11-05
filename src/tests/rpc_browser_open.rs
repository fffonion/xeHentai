//! `--rpc-open-browser`: the one-shot native WebUI opener.
//!
//! Every launch goes through the injected runner, so no test starts a browser.
//! Readiness now comes from the real `rpc::server::serve` startup path: the
//! bound address is handed off only after the listener binds and `SharedRpc`
//! initializes. No test (or production branch) probes the port, so a foreign
//! listener on the configured port can never cause a launch.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::{oneshot, watch};

use crate::rpc::server::{bind as bind_rpc, serve};
use crate::{
    BrowserOpenError, browser_opener_program, browser_url, open_webui_when_ready, rpc_browser_port,
};

/// `(program, args)` of one launched opener.
type Invocation = (String, Vec<String>);

fn record(
    invocations: &Mutex<Vec<Invocation>>,
) -> impl Fn(&mut Command) -> std::io::Result<()> + '_ {
    move |command| {
        invocations.lock().unwrap().push((
            command.get_program().to_string_lossy().into_owned(),
            command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect(),
        ));
        Ok(())
    }
}

#[test]
fn browser_opens_only_when_both_switches_allow_it() {
    assert_eq!(rpc_browser_port(true, true, Some(7000)), Some(7000));
    assert_eq!(rpc_browser_port(true, false, Some(7000)), None);
    assert_eq!(rpc_browser_port(false, true, Some(7000)), None);
    assert_eq!(rpc_browser_port(true, true, None), None);
    // The same range `serve_rpc` enforces: these never reach a listener.
    assert_eq!(rpc_browser_port(true, true, Some(0)), None);
    assert_eq!(rpc_browser_port(true, true, Some(-1)), None);
    assert_eq!(rpc_browser_port(true, true, Some(65536)), None);
}

/// The platform program is chosen at build time; a Windows CI run selects the
/// `explorer.exe` branch, every Unix run `open`.
#[test]
fn opener_program_matches_the_platform() {
    #[cfg(windows)]
    assert_eq!(browser_opener_program(), "explorer.exe");
    #[cfg(not(windows))]
    assert_eq!(browser_opener_program(), "open");
}

/// One program, one URL argument -- never a shell command line. The fragment's
/// commas and `#` stay inside that single argument.
#[test]
fn opener_command_carries_exactly_one_url_argument() {
    let invocations = Mutex::new(Vec::new());
    let launch = record(&invocations);
    let url = "http://[::1]:7000/ui/#host=[::1],port=7000,https=no";
    crate::open_browser(url, &launch).expect("injected runner");
    let invocations = invocations.lock().unwrap();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0].0, browser_opener_program());
    assert_eq!(invocations[0].1, vec![url.to_string()]);
}

/// The frozen WebUI only takes its connection from the URL fragment
/// (`host + ':' + port`). Both values must be the listener's own, so a custom
/// or ephemeral port is used instead of the UI's 8010/persisted fallback.
#[test]
fn browser_url_pins_the_bound_host_and_port_in_the_webui_fragment() {
    assert_eq!(
        browser_url(SocketAddr::from((Ipv4Addr::LOCALHOST, 7000))),
        "http://127.0.0.1:7000/ui/#host=127.0.0.1,port=7000,https=no"
    );
    // The WebUI concatenates `host + ':' + port`, so the fragment host keeps
    // the brackets even though the URL authority already has them.
    assert_eq!(
        browser_url(SocketAddr::from((Ipv6Addr::LOCALHOST, 7001))),
        "http://[::1]:7001/ui/#host=[::1],port=7001,https=no"
    );
    // Wildcard binds are not browsable; the loopback of the same family is.
    assert_eq!(
        browser_url(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 7002))),
        "http://127.0.0.1:7002/ui/#host=127.0.0.1,port=7002,https=no"
    );
    assert_eq!(
        browser_url(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 7003))),
        "http://[::1]:7003/ui/#host=[::1],port=7003,https=no"
    );
}

/// `rpc_secret` never enters the URL. The WebUI's fragment parser reads a
/// `token` key, so a secret here would both expose the opener argv and
/// overwrite the UI's stored secret; the config dialog owns it instead.
#[test]
fn browser_url_never_carries_the_rpc_secret() {
    const SECRET: &str = "dummy-secret-8f3a1c";
    let url = browser_url(SocketAddr::from((Ipv4Addr::LOCALHOST, 7000)));
    assert!(!url.contains(SECRET));
    assert!(!url.contains("token="));
    let keys: Vec<&str> = url
        .split_once('#')
        .expect("fragment")
        .1
        .split(',')
        .map(|pair| pair.split('=').next().expect("key"))
        .collect();
    assert_eq!(keys, ["host", "port", "https"]);
}

/// A port-0 listener is assigned a real port; the URL must name that port.
#[tokio::test]
async fn browser_url_never_keeps_a_zero_port() {
    let listener = bind_rpc("127.0.0.1", 0).await.unwrap();
    let bound = listener.local_addr().unwrap();
    assert_ne!(bound.port(), 0);
    assert_eq!(
        browser_url(bound),
        format!(
            "http://127.0.0.1:{0}/ui/#host=127.0.0.1,port={0},https=no",
            bound.port()
        )
    );
}

/// Real startup path: `serve` binds port 0 and initializes `SharedRpc`, then
/// publishes the actual bound address. The opener runs once with one URL
/// argument naming that address -- never the configured request.
#[tokio::test]
async fn readiness_hands_off_the_bound_listener_after_server_init() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = tempfile::tempdir().expect("root");
    let cache = tempfile::tempdir().expect("cache");
    let runtime =
        crate::core::Runtime::new(super::test_app(root.path(), &[]), manifest.to_path_buf());
    let handle = runtime.handle();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let (ready_tx, ready_rx) = oneshot::channel();
    let server = serve(
        "127.0.0.1",
        0,
        cache.path(),
        manifest,
        Arc::new(crate::worker::RealClock),
        None,
        "http://127.0.0.1:9".to_string(),
        shutdown_rx,
        handle,
        ready_tx,
    );
    tokio::pin!(server);

    let invocations = Mutex::new(Vec::new());
    let launch = record(&invocations);
    let open = open_webui_when_ready(ready_rx, Duration::from_secs(5), &launch);
    tokio::pin!(open);
    let addr = tokio::select! {
        result = &mut open => result.expect("startup must hand off the bound address"),
        result = &mut server => panic!("server ended before readiness: {result:?}"),
    };

    // The handoff is the listener's own address, not the configured port 0.
    assert_eq!(addr.ip(), Ipv4Addr::LOCALHOST);
    assert_ne!(addr.port(), 0);
    let recorded = invocations.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1, "exactly one launch: {recorded:?}");
    assert_eq!(recorded[0].0, browser_opener_program());
    assert_eq!(
        recorded[0].1,
        vec![format!(
            "http://127.0.0.1:{0}/ui/#host=127.0.0.1,port={0},https=no",
            addr.port()
        )]
    );
    assert_eq!(recorded[0].1.len(), 1, "one URL argument, no shell");

    let _ = shutdown_tx.send(true);
    server.as_mut().await.expect("clean shutdown");
}

/// A foreign process already owns the requested port. This app's real bind
/// fails, no readiness is published, and the browser never launches -- even
/// though the foreign listener is happily accepting connections.
#[tokio::test]
async fn foreign_listener_on_the_configured_port_never_opens_the_browser() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let root = tempfile::tempdir().expect("root");
    let cache = tempfile::tempdir().expect("cache");
    let foreign = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("foreign owner");
    let port = foreign.local_addr().expect("foreign addr").port();
    let runtime =
        crate::core::Runtime::new(super::test_app(root.path(), &[]), manifest.to_path_buf());
    let handle = runtime.handle();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let (ready_tx, ready_rx) = oneshot::channel();
    let server = serve(
        "127.0.0.1",
        port,
        cache.path(),
        manifest,
        Arc::new(crate::worker::RealClock),
        None,
        "http://127.0.0.1:9".to_string(),
        shutdown_rx,
        handle,
        ready_tx,
    );
    tokio::pin!(server);

    let invocations = Mutex::new(Vec::new());
    let launch = record(&invocations);
    let open = open_webui_when_ready(ready_rx, Duration::from_secs(5), &launch);
    tokio::pin!(open);
    let (server_result, open_result) = tokio::join!(server.as_mut(), open);

    assert!(
        server_result.is_err(),
        "the app bind must fail against the foreign owner"
    );
    assert!(matches!(open_result, Err(BrowserOpenError::NotReady)));
    assert!(
        invocations.lock().unwrap().is_empty(),
        "a foreign listener must never be opened"
    );
    drop(foreign);
}

/// Bind succeeds but `SharedRpc` initialization fails. Readiness is published
/// only after initialization, so the opener observes no handoff and stays
/// shut; the server error remains the real startup result.
#[tokio::test]
async fn failed_server_init_after_bind_never_opens_the_browser() {
    let empty = tempfile::tempdir().expect("empty resource root");
    let root = tempfile::tempdir().expect("root");
    let cache = tempfile::tempdir().expect("cache");
    let runtime = crate::core::Runtime::new(
        super::test_app(root.path(), &[]),
        empty.path().to_path_buf(),
    );
    let handle = runtime.handle();
    let (_shutdown_tx, shutdown_rx) = watch::channel(false);
    let (ready_tx, ready_rx) = oneshot::channel();
    let server = serve(
        "127.0.0.1",
        0,
        cache.path(),
        empty.path(),
        Arc::new(crate::worker::RealClock),
        None,
        "http://127.0.0.1:9".to_string(),
        shutdown_rx,
        handle,
        ready_tx,
    );
    tokio::pin!(server);

    let invocations = Mutex::new(Vec::new());
    let launch = record(&invocations);
    let open = open_webui_when_ready(ready_rx, Duration::from_secs(5), &launch);
    tokio::pin!(open);
    let (server_result, open_result) = tokio::join!(server.as_mut(), open);

    assert!(server_result.is_err(), "init must fail on an empty root");
    assert!(matches!(open_result, Err(BrowserOpenError::NotReady)));
    assert!(invocations.lock().unwrap().is_empty());
}

/// No readiness within the budget (startup hung) opens nothing and does not
/// wait past the budget.
#[tokio::test]
async fn no_launch_when_readiness_never_arrives() {
    let (_ready_tx, ready_rx) = oneshot::channel::<SocketAddr>();
    let invocations = Mutex::new(Vec::new());
    let launch = record(&invocations);

    let result = open_webui_when_ready(ready_rx, Duration::from_millis(50), &launch).await;

    assert!(matches!(result, Err(BrowserOpenError::NotReady)));
    assert!(invocations.lock().unwrap().is_empty());
}

/// A missing/failed OS opener is reported as a warning outcome, never as a
/// run failure: the caller only logs it.
#[tokio::test]
async fn a_failed_opener_is_reported_and_not_fatal() {
    let (ready_tx, ready_rx) = oneshot::channel();
    ready_tx
        .send(SocketAddr::from((Ipv4Addr::LOCALHOST, 7000)))
        .expect("ready receiver");
    let launch = |_: &mut Command| Err(std::io::Error::other("no opener"));

    let result = open_webui_when_ready(ready_rx, Duration::from_secs(1), &launch).await;

    assert!(matches!(result, Err(BrowserOpenError::Spawn(_))));
}
