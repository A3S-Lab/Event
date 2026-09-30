//! Subject ⇄ (topic) mapping for the Iggy provider
//!
//! Iggy has no subject wildcards — it organizes data as
//! stream → topic → partition. This module defines the one routing rule the
//! provider uses in **both** directions so that publishing and subscribing
//! always agree:
//!
//! - the event stream is the single configured `stream_name`
//! - the **topic is the first token after the subject prefix** (the category)
//! - the full subject travels in the message payload and the `a3s-subject`
//!   user header, and consumers narrow it client-side with
//!   [`crate::subject::subject_matches`]
//!
//! A filter whose category token is a wildcard (`events.>`, `events.*.x`)
//! resolves to "every topic in the stream"; Iggy cannot filter server-side
//! across topics, so those subscriptions poll all topics and filter
//! client-side.

/// Maximum length of a sanitized Iggy resource name.
///
/// Iggy identifiers are capped at 255 bytes on the wire; stay well below it
/// so a numeric-to-string swap or suffix can never overflow.
const MAX_NAME_LEN: usize = 200;

/// Where a subject routes inside the Iggy stream
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectRoute {
    /// Sanitized Iggy topic name (the subject's category token)
    pub topic: String,
}

/// A resolved subscription filter
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterRoute {
    /// Filter pins one topic (e.g. `events.market.>` → topic `market`)
    Single {
        /// Sanitized Iggy topic name
        topic: String,
    },
    /// Filter spans every topic in the stream (wildcard category token)
    AllTopics,
}

/// Sanitize an arbitrary string into a valid Iggy resource name.
///
/// Iggy names are restricted to alphanumeric characters, `_` and `-`.
/// Anything else (dots included — subjects are full of them) becomes `_`.
/// Empty input stays empty: callers reject empties where the subject
/// grammar already forbids them.
pub fn sanitize_name(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    sanitized.chars().take(MAX_NAME_LEN).collect()
}

/// Route a concrete event subject to its Iggy topic.
///
/// The subject must start with `prefix` and carry at least one token after
/// it: `events.market.forex.usd` with prefix `events` routes to topic
/// `market`. The tail is *not* part of the route — it rides in the payload
/// and user headers.
pub fn parse_subject(subject: &str, prefix: &str) -> Result<SubjectRoute, String> {
    let prefix_tokens: Vec<&str> = prefix.split('.').filter(|t| !t.is_empty()).collect();
    let tokens: Vec<&str> = subject.split('.').collect();

    if prefix_tokens.is_empty() {
        return Err(format!("empty subject prefix '{prefix}'"));
    }
    if tokens.len() < prefix_tokens.len() + 1 {
        return Err(format!(
            "subject '{subject}' must have at least one token after prefix '{prefix}'"
        ));
    }
    for (i, pt) in prefix_tokens.iter().enumerate() {
        if tokens[i] != *pt {
            return Err(format!(
                "subject '{subject}' does not start with prefix '{prefix}'"
            ));
        }
    }

    let category = tokens[prefix_tokens.len()];
    if category.is_empty() {
        return Err(format!("empty category token in subject '{subject}'"));
    }
    if matches!(category, "*" | ">") {
        return Err(format!(
            "wildcard token '{category}' cannot be published to (subject '{subject}')"
        ));
    }

    Ok(SubjectRoute {
        topic: sanitize_name(category),
    })
}

