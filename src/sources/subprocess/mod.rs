use super::{EventSource, EventSourcePipe};
use crate::{errors::Result, sources::cli::parsers};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicI32, Ordering},
    },
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct Config {
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub data_globs: Vec<String>,
    #[serde(default)]
    pub no_data: bool,
    #[serde(default)]
    pub parser: parsers::Config,
    /// Base metadata for `EventSources` (`context.source` injected here; user overrides in `run.overrides`)
    #[serde(default)]
    pub metadata: serde_json::Value,
    #[serde(default)]
    pub fail_on_collector_error: bool,
    #[serde(skip)]
    pub exit_code_out: Arc<AtomicI32>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            command: vec![],
            data_globs: vec![],
            no_data: false,
            parser: parsers::Config::default(),
            metadata: serde_json::Value::Null,
            fail_on_collector_error: false,
            exit_code_out: Arc::new(AtomicI32::new(0)),
        }
    }
}

pub(crate) struct SubprocessExtractor {
    config: Config,
    next: EventSourcePipe,
}

impl SubprocessExtractor {
    pub(crate) fn new(config: Config, next: EventSourcePipe) -> Self {
        Self { config, next }
    }

    pub(crate) async fn run(mut self, cancel_token: CancellationToken) -> Result<()> {
        if self.config.command.is_empty() {
            return Err(miette::miette!("subprocess source requires a non-empty 'command'"));
        }

        // Step 1: record started_at timestamp
        let started_at = jiff::Timestamp::now().to_string();

        // Step 2: Build and send "started" event (fire-and-forget)
        let mut started_metadata = self.config.metadata.clone();
        started_metadata["run"]["status"] = serde_json::json!("started");
        started_metadata["run"]["started_at"] = serde_json::json!(started_at);

        let started_event = EventSource {
            body: serde_json::json!({}),
            metadata: started_metadata,
            headers: HashMap::new(),
        };

        if let Err(err) = self.next.send(started_event) {
            if self.config.fail_on_collector_error {
                return Err(err);
            }
            tracing::warn!(?err, "failed to send 'started' event");
        }

        // Step 3: spawn child process with inherited stdio (no capture)
        let (program, args) =
            self.config.command.split_first().ok_or_else(|| miette::miette!("command is empty"))?;

        let cdviz_keys: Vec<String> = std::env::vars()
            .map(|(k, _)| k)
            .filter(|k| k.starts_with("CDVIZ_COLLECTOR__"))
            .collect();

        let mut cmd = tokio::process::Command::new(program);
        cmd.args(args);
        for key in &cdviz_keys {
            cmd.env_remove(key);
        }
        let mut child =
            cmd.spawn().map_err(|e| miette::miette!("failed to spawn child process: {e}"))?;

        // Step 4: wait for child or cancellation
        let exit_code: i32 = tokio::select! {
            result = child.wait() => {
                match result {
                    // code() is None when terminated by signal; treat as failure (1)
                    Ok(status) => status.code().unwrap_or(1),
                    Err(err) => {
                        tracing::warn!(?err, "child process wait failed");
                        1
                    }
                }
            }
            () = cancel_token.cancelled() => {
                child.start_kill().ok();
                let _: Option<std::process::ExitStatus> = child.wait().await.ok();
                130
            }
        };

        // Step 5: collect result files via glob patterns (dedup across overlapping patterns)
        let body = if !self.config.no_data && !self.config.data_globs.is_empty() {
            let mut seen = std::collections::HashSet::new();
            let mut results = vec![];
            for pattern in &self.config.data_globs {
                for path in collect_glob_matches(pattern) {
                    if !seen.insert(path.clone()) {
                        continue; // skip file already matched by an earlier pattern
                    }
                    let content = match std::fs::read_to_string(&path) {
                        Ok(c) => c,
                        Err(err) => {
                            tracing::warn!(?err, file = %path.display(), "failed to read result file");
                            continue;
                        }
                    };
                    let filename = path.to_string_lossy().to_string();
                    match parsers::parse_with_config(&content, &self.config.parser, Some(&filename))
                    {
                        Ok(parsed) => results.push(parsed),
                        Err(err) => {
                            tracing::warn!(?err, file = %path.display(), "failed to parse result file");
                        }
                    }
                }
            }
            serde_json::Value::Array(results)
        } else {
            serde_json::Value::Array(vec![])
        };

        // Step 6: build and send "finished" event (sync — must complete before exit)
        let finished_at = jiff::Timestamp::now().to_string();
        let mut finished_metadata = self.config.metadata.clone();
        finished_metadata["run"]["status"] = serde_json::json!("finished");
        finished_metadata["run"]["exit_code"] = serde_json::json!(exit_code);
        finished_metadata["run"]["started_at"] = serde_json::json!(started_at);
        finished_metadata["run"]["finished_at"] = serde_json::json!(finished_at);

        let finished_event =
            EventSource { body, metadata: finished_metadata, headers: HashMap::new() };

        if let Err(err) = self.next.send(finished_event) {
            tracing::error!(?err, "failed to send 'finished' event");
        }

        // Step 7: drop next to close broadcast channel → sinks drain
        drop(self.next);

        // Step 8: store exit code for caller to read after pipeline drains
        self.config.exit_code_out.store(exit_code, Ordering::SeqCst);

        Ok(())
    }
}

