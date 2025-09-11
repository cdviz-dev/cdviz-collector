use super::Sink;
use crate::{
    Message,
    errors::{IntoDiagnostic, Report, Result},
};
use secrecy::{ExposeSecret, SecretString, zeroize::Zeroize};
use serde::Deserialize;
use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tracing::Instrument;

/// The database client config
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Config {
    /// Is the sink is enabled?
    pub(crate) enabled: bool,

    /// The database url (with username, password and the database)
    url: SecretString,

    /// The minimum number of connections to the database to maintain at all times.
    /// minimum > 0, require to have access to the database at startup time,
    /// consume a little more resource on idle
    /// and could increase performance on low load (keep prepared statement,...)
    // https://docs.rs/sqlx/latest/sqlx/pool/struct.PoolOptions.html#method.min_connections
    pool_connections_min: u32,

    /// The maximum number of connections to the database to open / to maintain.
    // https://docs.rs/sqlx/latest/sqlx/pool/struct.PoolOptions.html#method.max_connections
    pool_connections_max: u32,
}

/// Build database connections pool
///
/// # Errors
///
/// Fail if we cannot connect to the database
impl TryFrom<Config> for DbSink {
    type Error = Report;

    fn try_from(config: Config) -> Result<Self> {
        let pool_options = PgPoolOptions::new()
            .min_connections(config.pool_connections_min)
            .max_connections(config.pool_connections_max);
        tracing::info!(
            max_connections = pool_options.get_max_connections(),
            min_connections = pool_options.get_min_connections(),
            acquire_timeout = ?pool_options.get_acquire_timeout(),
            idle_timeout = ?pool_options.get_idle_timeout(),
            max_lifetime = ?pool_options.get_max_lifetime(),
            test_before_acquire = pool_options.get_test_before_acquire(),
            "Using the database"
        );

        let pool = pool_options.connect_lazy(config.url.expose_secret()).into_diagnostic()?;
        let mut config = config;
        config.url.zeroize();
        Ok(Self { pool })
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DbSink {
    pool: PgPool,
}

impl Sink for DbSink {
    #[tracing::instrument(skip(self, message), fields(cdevent_id = %message.cdevent.id()))]
    async fn send(&self, message: &Message) -> Result<()> {
        store_event(
            &self.pool,
            // TODO build Event from raw json
            Event { payload: serde_json::to_value(&message.cdevent).into_diagnostic()? },
        )
        .await?;
        Ok(())
    }
}

#[derive(sqlx::FromRow)]
struct Event {
    payload: serde_json::Value,
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

// store event as json in db (postgresql using sqlx)
async fn store_event(pg_pool: &PgPool, event: Event) -> Result<()> {
    sqlx::query!("CALL cdviz.store_cdevent($1)", event.payload)
        .execute(pg_pool)
        .instrument(build_otel_span("store_cdevent"))
        .await
        .into_diagnostic()?;
    Ok(())
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
    async fn async_pg() -> (DbSink, ContainerAsync<GenericImage>) {
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

        let config = Config {
            enabled: true,
            url: {
                // testcontainers automatically maps container port 5432 to a random host port
                let host_port = pg_container.get_host_port_ipv4(5432).await.expect("get port");
                format!("postgresql://postgres:postgres@127.0.0.1:{host_port}/postgres").into()
            },
            pool_connections_min: 1,
            pool_connections_max: 30,
        };
        let dbsink = DbSink::try_from(config).unwrap();
        //Basic initialize the db schema
        // A transaction is implicitly created for the all file so some instruction could be applied
        // -- { severity: Error, code: "25001", message: "CREATE INDEX CONCURRENTLY cannot run inside a transaction block",
        sqlx::raw_sql(read_to_string("tests/assets/db/schema.sql").unwrap().as_str())
            .execute(&dbsink.pool)
            .await
            .unwrap();
        // container should be keep, else it is remove on drop
        (dbsink, pg_container)
    }

    // servers() is called once per test, so db could only started several times.
    // We could not used `static` (or the once on fixtures) because statis are not dropped at end of the test
    #[fixture]
    async fn testcontext(
        #[future] async_pg: (DbSink, ContainerAsync<GenericImage>),
    ) -> TestContext {
        let subscriber = tracing_subscriber::FmtSubscriber::builder()
            .with_max_level(tracing::Level::WARN)
            .finish();
        let tracing_guard = tracing::subscriber::set_default(subscriber);

        let (sink, db_guard) = async_pg.await;
        TestContext { sink, db_guard, tracing_guard }
    }

    #[rstest()]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_send_random_cdevents(#[future] testcontext: TestContext) {
        use proptest::prelude::*;
        use proptest::test_runner::TestRunner;
        use sqlx::Row;
        let testcontext = testcontext.await; // to keep guard & DB up
        let sink = testcontext.sink;
        let mut runner = TestRunner::default();
        let mut count: i64 = sqlx::QueryBuilder::new("SELECT count(*) from cdviz.cdevents_lake")
            .build()
            .fetch_one(&sink.pool)
            .await
            .unwrap()
            .get(0);

        for _ in 0..1 {
            let val = any::<Message>().new_tree(&mut runner).unwrap();
            sink.send(&val.current()).await.unwrap();
            //TODO check insertion content
            let count_n: i64 = sqlx::QueryBuilder::new("SELECT count(*) from cdviz.cdevents_lake")
                .build()
                .fetch_one(&sink.pool)
                .await
                .unwrap()
                .get(0);
            count += 1;
            assert_eq!(count_n, count);
        }
    }
}
