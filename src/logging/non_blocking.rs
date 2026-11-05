//! Optional non-blocking delivery shared by every log driver.
//!
//! Blocking mode (the default) calls the driver on the stdio thread and does
//! not allocate a queue. Non-blocking mode copies each `write` into one
//! bounded FIFO and a single worker delivers it. The byte limit counts queued
//! payload only; the message the worker is currently handing to a driver is
//! not included, and a slow driver still delays the other drivers that share
//! this queue. Shutdown waits for that in-flight call to return — drivers are
//! not cancelled.

use crate::{
    error::{ConmonError, ConmonResult},
    logging::{plugin::LogPlugin, util::parse_byte_size},
};
use log::{error, warn};
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const DEFAULT_MAX_BUFFER: usize = 1_000_000;

/// Minimum time between runtime warnings about dropped messages.
///
/// The check runs only when another message is dropped, so a quiet container
/// does not emit warnings. Shutdown reports whatever has not been warned yet.
const DROP_WARN_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum DeliveryMode {
    #[default]
    Blocking,
    NonBlocking {
        max_buffer_size: usize,
    },
}

/// Parse one `--log-mode` value and one `--log-max-buffer-size` value.
///
/// `None` means the flag was omitted. Repeats are rejected in
/// [`crate::cli::log_delivery_mode`] before this is called.
/// `--log-max-buffer-size` is valid only with `--log-mode non-blocking`.
pub fn parse_delivery_mode(
    mode: Option<&str>,
    max_buffer: Option<&str>,
) -> ConmonResult<DeliveryMode> {
    match mode.unwrap_or("blocking") {
        "blocking" => {
            if max_buffer.is_some() {
                return Err(ConmonError::new(
                    "--log-max-buffer-size requires --log-mode non-blocking",
                    1,
                ));
            }
            Ok(DeliveryMode::Blocking)
        }
        "non-blocking" => {
            let max_buffer_size = match max_buffer {
                Some(raw) => parse_byte_size(raw).ok_or_else(|| {
                    ConmonError::new(format!("invalid --log-max-buffer-size '{raw}'"), 1)
                })?,
                None => DEFAULT_MAX_BUFFER,
            };
            Ok(DeliveryMode::NonBlocking { max_buffer_size })
        }
        other => Err(ConmonError::new(format!("invalid --log-mode '{other}'"), 1)),
    }
}

enum Item {
    Write {
        is_stdout: bool,
        data: Vec<u8>,
    },
    Reopen {
        reply: mpsc::SyncSender<ConmonResult<()>>,
    },
    Shutdown,
}

struct State {
    items: VecDeque<Item>,
    /// Sum of queued `Write` payloads. The worker's current message is excluded.
    bytes: usize,
    max_bytes: usize,
    drops: DropReporter,
    closed: bool,
}

/// Cumulative drop count plus what a rate-limited warning has already covered.
///
/// `dropped` only grows. Each warning, including the shutdown warning, covers
/// the messages dropped since the previous warning, so adding the warning
/// counts yields `dropped` with nothing counted twice.
struct DropReporter {
    dropped: u64,
    reported: u64,
    last_warn: Option<Instant>,
}

impl DropReporter {
    fn new() -> Self {
        Self {
            dropped: 0,
            reported: 0,
            last_warn: None,
        }
    }

    /// Record one dropped message. The first drop warns immediately. Later
    /// drops warn only once `interval` has passed since that warning, and the
    /// returned count includes the drops that were held back.
    fn record_at(&mut self, now: Instant, interval: Duration) -> Option<u64> {
        self.dropped = self.dropped.saturating_add(1);
        let due = match self.last_warn {
            None => true,
            Some(prev) => now.saturating_duration_since(prev) >= interval,
        };
        if !due {
            return None;
        }
        let since_last = self.dropped.saturating_sub(self.reported);
        self.reported = self.dropped;
        self.last_warn = Some(now);
        Some(since_last)
    }

    fn record(&mut self, now: Instant) -> Option<u64> {
        self.record_at(now, DROP_WARN_INTERVAL)
    }

    #[cfg(test)]
    fn unreported(&self) -> u64 {
        self.dropped.saturating_sub(self.reported)
    }

    /// Drops not yet included in a warning. Marks them reported.
    fn take_unreported(&mut self) -> u64 {
        let n = self.dropped.saturating_sub(self.reported);
        self.reported = self.dropped;
        n
    }
}