/// Collect all files that match `pattern`.
///
/// If `pattern` is an absolute path with no glob metacharacters, it is treated
/// as a literal file path and returned directly (if the file exists).
/// Otherwise, uses `globset` for pattern matching with a recursive `std::fs`
/// walk starting from the current working directory.
fn collect_glob_matches(pattern: &str) -> Vec<std::path::PathBuf> {
    let path = std::path::Path::new(pattern);
    if path.is_absolute() && !pattern.contains(['*', '?', '[', '{']) {
        return if path.is_file() { vec![path.to_path_buf()] } else { vec![] };
    }
    let matcher = match globset::Glob::new(pattern).and_then(|g| {
        let mut b = globset::GlobSetBuilder::new();
        b.add(g);
        b.build()
    }) {
        Ok(m) => m,
        Err(err) => {
            tracing::warn!(?err, pattern, "invalid glob pattern");
            return vec![];
        }
    };
    let mut out = vec![];
    walk_dir(std::path::Path::new("."), &matcher, &mut out);
    out
}

fn walk_dir(dir: &std::path::Path, matcher: &globset::GlobSet, out: &mut Vec<std::path::PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(err) => {
            tracing::warn!(?err, dir = %dir.display(), "failed to read directory");
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_dir(&path, matcher, out);
        } else {
            // Strip leading "./" so the pattern `**/foo.xml` matches `./a/b/foo.xml`
            let relative = path.strip_prefix(".").unwrap_or(&path);
            if matcher.is_match(relative) {
                out.push(path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transformers::collect_to_vec::Collector;
    use std::time::Duration;
    use tokio::time::timeout;

    #[tokio::test]
    async fn test_child_exit_0_emits_two_events() {
        let exit_code_out = Arc::new(AtomicI32::new(-1));
        let config = Config {
            command: vec!["true".to_string()],
            exit_code_out: exit_code_out.clone(),
            ..Default::default()
        };

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let cancel_token = CancellationToken::new();

        let extractor = SubprocessExtractor::new(config, pipe);
        timeout(Duration::from_secs(5), extractor.run(cancel_token)).await.unwrap().unwrap();

        let events: Vec<EventSource> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].metadata["run"]["status"], "started");
        assert_eq!(events[1].metadata["run"]["status"], "finished");
        assert_eq!(events[1].metadata["run"]["exit_code"], 0);
        assert_eq!(exit_code_out.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn test_child_exit_1_emits_two_events_fail() {
        let exit_code_out = Arc::new(AtomicI32::new(-1));
        let config = Config {
            command: vec!["false".to_string()],
            exit_code_out: exit_code_out.clone(),
            ..Default::default()
        };

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let cancel_token = CancellationToken::new();

        let extractor = SubprocessExtractor::new(config, pipe);
        timeout(Duration::from_secs(5), extractor.run(cancel_token)).await.unwrap().unwrap();

        let events: Vec<EventSource> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 2);
        assert_eq!(events[1].metadata["run"]["exit_code"], 1);
        assert_eq!(exit_code_out.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_exit_code_stored_in_arc() {
        let exit_code_out = Arc::new(AtomicI32::new(-1));
        let config = Config {
            command: vec!["sh".to_string(), "-c".to_string(), "exit 42".to_string()],
            exit_code_out: exit_code_out.clone(),
            ..Default::default()
        };

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let cancel_token = CancellationToken::new();

        let extractor = SubprocessExtractor::new(config, pipe);
        timeout(Duration::from_secs(5), extractor.run(cancel_token)).await.unwrap().unwrap();

        assert_eq!(exit_code_out.load(Ordering::SeqCst), 42);
    }

    #[tokio::test]
    async fn test_no_data_flag_skips_glob() {
        let config = Config {
            command: vec!["true".to_string()],
            data_globs: vec!["**/TEST-*.xml".to_string()],
            no_data: true,
            ..Default::default()
        };

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let cancel_token = CancellationToken::new();

        let extractor = SubprocessExtractor::new(config, pipe);
        timeout(Duration::from_secs(5), extractor.run(cancel_token)).await.unwrap().unwrap();

        let events: Vec<EventSource> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 2);
        // body should be empty array when no_data=true
        assert_eq!(events[1].body, serde_json::json!([]));
    }

    #[tokio::test]
    async fn test_finished_event_sent_after_subprocess_completes() {
        // Regression: both events were sharing the same `started_at` timestamp,
        // making them appear simultaneous even when the child ran for many seconds.
        let config =
            Config { command: vec!["sleep".to_string(), "0.2".to_string()], ..Default::default() };

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let cancel_token = CancellationToken::new();

        let t_before = std::time::Instant::now();
        let extractor = SubprocessExtractor::new(config, pipe);
        timeout(Duration::from_secs(5), extractor.run(cancel_token)).await.unwrap().unwrap();
        let elapsed = t_before.elapsed();

        let events: Vec<EventSource> = collector.try_into_iter().unwrap().collect();
        assert_eq!(events.len(), 2);

        // The extractor must have actually waited for the 200 ms subprocess.
        assert!(
            elapsed >= Duration::from_millis(150),
            "pipeline did not wait for the subprocess: elapsed={elapsed:?}"
        );

        // The "finished" event must carry its own `finished_at` timestamp …
        let started_at = events[0].metadata["run"]["started_at"].as_str().unwrap().to_string();
        let finished_at = events[1].metadata["run"]["finished_at"]
            .as_str()
            .expect("finished event must have a 'finished_at' field separate from 'started_at'");

        // … and it must be strictly later than `started_at`.
        assert_ne!(started_at, finished_at, "started_at and finished_at must differ");
        let started_ts: jiff::Timestamp = started_at.parse().unwrap();
        let finished_ts: jiff::Timestamp = finished_at.parse().unwrap();
        assert!(
            finished_ts > started_ts,
            "finished_at ({finished_ts}) must be after started_at ({started_ts})"
        );
    }

    #[tokio::test]
    async fn test_cancel_token_fires_sigkill() {
        let exit_code_out = Arc::new(AtomicI32::new(-1));
        let config = Config {
            command: vec!["sleep".to_string(), "60".to_string()],
            exit_code_out: exit_code_out.clone(),
            ..Default::default()
        };

        let collector = Collector::<EventSource>::new();
        let pipe = Box::new(collector.create_pipe());
        let cancel_token = CancellationToken::new();
        let cancel_clone = cancel_token.clone();

        // Cancel after a short delay
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });

        let extractor = SubprocessExtractor::new(config, pipe);
        timeout(Duration::from_secs(5), extractor.run(cancel_token)).await.unwrap().unwrap();

        // Exit code should be 130 (cancelled)
        assert_eq!(exit_code_out.load(Ordering::SeqCst), 130);
    }
}
