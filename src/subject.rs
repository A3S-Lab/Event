//! Subject matching utilities
//!
//! Shared subject wildcard matching used by both the in-memory provider
//! and the Broker/Trigger pattern.

/// Check if a subject matches a filter pattern
///
/// Supports NATS-style wildcards:
/// - `>` — match everything after (greedy, must be last token)
/// - `*` — match exactly one token (single level)
///
/// # Examples
///
/// ```
/// use a3s_event::subject::subject_matches;
///
/// assert!(subject_matches("events.market.forex", "events.market.forex"));
/// assert!(subject_matches("events.market.forex", "events.market.>"));
/// assert!(subject_matches("events.market.forex.usd", "events.market.>"));
/// assert!(subject_matches("events.market.forex", "events.*.forex"));
/// assert!(!subject_matches("events.market.forex", "events.system.>"));
/// ```
pub fn subject_matches(subject: &str, filter: &str) -> bool {
    let sub_parts: Vec<&str> = subject.split('.').collect();
    let filter_parts: Vec<&str> = filter.split('.').collect();

    for (i, fp) in filter_parts.iter().enumerate() {
        if *fp == ">" {
            return true; // Match everything after
        }
        if i >= sub_parts.len() {
            return false;
        }
        if *fp != "*" && *fp != sub_parts[i] {
            return false;
        }
    }

    sub_parts.len() == filter_parts.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exact_match() {
        assert!(subject_matches(
            "events.market.forex",
            "events.market.forex"
        ));
    }

    #[test]
    fn test_greedy_wildcard() {
        assert!(subject_matches("events.market.forex", "events.market.>"));
        assert!(subject_matches(
            "events.market.forex.usd",
            "events.market.>"
        ));
        assert!(subject_matches("events.market.forex", "events.>"));
    }

    #[test]
    fn test_single_wildcard() {
        assert!(subject_matches("events.market.forex", "events.*.forex"));
        assert!(!subject_matches("events.market.crypto", "events.*.forex"));
    }

    #[test]
    fn test_no_match() {
        assert!(!subject_matches("events.market.forex", "events.system.>"));
        assert!(!subject_matches("events.market", "events.market.forex"));
    }

    #[test]
    fn test_match_all() {
        assert!(subject_matches("events.anything.here", ">"));
    }
}
