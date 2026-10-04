//! `ClickHouse` sink implementation.
//!
//! This module provides a `ClickHouse` sink that stores `CDEvents` with user-configurable
//! schema via query templates with `{field}` placeholders.
//!
//! ## Supported Placeholders
//!
//! - `{payload}` - Full `CDEvent` as JSON string
//! - `{id}` - Event ID (UUID)
//! - `{timestamp}` - Event timestamp (RFC 3339)
//! - `{type}` - Full event type (e.g., "dev.cdevents.service.deployed.0.1.1")
//! - `{source}` - Event source URI
//! - `{subject}` - Subject extracted from type (3rd segment, e.g., "service")
//! - `{predicate}` - Predicate extracted from type (4th segment, e.g., "deployed")
//! - `{specversion}` - `CDEvents` spec version
//!
//! ## Configuration Example
//!
//! ```toml
//! [sinks.clickhouse]
//! type = "clickhouse"
//! enabled = true
//! url = "http://localhost:8123"
//! database = "default"
//! user = "default"
//! password = "secret"
//! query = "INSERT INTO cdevents_lake (id, type, source, subject, predicate, specversion, timestamp, payload) VALUES ({id}, {type}, {source}, {subject}, {predicate}, {specversion}, {timestamp}, {payload})"
//! ```

use std::path::PathBuf;
use std::time::Duration;

use super::batch::{BatchStore, Batcher, default_batch_max_size, default_batch_max_wait};
use super::{Sink, retry};
use crate::Message;
use crate::errors::{IntoDiagnostic, Report, Result};
use retry_policies::policies::{ExponentialBackoff, ExponentialBackoffTimed};
use secrecy::{ExposeSecret, SecretString, zeroize::Zeroize};
use serde::Deserialize;
use tracing::Instrument;

use retry::default_total_duration_of_retries;

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Config {
    /// Is the sink enabled?
    pub(crate) enabled: bool,
    /// `ClickHouse` HTTP URL
    url: String,
    /// Database name
    database: String,
    /// Username (optional)
    user: Option<String>,
    /// Password (optional)
    #[serde(skip_serializing)]
    password: Option<SecretString>,
    /// INSERT query template with {field} placeholders
    query: String,
    /// Total duration across all retry attempts for transient connection errors.
    /// Set to `0s` to disable retries.
    #[serde(default = "default_total_duration_of_retries", with = "humantime_serde")]
    total_duration_of_retries: Duration,
    /// Flush once this many events are buffered, and insert at most this many rows per `INSERT`.
    /// Not a buffer cap: while a flush is in flight, new events keep buffering past it and are
    /// inserted later in several `INSERT`s. `ClickHouse` prefers few large inserts over many small.
    #[serde(default = "default_batch_max_size")]
    batch_max_size: usize,
    /// Flush buffered events at least this often, even if `batch_max_size` wasn't reached.
    #[serde(default = "default_batch_max_wait", with = "humantime_serde")]
    batch_max_wait: Duration,
    /// Directory where buffered (not yet stored) events are also appended, so they survive a
    /// crash and are replayed at startup. `None` (default) keeps them in memory only.
    /// Replays may re-insert already stored events: use e.g. a `ReplacingMergeTree` keyed on `id`
    /// if duplicates matter.
    #[serde(default)]
    batch_spool_dir: Option<PathBuf>,
}

/// Supported placeholder fields in query templates
#[derive(Debug, Clone, PartialEq, Eq)]
enum PlaceholderField {
    Payload,
    Id,
    Timestamp,
    Type,
    Source,
    Subject,
    Predicate,
    Specversion,
}