/// Resolve a subscription filter to the topics it must poll.
///
/// Same routing rule as [`parse_subject`]: the token after the prefix picks
/// the topic, unless it is a wildcard (`*` or `>`), in which case the filter
/// spans every topic and matching happens client-side. A bare `>` matches
/// the whole stream.
pub fn resolve_filter(filter_subject: &str, prefix: &str) -> Result<FilterRoute, String> {
    let prefix_tokens: Vec<&str> = prefix.split('.').filter(|t| !t.is_empty()).collect();
    let tokens: Vec<&str> = filter_subject.split('.').collect();

    if prefix_tokens.is_empty() {
        return Err(format!("empty subject prefix '{prefix}'"));
    }
    if tokens.is_empty() {
        return Err(format!("empty filter '{filter_subject}'"));
    }

    // Bare ">" (or a prefix shorter than the filter grammar) matches all.
    if tokens == vec![">"] {
        return Ok(FilterRoute::AllTopics);
    }

    if tokens.len() < prefix_tokens.len() {
        // e.g. filter ">" handled above; anything shorter than the prefix
        // cannot name a category — treat as all-topics catch-all only when
        // it is a trailing ">" on the prefix itself, else reject.
        if tokens.last() == Some(&">")
            && tokens[..tokens.len() - 1] == prefix_tokens[..tokens.len() - 1]
        {
            return Ok(FilterRoute::AllTopics);
        }
        return Err(format!(
            "filter '{filter_subject}' is shorter than prefix '{prefix}'"
        ));
    }

    for (i, pt) in prefix_tokens.iter().enumerate() {
        if tokens[i] != *pt {
            return Err(format!(
                "filter '{filter_subject}' does not start with prefix '{prefix}'"
            ));
        }
    }

    let category = tokens[prefix_tokens.len()];
    if matches!(category, "*" | ">") {
        return Ok(FilterRoute::AllTopics);
    }

    Ok(FilterRoute::Single {
        topic: sanitize_name(category),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_name_passes_valid_chars() {
        assert_eq!(sanitize_name("market"), "market");
        assert_eq!(sanitize_name("a3s-cloud_events"), "a3s-cloud_events");
        assert_eq!(sanitize_name("Node-1"), "Node-1");
    }

    #[test]
    fn test_sanitize_name_replaces_invalid_chars() {
        assert_eq!(sanitize_name("cloud.events"), "cloud_events");
        assert_eq!(sanitize_name("usd/cny rate"), "usd_cny_rate");
        assert_eq!(sanitize_name("a.b*c"), "a_b_c");
    }

    #[test]
    fn test_sanitize_name_caps_length() {
        let long = "x".repeat(500);
        let sanitized = sanitize_name(&long);
        assert_eq!(sanitized.len(), MAX_NAME_LEN);
    }

    #[test]
    fn test_sanitize_name_empty_stays_empty() {
        assert_eq!(sanitize_name(""), "");
    }

    #[test]
    fn test_parse_subject_routes_category_to_topic() {
        let route = parse_subject("events.market.forex.usd_cny", "events").unwrap();
        assert_eq!(route.topic, "market");
    }

    #[test]
    fn test_parse_subject_single_token_category() {
        let route = parse_subject("events.system.deploy", "events").unwrap();
        assert_eq!(route.topic, "system");
    }

    #[test]
    fn test_parse_subject_multi_token_prefix() {
        let route = parse_subject("a3s.events.market.forex", "a3s.events").unwrap();
        assert_eq!(route.topic, "market");
    }

    #[test]
    fn test_parse_subject_wrong_prefix_fails() {
        let err = parse_subject("other.market.forex", "events").unwrap_err();
        assert!(err.contains("does not start with"));
    }

    #[test]
    fn test_parse_subject_too_short_fails() {
        assert!(parse_subject("events", "events").is_err());
        assert!(parse_subject("events.", "events").is_err());
    }

    #[test]
    fn test_parse_subject_wildcard_publish_fails() {
        assert!(parse_subject("events.>.x", "events").is_err());
        assert!(parse_subject("events.*.x", "events").is_err());
    }

    #[test]
    fn test_resolve_filter_single_topic() {
        let route = resolve_filter("events.market.>", "events").unwrap();
        assert_eq!(
            route,
            FilterRoute::Single {
                topic: "market".to_string()
            }
        );

        let route = resolve_filter("events.market.forex", "events").unwrap();
        assert_eq!(
            route,
            FilterRoute::Single {
                topic: "market".to_string()
            }
        );

        let route = resolve_filter("events.market.*.rate", "events").unwrap();
        assert_eq!(
            route,
            FilterRoute::Single {
                topic: "market".to_string()
            }
        );
    }

    #[test]
    fn test_resolve_filter_all_topics() {
        assert_eq!(
            resolve_filter("events.>", "events").unwrap(),
            FilterRoute::AllTopics
        );
        assert_eq!(
            resolve_filter("events.*", "events").unwrap(),
            FilterRoute::AllTopics
        );
        assert_eq!(
            resolve_filter(">", "events").unwrap(),
            FilterRoute::AllTopics
        );
        assert_eq!(
            resolve_filter("events.*.forex", "events").unwrap(),
            FilterRoute::AllTopics
        );
    }

    #[test]
    fn test_resolve_filter_wrong_prefix_fails() {
        let err = resolve_filter("queues.work.>", "events").unwrap_err();
        assert!(err.contains("does not start with"));
    }

    #[test]
    fn test_resolve_and_publish_agree() {
        // The publish route for a subject must always land inside the topics
        // its category filter resolves to.
        let subject = "events.cloud.workload.deployment.failed";
        let pub_route = parse_subject(subject, "events").unwrap();
        let sub_route = resolve_filter("events.cloud.>", "events").unwrap();
        assert_eq!(
            sub_route,
            FilterRoute::Single {
                topic: pub_route.topic
            }
        );
    }
}
