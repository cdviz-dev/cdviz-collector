use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use super::{Sink, retry};
use crate::{
    Message,
    errors::{IntoDiagnostic, Result},
};
use retry_policies::policies::ExponentialBackoff;
use secrecy::{ExposeSecret, SecretString, zeroize::Zeroize};
use serde::Deserialize;
use serde_json::Value;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::{Notify, mpsc, oneshot};
use tracing::Instrument;

fn default_pool_connections_max() -> u32 {
    10
}
fn default_pool_connections_min() -> u32 {
    0
}
fn default_pool_acquire_timeout() -> Duration {
    Duration::from_secs(30)
}
fn default_pool_idle_timeout() -> Duration {
    Duration::from_mins(10)
}
fn default_pool_max_lifetime() -> Duration {
    Duration::from_mins(30)
}
fn default_pool_test_before_acquire() -> bool {
    true
}
fn default_lazy_connection() -> bool {
    false
}
fn default_batch_max_size() -> usize {
    50
}
fn default_batch_max_wait() -> Duration {
    Duration::from_secs(1)
}
use retry::default_total_duration_of_retries;

/// The database client config
#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// Is the sink is enabled?
    pub(crate) enabled: bool,

    /// The database url (with username, password and the database)
    #[serde(skip_serializing)]
    url: SecretString,

    /// The minimum number of connections to the database to maintain at all times.
    /// minimum > 0, requires access to the database at startup time,
    /// consumes a little more resource on idle
    /// and could increase performance on low load (keep prepared statements, etc.)
    // https://docs.rs/sqlx/latest/sqlx/pool/struct.PoolOptions.html#method.min_connections
    #[serde(default = "default_pool_connections_min")]
    pool_connections_min: u32,

    /// The maximum number of connections to the database to open / to maintain.
    // https://docs.rs/sqlx/latest/sqlx/pool/struct.PoolOptions.html#method.max_connections
    #[serde(default = "default_pool_connections_max")]
    pool_connections_max: u32,

    /// The maximum time to wait when acquiring a connection from the pool.
    // https://docs.rs/sqlx/latest/sqlx/pool/struct.PoolOptions.html#method.acquire_timeout
    #[serde(default = "default_pool_acquire_timeout", with = "humantime_serde")]
    pool_acquire_timeout: Duration,

    /// The maximum idle duration for individual connections.
    /// Connections idle for longer than this are closed and removed from the pool.
    /// `None` disables idle timeout.
    // https://docs.rs/sqlx/latest/sqlx/pool/struct.PoolOptions.html#method.idle_timeout
    #[serde(default = "default_pool_idle_timeout", with = "humantime_serde")]
    pool_idle_timeout: Duration,

    /// The maximum lifetime of individual connections.
    /// Connections older than this are closed and replaced.
    /// `None` disables max lifetime.
    // https://docs.rs/sqlx/latest/sqlx/pool/struct.PoolOptions.html#method.max_lifetime
    #[serde(default = "default_pool_max_lifetime", with = "humantime_serde")]
    pool_max_lifetime: Duration,

    /// If `true`, a health check query is run before returning a connection from the pool.
    // https://docs.rs/sqlx/latest/sqlx/pool/struct.PoolOptions.html#method.test_before_acquire
    #[serde(default = "default_pool_test_before_acquire")]
    pool_test_before_acquire: bool,

    /// Total duration across all retry attempts for transient connection errors.
    /// Set to `0s` to disable retries.
    #[serde(default = "default_total_duration_of_retries", with = "humantime_serde")]
    total_duration_of_retries: Duration,

    /// If `false` (default), establish a connection to the database eagerly at sink creation time.
    /// An error is raised immediately if the database is unreachable, preventing the service
    /// from starting. If `true`, connections are established lazily on first use.
    #[serde(default = "default_lazy_connection")]
    lazy_connection: bool,

    /// Insert events in batches of up to this many, instead of one `CALL` per event.
    /// Reduces round trips when a source produces events faster than one-at-a-time
    /// inserts can keep up (polling, backfill). `1` disables batching in practice
    /// (every push flushes immediately).
    #[serde(default = "default_batch_max_size")]
    batch_max_size: usize,

    /// Flush buffered events at least this often, even if `batch_max_size` wasn't reached,
    /// so low-traffic events aren't held indefinitely. A long value (e.g. `30m`) with a short
    /// `pool_idle_timeout` lets a scale-to-zero database (e.g. Neon) suspend between flushes.
    #[serde(default = "default_batch_max_wait", with = "humantime_serde")]
    batch_max_wait: Duration,

    /// Directory where buffered (not yet stored) events are also appended, so they survive a
    /// crash and are replayed at startup. `None` (default) keeps them in memory only.
    /// Replays may re-send already stored events; `cdviz.store_cdevents` ignores duplicates.
    #[serde(default)]
    batch_spool_dir: Option<PathBuf>,
}

