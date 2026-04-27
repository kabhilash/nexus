//! Internet-connectivity probe.
//!
//! Runs as a supervised daemon-scope task. The state machine is
//! event-driven on top of a periodic backstop:
//!
//! 1. Track the set of currently link-ready interfaces (Wi-Fi and
//!    Ethernet). Membership flips on
//!    `WifiLinkReady`/`EthLinkReady` (insert) and
//!    `WifiLinkLost`/`EthLinkLost` (remove).
//! 2. Whenever an interface becomes link-ready, immediately probe.
//! 3. Whenever the last link-ready interface drops, immediately
//!    publish `Offline` — no point waiting for the next probe to time
//!    out.
//! 4. While at least one link is ready, run the periodic backstop
//!    probe every `interval`. The backstop is what catches the
//!    online → captive-portal transition that happens *after* the
//!    link came up (portal session expiry, ISP blip, …).
//!
//! When the set is empty, the periodic timer is skipped — there's
//! nothing to probe and the state is already pinned at `Offline`.
//!
//! Only transitions are emitted on the bus; same-state probes are
//! silenced to avoid pinging the D-Bus signal bus 1:1 with the probe
//! cadence.
//!
//! ## HTTP probe
//!
//! `204 No Content` → `Online`; `200`/`3xx` → `CaptivePortal`;
//! anything else (DNS failure, connect refused, timeout, malformed
//! response, `4xx`/`5xx`) → `Offline`.
//!
//! No HTTP client crate; the probe URL is plain HTTP and we only need
//! the status line, so a 30-line tokio-TCP HTTP/1.1 implementation is
//! both lighter and easier to reason about than pulling in `reqwest`.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::{Result, anyhow};
use nexus_core::{ConnectivityState, NexusEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::broadcast;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Tunables for the connectivity probe. Defaults match the values the
/// daemon's config layer supplies for the Ubuntu probe URL.
#[derive(Debug, Clone)]
pub struct ConnectivityConfig {
    /// The URL to GET. Plain HTTP only — captive-portal detection on
    /// HTTPS is meaningless because the portal would have to MITM the
    /// connection (which fails TLS in a way that's already
    /// indistinguishable from "offline").
    pub url: String,
    /// How often the timer-driven probe fires. Event-driven probes
    /// (link-ready) fire on top of this without resetting the timer.
    pub interval: Duration,
    /// Total time budget for one probe — covers DNS, connect, write,
    /// and reading back the status line.
    pub timeout: Duration,
}

impl Default for ConnectivityConfig {
    fn default() -> Self {
        Self {
            url: "http://connectivity-check.ubuntu.com/".to_owned(),
            interval: Duration::from_secs(30),
            timeout: Duration::from_secs(5),
        }
    }
}

/// Parsed view of an `http://host[:port]/path` URL — the only shape
/// the probe accepts. Lives in this module rather than depending on a
/// URL crate.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeUrl {
    host: String,
    port: u16,
    path: String,
}

fn parse_http_url(url: &str) -> Result<ProbeUrl> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("connectivity url must be http://, got {url:?}"))?;
    let (authority, path) = match rest.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (rest, "/".to_owned()),
    };
    if authority.is_empty() {
        return Err(anyhow!("connectivity url has no host: {url:?}"));
    }
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, port_str)) if !h.is_empty() => {
            let port: u16 = port_str
                .parse()
                .map_err(|_| anyhow!("invalid port in connectivity url: {url:?}"))?;
            (h.to_owned(), port)
        }
        _ => (authority.to_owned(), 80),
    };
    Ok(ProbeUrl { host, port, path })
}

const PROBE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Send the GET, read up to the end of the status line, classify.
/// Wrapped in a single outer timeout that covers connect / write /
/// read together.
async fn probe_once(url: &ProbeUrl, request_timeout: Duration) -> ConnectivityState {
    let target = format!("{}:{}", url.host, url.port);
    let result = timeout(request_timeout, async {
        let mut stream = TcpStream::connect(&target).await?;
        let req = format!(
            "GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: nexus-connectivity/{ver}\r\nAccept: */*\r\nConnection: close\r\n\r\n",
            path = url.path,
            host = url.host,
            ver = PROBE_VERSION,
        );
        stream.write_all(req.as_bytes()).await?;
        // The body could be a multi-MB captive-portal page; we only
        // need the status line. Read until we've seen the first CRLF
        // or hit the small cap.
        let mut buf = [0u8; 256];
        let mut total = 0usize;
        while total < buf.len() {
            match stream.read(&mut buf[total..]).await? {
                0 => break,
                n => total += n,
            }
            if buf[..total].windows(2).any(|w| w == b"\r\n") {
                break;
            }
        }
        Ok::<ConnectivityState, std::io::Error>(parse_status(&buf[..total]))
    })
    .await;

    match result {
        Ok(Ok(state)) => state,
        Ok(Err(e)) => {
            debug!(error = %e, target, "connectivity: probe io error");
            ConnectivityState::Offline
        }
        Err(_) => {
            debug!(target, "connectivity: probe timed out");
            ConnectivityState::Offline
        }
    }
}

