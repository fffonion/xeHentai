//! `--rpc-open-browser`: the one-shot native WebUI opener.
//!
//! Every launch goes through the injected runner, so no test starts a browser.
//! The assertions cover the exact OS program, the single URL argument, the
//! real listener address the URL is built from, and the non-fatal spawn error.

use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

use tokio::net::TcpListener;

use crate::{
    BrowserOpenError, browser_opener_program, browser_url, open_webui_when_listening,
    rpc::server::bind as bind_rpc, rpc_browser_port,
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

/// One program, one URL argument -- never a shell command line.
#[test]
fn opener_command_carries_exactly_one_url_argument() {
    let invocations = Mutex::new(Vec::new());
    let launch = record(&invocations);
    crate::open_browser("http://[::1]:7000/ui/", &launch).expect("injected runner");
    let invocations = invocations.lock().unwrap();
    assert_eq!(invocations.len(), 1);
    assert_eq!(invocations[0].0, browser_opener_program());
    assert_eq!(invocations[0].1, vec!["http://[::1]:7000/ui/".to_string()]);
}

#[test]
fn browser_url_uses_the_live_address_with_ipv6_brackets() {
    assert_eq!(
        browser_url(SocketAddr::from((Ipv4Addr::LOCALHOST, 7000))),
        "http://127.0.0.1:7000/ui/"
    );
    assert_eq!(
        browser_url(SocketAddr::from((Ipv6Addr::LOCALHOST, 7001))),
        "http://[::1]:7001/ui/"
    );
    // Wildcard binds are not browsable; the loopback of the same family is.
    assert_eq!(
        browser_url(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 7002))),
        "http://127.0.0.1:7002/ui/"
    );
    assert_eq!(
        browser_url(SocketAddr::from((Ipv6Addr::UNSPECIFIED, 7003))),
        "http://[::1]:7003/ui/"
    );
}

/// A port-0 listener is assigned a real port; the URL must name that port.
#[tokio::test]
async fn browser_url_never_keeps_a_zero_port() {
    let listener = bind_rpc("127.0.0.1", 0).await.unwrap();
    let bound = listener.local_addr().unwrap();
    assert_ne!(bound.port(), 0);
    assert_eq!(
        browser_url(bound),
        format!("http://127.0.0.1:{}/ui/", bound.port())
    );
}

/// The opener runs only after the production RPC bind accepts, and it runs
/// once with one URL argument naming the listener's own address.
#[tokio::test]
async fn browser_opens_once_after_the_real_listener_binds() {
    let listener = bind_rpc("127.0.0.1", 0).await.unwrap();
    let bound = listener.local_addr().unwrap();
    let invocations = Mutex::new(Vec::new());
    let launch = record(&invocations);

    let result =
        open_webui_when_listening("127.0.0.1", bound.port(), Duration::from_secs(5), &launch).await;

    assert!(result.is_ok());
    let invocations = invocations.lock().unwrap();
    assert_eq!(invocations.len(), 1, "exactly one launch: {invocations:?}");
    assert_eq!(invocations[0].0, browser_opener_program());
    assert_eq!(
        invocations[0].1,
        vec![format!("http://127.0.0.1:{}/ui/", bound.port())]
    );
    assert_eq!(invocations[0].1.len(), 1, "one URL argument, no shell");
    #[cfg(not(windows))]
    assert_eq!(invocations[0].0, "open");
    #[cfg(windows)]
    assert_eq!(invocations[0].0, "explorer.exe");
}

/// A listener that never binds (bind failure) leaves the default browser shut.
#[tokio::test]
async fn browser_stays_shut_when_nothing_listens() {
    let probe = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = probe.local_addr().unwrap().port();
    drop(probe);
    let invocations = Mutex::new(Vec::new());
    let launch = record(&invocations);

    let result =
        open_webui_when_listening("127.0.0.1", port, Duration::from_millis(150), &launch).await;

    assert!(matches!(result, Err(BrowserOpenError::NotListening)));
    assert!(invocations.lock().unwrap().is_empty());
}

/// A missing/failed OS opener is reported as a warning outcome, never as a
/// run failure: the caller only logs it.
#[tokio::test]
async fn a_failed_opener_is_reported_and_not_fatal() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
    let bound = listener.local_addr().unwrap();

    let result =
        open_webui_when_listening("127.0.0.1", bound.port(), Duration::from_secs(5), &|_| {
            Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "no opener on this host",
            ))
        })
        .await;

    if let Err(BrowserOpenError::Spawn(error)) = result {
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    } else {
        panic!("a failed opener must surface as a spawn error");
    }
}
