use jiff::Timestamp;
use std::time::Duration;

const TIME_MARGIN: Duration = Duration::from_secs(1);

#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone)]
pub(crate) struct TimeWindowFilter {
    ts_after: Timestamp,
    ts_before: Timestamp,
    ts_before_limit: Option<Timestamp>,
}

impl TimeWindowFilter {
    pub(crate) fn new(ts_after: Timestamp, ts_before_limit: Option<Timestamp>) -> Self {
        let ts_before = compute_ts_before(ts_before_limit);
        Self { ts_after, ts_before, ts_before_limit }
    }

    /// Advance window: `ts_after` = old `ts_before`, `ts_before` = now - margin (capped at limit).
    pub(crate) fn advance(&mut self) {
        self.ts_after = self.ts_before;
        self.ts_before = compute_ts_before(self.ts_before_limit);
    }

    /// Returns `true` if `ts_after` has reached or passed `ts_before_limit`.
    pub(crate) fn is_at_limit(&self) -> bool {
        self.ts_before_limit.is_some_and(|limit| self.ts_after >= limit)
    }

    /// Expose the time window as metadata for VRL/transformer access.
    pub(crate) fn as_metadata(&self) -> serde_json::Value {
        serde_json::json!({
            "ts_after": self.ts_after.to_string(),
            "ts_before": self.ts_before.to_string(),
        })
    }

    pub(crate) fn ts_after(&self) -> Timestamp {
        self.ts_after
    }

    pub(crate) fn set_ts_after(&mut self, ts: Timestamp) {
        self.ts_after = ts;
    }
}

fn compute_ts_before(limit: Option<Timestamp>) -> Timestamp {
    let now_minus_margin = Timestamp::now() - TIME_MARGIN;
    match limit {
        Some(lim) if now_minus_margin > lim => lim,
        _ => now_minus_margin,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_sets_ts_after() {
        let ts = Timestamp::MIN;
        let f = TimeWindowFilter::new(ts, None);
        assert_eq!(f.ts_after(), ts);
    }

    #[test]
    fn test_advance_moves_window() {
        let mut f = TimeWindowFilter::new(Timestamp::MIN, None);
        let old_ts_before = f.ts_before;
        f.advance();
        assert_eq!(f.ts_after(), old_ts_before);
    }

    #[test]
    fn test_is_at_limit_no_limit() {
        let f = TimeWindowFilter::new(Timestamp::MIN, None);
        assert!(!f.is_at_limit());
    }

    #[test]
    fn test_is_at_limit_past_limit() {
        let limit = Timestamp::MIN;
        // ts_after starts at MIN which equals limit
        let f = TimeWindowFilter::new(Timestamp::MIN, Some(limit));
        assert!(f.is_at_limit());
    }

    #[test]
    fn test_ts_before_capped_at_limit() {
        let limit = Timestamp::MIN + Duration::from_secs(100);
        let f = TimeWindowFilter::new(Timestamp::MIN, Some(limit));
        // ts_before should not exceed limit
        assert!(f.ts_before <= limit);
    }
}