/// Inspect the start of an HTTP/1.1 response and decide.
fn parse_status(buf: &[u8]) -> ConnectivityState {
    // Status line is ASCII; truncating at the first non-UTF-8 byte is
    // safe and deterministic.
    let prefix = std::str::from_utf8(buf).unwrap_or_else(|e| {
        std::str::from_utf8(&buf[..e.valid_up_to()]).unwrap_or("")
    });
    let mut parts = prefix.split_whitespace();
    match parts.next() {
        Some(v) if v.starts_with("HTTP/") => {}
        _ => return ConnectivityState::Offline,
    }
    let code = match parts.next().and_then(|c| c.parse::<u16>().ok()) {
        Some(c) => c,
        None => return ConnectivityState::Offline,
    };
    match code {
        204 => ConnectivityState::Online,
        // 200 with a body on a generate_204 endpoint is a portal
        // serving its login page; 3xx is a portal redirecting us to
        // its login page.
        200 | 301 | 302 | 303 | 307 | 308 => ConnectivityState::CaptivePortal,
        _ => ConnectivityState::Offline,
    }
}

/// What kind of interface owns a given ifindex in the link-ready
/// set. Tracked for log clarity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkKind {
    Wifi,
    Ethernet,
}

impl LinkKind {
    fn as_str(self) -> &'static str {
        match self {
            LinkKind::Wifi => "wifi",
            LinkKind::Ethernet => "ethernet",
        }
    }
}

