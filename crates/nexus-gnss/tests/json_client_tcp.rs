//! Sanity test for [`JsonGpsdClient`]: spin up a tiny TCP listener
//! that pretends to be gpsd, feed the client a VERSION + a couple
//! of canned TPV / SKY lines, and verify the backend receives the
//! translated events on the bus.
//!
//! This is part of the exit criterion for phase 2 — mock-gpsd over
//! TCP streaming 100 TPVs round-trips without loss.

use std::sync::Arc;
use std::time::Duration;

use nexus_core::NexusEvent;
use nexus_gnss::{GpsdClient, JsonGpsdClient};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

#[tokio::test]
async fn json_client_receives_100_tpvs_over_tcp() {
    // Bind on an ephemeral port so multiple test runs don't collide.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Fake gpsd server: send VERSION on connect, swallow ?WATCH,
    // stream 100 TPVs, then close.
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        writer
            .write_all(
                br#"{"class":"VERSION","release":"3.23","rev":"3.23","proto_major":3,"proto_minor":14}
"#,
            )
            .await
            .unwrap();
        // Eat one line (the ?WATCH).
        let mut r = BufReader::new(reader);
        let mut buf = String::new();
        let _ = r.read_line(&mut buf).await;
        for i in 0..100 {
            let line = format!(
                r#"{{"class":"TPV","device":"/dev/ttyUSB0","mode":3,"lat":{:.3},"lon":{:.3},"altHAE":50.0,"used":8}}
"#,
                37.0 + (i as f64) * 0.0001,
                -122.0 + (i as f64) * 0.0001,
            );
            writer.write_all(line.as_bytes()).await.unwrap();
        }
        // Linger so the client can drain.
        tokio::time::sleep(Duration::from_millis(200)).await;
    });

    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(256);
    let client: Arc<dyn GpsdClient> = Arc::new(JsonGpsdClient::new(addr, event_tx));
    client.connect().await.expect("connect fake gpsd");

    let mut tpvs = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while tpvs < 100 && tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Ok(NexusEvent::GnssTpvReceived { .. })) => tpvs += 1,
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    assert_eq!(tpvs, 100, "expected 100 TPVs, got {tpvs}");

    let _ = server.await;
}

#[tokio::test]
async fn json_client_rejects_protocol_older_than_3() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        // proto_major=2 → reject.
        stream
            .write_all(
                br#"{"class":"VERSION","release":"2.95","rev":"2.95","proto_major":2,"proto_minor":0}
"#,
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
    });
    let (event_tx, _rx) = broadcast::channel::<NexusEvent>(8);
    let client = JsonGpsdClient::new(addr, event_tx);
    let err = client.connect().await.unwrap_err();
    assert!(matches!(
        err,
        nexus_gnss::GnssError::GpsdProtocolTooOld { got: 2 }
    ));
    let _ = server.await;
}

#[tokio::test]
async fn json_client_signals_disconnected_when_server_closes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        writer
            .write_all(
                br#"{"class":"VERSION","release":"3.23","rev":"3.23","proto_major":3,"proto_minor":14}
"#,
            )
            .await
            .unwrap();
        // Swallow the ?WATCH the client sends post-handshake, then
        // close. The explicit read ensures the write half has
        // drained before we drop the connection.
        let mut r = BufReader::new(reader);
        let mut buf = String::new();
        let _ = r.read_line(&mut buf).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        drop(writer);
    });
    let (event_tx, mut event_rx) = broadcast::channel::<NexusEvent>(8);
    let client: Arc<dyn GpsdClient> = Arc::new(JsonGpsdClient::new(addr, event_tx));
    client.connect().await.unwrap();
    // Wait for the reader to observe EOF.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let mut saw_disconnect = false;
    while tokio::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        match tokio::time::timeout(remaining, event_rx.recv()).await {
            Ok(Ok(NexusEvent::GnssGpsdDisconnected)) => {
                saw_disconnect = true;
                break;
            }
            Ok(Ok(_)) => continue,
            _ => break,
        }
    }
    assert!(
        saw_disconnect,
        "expected GnssGpsdDisconnected after server close"
    );
    let _ = server.await;
}
