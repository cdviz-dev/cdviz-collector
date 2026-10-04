//! Double-buffer batching shared by the database sinks (`db`, `clickhouse`).
//!
//! `push()` never waits on the target: items go into the active slot, a background flusher
//! drains the other (sealed) slot every `batch_max_wait`, when the active slot reaches
//! `batch_max_size`, and on `flush()`. A sealed slot is only cleared (memory + optional JSONL
//! spool) once stored, so items survive an outage (and a crash, if spooled).
//!
//! `batch_max_size` is the flush trigger and the number of items per `store` call, NOT a cap on
//! a slot: while the sealed slot is still being written, the active slot keeps growing past it,
//! and is later stored in several chunks.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use crate::errors::{IntoDiagnostic, Result};
use serde::{Serialize, de::DeserializeOwned};
use tokio::sync::{Notify, mpsc, oneshot};

pub(crate) fn default_batch_max_size() -> usize {
    50
}

pub(crate) fn default_batch_max_wait() -> Duration {
    Duration::from_secs(1)
}

/// Where the flusher writes a chunk of (at most `batch_max_size`) items.
pub(crate) trait BatchStore: Send + Sync + 'static {
    type Item: Serialize + DeserializeOwned + Send + Sync + 'static;

    /// Returns `false` only when the target is unreachable (transient error): the chunk is kept
    /// for the next drain. `true` means stored, or a non-transient failure already handled.
    fn store(&self, chunk: &[Self::Item]) -> impl Future<Output = bool> + Send;
}

pub(crate) struct Batcher<T> {
    tx: mpsc::Sender<BatchCmd<T>>,
}

impl<T> Clone for Batcher<T> {
    fn clone(&self) -> Self {
        Self { tx: self.tx.clone() }
    }
}

impl<T> std::fmt::Debug for Batcher<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Batcher").finish_non_exhaustive()
    }
}

impl<T: Serialize + DeserializeOwned + Send + Sync + 'static> Batcher<T> {
    /// Spawn the writer and flusher tasks (must be called inside a tokio runtime).
    /// Spooled items left by a previous run are stored right away.
    pub(crate) fn start<S: BatchStore<Item = T>>(
        store: S,
        batch_max_size: usize,
        batch_max_wait: Duration,
        spool_dir: Option<&Path>,
    ) -> Result<Self> {
        let batch_max_size = batch_max_size.max(1);
        let buffers = Arc::new(Mutex::new(Buffers::open(spool_dir)?));
        let notify = Arc::new(Notify::new());
        // Bounded, but the writer never waits on the target (the flusher does), so it drains fast.
        let (tx, rx) = mpsc::channel(batch_max_size);
        let (flush_tx, flush_rx) = mpsc::channel(1);
        tokio::spawn(run_writer(
            Arc::clone(&buffers),
            rx,
            batch_max_size,
            Arc::clone(&notify),
            flush_tx,
        ));
        tokio::spawn(run_flusher(store, buffers, batch_max_size, batch_max_wait, notify, flush_rx));
        Ok(Self { tx })
    }

    pub(crate) async fn push(&self, item: T) -> Result<()> {
        self.tx
            .send(BatchCmd::Push(item))
            .await
            .map_err(|_| crate::errors::miette!("batcher stopped"))
    }

    /// Store everything buffered so far (best effort: leftovers stay buffered if unreachable).
    pub(crate) async fn flush(&self) -> Result<()> {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(BatchCmd::Flush(ack_tx)).await.is_err() {
            return Ok(()); // batcher already gone, nothing pending
        }
        ack_rx.await.into_diagnostic()
    }
}

enum BatchCmd<T> {
    Push(T),
    Flush(oneshot::Sender<()>),
}

/// One of the two buffers: items in memory, mirrored to an append-only JSONL file if spooling.
struct Slot<T> {
    items: Vec<T>,
    file: Option<File>,
}

impl<T: Serialize + DeserializeOwned> Slot<T> {
    fn new() -> Self {
        Self { items: Vec::new(), file: None }
    }

