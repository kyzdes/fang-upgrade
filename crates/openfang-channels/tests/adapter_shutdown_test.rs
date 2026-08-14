//! FANG-40 — `ChannelAdapter::stop()` must actually be called, and dropping an
//! adapter must never leave a task spinning.
//!
//! Two separate claims are checked here, because they fail for different
//! reasons and one of them used to *cause* the other:
//!
//! 1. `BridgeManager::stop()` calls `stop()` on every adapter it started.
//!    Before this change it only flipped its own `watch` flag, so an adapter's
//!    own I/O task (Telegram's poller, a WebSocket read loop) survived until
//!    the last `Arc` happened to be dropped.
//!
//! 2. An adapter that is dropped *without* `stop()` must not spin. Every
//!    adapter clones a `watch::Receiver` into its task; when the last `Sender`
//!    goes, `changed()` starts returning `Err` immediately and forever. A
//!    `select!` arm that reads that as "spurious wakeup, keep looping" turns
//!    into a 100% CPU loop. This is not hypothetical: `DashMap::insert`
//!    replaces the value, so a channel hot-reload already drops the previous
//!    adapter out of `kernel.channel_adapters`.
//!
//! Only one CPU measurement lives in this binary. `/proc/self/stat` is
//! process-wide, so a second spinning task in the same process would show up in
//! this one's sample and make the number unattributable. The Mattermost
//! measurement therefore has its own binary,
//! `mattermost_shutdown_spin_test.rs`.

use async_trait::async_trait;
use futures::Stream;
use openfang_channels::bridge::{BridgeManager, ChannelBridgeHandle};
use openfang_channels::router::AgentRouter;
use openfang_channels::types::{
    ChannelAdapter, ChannelContent, ChannelMessage, ChannelType, ChannelUser,
};
use openfang_types::agent::AgentId;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

// ---------------------------------------------------------------------------
// CPU sampling
// ---------------------------------------------------------------------------

/// Process CPU time (user + system) in seconds, from `/proc/self/stat`.
///
/// Fields 14 and 15 are `utime`/`stime` in clock ticks. `USER_HZ` is fixed at
/// 100 on Linux by ABI, so no `sysconf` call is needed.
fn cpu_seconds() -> f64 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("read /proc/self/stat");
    // `comm` (field 2) may contain spaces and parentheses; everything we want
    // is after the last ')'.
    let rest = &stat[stat.rfind(')').expect("malformed stat") + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the ')' the first field is `state` (field 3), so utime (14) is at
    // index 11 and stime (15) at index 12.
    let utime: u64 = fields[11].parse().expect("utime");
    let stime: u64 = fields[12].parse().expect("stime");
    (utime + stime) as f64 / 100.0
}

/// Burn CPU for `window` and report the fraction of a core used.
async fn cpu_fraction_over(window: Duration) -> f64 {
    let cpu0 = cpu_seconds();
    let t0 = Instant::now();
    tokio::time::sleep(window).await;
    let wall = t0.elapsed().as_secs_f64();
    (cpu_seconds() - cpu0) / wall
}

/// A spinning `select!` pins a core; anything idle stays far below this.
const SPIN_THRESHOLD: f64 = 0.25;

// ---------------------------------------------------------------------------
// Minimal bridge handle — only the methods without a default body
// ---------------------------------------------------------------------------

struct NoopHandle;

#[async_trait]
impl ChannelBridgeHandle for NoopHandle {
    async fn send_message(&self, _agent_id: AgentId, _message: &str) -> Result<String, String> {
        Ok("ok".to_string())
    }
    async fn find_agent_by_name(&self, _name: &str) -> Result<Option<AgentId>, String> {
        Ok(None)
    }
    async fn list_agents(&self) -> Result<Vec<(AgentId, String)>, String> {
        Ok(Vec::new())
    }
    async fn spawn_agent_by_name(&self, _manifest_name: &str) -> Result<AgentId, String> {
        Err("no agents in this test".to_string())
    }
}

// ---------------------------------------------------------------------------
// Counting adapter — records how often stop() was called
// ---------------------------------------------------------------------------

struct CountingAdapter {
    stop_calls: AtomicUsize,
    rx: std::sync::Mutex<Option<mpsc::Receiver<ChannelMessage>>>,
}

impl CountingAdapter {
    fn new() -> (Arc<Self>, mpsc::Sender<ChannelMessage>) {
        let (tx, rx) = mpsc::channel(8);
        (
            Arc::new(Self {
                stop_calls: AtomicUsize::new(0),
                rx: std::sync::Mutex::new(Some(rx)),
            }),
            tx,
        )
    }
}

#[async_trait]
impl ChannelAdapter for CountingAdapter {
    fn name(&self) -> &str {
        "counting"
    }
    fn channel_type(&self) -> ChannelType {
        ChannelType::WebChat
    }
    async fn start(
        &self,
    ) -> Result<Pin<Box<dyn Stream<Item = ChannelMessage> + Send>>, Box<dyn std::error::Error>>
    {
        let rx = self.rx.lock().unwrap().take().expect("start() called twice");
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
    async fn send(
        &self,
        _user: &ChannelUser,
        _content: ChannelContent,
    ) -> Result<(), Box<dyn std::error::Error>> {
        Ok(())
    }
    async fn stop(&self) -> Result<(), Box<dyn std::error::Error>> {
        self.stop_calls.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

/// FANG-40 core claim: the manager must call `stop()` on what it started.
#[tokio::test]
async fn bridge_manager_stop_calls_adapter_stop() {
    let (adapter, _tx) = CountingAdapter::new();
    let mut manager = BridgeManager::new(Arc::new(NoopHandle), Arc::new(AgentRouter::new()));
    manager.start_adapter(adapter.clone()).await.unwrap();

    assert_eq!(
        adapter.stop_calls.load(Ordering::SeqCst),
        0,
        "stop() must not be called before the manager is stopped"
    );

    manager.stop().await;

    assert_eq!(
        adapter.stop_calls.load(Ordering::SeqCst),
        1,
        "BridgeManager::stop() must call ChannelAdapter::stop() exactly once"
    );
}

// ---------------------------------------------------------------------------
// Spin measurements
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dropping_the_manager_without_stop_does_not_spin() {
    // Dropping the manager drops its `watch::Sender`. The dispatch task is
    // detached and keeps running, so its `shutdown.changed()` starts failing
    // immediately and forever.
    let idle = cpu_fraction_over(Duration::from_millis(500)).await;
    assert!(
        idle < SPIN_THRESHOLD,
        "test harness is already busy ({idle:.2} cores) — measurement would be meaningless"
    );

    let (adapter, _tx) = CountingAdapter::new();
    let mut manager = BridgeManager::new(Arc::new(NoopHandle), Arc::new(AgentRouter::new()));
    manager.start_adapter(adapter).await.unwrap();
    drop(manager); // deliberately NOT stop()

    let dispatch_cpu = cpu_fraction_over(Duration::from_secs(2)).await;
    eprintln!("FANG40_IDLE_CORES={idle:.3} FANG40_DISPATCH_CORES={dispatch_cpu:.3}");
    assert!(
        dispatch_cpu < SPIN_THRESHOLD,
        "BridgeManager dispatch loop spins after the manager is dropped: \
         {dispatch_cpu:.2} cores"
    );
}
