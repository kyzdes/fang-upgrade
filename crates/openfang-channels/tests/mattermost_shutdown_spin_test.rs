//! FANG-40 — a Mattermost adapter dropped without `stop()` must not spin.
//!
//! Three adapters answer a failing `shutdown.changed()` with `continue`
//! (discord, mattermost, slack). Once the last `watch::Sender` is dropped that
//! call fails instantly and permanently, so the `select!` is re-armed on a
//! future that is always ready: one core, forever.
//!
//! Mattermost is the only one of the three reachable from a test. Discord and
//! Slack hardcode `discord.com` / `slack.com` and validate the token inside
//! `start()` before spawning anything, so their loops cannot be entered without
//! the real vendor endpoint over TLS.
//!
//! This is its own test binary because `/proc/self/stat` measures the whole
//! process: another spinning task in the same binary would be indistinguishable
//! from this one.

use std::sync::Arc;
use std::time::{Duration, Instant};

use openfang_channels::mattermost::MattermostAdapter;
use openfang_channels::types::ChannelAdapter;

/// Process CPU time (user + system) in seconds, from `/proc/self/stat`.
///
/// Fields 14 and 15 are `utime`/`stime` in clock ticks; `USER_HZ` is fixed at
/// 100 on Linux by ABI.
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("read /proc/self/stat");
    // `comm` (field 2) can contain spaces and parentheses — skip past the last ')'.
    let rest = &stat[stat.rfind(')').expect("malformed stat") + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let utime: u64 = fields[11].parse().expect("utime");
    let stime: u64 = fields[12].parse().expect("stime");
    (utime + stime) as f64 / 100.0
}

/// Fraction of one core this process burned over `window`.
async fn cpu_fraction_over(window: Duration) -> f64 {
    let cpu0 = cpu_seconds();
    let t0 = Instant::now();
    tokio::time::sleep(window).await;
    let wall = t0.elapsed().as_secs_f64();
    (cpu_seconds() - cpu0) / wall
}

/// A spinning `select!` pins a core; anything idle stays far below this.
const SPIN_THRESHOLD: f64 = 0.25;

/// Stub Mattermost server: just enough API for the real adapter to
/// authenticate and park in its WebSocket read loop.
///
/// Returns the base URL and a receiver that fires once the adapter's WebSocket
/// has been upgraded. Waiting for that signal is what makes the measurement
/// mean anything — an adapter stuck in connect-backoff looks idle whether or
/// not the bug is present, which would be a false pass.
async fn spawn_mattermost_stub() -> (String, tokio::sync::oneshot::Receiver<()>) {
    use axum::extract::ws::{WebSocket, WebSocketUpgrade};
    use axum::routing::get;
    use axum::{Json, Router};

    let (connected_tx, connected_rx) = tokio::sync::oneshot::channel();
    let connected_tx = Arc::new(std::sync::Mutex::new(Some(connected_tx)));

    let ws_handler = move |upgrade: WebSocketUpgrade| {
        let connected_tx = connected_tx.clone();
        async move {
            upgrade.on_upgrade(move |mut socket: WebSocket| async move {
                if let Some(tx) = connected_tx.lock().unwrap().take() {
                    let _ = tx.send(());
                }
                // Swallow the authentication_challenge, then stay silent so the
                // adapter parks in `ws_rx.next()`.
                while socket.recv().await.is_some() {}
            })
        }
    };

    let app = Router::new()
        .route(
            "/api/v4/users/me",
            get(|| async { Json(serde_json::json!({ "id": "botid", "username": "stub-bot" })) }),
        )
        .route("/api/v4/websocket", get(ws_handler));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), connected_rx)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_a_mattermost_adapter_without_stop_does_not_spin() {
    let idle = cpu_fraction_over(Duration::from_millis(500)).await;
    assert!(
        idle < SPIN_THRESHOLD,
        "test harness is already busy ({idle:.2} cores) — measurement would be meaningless"
    );

    let (base_url, connected) = spawn_mattermost_stub().await;
    let adapter = Arc::new(MattermostAdapter::new(
        base_url,
        "stub-token".to_string(),
        Vec::new(),
    ));
    let _stream = adapter.start().await.expect("stub start");

    tokio::time::timeout(Duration::from_secs(5), connected)
        .await
        .expect("adapter never opened the WebSocket — nothing to measure")
        .expect("stub dropped the signal");
    // Let it get past the auth send and settle into the read loop.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        Arc::strong_count(&adapter),
        1,
        "the test must hold the only Arc, or dropping it will not drop the Sender"
    );
    drop(adapter); // deliberately NOT stop() — this is the FANG-40 hazard

    let spin = cpu_fraction_over(Duration::from_secs(2)).await;
    eprintln!("FANG40_IDLE_CORES={idle:.3} FANG40_MATTERMOST_CORES={spin:.3}");
    assert!(
        spin < SPIN_THRESHOLD,
        "Mattermost read loop spins after the adapter is dropped: {spin:.2} cores"
    );
}