    /// Open (or create) the spool file, loading items left by a previous run.
    fn open(path: &Path) -> Result<Self> {
        let mut items = Vec::new();
        if path.exists() {
            for line in BufReader::new(File::open(path).into_diagnostic()?).lines() {
                let line = line.into_diagnostic()?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str(&line) {
                    Ok(item) => items.push(item),
                    Err(err) => tracing::warn!(?err, ?path, "skip unreadable spooled event"),
                }
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path).into_diagnostic()?;
        Ok(Self { items, file: Some(file) })
    }

    // ponytail: fsync per item under a std Mutex, fine at CI rates; batch fsyncs /
    // spawn_blocking if throughput matters.
    fn push(&mut self, item: T) {
        if let Some(file) = &mut self.file {
            // Leading newline: a record torn by a crash / failed write is terminated by the next
            // one, so it only loses itself (skipped at replay) instead of corrupting the next.
            let mut record = b"\n".to_vec();
            let written = serde_json::to_writer(&mut record, &item)
                .map_err(std::io::Error::from)
                .and_then(|()| file.write_all(&record))
                .and_then(|()| file.sync_data());
            if let Err(err) = written {
                tracing::warn!(?err, "fail to spool event, kept in memory only");
            }
        }
        self.items.push(item);
    }

    /// Forget the spooled copy once its items are stored (or definitively rejected).
    fn truncate(&mut self) {
        if let Some(file) = &mut self.file
            && let Err(err) = file.set_len(0)
        {
            tracing::warn!(?err, "fail to truncate spool, events will be replayed at restart");
        }
    }
}

/// Double buffer: the writer appends to `slots[active]` while the flusher drains the other
/// (sealed) one, then they swap.
// ponytail: memory and spool are unbounded while the target is down; cap + drop-oldest if that
// becomes real. Items still in the mpsc channel (≤ batch_max_size) aren't spooled yet.
struct Buffers<T> {
    slots: [Slot<T>; 2],
    active: usize,
}

impl<T: Serialize + DeserializeOwned> Buffers<T> {
    fn open(spool_dir: Option<&Path>) -> Result<Self> {
        let slots = match spool_dir {
            None => [Slot::new(), Slot::new()],
            Some(dir) => {
                std::fs::create_dir_all(dir).into_diagnostic()?;
                [Slot::open(&dir.join("spool-0.jsonl"))?, Slot::open(&dir.join("spool-1.jsonl"))?]
            }
        };
        Ok(Self { slots, active: 0 })
    }
}

fn lock<T>(buffers: &Mutex<Buffers<T>>) -> MutexGuard<'_, Buffers<T>> {
    buffers.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Appends pushed items to the active slot, waking the flusher once `batch_max_size` is
/// reached. Exits once `tx` is dropped; dropping `flush_tx` then makes the flusher drain all.
async fn run_writer<T: Serialize + DeserializeOwned>(
    buffers: Arc<Mutex<Buffers<T>>>,
    mut rx: mpsc::Receiver<BatchCmd<T>>,
    batch_max_size: usize,
    notify: Arc<Notify>,
    flush_tx: mpsc::Sender<oneshot::Sender<()>>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            BatchCmd::Push(item) => {
                let len = {
                    let mut buffers = lock(&buffers);
                    let active = buffers.active;
                    buffers.slots[active].push(item);
                    buffers.slots[active].items.len()
                };
                if len >= batch_max_size {
                    notify.notify_one();
                }
            }
            BatchCmd::Flush(ack) => {
                if flush_tx.send(ack).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Drains buffered items every `batch_max_wait`, when the writer reports a full batch,
/// on an explicit `Flush` (graceful shutdown) and at startup (spool replay).
async fn run_flusher<S: BatchStore>(
    store: S,
    buffers: Arc<Mutex<Buffers<S::Item>>>,
    batch_max_size: usize,
    batch_max_wait: Duration,
    notify: Arc<Notify>,
    mut flush_rx: mpsc::Receiver<oneshot::Sender<()>>,
) {
    // Keep swapping + draining until both slots are empty (items pushed meanwhile included).
    // Stops early if the target is unreachable: leftovers stay in memory + spool, never a hang.
    let drain_all = async || {
        while drain_sealed(&store, &buffers, batch_max_size).await {
            if lock(&buffers).slots.iter().all(|slot| slot.items.is_empty()) {
                break;
            }
        }
    };
    drain_all().await;
    loop {
        tokio::select! {
            () = tokio::time::sleep(batch_max_wait) => {
                drain_sealed(&store, &buffers, batch_max_size).await;
            }
            () = notify.notified() => {
                drain_sealed(&store, &buffers, batch_max_size).await;
            }
            received = flush_rx.recv() => {
                drain_all().await;
                match received {
                    Some(ack) => {
                        let _ = ack.send(());
                    }
                    None => break,
                }
            }
        }
    }
}

/// Swap slots (unless the sealed one still holds items from a failed drain), then store
/// the sealed slot in chunks of `batch_max_size`. Returns `false` when the target is
/// unreachable: unstored items stay in the sealed slot (memory + spool) for the next drain.
async fn drain_sealed<S: BatchStore>(
    store: &S,
    buffers: &Mutex<Buffers<S::Item>>,
    batch_max_size: usize,
) -> bool {
    let (sealed, items) = {
        let mut buffers = lock(buffers);
        if buffers.slots[1 - buffers.active].items.is_empty() {
            buffers.active = 1 - buffers.active;
        }
        let sealed = 1 - buffers.active;
        (sealed, std::mem::take(&mut buffers.slots[sealed].items))
    };
    if items.is_empty() {
        return true;
    }
    let mut stored = 0;
    for chunk in items.chunks(batch_max_size) {
        if !store.store(chunk).await {
            // Put back what's left. The spool keeps the whole slot until it's fully stored, so a
            // crash now replays already stored items (duplicates, see each sink's docs).
            let mut items = items;
            items.drain(..stored);
            lock(buffers).slots[sealed].items = items;
            return false;
        }
        stored += chunk.len();
    }
    lock(buffers).slots[sealed].truncate();
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Clone, Default)]
    struct FakeStore {
        chunks: Arc<Mutex<Vec<Vec<u32>>>>,
        fail: Arc<AtomicBool>,
    }

    impl BatchStore for FakeStore {
        type Item = u32;
        fn store(&self, chunk: &[u32]) -> impl Future<Output = bool> + Send {
            let ok = !self.fail.load(Ordering::SeqCst);
            if ok {
                self.chunks.lock().unwrap().push(chunk.to_vec());
            }
            std::future::ready(ok)
        }
    }

    fn spooled_lines(spool_dir: &Path) -> usize {
        ["spool-0.jsonl", "spool-1.jsonl"]
            .iter()
            .map(|name| {
                std::fs::read_to_string(spool_dir.join(name))
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| !l.is_empty())
                    .count()
            })
            .sum()
    }

