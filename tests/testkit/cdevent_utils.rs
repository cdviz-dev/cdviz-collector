#![allow(clippy::unwrap_used)]

use cdevents_sdk::CDEvent;
use cdviz_collector::cdevent_utils::sanitize_id;
use miette::Result;
use proptest::arbitrary::any;
use proptest::strategy::Strategy;
use proptest::test_runner::TestRunner;

/// Generates a list of random `CDEvents` using the Arbitrary trait
///
/// # Arguments
/// * `count` - Number of `CDEvents` to generate
///
/// # Returns
/// * `Result<Vec<CDEvent>>` - Vector of randomly generated `CDEvents`
///
/// # Example
/// ```
/// use cdviz_collector::tests::framework::generate_random_cdevents;
/// let events = generate_random_cdevents(5).unwrap();
/// assert_eq!(events.len(), 5);
/// ```
pub fn generate_random_cdevents(count: usize) -> Result<Vec<CDEvent>> {
    let mut runner = TestRunner::default();
    let mut events = Vec::with_capacity(count);

    for _ in 0..count {
        let strategy = any::<CDEvent>();
        let mut event = strategy
            .new_tree(&mut runner)
            .map_err(|e| miette::miette!("Failed to generate random CDEvent: {}", e))?
            .current();

        // sanitize events'id (as done internally)
        let sanitized_id = sanitize_id(event.id())?;
        event = event.with_id(sanitized_id);

        events.push(event);
    }

    Ok(events)
}

/// Converts a `CDEvent` to a JSON string representation
///
/// # Arguments
/// * `event` - The `CDEvent` to convert
///
/// # Returns
/// * `Result<String>` - JSON string representation of the `CDEvent`
///
/// # Example
/// ```
/// use cdviz_collector::tests::framework::{generate_random_cdevents, cdevent_to_json};
/// let events = generate_random_cdevents(1).unwrap();
/// let json_string = cdevent_to_json(&events[0]).unwrap();
/// assert!(json_string.contains("specversion"));
/// ```
pub fn cdevent_to_json(event: &CDEvent) -> Result<String> {
    serde_json::to_string(event)
        .map_err(|e| miette::miette!("Failed to serialize CDEvent to JSON: {}", e))
}

/// Compares two lists of `CDEvents` and returns differences
///
/// Both lists are sorted by event ID before comparison
///
/// # Arguments
/// * `expected` - Expected `CDEvents` (input events)
/// * `actual` - Actual `CDEvents` (output events)
///
/// # Returns
/// * `Result<Vec<String>>` - List of difference descriptions, empty if events match
///
/// # Example
/// ```
/// use cdviz_collector::tests::framework::{generate_random_cdevents, compare_cdevents};
/// let events1 = generate_random_cdevents(3).unwrap();
/// let events2 = events1.clone(); // Same events
/// let differences = compare_cdevents(&events1, &events2).unwrap();
/// assert!(differences.is_empty());
/// ```
pub fn compare_cdevents(expected: &[CDEvent], actual: &[CDEvent]) -> Result<Vec<String>> {
    let mut differences = Vec::new();

    // Sort both lists by event ID for consistent comparison
    let mut expected_sorted = expected.to_vec();
    let mut actual_sorted = actual.to_vec();

    expected_sorted.sort_by_key(|a| a.id().to_string());
    actual_sorted.sort_by_key(|a| a.id().to_string());

    // Check count first
    if expected_sorted.len() != actual_sorted.len() {
        differences.push(format!(
            "Event count mismatch: expected {} events, got {} events",
            expected_sorted.len(),
            actual_sorted.len()
        ));
    }

    // Compare events by pairs (up to the minimum count)
    let min_count = expected_sorted.len().min(actual_sorted.len());

    for i in 0..min_count {
        let expected_event = &expected_sorted[i];
        let actual_event = &actual_sorted[i];

        // Convert both to JSON for detailed comparison
        let expected_json = cdevent_to_json(expected_event)?;
        let actual_json = cdevent_to_json(actual_event)?;

        if expected_json != actual_json {
            differences.push(format!(
                "Event {} differs:\n  Expected: {}\n  Actual:   {}",
                expected_event.id(),
                expected_json,
                actual_json
            ));
        }
    }

    // Report extra events in expected
    if expected_sorted.len() > actual_sorted.len() {
        for item in expected_sorted.iter().skip(min_count) {
            differences.push(format!("Missing expected event: {} ({})", item.id(), item.ty()));
        }
    }

    // Report extra events in actual
    if actual_sorted.len() > expected_sorted.len() {
        for item in actual_sorted.iter().skip(min_count) {
            differences.push(format!("Unexpected extra event: {} ({})", item.id(), item.ty()));
        }
    }

    Ok(differences)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_random_cdevents() {
        let events = generate_random_cdevents(5).unwrap();
        assert_eq!(events.len(), 5);

        // Verify each event has required CDEvent structure
        for event in &events {
            assert!(!event.id().to_string().is_empty());
            assert!(!event.ty().is_empty());
            assert!(!event.source().to_string().is_empty());
        }
    }

    #[test]
    fn test_cdevent_to_json() {
        let events = generate_random_cdevents(1).unwrap();
        let json_string = cdevent_to_json(&events[0]).unwrap();

        // Verify it's valid JSON and contains CDEvent structure
        let parsed: serde_json::Value = serde_json::from_str(&json_string).unwrap();
        // CDEvent format has context and subject, not CloudEvents format
        assert!(parsed.get("context").is_some() || parsed.get("specversion").is_some());
        assert!(parsed.get("subject").is_some() || parsed.get("type").is_some());
    }

    #[test]
    fn test_compare_cdevents_identical() {
        let events1 = generate_random_cdevents(3).unwrap();
        let events2 = events1.clone();

        let differences = compare_cdevents(&events1, &events2).unwrap();
        assert!(differences.is_empty());
    }

    #[test]
    fn test_compare_cdevents_count_mismatch() {
        let events1 = generate_random_cdevents(3).unwrap();
        let events2 = generate_random_cdevents(2).unwrap();

        let differences = compare_cdevents(&events1, &events2).unwrap();
        assert!(!differences.is_empty());
        assert!(differences.iter().any(|d| d.contains("Event count mismatch")));
    }

    #[test]
    fn test_compare_cdevents_empty() {
        let empty1: Vec<CDEvent> = vec![];
        let empty2: Vec<CDEvent> = vec![];

        let differences = compare_cdevents(&empty1, &empty2).unwrap();
        assert!(differences.is_empty());
    }
}