fn warn_dropped(count: u64) {
    warn!("non-blocking log: dropped {count} message(s) because the buffer was full");
}

struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

/// Queues container output and delivers it on one background thread.
///
/// The worker is not started here. `RuntimeProcess::spawn` double-forks after
/// log plugins are built, and the parent then `_exit`s. A thread created
/// before that fork is not in the child, and joining it there never returns.
/// The first `write` or `reopen` in the surviving process starts the worker.
pub struct NonBlockingLogPlugin {
    shared: Arc<Shared>,
    inner: Option<Box<dyn LogPlugin + Send>>,
    worker: Option<JoinHandle<()>>,
    shut_down: bool,
}

impl NonBlockingLogPlugin {
    pub fn start(inner: Box<dyn LogPlugin + Send>, max_buffer_size: usize) -> ConmonResult<Self> {
        if max_buffer_size == 0 {
            return Err(ConmonError::new("invalid --log-max-buffer-size '0'", 1));
        }
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                items: VecDeque::new(),
                bytes: 0,
                max_bytes: max_buffer_size,
                drops: DropReporter::new(),
                closed: false,
            }),
            ready: Condvar::new(),
        });
        Ok(Self {
            shared,
            inner: Some(inner),
            worker: None,
            shut_down: false,
        })
    }

    /// Start the worker in the process that will deliver logs.
    fn ensure_worker(&mut self) -> ConmonResult<()> {
        if self.worker.is_some() {
            return Ok(());
        }
        if self.shut_down {
            return Err(ConmonError::new("log delivery is shut down", 1));
        }
        let Some(inner) = self.inner.take() else {
            return Err(ConmonError::new("log worker is not available", 1));
        };
        let worker_shared = Arc::clone(&self.shared);
        match thread::Builder::new()
            .name("conmon-log".into())
            .spawn(move || worker_loop(worker_shared, inner))
        {
            Ok(worker) => {
                self.worker = Some(worker);
                Ok(())
            }
            Err(e) => {
                // `spawn` drops the closure, including the driver, on failure.
                // Later writes hit `shut_down` above and do not log this again.
                self.shut_down = true;
                self.lock().closed = true;
                error!(
                    "non-blocking log: failed to start the delivery thread ({e}); log delivery is unavailable and subsequent messages cannot be delivered"
                );
                Err(ConmonError::new(
                    format!("failed to start log worker: {e}"),
                    1,
                ))
            }
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.shared.state.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl LogPlugin for NonBlockingLogPlugin {
    fn write(&mut self, is_stdout: bool, data: &[u8]) -> ConmonResult<()> {
        self.ensure_worker()?;
        let dropped_now = {
            let mut state = self.lock();
            if state.closed {
                return Err(ConmonError::new("log delivery is shut down", 1));
            }
            // Empty writes flush a partial line. They are ordered control
            // operations, not capacity-limited payloads.
            let len = data.len();
            let fits = data.is_empty()
                || state
                    .bytes
                    .checked_add(len)
                    .is_some_and(|total| total <= state.max_bytes);
            let queue_empty = state.items.is_empty();
            let drop = !data.is_empty()
                && ((!fits && !queue_empty)
                    || (!fits && queue_empty && state.bytes.checked_add(len).is_none()));
            if drop {
                state.drops.record(Instant::now())
            } else {
                state.bytes = state.bytes.saturating_add(len);
                state.items.push_back(Item::Write {
                    is_stdout,
                    data: data.to_vec(),
                });
                self.shared.ready.notify_one();
                None
            }
        };
        // The queue lock is released before logging. `None` is also returned
        // when the drop is inside the warning interval.
        if let Some(count) = dropped_now {
            warn_dropped(count);
        }
        Ok(())
    }

    fn reopen(&mut self) -> ConmonResult<()> {
        self.ensure_worker()?;
        let (reply_tx, reply_rx) = mpsc::sync_channel(1);
        {
            let mut state = self.lock();
            if state.closed {
                return Err(ConmonError::new("log delivery is shut down", 1));
            }
            state.items.push_back(Item::Reopen { reply: reply_tx });
            self.shared.ready.notify_one();
        }
        reply_rx
            .recv()
            .map_err(|_| ConmonError::new("log worker stopped during reopen", 1))?
    }

    fn shutdown(&mut self) -> ConmonResult<()> {
        if self.shut_down {
            return Ok(());
        }
        self.shut_down = true;
        if self.worker.is_none() {
            // This process never delivered a message (the post-fork parent, or
            // a caller that only built the plugin). Drop the driver here.
            self.inner.take();
            self.lock().closed = true;
            return Ok(());
        }
        {
            let mut state = self.lock();
            state.closed = true;
            state.items.push_back(Item::Shutdown);
            self.shared.ready.notify_one();
        }
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| ConmonError::new("log worker panicked", 1))?;
        }
        Ok(())
    }
}