    #[tokio::test]
    async fn unreachable_target_keeps_items_then_stores_them_in_chunks() {
        let spool_dir = tempfile::tempdir().unwrap();
        let store = FakeStore::default();
        store.fail.store(true, Ordering::SeqCst);
        let batcher =
            Batcher::start(store.clone(), 2, Duration::from_secs(3600), Some(spool_dir.path()))
                .unwrap();
        for i in 0..5 {
            batcher.push(i).await.unwrap();
        }
        batcher.flush().await.unwrap(); // fails to store, must keep everything
        assert_eq!(spooled_lines(spool_dir.path()), 5);
        assert!(store.chunks.lock().unwrap().is_empty());

        // The slot grew past batch_max_size: it's stored in several chunks, nothing lost.
        store.fail.store(false, Ordering::SeqCst);
        batcher.flush().await.unwrap();
        let chunks = store.chunks.lock().unwrap().clone();
        assert!(chunks.iter().all(|c| c.len() <= 2), "{chunks:?}");
        let mut stored: Vec<u32> = chunks.into_iter().flatten().collect();
        stored.sort_unstable();
        assert_eq!(stored, vec![0, 1, 2, 3, 4]);
        assert_eq!(spooled_lines(spool_dir.path()), 0);
    }

    #[tokio::test]
    async fn empty_batch_never_reaches_the_store() {
        // A store call may open a connection (e.g. db sink with a lazy pool): skip it when idle.
        let store = FakeStore::default();
        let batcher = Batcher::start(store.clone(), 2, Duration::from_millis(5), None).unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await; // several timer ticks
        batcher.flush().await.unwrap();
        assert!(store.chunks.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn spooled_items_are_replayed_at_startup() {
        let spool_dir = tempfile::tempdir().unwrap();
        // Leftovers of a crashed run (incl. a torn record): 2 items in a slot, 1 in the other.
        std::fs::write(spool_dir.path().join("spool-0.jsonl"), "\n1\n2\n{torn").unwrap();
        std::fs::write(spool_dir.path().join("spool-1.jsonl"), "3").unwrap();
        let store = FakeStore::default();
        let batcher =
            Batcher::start(store.clone(), 50, Duration::from_secs(3600), Some(spool_dir.path()))
                .unwrap();
        batcher.flush().await.unwrap();
        let mut stored: Vec<u32> = store.chunks.lock().unwrap().iter().flatten().copied().collect();
        stored.sort_unstable();
        assert_eq!(stored, vec![1, 2, 3]);
        assert_eq!(spooled_lines(spool_dir.path()), 0);
    }
}
