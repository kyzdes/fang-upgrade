//! FANG-40 — a Discourse adapter dropped without `stop()` must not storm the
//! forum with requests.
//!
//! Eleven adapters were left with the arm the first FANG-40 commit fixed in six
//! others:
//!
//! ```ignore
//! _ = shutdown_rx.changed() => {
//!     if *shutdown_rx.borrow() { break; }
//! }
//! _ = tokio::time::sleep(poll_interval) => {}
//! ```
//!
//! Once the last `watch::Sender` is dropped, `changed()` resolves with `Err`
//! instantly and forever, `borrow()` still reports the old `false`, and the arm
//! falls through. `tests/shutdown_arm_audit.rs` proves none of them is written
//! that way any more. This test is the other half: it shows what the shape
//! actually costs on a live adapter.
//!
//! Discourse is the one worth measuring. Its `select!` races the shutdown
//! against the poll *interval*, so a fall-through does not merely re-arm a
//! ready future — it skips the 10 s sleep and re-enters the HTTP poll. The
//! symptom is not a warm CPU, it is thousands of requests a second at a forum
//! that has no idea the operator turned the channel off. `POLLS_AFTER_DROP` is
//! the number that matters; cores are recorded alongside because it is free.
//!
//! Discourse is also reachable from a test at all, which most of the eleven are
//! not: it takes its `base_url` as a constructor argument. gitter, gotify, irc,
//! linkedin, mqtt, mumble, ntfy, dingtalk and feishu each hardcode a vendor
//! host or need a non-HTTP protocol handshake before their loop is entered.
//!
//! Its own test binary: `/proc/self/stat` is process-wide, so a second spinning
//! task in the same binary would make the core count unattributable.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use openfang_channels::discourse::DiscourseAdapter;
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

/// A spinning `select!` pins a core; anything idle stays far below this.
const SPIN_THRESHOLD: f64 = 0.25;

/// The adapter's own poll interval is 10 s (`POLL_INTERVAL_SECS`), so a healthy
/// adapter makes zero polls in the two-second measurement window. Allowing two
/// leaves room for a poll that was already in flight when the drop happened
/// without letting a storm through.
const MAX_POLLS_AFTER_DROP: usize = 2;

/// Stub Discourse instance: enough of the REST API for the real adapter to
/// authenticate and settle into its polling loop, plus a counter on the polled
/// endpoint.
async fn spawn_discourse_stub() -> (String, Arc<AtomicUsize>) {
    use axum::routing::get;
    use axum::{Json, Router};

    let polls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&polls);

    let app = Router::new()
        .route(
            "/session/current.json",
            get(|| async {
                Json(serde_json::json!({ "current_user": { "username": "stub-bot" } }))
            }),
        )
        .route(
            "/posts.json",
            get(move || {
                let counter = Arc::clone(&counter);
                async move {
                    counter.fetch_add(1, Ordering::Relaxed);
                    Json(serde_json::json!({ "latest_posts": [] }))
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (format!("http://{addr}"), polls)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_a_discourse_adapter_without_stop_does_not_storm_the_forum() {
    let (base_url, polls) = spawn_discourse_stub().await;

    let adapter = Arc::new(DiscourseAdapter::new(
        base_url,
        "stub-key".to_string(),
        "stub-bot".to_string(),
        Vec::new(),
    ));
    let _stream = adapter.start().await.expect("stub start");

    // `start()` primes `last_post_id` with one poll before spawning the loop.
    // Wait for it: an adapter that never reached its loop would look innocent
    // whether or not the bug is present, which would be a false pass.
    let deadline = Instant::now() + Duration::from_secs(5);
    while polls.load(Ordering::Relaxed) == 0 {
        assert!(
            Instant::now() < deadline,
            "adapter never polled the stub — nothing to measure"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(200)).await;

    assert_eq!(
        Arc::strong_count(&adapter),
        1,
        "the test must hold the only Arc, or dropping it will not drop the Sender"
    );

    let polls_before = polls.load(Ordering::Relaxed);
    let cpu_before = cpu_seconds();
    let t0 = Instant::now();

    drop(adapter); // deliberately NOT stop() — this is the FANG-40 hazard

    tokio::time::sleep(Duration::from_secs(2)).await;

    let after_drop = polls.load(Ordering::Relaxed) - polls_before;
    let cores = (cpu_seconds() - cpu_before) / t0.elapsed().as_secs_f64();
    eprintln!("FANG40_DISCOURSE_POLLS_AFTER_DROP={after_drop} FANG40_DISCOURSE_CORES={cores:.3}");

    assert!(
        after_drop <= MAX_POLLS_AFTER_DROP,
        "Discourse adapter kept polling after being dropped: {after_drop} requests in 2 s"
    );
    assert!(
        cores < SPIN_THRESHOLD,
        "Discourse poll loop spins after the adapter is dropped: {cores:.2} cores"
    );
}