impl PlaceholderField {
    /// Parse a placeholder string (without braces) into a field variant
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "payload" => Some(Self::Payload),
            "id" => Some(Self::Id),
            "timestamp" => Some(Self::Timestamp),
            "type" => Some(Self::Type),
            "source" => Some(Self::Source),
            "subject" => Some(Self::Subject),
            "predicate" => Some(Self::Predicate),
            "specversion" => Some(Self::Specversion),
            _ => None,
        }
    }

    /// Extract the field value from a `CDEvent`
    fn extract_value(&self, msg: &Message) -> Result<String> {
        match self {
            Self::Payload => serde_json::to_string(&msg.cdevent).into_diagnostic(),
            Self::Id => Ok(msg.cdevent.id().to_string()),
            Self::Timestamp => Ok(msg.cdevent.timestamp().to_string()),
            Self::Type => Ok(msg.cdevent.ty().to_string()),
            Self::Source => Ok(msg.cdevent.source().to_string()),
            Self::Subject => extract_type_segment(msg.cdevent.ty(), 2),
            Self::Predicate => extract_type_segment(msg.cdevent.ty(), 3),
            Self::Specversion => Ok(msg.cdevent.version().to_string()),
        }
    }
}

/// Extract a segment from `CDEvent` type string (e.g., "dev.cdevents.service.deployed.0.1.1")
/// Segments are 0-indexed: dev=0, cdevents=1, subject=2, predicate=3, version=4
fn extract_type_segment(type_str: impl AsRef<str>, index: usize) -> Result<String> {
    let type_str = type_str.as_ref();
    type_str.split('.').nth(index).map(String::from).ok_or_else(|| {
        crate::errors::miette!("Failed to extract segment {} from type string: {}", index, type_str)
    })
}

/// Parsed query template with placeholder fields
#[derive(Debug, Clone)]
struct ParsedQuery {
    /// SQL query with placeholders replaced by `?`
    sql: String,
    /// Ordered list of fields to bind
    fields: Vec<PlaceholderField>,
    /// `sql` split as (`... VALUES`, `(?, ...)`) to build multi-row inserts,
    /// `None` if the template has no trailing `VALUES (...)` (e.g. `INSERT ... SELECT`).
    values: Option<(String, String)>,
}

impl ParsedQuery {
    /// Parse a query template, replacing {field} with ? and tracking field order
    fn parse(template: &str) -> Result<Self> {
        let mut sql = String::new();
        let mut fields = Vec::new();
        let mut chars = template.chars().peekable();

        while let Some(c) = chars.next() {
            if c == '{' {
                // Parse placeholder
                let mut placeholder = String::new();
                for next in chars.by_ref() {
                    if next == '}' {
                        break;
                    }
                    placeholder.push(next);
                }

                // Validate and store field
                let field = PlaceholderField::from_str(&placeholder).ok_or_else(|| {
                    crate::errors::miette!(
                        "Unknown placeholder '{{{}}}' in query template. Supported: {{payload}}, {{id}}, {{timestamp}}, {{type}}, {{source}}, {{subject}}, {{predicate}}, {{specversion}}",
                        placeholder
                    )
                })?;
                fields.push(field);
                sql.push('?');
            } else {
                sql.push(c);
            }
        }

        if fields.is_empty() {
            return Err(crate::errors::miette!(
                "Query template must contain at least one placeholder (e.g., {{id}}, {{payload}})"
            ));
        }

        // ASCII uppercase keeps byte offsets, so `idx` is valid in `sql`.
        // Whole word only, so `json_values(...)` in an `INSERT ... SELECT` isn't taken for it.
        let is_ident = |c: Option<char>| c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        let upper = sql.to_ascii_uppercase();
        let values_idx = upper
            .match_indices("VALUES")
            .map(|(idx, _)| idx)
            .filter(|&idx| {
                !is_ident(sql[..idx].chars().next_back())
                    && !is_ident(sql[idx + "VALUES".len()..].chars().next())
            })
            .last();
        let values = values_idx.and_then(|idx| {
            let (head, tuple) = sql.split_at(idx + "VALUES".len());
            let tuple = tuple.trim().trim_end_matches(';').trim_end();
            (tuple.starts_with('(') && tuple.ends_with(')'))
                .then(|| (head.to_owned(), tuple.to_owned()))
        });

        Ok(Self { sql, fields, values })
    }