impl Drop for NonBlockingLogPlugin {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

fn worker_loop(shared: Arc<Shared>, mut plugin: Box<dyn LogPlugin + Send>) {
    loop {
        let item = {
            let mut state = shared.state.lock().unwrap_or_else(|p| p.into_inner());
            while state.items.is_empty() {
                state = shared.ready.wait(state).unwrap_or_else(|p| p.into_inner());
            }
            let item = state.items.pop_front().expect("item just observed");
            if let Item::Write { data, .. } = &item {
                state.bytes = state.bytes.saturating_sub(data.len());
            }
            item
        };
        match item {
            Item::Write { is_stdout, data } => {
                if let Err(err) = plugin.write(is_stdout, &data) {
                    error!("non-blocking log: {err}");
                }
            }
            Item::Reopen { reply } => {
                let result = plugin.reopen();
                let _ = reply.send(result);
            }
            Item::Shutdown => {
                let unreported = shared
                    .state
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .drops
                    .take_unreported();
                if unreported > 0 {
                    warn_dropped(unreported);
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logging::plugin::{LogPluginCfg, initialize_log_plugins};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn delivery_option_parsing() {
        assert_eq!(
            parse_delivery_mode(None, None).unwrap(),
            DeliveryMode::Blocking
        );
        assert_eq!(
            parse_delivery_mode(Some("blocking"), None).unwrap(),
            DeliveryMode::Blocking
        );
        assert_eq!(
            parse_delivery_mode(Some("non-blocking"), None).unwrap(),
            DeliveryMode::NonBlocking {
                max_buffer_size: DEFAULT_MAX_BUFFER
            }
        );
        assert_eq!(
            parse_delivery_mode(Some("non-blocking"), Some("4m")).unwrap(),
            DeliveryMode::NonBlocking {
                max_buffer_size: 4_000_000
            }
        );

        assert!(
            parse_delivery_mode(Some("async"), None)
                .unwrap_err()
                .msg
                .contains("--log-mode")
        );
        assert!(
            parse_delivery_mode(None, Some("1m"))
                .unwrap_err()
                .msg
                .contains("--log-mode non-blocking")
        );
        assert!(
            parse_delivery_mode(Some("non-blocking"), Some("nope"))
                .unwrap_err()
                .msg
                .contains("invalid --log-max-buffer-size")
        );
    }

    #[test]
    fn initialize_non_blocking_wraps_every_driver() {
        let cfg = LogPluginCfg::default();
        let mut plugin = initialize_log_plugins(
            &[("none".into(), cfg.clone()), ("none".into(), cfg)],
            DeliveryMode::NonBlocking {
                max_buffer_size: 1_000,
            },
        )
        .unwrap();
        plugin.write(true, b"hi").unwrap();
        plugin.shutdown().unwrap();
        plugin.shutdown().unwrap();
    }

    struct Block {
        entered: Condvar,
        entered_mu: Mutex<bool>,
        release: Mutex<bool>,
        release_cv: Condvar,
    }

    impl Block {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                entered: Condvar::new(),
                entered_mu: Mutex::new(false),
                release: Mutex::new(false),
                release_cv: Condvar::new(),
            })
        }

        fn wait_entered(&self) {
            let mut entered = self.entered_mu.lock().unwrap();
            while !*entered {
                entered = self.entered.wait(entered).unwrap();
            }
        }

        fn unblock(&self) {
            *self.release.lock().unwrap() = true;
            self.release_cv.notify_all();
        }
    }

    impl Drop for Block {
        fn drop(&mut self) {
            self.unblock();
        }
    }

    type Events = Arc<Mutex<Vec<(bool, Vec<u8>)>>>;

    struct Rec {
        events: Events,
        fail_writes: Arc<AtomicUsize>,
        /// When set, the first `write` blocks until [`Block::unblock`].
        block: Option<Arc<Block>>,
        fail_reopen: bool,
    }

    impl LogPlugin for Rec {
        fn write(&mut self, is_stdout: bool, data: &[u8]) -> ConmonResult<()> {
            let first = self.events.lock().unwrap().is_empty();
            if first {
                if let Some(block) = &self.block {
                    {
                        let mut entered = block.entered_mu.lock().unwrap();
                        *entered = true;
                        block.entered.notify_all();
                    }
                    let mut release = block.release.lock().unwrap();
                    while !*release {
                        release = block.release_cv.wait(release).unwrap();
                    }
                }
            }
            self.events.lock().unwrap().push((is_stdout, data.to_vec()));
            if self.fail_writes.load(Ordering::SeqCst) > 0 {
                self.fail_writes.fetch_sub(1, Ordering::SeqCst);
                return Err(ConmonError::new("mock failed", 1));
            }
            Ok(())
        }

        fn reopen(&mut self) -> ConmonResult<()> {
            self.events.lock().unwrap().push((true, b"reopen".to_vec()));
            if self.fail_reopen {
                Err(ConmonError::new("reopen failed", 1))
            } else {
                Ok(())
            }
        }
    }

    #[test]
    fn write_returns_while_driver_is_blocked() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let block = Block::new();
        let mut plugin = NonBlockingLogPlugin::start(
            Box::new(Rec {
                events: Arc::clone(&events),
                fail_writes: Arc::new(AtomicUsize::new(0)),
                block: Some(Arc::clone(&block)),
                fail_reopen: false,
            }),
            100,
        )
        .unwrap();

        plugin.write(true, &[0xff, 0x00, b'x']).unwrap();
        block.wait_entered();
        assert!(!*block.release.lock().unwrap());
        block.unblock();
        plugin.shutdown().unwrap();

        let got = events.lock().unwrap().clone();
        assert_eq!(got, vec![(true, vec![0xff, 0x00, b'x'])]);
        plugin.shutdown().unwrap();
    }

    #[test]
    fn overflow_keeps_queued_data_and_one_oversized_message() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let block = Block::new();
        let mut plugin = NonBlockingLogPlugin::start(
            Box::new(Rec {
                events: Arc::clone(&events),
                fail_writes: Arc::new(AtomicUsize::new(0)),
                block: Some(Arc::clone(&block)),
                fail_reopen: false,
            }),
            4,
        )
        .unwrap();

        plugin.write(true, b"ab").unwrap();
        block.wait_entered();
        // "ab" is in flight and does not consume queue capacity.
        plugin.write(false, b"cd").unwrap();
        plugin.write(true, b"zzzz").unwrap();
        plugin.write(true, b"").unwrap();
        // Queue holds "cd" plus a flush, so this oversized write is dropped.
        plugin.write(true, b"0123456789").unwrap();
        // Fresh empty queue accepts one oversized message. Drain first.
        block.unblock();
        plugin.shutdown().unwrap();

        let got = events.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                (true, b"ab".to_vec()),
                (false, b"cd".to_vec()),
                (true, b"".to_vec()),
            ]
        );

        let events = Arc::new(Mutex::new(Vec::new()));
        let block = Block::new();
        let mut plugin = NonBlockingLogPlugin::start(
            Box::new(Rec {
                events: Arc::clone(&events),
                fail_writes: Arc::new(AtomicUsize::new(0)),
                block: Some(Arc::clone(&block)),
                fail_reopen: false,
            }),
            4,
        )
        .unwrap();
        plugin.write(true, b"ab").unwrap();
        block.wait_entered();
        plugin.write(true, b"0123456789").unwrap();
        block.unblock();
        plugin.shutdown().unwrap();
        let got = events.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![(true, b"ab".to_vec()), (true, b"0123456789".to_vec())]
        );
    }

    #[test]
    fn driver_errors_do_not_stop_later_messages_or_retry() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut plugin = NonBlockingLogPlugin::start(
            Box::new(Rec {
                events: Arc::clone(&events),
                fail_writes: Arc::new(AtomicUsize::new(1)),
                block: None,
                fail_reopen: false,
            }),
            1000,
        )
        .unwrap();
        plugin.write(true, b"one").unwrap();
        plugin.write(false, b"two").unwrap();
        plugin.shutdown().unwrap();
        let got = events.lock().unwrap().clone();
        assert_eq!(got, vec![(true, b"one".to_vec()), (false, b"two".to_vec())]);
    }

    #[test]
    fn reopen_preserves_order_and_returns_the_driver_error() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut plugin = NonBlockingLogPlugin::start(
            Box::new(Rec {
                events: Arc::clone(&events),
                fail_writes: Arc::new(AtomicUsize::new(0)),
                block: None,
                fail_reopen: true,
            }),
            1000,
        )
        .unwrap();
        plugin.write(true, b"before").unwrap();
        let err = plugin.reopen().unwrap_err();
        assert!(err.msg.contains("reopen failed"), "{err}");
        plugin.write(false, b"after").unwrap();
        plugin.shutdown().unwrap();
        let got = events.lock().unwrap().clone();
        assert_eq!(
            got,
            vec![
                (true, b"before".to_vec()),
                (true, b"reopen".to_vec()),
                (false, b"after".to_vec()),
            ]
        );
    }

    #[test]
    fn shutdown_drains_then_rejects_new_writes() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut plugin = NonBlockingLogPlugin::start(
            Box::new(Rec {
                events: Arc::clone(&events),
                fail_writes: Arc::new(AtomicUsize::new(0)),
                block: None,
                fail_reopen: false,
            }),
            1000,
        )
        .unwrap();
        plugin.write(true, b"queued").unwrap();
        plugin.write(true, b"").unwrap();
        plugin.shutdown().unwrap();
        assert!(plugin.write(true, b"late").is_err());
        plugin.shutdown().unwrap();
        let got = events.lock().unwrap().clone();
        assert_eq!(got, vec![(true, b"queued".to_vec()), (true, b"".to_vec())]);
    }

    #[test]
    fn drop_warnings_are_rate_limited_and_shutdown_reports_the_rest() {
        let interval = DROP_WARN_INTERVAL;
        let start = Instant::now();
        let mut drops = DropReporter::new();

        // The first drop is reported immediately.
        assert_eq!(drops.record_at(start, interval), Some(1));
        assert_eq!(drops.dropped, 1);
        assert_eq!(drops.take_unreported(), 0);

        // More drops inside the interval stay quiet and stay unreported.
        assert_eq!(
            drops.record_at(start + interval - Duration::from_secs(1), interval),
            None
        );
        assert_eq!(
            drops.record_at(start + interval - Duration::from_millis(1), interval),
            None
        );
        assert_eq!(drops.dropped, 3);
        assert_eq!(drops.unreported(), 2);

        // The next drop after the interval covers everything held back.
        let later = start + interval;
        assert_eq!(drops.record_at(later, interval), Some(3));
        assert_eq!(drops.unreported(), 0);

        assert_eq!(
            drops.record_at(later + Duration::from_secs(1), interval),
            None
        );
        assert_eq!(drops.take_unreported(), 1);
        assert_eq!(drops.take_unreported(), 0);
        assert_eq!(drops.dropped, 5);
    }

    #[test]
    fn repeated_overflow_accumulates_the_drop_counter() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let block = Block::new();
        let mut plugin = NonBlockingLogPlugin::start(
            Box::new(Rec {
                events: Arc::clone(&events),
                fail_writes: Arc::new(AtomicUsize::new(0)),
                block: Some(Arc::clone(&block)),
                fail_reopen: false,
            }),
            4,
        )
        .unwrap();

        plugin.write(true, b"ab").unwrap();
        block.wait_entered();
        plugin.write(false, b"cd").unwrap();
        plugin.write(true, b"zzzz").unwrap();
        plugin.write(true, b"yyyy").unwrap();
        assert_eq!(plugin.lock().drops.dropped, 2);

        block.unblock();
        plugin.shutdown().unwrap();
        let got = events.lock().unwrap().clone();
        assert_eq!(got, vec![(true, b"ab".to_vec()), (false, b"cd".to_vec())]);
        // Both drops were inside one warning interval, so shutdown still
        // accounts for the one that the first warning did not include.
        assert_eq!(plugin.lock().drops.dropped, 2);
        assert_eq!(plugin.lock().drops.unreported(), 0);
    }
}