/// Run the connectivity probe loop. Returns `Ok(())` only on
/// `cancel.cancelled()`. Returns `Err` if the URL is malformed
/// (caught at startup) — the supervised wrapper logs and stops.
///
/// `event_rx` must be subscribed *before* the Wi-Fi / Ethernet
/// backends spawn so the initial `LinkReady` events the backends emit
/// during their startup evaluation are not missed (`broadcast::Sender::subscribe`
/// after the fact returns a receiver that only sees messages from
/// then on). The daemon's `main.rs` follows the same pattern as the
/// D-Bus arm and pre-subscribes at daemon scope.
pub async fn run_connectivity(
    cfg: ConnectivityConfig,
    event_tx: broadcast::Sender<NexusEvent>,
    mut event_rx: broadcast::Receiver<NexusEvent>,
    cancel: CancellationToken,
) -> Result<()> {
    let url = parse_http_url(&cfg.url)?;
    let mut tick = tokio::time::interval(cfg.interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // The first `tick.tick()` resolves immediately; the initial probe
    // below covers the t=0 reading, so consume that tick.
    tick.tick().await;

    // Set of (ifindex → kind) for currently link-ready interfaces.
    // Wi-Fi and Ethernet ifindexes share the same global namespace, so
    // a HashSet of ifindex is enough for membership; the LinkKind side
    // table is purely for the structured-log "which kind dropped" line.
    let mut link_ready: HashSet<u32> = HashSet::new();
    let mut link_kinds: std::collections::HashMap<u32, LinkKind> = Default::default();

    info!(
        url = %cfg.url,
        interval_s = cfg.interval.as_secs(),
        timeout_s = cfg.timeout.as_secs(),
        "connectivity probe starting"
    );

    // Initial probe so the state reflects actual reachability from
    // t=0 rather than sitting at `Unknown`. This matters when nexusd
    // restarts on a host where Wi-Fi / Ethernet were already
    // connected — backends will still emit `LinkReady` as they
    // evaluate, but the probe gives an authoritative answer
    // immediately rather than waiting for the next backend tick.
    let initial = probe_once(&url, cfg.timeout).await;
    info!(state = initial.as_str(), "connectivity: initial state");
    let _ = event_tx.send(NexusEvent::InternetConnectivityChanged { state: initial });
    let mut last = initial;

    loop {
        // Drive the periodic timer only while we have a link to probe
        // through. With no link, every probe would just time out and
        // emit a same-state same-result event.
        let timer = async {
            if link_ready.is_empty() {
                std::future::pending::<()>().await;
            } else {
                tick.tick().await;
            }
        };

        let action = tokio::select! {
            _ = cancel.cancelled() => {
                info!("connectivity probe shutting down");
                return Ok(());
            }
            _ = timer => Action::Probe(ProbeReason::Timer),
            ev = event_rx.recv() => {
                match ev {
                    Ok(NexusEvent::WifiLinkReady { ifindex }) => {
                        let inserted = link_ready.insert(ifindex);
                        link_kinds.insert(ifindex, LinkKind::Wifi);
                        if inserted {
                            debug!(ifindex, "connectivity: wifi link ready");
                        }
                        Action::Probe(ProbeReason::Link(LinkKind::Wifi))
                    }
                    Ok(NexusEvent::EthLinkReady { ifindex }) => {
                        let inserted = link_ready.insert(ifindex);
                        link_kinds.insert(ifindex, LinkKind::Ethernet);
                        if inserted {
                            debug!(ifindex, "connectivity: ethernet link ready");
                        }
                        Action::Probe(ProbeReason::Link(LinkKind::Ethernet))
                    }
                    Ok(NexusEvent::WifiLinkLost { ifindex })
                    | Ok(NexusEvent::EthLinkLost { ifindex }) => {
                        let kind = link_kinds.remove(&ifindex);
                        let was_member = link_ready.remove(&ifindex);
                        if was_member && link_ready.is_empty() {
                            debug!(
                                ifindex,
                                kind = kind.map(LinkKind::as_str).unwrap_or("unknown"),
                                "connectivity: last link dropped — going offline"
                            );
                            Action::ForceOffline
                        } else {
                            // Either we never tracked this ifindex
                            // (e.g., quick LinkLost without a prior
                            // LinkReady), or another link is still up.
                            // Trust the periodic probe to confirm.
                            continue;
                        }
                    }
                    Ok(_) => continue,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!(skipped = n, "connectivity: event bus lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        warn!("connectivity: event bus closed; exiting");
                        return Ok(());
                    }
                }
            }
        };

        let next = match action {
            Action::ForceOffline => ConnectivityState::Offline,
            Action::Probe(reason) => {
                if let ProbeReason::Link(kind) = reason {
                    debug!(link = kind.as_str(), "connectivity: probing on link-ready");
                }
                probe_once(&url, cfg.timeout).await
            }
        };

        if next != last {
            info!(
                from = last.as_str(),
                to = next.as_str(),
                "connectivity transition"
            );
            // A failed send means no D-Bus subscriber yet — fine, the
            // next transition will be picked up.
            let _ = event_tx.send(NexusEvent::InternetConnectivityChanged { state: next });
            last = next;
        } else {
            debug!(state = next.as_str(), "connectivity: same-state, no signal");
        }
    }
}

enum Action {
    Probe(ProbeReason),
    ForceOffline,
}

enum ProbeReason {
    Timer,
    Link(LinkKind),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_http_url_default() {
        let u = parse_http_url("http://connectivity-check.ubuntu.com/").unwrap();
        assert_eq!(u.host, "connectivity-check.ubuntu.com");
        assert_eq!(u.port, 80);
        assert_eq!(u.path, "/");
    }

    #[test]
    fn parse_http_url_with_port_and_path() {
        let u = parse_http_url("http://example.test:8080/generate_204").unwrap();
        assert_eq!(u.host, "example.test");
        assert_eq!(u.port, 8080);
        assert_eq!(u.path, "/generate_204");
    }

    #[test]
    fn parse_http_url_no_path() {
        let u = parse_http_url("http://example.test").unwrap();
        assert_eq!(u.path, "/");
    }

    #[test]
    fn parse_http_url_rejects_https() {
        assert!(parse_http_url("https://example.test/").is_err());
    }

    #[test]
    fn parse_http_url_rejects_garbage() {
        assert!(parse_http_url("nonsense").is_err());
        assert!(parse_http_url("http://").is_err());
        assert!(parse_http_url("http://host:notaport/").is_err());
    }

    #[test]
    fn parse_status_204_is_online() {
        let resp = b"HTTP/1.1 204 No Content\r\nServer: nginx\r\n\r\n";
        assert_eq!(parse_status(resp), ConnectivityState::Online);
    }

    #[test]
    fn parse_status_200_is_captive_portal() {
        let resp = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n<html>";
        assert_eq!(parse_status(resp), ConnectivityState::CaptivePortal);
    }

    #[test]
    fn parse_status_302_is_captive_portal() {
        let resp = b"HTTP/1.1 302 Found\r\nLocation: http://login.local/\r\n\r\n";
        assert_eq!(parse_status(resp), ConnectivityState::CaptivePortal);
    }

    #[test]
    fn parse_status_500_is_offline() {
        let resp = b"HTTP/1.1 500 Internal Server Error\r\n\r\n";
        assert_eq!(parse_status(resp), ConnectivityState::Offline);
    }

    #[test]
    fn parse_status_garbage_is_offline() {
        assert_eq!(parse_status(b""), ConnectivityState::Offline);
        assert_eq!(parse_status(b"not http"), ConnectivityState::Offline);
        assert_eq!(parse_status(b"HTTP/1.1 abc"), ConnectivityState::Offline);
    }

    #[test]
    fn connectivity_state_strings_match_wire() {
        assert_eq!(ConnectivityState::Online.as_str(), "internetOnline");
        assert_eq!(ConnectivityState::Offline.as_str(), "internetOffline");
        assert_eq!(
            ConnectivityState::CaptivePortal.as_str(),
            "internetCaptivePortal"
        );
    }
}