    /// SQL inserting `rows` rows in one statement (the `VALUES` tuple repeated).
    fn batch_sql(&self, rows: usize) -> Option<String> {
        self.values
            .as_ref()
            .map(|(head, tuple)| format!("{head} {}", vec![tuple.as_str(); rows].join(", ")))
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ClickHouseSink {
    parsed_query: ParsedQuery,
    batcher: Batcher<Vec<String>>,
}

impl TryFrom<Config> for ClickHouseSink {
    type Error = Report;

    fn try_from(mut config: Config) -> Result<Self> {
        // Parse query template
        let parsed_query = ParsedQuery::parse(&config.query)?;

        // Build ClickHouse client
        let mut client =
            clickhouse::Client::default().with_url(&config.url).with_database(&config.database);

        if let Some(ref user) = config.user {
            client = client.with_user(user);
        }

        if let Some(ref password) = config.password {
            client = client.with_password(password.expose_secret());
        }

        // Zeroize password
        if let Some(ref mut password) = config.password {
            password.zeroize();
        }

        tracing::info!(
            url = %config.url,
            database = %config.database,
            user = ?config.user,
            placeholders = ?parsed_query.fields.len(),
            multi_row_insert = parsed_query.values.is_some(),
            "Using ClickHouse sink"
        );

        let policy = ExponentialBackoff::builder()
            .build_with_total_retry_duration(config.total_duration_of_retries);
        let batcher = Batcher::start(
            ChStore { client, parsed_query: parsed_query.clone(), policy },
            config.batch_max_size,
            config.batch_max_wait,
            config.batch_spool_dir.as_deref(),
        )?;
        Ok(Self { parsed_query, batcher })
    }
}

impl Sink for ClickHouseSink {
    #[tracing::instrument(skip(self, message), fields(cdevent_id = %message.cdevent.id()))]
    async fn send(&self, message: &Message) -> Result<()> {
        let values: Result<Vec<String>> =
            self.parsed_query.fields.iter().map(|f| f.extract_value(message)).collect();
        self.batcher.push(values?).await
    }

    async fn flush(&self) -> Result<()> {
        self.batcher.flush().await
    }
}

struct ChStore {
    client: clickhouse::Client,
    parsed_query: ParsedQuery,
    policy: ExponentialBackoffTimed,
}

impl BatchStore for ChStore {
    /// The placeholder values of one row, in `parsed_query.fields` order.
    type Item = Vec<String>;

    /// Insert all rows in one statement, falling back to one by one on a non-transient error
    /// (or a template without `VALUES`) so only the offending row(s) are lost. Returns `false`
    /// only if `ClickHouse` stayed unreachable (transient error).
    async fn store(&self, rows: &[Vec<String>]) -> bool {
        let (client, policy) = (&self.client, &self.policy);
        if let Some(sql) = self.parsed_query.batch_sql(rows.len()) {
            let values: Vec<&String> = rows.iter().flatten().collect();
            let Err(err) = retry::retry_on_transient(policy, is_transient_clickhouse_error, || {
                execute_query(client, &sql, &values)
            })
            .await
            else {
                return true;
            };
            tracing::warn!(?err, batch_len = rows.len(), "fail during batch insert of events");
            if is_transient_clickhouse_error(&err) {
                return false;
            }
            if rows.len() == 1 {
                return true;
            }
        }
        for row in rows {
            if let Err(err) =
                retry::retry_on_transient(policy, is_transient_clickhouse_error, || {
                    execute_query(client, &self.parsed_query.sql, row)
                })
                .await
            {
                tracing::warn!(?err, "fail during insert of event");
                if is_transient_clickhouse_error(&err) {
                    // ponytail: rows already inserted by this loop are re-inserted on the next
                    // drain; dedup in the table (ReplacingMergeTree) if that matters.
                    return false;
                }
            }
        }
        true
    }
}

async fn execute_query(
    client: &clickhouse::Client,
    sql: &str,
    values: &[impl AsRef<str>],
) -> clickhouse::error::Result<()> {
    let mut query = client.query(sql);
    for value in values {
        query = query.bind(value.as_ref());
    }
    query.execute().instrument(build_otel_span("INSERT")).await
}

/// Classifies the native `clickhouse::error::Error` directly, before it would be erased into a
/// `miette::Report` — `Report::downcast_ref` only matches the outermost wrapped type, not its
/// `source()` chain, so classifying post-conversion would never match.
fn is_transient_clickhouse_error(err: &clickhouse::error::Error) -> bool {
    matches!(err, clickhouse::error::Error::Network(_) | clickhouse::error::Error::TimedOut)
}

// OTel span for ClickHouse operations
fn build_otel_span(db_operation: &str) -> tracing::Span {
    tracing::trace_span!(
        target: tracing_opentelemetry_instrumentation_sdk::TRACING_TARGET,
        "DB request",
        db.system = "clickhouse",
        db.operation = db_operation,
        otel.name = db_operation,
        otel.kind = "CLIENT",
        otel.status_code = tracing::field::Empty,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::*;
    use testcontainers::{
        GenericImage, ImageExt, core::ContainerAsync, core::IntoContainerPort, runners::AsyncRunner,
    };

    #[test]
    fn test_parse_valid_template() {
        let template = "INSERT INTO t (id, payload) VALUES ({id}, {payload})";
        let parsed = ParsedQuery::parse(template).unwrap();

        assert_eq!(parsed.sql, "INSERT INTO t (id, payload) VALUES (?, ?)");
        assert_eq!(parsed.fields.len(), 2);
        assert_eq!(parsed.fields[0], PlaceholderField::Id);
        assert_eq!(parsed.fields[1], PlaceholderField::Payload);
    }

    #[test]
    fn test_parse_all_fields() {
        let template = "INSERT INTO t VALUES ({payload}, {id}, {timestamp}, {type}, {source}, {subject}, {predicate}, {specversion})";
        let parsed = ParsedQuery::parse(template).unwrap();

        assert_eq!(parsed.fields.len(), 8);
        assert_eq!(parsed.fields[0], PlaceholderField::Payload);
        assert_eq!(parsed.fields[1], PlaceholderField::Id);
        assert_eq!(parsed.fields[2], PlaceholderField::Timestamp);
        assert_eq!(parsed.fields[3], PlaceholderField::Type);
        assert_eq!(parsed.fields[4], PlaceholderField::Source);
        assert_eq!(parsed.fields[5], PlaceholderField::Subject);
        assert_eq!(parsed.fields[6], PlaceholderField::Predicate);
        assert_eq!(parsed.fields[7], PlaceholderField::Specversion);
    }

    #[test]
    fn test_parse_unknown_placeholder() {
        let template = "INSERT INTO t (id, invalid) VALUES ({id}, {invalid_field})";
        let result = ParsedQuery::parse(template);

        assert!(result.is_err());
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("Unknown placeholder"));
        assert!(err_msg.contains("invalid_field"));
    }

    #[test]
    fn test_parse_no_placeholders() {
        let template = "INSERT INTO t (col) VALUES ('static')";
        let result = ParsedQuery::parse(template);

        assert!(result.is_err());
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(err_msg.contains("at least one placeholder"));
    }

    #[test]
    fn test_batch_sql_repeats_values_tuple() {
        let parsed =
            ParsedQuery::parse("INSERT INTO t (id, payload) values ({id}, f({payload}));").unwrap();
        assert_eq!(
            parsed.batch_sql(2).unwrap(),
            "INSERT INTO t (id, payload) values (?, f(?)), (?, f(?))"
        );
    }

    #[test]
    fn test_batch_sql_none_without_values() {
        let parsed = ParsedQuery::parse("INSERT INTO t SELECT {id}, {payload}").unwrap();
        assert!(parsed.batch_sql(2).is_none());
        let parsed =
            ParsedQuery::parse("INSERT INTO t SELECT {id}, json_values({payload})").unwrap();
        assert!(parsed.batch_sql(2).is_none());
    }

    #[test]
    fn test_extract_type_segment_subject() {
        let type_str = "dev.cdevents.service.deployed.0.1.1";
        let subject = extract_type_segment(type_str, 2).unwrap();
        assert_eq!(subject, "service");
    }

    #[test]
    fn test_extract_type_segment_predicate() {
        let type_str = "dev.cdevents.artifact.published.0.1.1";
        let predicate = extract_type_segment(type_str, 3).unwrap();
        assert_eq!(predicate, "published");
    }

    #[test]
    fn test_extract_type_segment_invalid() {
        let type_str = "dev.cdevents.service";
        let result = extract_type_segment(type_str, 5);
        assert!(result.is_err());
    }

    struct TestContext {
        pub sink: ClickHouseSink,
        pub client: clickhouse::Client,
        #[allow(dead_code)]
        db_guard: ContainerAsync<GenericImage>,
        #[allow(dead_code)]
        tracing_guard: tracing::subscriber::DefaultGuard,
    }

    #[fixture]
    async fn async_clickhouse() -> (ClickHouseSink, clickhouse::Client, ContainerAsync<GenericImage>)
    {
        let ch_container = GenericImage::new("clickhouse/clickhouse-server", "24")
            .with_exposed_port(8123.tcp())
            .with_network("bridge")
            .start()
            .await
            .expect("start container");

        let host_port = ch_container.get_host_port_ipv4(8123).await.expect("get port");

        // Wait for ClickHouse to be ready via HTTP health check
        let client = reqwest::Client::new();
        for _ in 0..30 {
            if client.get(format!("http://127.0.0.1:{host_port}/ping")).send().await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }

        let config = Config {
            enabled: true,
            url: format!("http://127.0.0.1:{host_port}"),
            database: "default".to_string(),
            user: Some("default".to_string()),
            password: None,
            query: "INSERT INTO cdevents_lake (id, type, source, subject, predicate, specversion, timestamp, payload) VALUES ({id}, {type}, {source}, {subject}, {predicate}, {specversion}, {timestamp}, {payload})".to_string(),
            total_duration_of_retries: default_total_duration_of_retries(),
            batch_max_size: default_batch_max_size(),
            batch_max_wait: default_batch_max_wait(),
            batch_spool_dir: None,
        };

        let sink = ClickHouseSink::try_from(config).unwrap();
        let client = clickhouse::Client::default()
            .with_url(format!("http://127.0.0.1:{host_port}"))
            .with_database("default")
            .with_user("default");

        // Initialize schema
        // Note: timestamp stored as String because CDEvent timestamps include timezone offsets
        // Users can cast to DateTime64 in queries if needed: parseDateTime64BestEffort(timestamp)
        client
            .query(
                r"
                CREATE TABLE IF NOT EXISTS cdevents_lake (
                    id String,
                    type String,
                    source String,
                    subject String,
                    predicate String,
                    specversion String,
                    timestamp String,
                    payload String,
                    inserted_at DateTime64(3) DEFAULT now64(3)
                ) ENGINE = MergeTree()
                ORDER BY (type, inserted_at)
                ",
            )
            .execute()
            .await
            .unwrap();

        (sink, client, ch_container)
    }

    #[fixture]
    async fn testcontext(
        #[future] async_clickhouse: (
            ClickHouseSink,
            clickhouse::Client,
            ContainerAsync<GenericImage>,
        ),
    ) -> TestContext {
        let subscriber = tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(tracing::Level::WARN)
            .finish();
        let tracing_guard = tracing::subscriber::set_default(subscriber);

        let (sink, client, db_guard) = async_clickhouse.await;
        TestContext { sink, client, db_guard, tracing_guard }
    }

    #[rstest()]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_send_cdevent_to_clickhouse(#[future] testcontext: TestContext) {
        use proptest::prelude::*;
        use proptest::test_runner::TestRunner;

        let testcontext = testcontext.await;
        let sink = testcontext.sink;
        let mut runner = TestRunner::default();

        // Send random CDEvents, batched into a single multi-row insert by flush()
        for _ in 0..3 {
            let test_message = any::<Message>().new_tree(&mut runner).unwrap().current();
            sink.send(&test_message).await.unwrap();
        }
        sink.flush().await.unwrap();

        // Query to verify insertion
        let count: u64 = testcontext
            .client
            .query("SELECT count(*) FROM cdevents_lake")
            .fetch_one()
            .await
            .unwrap();

        assert_eq!(count, 3);
    }
}