/// Build database connections pool
///
/// # Errors
///
/// Fail if we cannot connect to the database
impl DbSink {
    pub(crate) async fn try_from_config(mut config: Config) -> Result<Self> {
        if config.pool_connections_min > config.pool_connections_max {
            miette::bail!(
                "pool_connections_min ({}) must be <= pool_connections_max ({})",
                config.pool_connections_min,
                config.pool_connections_max
            );
        }
        let pool_options = PgPoolOptions::new()
            .min_connections(config.pool_connections_min)
            .max_connections(config.pool_connections_max)
            .acquire_timeout(config.pool_acquire_timeout)
            .idle_timeout(config.pool_idle_timeout)
            .max_lifetime(config.pool_max_lifetime)
            .test_before_acquire(config.pool_test_before_acquire);
        tracing::info!(
            max_connections = pool_options.get_max_connections(),
            min_connections = pool_options.get_min_connections(),
            acquire_timeout = ?pool_options.get_acquire_timeout(),
            idle_timeout = ?pool_options.get_idle_timeout(),
            max_lifetime = ?pool_options.get_max_lifetime(),
            test_before_acquire = pool_options.get_test_before_acquire(),
            "Using the database"
        );

        let url = config.url.expose_secret().to_owned();
        let lazy_connection = config.lazy_connection;
        let total_duration_of_retries = config.total_duration_of_retries;
        let batch_max_size = config.batch_max_size.max(1);
        let batch_max_wait = config.batch_max_wait;
        config.url.zeroize();
        let pool = if lazy_connection {
            pool_options.connect_lazy(&url).into_diagnostic()?
        } else {
            pool_options.connect(&url).await.into_diagnostic()?
        };

        let buffers = Arc::new(Mutex::new(Buffers::open(config.batch_spool_dir.as_deref())?));
        let notify = Arc::new(Notify::new());
        // Bounded, but the writer never waits on the DB (the flusher does), so it drains fast.
        let (tx, rx) = mpsc::channel(batch_max_size);
        let (flush_tx, flush_rx) = mpsc::channel(1);
        tokio::spawn(run_writer(
            Arc::clone(&buffers),
            rx,
            batch_max_size,
            Arc::clone(&notify),
            flush_tx,
        ));
        tokio::spawn(run_flusher(
            pool,
            buffers,
            batch_max_size,
            batch_max_wait,
            total_duration_of_retries,
            notify,
            flush_rx,
        ));

        Ok(Self { tx })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DbSink {
    tx: mpsc::Sender<BatchCmd>,
}

impl Sink for DbSink {
    #[tracing::instrument(skip(self, message), fields(cdevent_id = %message.cdevent.id()))]
    async fn send(&self, message: &Message) -> Result<()> {
        let payload = serde_json::to_value(&message.cdevent).into_diagnostic()?;
        self.tx.send(BatchCmd::Push(payload)).await.into_diagnostic()
    }

    async fn flush(&self) -> Result<()> {
        let (ack_tx, ack_rx) = oneshot::channel();
        if self.tx.send(BatchCmd::Flush(ack_tx)).await.is_err() {
            return Ok(()); // batcher already gone, nothing pending
        }
        ack_rx.await.into_diagnostic()
    }
}

enum BatchCmd {
    Push(Value),
    Flush(oneshot::Sender<()>),
}

/// One of the two buffers: events in memory, mirrored to an append-only JSONL file if spooling.
#[derive(Default)]
struct Slot {
    events: Vec<Value>,
    file: Option<File>,
}

impl Slot {
    /// Open (or create) the spool file, loading events left by a previous run.
    fn open(path: &Path) -> Result<Self> {
        let mut events = Vec::new();
        if path.exists() {
            for line in BufReader::new(File::open(path).into_diagnostic()?).lines() {
                let line = line.into_diagnostic()?;
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str(&line) {
                    Ok(event) => events.push(event),
                    Err(err) => tracing::warn!(?err, ?path, "skip unreadable spooled event"),
                }
            }
        }
        let file = OpenOptions::new().create(true).append(true).open(path).into_diagnostic()?;
        Ok(Self { events, file: Some(file) })
    }

    // ponytail: fsync per event under a std Mutex, fine at CI rates; batch fsyncs /
    // spawn_blocking if throughput matters.
    fn push(&mut self, event: Value) {
        if let Some(file) = &mut self.file {
            // Leading newline: a record torn by a crash / failed write is terminated by the next
            // one, so it only loses itself (skipped at replay) instead of corrupting the next.
            let mut record = b"\n".to_vec();
            let written = serde_json::to_writer(&mut record, &event)
                .map_err(std::io::Error::from)
                .and_then(|()| file.write_all(&record))
                .and_then(|()| file.sync_data());
            if let Err(err) = written {
                tracing::warn!(?err, "fail to spool event, kept in memory only");
            }
        }
        self.events.push(event);
    }

    /// Forget the spooled copy once its events are stored (or definitively rejected).
    fn truncate(&mut self) {
        if let Some(file) = &mut self.file
            && let Err(err) = file.set_len(0)
        {
            tracing::warn!(?err, "fail to truncate spool, events will be replayed at restart");
        }
    }
}

/// Double buffer: the writer appends to `slots[active]` while the flusher drains the other
/// (sealed) one, then they swap. So `send()` never waits on the DB, and a sealed slot is only
/// cleared (memory + file) after its events are stored.
// ponytail: memory and spool are unbounded while the DB is down; cap + drop-oldest if that
// becomes real. Events still in the mpsc channel (≤ batch_max_size) aren't spooled yet.
#[derive(Default)]
struct Buffers {
    slots: [Slot; 2],
    active: usize,
}

impl Buffers {
    fn open(spool_dir: Option<&Path>) -> Result<Self> {
        let Some(dir) = spool_dir else {
            return Ok(Self::default());
        };
        std::fs::create_dir_all(dir).into_diagnostic()?;
        let slots =
            [Slot::open(&dir.join("spool-0.jsonl"))?, Slot::open(&dir.join("spool-1.jsonl"))?];
        Ok(Self { slots, active: 0 })
    }
}

fn lock(buffers: &Mutex<Buffers>) -> MutexGuard<'_, Buffers> {
    buffers.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Appends pushed events to the active slot, waking the flusher once `batch_max_size` is
/// reached. Exits once `tx` is dropped; dropping `flush_tx` then makes the flusher drain all.
async fn run_writer(
    buffers: Arc<Mutex<Buffers>>,
    mut rx: mpsc::Receiver<BatchCmd>,
    batch_max_size: usize,
    notify: Arc<Notify>,
    flush_tx: mpsc::Sender<oneshot::Sender<()>>,
) {
    while let Some(cmd) = rx.recv().await {
        match cmd {
            BatchCmd::Push(event) => {
                let len = {
                    let mut buffers = lock(&buffers);
                    let active = buffers.active;
                    buffers.slots[active].push(event);
                    buffers.slots[active].events.len()
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

/// Drains buffered events to the DB every `batch_max_wait`, when the writer reports a full
/// batch, on an explicit `Flush` (graceful shutdown) and at startup (spool replay).
async fn run_flusher(
    pool: PgPool,
    buffers: Arc<Mutex<Buffers>>,
    batch_max_size: usize,
    batch_max_wait: Duration,
    total_duration_of_retries: Duration,
    notify: Arc<Notify>,
    mut flush_rx: mpsc::Receiver<oneshot::Sender<()>>,
) {
    let policy =
        ExponentialBackoff::builder().build_with_total_retry_duration(total_duration_of_retries);
    // Keep swapping + draining until both slots are empty (events pushed meanwhile included).
    // Stops early if the DB is unreachable: leftovers stay in memory + spool, never a hang.
    let drain_all = async |pool: &PgPool| {
        while drain_sealed(pool, &buffers, &policy, batch_max_size).await {
            if lock(&buffers).slots.iter().all(|slot| slot.events.is_empty()) {
                break;
            }
        }
    };
    drain_all(&pool).await;
    loop {
        tokio::select! {
            () = tokio::time::sleep(batch_max_wait) => {
                drain_sealed(&pool, &buffers, &policy, batch_max_size).await;
            }
            () = notify.notified() => {
                drain_sealed(&pool, &buffers, &policy, batch_max_size).await;
            }
            received = flush_rx.recv() => {
                drain_all(&pool).await;
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

/// Swap slots (unless the sealed one still holds events from a failed drain), then store
/// the sealed slot. Returns `false` when the DB is unreachable: events stay in the sealed slot
/// (memory + spool) to be retried on the next drain.
async fn drain_sealed<P: retry_policies::RetryPolicy>(
    pool: &PgPool,
    buffers: &Mutex<Buffers>,
    policy: &P,
    batch_max_size: usize,
) -> bool {
    let (sealed, events) = {
        let mut buffers = lock(buffers);
        if buffers.slots[1 - buffers.active].events.is_empty() {
            buffers.active = 1 - buffers.active;
        }
        let sealed = 1 - buffers.active;
        (sealed, std::mem::take(&mut buffers.slots[sealed].events))
    };
    if events.is_empty() {
        return true;
    }
    for batch in events.chunks(batch_max_size) {
        if !store_with_fallback(pool, policy, batch).await {
            // Put everything back (already stored chunks too: duplicates are ignored by the DB).
            lock(buffers).slots[sealed].events = events;
            return false;
        }
    }
    lock(buffers).slots[sealed].truncate();
    true
}

/// Store a batch, falling back to one by one on a non-transient error so only the offending
/// event(s) are lost. Returns `false` only if the DB stayed unreachable (transient error).
async fn store_with_fallback<P: retry_policies::RetryPolicy>(
    pool: &PgPool,
    policy: &P,
    batch: &[Value],
) -> bool {
    let Err(err) = retry::retry_on_transient(policy, is_transient_sqlx_error, || {
        store_events_batch(pool, batch)
    })
    .await
    else {
        return true;
    };
    tracing::warn!(?err, batch_len = batch.len(), "fail during batch insert of events");
    if is_transient_sqlx_error(&err) {
        return false;
    }
    if batch.len() > 1 {
        for event in batch {
            if let Err(err) = retry::retry_on_transient(policy, is_transient_sqlx_error, || {
                store_events_batch(pool, std::slice::from_ref(event))
            })
            .await
            {
                tracing::warn!(?err, "fail during insert of event");
                if is_transient_sqlx_error(&err) {
                    return false; // DB went down mid-fallback: keep the batch for the next drain
                }
            }
        }
    }
    true
}

fn is_transient_sqlx_error(err: &sqlx::Error) -> bool {
    matches!(err, sqlx::Error::PoolTimedOut | sqlx::Error::PoolClosed | sqlx::Error::Io(_))
}

// basic handmade span far to be compliant with
//[opentelemetry-specification/.../database.md](https://github.com/open-telemetry/opentelemetry-specification/blob/v1.22.0/specification/trace/semantic_conventions/database.md)
#[allow(dead_code)]
fn build_otel_span(db_operation: &str) -> tracing::Span {
    tracing::trace_span!(
        target: tracing_opentelemetry_instrumentation_sdk::TRACING_TARGET,
        "DB request",
        db.system = "postgresql",
        // db.statement = stmt,
        db.operation = db_operation,
        otel.name = db_operation, // should be <db.operation> <db.name>.<db.sql.table>,
        otel.kind = "CLIENT",
        otel.status_code = tracing::field::Empty,
    )
}

// store events as json in db (postgresql using sqlx), one round trip for the whole batch.
// Duplicates (`PostgreSQL` 23505 `unique_violation`, expected on restart when opendal
// replays already-processed files — see TODO in sources/opendal/mod.rs about state
// persistence) are caught per-row inside `cdviz.store_cdevents` itself, not here.
// Older schemas only have `cdviz.store_cdevent` (one event per call): fall back to it,
// one by one, and remember it to skip the failing batch call next time.
async fn store_events_batch(pg_pool: &PgPool, events: &[Value]) -> sqlx::Result<()> {
    if !USE_STORE_CDEVENT.load(Ordering::Relaxed) {
        let result = sqlx::query!("CALL cdviz.store_cdevents($1)", events)
            .execute(pg_pool)
            .instrument(build_otel_span("store_cdevents"))
            .await;
        match result {
            Err(err) if has_code(&err, "42883") => {
                tracing::warn!(
                    ?err,
                    "`cdviz.store_cdevents` not found, fallback to `cdviz.store_cdevent` (1 by 1), upgrade the db schema to batch inserts"
                );
                USE_STORE_CDEVENT.store(true, Ordering::Relaxed);
            }
            other => return other.map(|_| ()),
        }
    }
    for event in events {
        let result = sqlx::query("CALL cdviz.store_cdevent($1)")
            .bind(event)
            .execute(pg_pool)
            .instrument(build_otel_span("store_cdevent"))
            .await;
        match result {
            // Duplicates (replay) are ignored, like `store_cdevents` does per row.
            Ok(_) => {}
            Err(err) if has_code(&err, "23505") => {}
            // Earlier events are already stored (not atomic): only a transient error makes the
            // caller retry the batch; any other error drops just this event, no re-insert.
            // ponytail: a transient retry re-inserts the already stored events, deduped only
            // if the schema has a unique constraint; a savepoint per event if that matters.
            Err(err) if is_transient_sqlx_error(&err) => return Err(err),
            Err(err) => tracing::warn!(?err, "fail during insert of event"),
        }
    }
    Ok(())
}

// ponytail: process-wide flag, shared by all db sinks; a sink on an up-to-date schema then
// also goes 1 by 1 (correct, just slower). Move into DbSink if mixed schemas ever matter.
static USE_STORE_CDEVENT: AtomicBool = AtomicBool::new(false);

fn has_code(err: &sqlx::Error, code: &str) -> bool {
    err.as_database_error().and_then(sqlx::error::DatabaseError::code).is_some_and(|c| c == code)
}

#[cfg(test)]
mod tests {
    use std::fs::read_to_string;

    use super::*;
    use rstest::*;
    use testcontainers::{
        GenericImage, ImageExt,
        core::ContainerAsync,
        core::{IntoContainerPort, WaitFor},
        runners::AsyncRunner,
    };

    struct TestContext {
        pub sink: DbSink,
        // Separate pool for test assertions/setup: DbSink no longer exposes its own pool,
        // it owns one privately for the batcher task.
        pub pool: PgPool,
        pub url: String,
        // Keep db container reference - testcontainers will automatically remove container when dropped
        #[allow(dead_code)]
        db_guard: ContainerAsync<GenericImage>,
        // Keep tracing subscriber
        #[allow(dead_code)]
        tracing_guard: tracing::subscriber::DefaultGuard,
    }

    // #[fixture]
    // //#[once] // only work with non-async, non generic fixtures
    // // workaround explained at [Async once fixtures · Issue #141 · la10736/rstest](https://github.com/la10736/rstest/issues/141)
    // // no drop call on the fixture like on static
    // fn pg() -> (PgPool, Container<Postgres>) {
    //     futures::executor::block_on(async { async_pg().await })
    // }

    #[fixture]
    async fn async_pg() -> (DbSink, PgPool, String, ContainerAsync<GenericImage>) {
        let pg_container = GenericImage::new("postgres", "16")
            .with_exposed_port(5432.tcp())
            .with_wait_for(WaitFor::message_on_stdout(
                "database system is ready to accept connections",
            ))
            .with_wait_for(WaitFor::message_on_stderr(
                "database system is ready to accept connections",
            ))
            .with_network("bridge")
            .with_env_var("POSTGRES_DB", "postgres")
            .with_env_var("POSTGRES_USER", "postgres")
            .with_env_var("POSTGRES_PASSWORD", "postgres")
            .start()
            .await
            .expect("start container");

        // testcontainers automatically maps container port 5432 to a random host port
        let host_port = pg_container.get_host_port_ipv4(5432).await.expect("get port");
        let url = format!("postgresql://postgres:postgres@127.0.0.1:{host_port}/postgres");

        let config = Config {
            enabled: true,
            url: url.clone().into(),
            pool_connections_min: 1,
            pool_connections_max: 30,
            pool_acquire_timeout: default_pool_acquire_timeout(),
            pool_idle_timeout: default_pool_idle_timeout(),
            pool_max_lifetime: default_pool_max_lifetime(),
            pool_test_before_acquire: default_pool_test_before_acquire(),
            total_duration_of_retries: default_total_duration_of_retries(),
            lazy_connection: true,
            batch_max_size: default_batch_max_size(),
            batch_max_wait: default_batch_max_wait(),
            batch_spool_dir: None,
        };

        // Own pool for schema setup and assertions: DbSink keeps its pool private for the
        // batcher task.
        let pool = PgPoolOptions::new().connect(&url).await.unwrap();
        //Basic initialize the db schema
        // A transaction is implicitly created for the all file so some instruction could be applied
        // -- { severity: Error, code: "25001", message: "CREATE INDEX CONCURRENTLY cannot run inside a transaction block",
        sqlx::raw_sql(sqlx::AssertSqlSafe(read_to_string("tests/assets/db/schema.sql").unwrap()))
            .execute(&pool)
            .await
            .unwrap();

        let dbsink = DbSink::try_from_config(config).await.unwrap();
        // container should be keep, else it is remove on drop
        (dbsink, pool, url, pg_container)
    }

    // testcontext() is called once per test, so db could be started several times.
    // We could not used `static` (or the once on fixtures) because static are not dropped at end of the test
    // if needed look at testkit::shared_async_resource
    #[fixture]
    async fn testcontext(
        #[future] async_pg: (DbSink, PgPool, String, ContainerAsync<GenericImage>),
    ) -> TestContext {
        let subscriber = tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(tracing::Level::WARN)
            .finish();
        let tracing_guard = tracing::subscriber::set_default(subscriber);

        let (sink, pool, url, db_guard) = async_pg.await;
        TestContext { sink, pool, url, db_guard, tracing_guard }
    }

    #[test]
    fn transient_errors_are_retried() {
        assert!(is_transient_sqlx_error(&sqlx::Error::PoolTimedOut));
        assert!(is_transient_sqlx_error(&sqlx::Error::PoolClosed));
    }

    #[test]
    fn non_transient_errors_are_not_retried() {
        assert!(!is_transient_sqlx_error(&sqlx::Error::RowNotFound));
    }

    fn spool_config(url: &str, spool_dir: &Path) -> Config {
        Config {
            enabled: true,
            url: url.to_owned().into(),
            pool_connections_min: 0,
            pool_connections_max: 2,
            pool_acquire_timeout: Duration::from_secs(1),
            pool_idle_timeout: default_pool_idle_timeout(),
            pool_max_lifetime: default_pool_max_lifetime(),
            pool_test_before_acquire: default_pool_test_before_acquire(),
            total_duration_of_retries: Duration::ZERO,
            lazy_connection: true,
            batch_max_size: default_batch_max_size(),
            batch_max_wait: Duration::from_secs(3600),
            batch_spool_dir: Some(spool_dir.to_path_buf()),
        }
    }

    fn spooled_lines(spool_dir: &Path) -> usize {
        ["spool-0.jsonl", "spool-1.jsonl"]
            .iter()
            .map(|name| {
                read_to_string(spool_dir.join(name))
                    .unwrap_or_default()
                    .lines()
                    .filter(|l| !l.is_empty())
                    .count()
            })
            .sum()
    }

    #[tokio::test]
    async fn unreachable_db_keeps_events_in_spool() {
        let spool_dir = tempfile::tempdir().unwrap();
        let config = spool_config("postgres://user:pass@127.0.0.1:1/db", spool_dir.path());
        let sink = DbSink::try_from_config(config).await.unwrap();
        let mut runner = proptest::test_runner::TestRunner::default();
        for _ in 0..3 {
            use proptest::prelude::*;
            let message = any::<Message>().new_tree(&mut runner).unwrap().current();
            sink.send(&message).await.unwrap();
        }
        sink.flush().await.unwrap(); // fails to store, must keep everything
        assert_eq!(spooled_lines(spool_dir.path()), 3);
    }

    #[rstest()]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fallback_to_store_cdevent_on_older_schema(#[future] testcontext: TestContext) {
        use proptest::prelude::*;
        use sqlx::Row;
        let testcontext = testcontext.await;
        sqlx::raw_sql(sqlx::AssertSqlSafe("DROP PROCEDURE cdviz.store_cdevents(jsonb[])"))
            .execute(&testcontext.pool)
            .await
            .unwrap();
        let mut runner = proptest::test_runner::TestRunner::default();
        for _ in 0..3 {
            let message = any::<Message>().new_tree(&mut runner).unwrap().current();
            testcontext.sink.send(&message).await.unwrap();
        }
        testcontext.sink.flush().await.unwrap();
        let count: i64 = sqlx::QueryBuilder::new("SELECT count(*) from cdviz.cdevents_lake")
            .build()
            .fetch_one(&testcontext.pool)
            .await
            .unwrap()
            .get(0);
        assert_eq!(count, 3);
    }

    #[rstest()]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spooled_events_are_replayed_at_startup(#[future] testcontext: TestContext) {
        use sqlx::Row;
        let testcontext = testcontext.await;
        let count = async || -> i64 {
            sqlx::QueryBuilder::new("SELECT count(*) from cdviz.cdevents_lake")
                .build()
                .fetch_one(&testcontext.pool)
                .await
                .unwrap()
                .get(0)
        };
        let before = count().await;

        // Leftovers of a crashed run: 2 events in a slot, 1 in the other.
        let spool_dir = tempfile::tempdir().unwrap();
        let mut runner = proptest::test_runner::TestRunner::default();
        let mut lines = Vec::new();
        for _ in 0..3 {
            use proptest::prelude::*;
            let message = any::<Message>().new_tree(&mut runner).unwrap().current();
            lines.push(serde_json::to_string(&message.cdevent).unwrap());
        }
        std::fs::write(spool_dir.path().join("spool-0.jsonl"), lines[..2].join("\n")).unwrap();
        std::fs::write(spool_dir.path().join("spool-1.jsonl"), &lines[2]).unwrap();

        let sink = DbSink::try_from_config(spool_config(&testcontext.url, spool_dir.path()))
            .await
            .unwrap();
        sink.flush().await.unwrap();
        assert_eq!(count().await, before + 3);
        assert_eq!(spooled_lines(spool_dir.path()), 0);
    }

    #[tokio::test]
    async fn min_greater_than_max_pool_connections_is_rejected() {
        let config = Config {
            enabled: true,
            url: "postgres://user:pass@localhost/db".into(),
            pool_connections_min: 5,
            pool_connections_max: 1,
            pool_acquire_timeout: default_pool_acquire_timeout(),
            pool_idle_timeout: default_pool_idle_timeout(),
            pool_max_lifetime: default_pool_max_lifetime(),
            pool_test_before_acquire: default_pool_test_before_acquire(),
            total_duration_of_retries: default_total_duration_of_retries(),
            lazy_connection: true,
            batch_max_size: default_batch_max_size(),
            batch_max_wait: default_batch_max_wait(),
            batch_spool_dir: None,
        };
        assert!(DbSink::try_from_config(config).await.is_err());
    }

    #[rstest()]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_send_random_cdevents(#[future] testcontext: TestContext) {
        use proptest::prelude::*;
        use proptest::test_runner::TestRunner;
        use sqlx::Row;
        let testcontext = testcontext.await; // to keep guard & DB up
        let sink = testcontext.sink;
        let pool = testcontext.pool;
        let mut runner = TestRunner::default();
        let mut count: i64 = sqlx::QueryBuilder::new("SELECT count(*) from cdviz.cdevents_lake")
            .build()
            .fetch_one(&pool)
            .await
            .unwrap()
            .get(0);

        for _ in 0..1 {
            let val = any::<Message>().new_tree(&mut runner).unwrap();
            sink.send(&val.current()).await.unwrap();
            sink.flush().await.unwrap(); // events are batched, force them to land now
            //TODO check insertion content
            let count_n: i64 = sqlx::QueryBuilder::new("SELECT count(*) from cdviz.cdevents_lake")
                .build()
                .fetch_one(&pool)
                .await
                .unwrap()
                .get(0);
            count += 1;
            assert_eq!(count_n, count);
        }
    }
}
