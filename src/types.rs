//! Core event types for the a3s-event system
//!
//! All types use camelCase JSON serialization for wire compatibility.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A single event in the system
///
/// Events are published to subjects following the dot-separated convention:
/// `events.<category>.<topic>` (e.g., `events.market.forex.usd_cny`)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    /// Unique event identifier (evt-<uuid>)
    pub id: String,

    /// Subject this event was published to
    pub subject: String,

    /// Top-level category for grouping (e.g., "market", "system")
    pub category: String,

    /// Event payload — arbitrary JSON data
    pub payload: serde_json::Value,

    /// Human-readable summary
    pub summary: String,

    /// Source system or service that produced this event
    pub source: String,

    /// Unix timestamp in milliseconds
    pub timestamp: u64,

    /// Optional key-value metadata
    #[serde(default)]
    pub metadata: HashMap<String, String>,
}

impl Event {
    /// Create a new event with auto-generated id and timestamp
    pub fn new(
        subject: impl Into<String>,
        category: impl Into<String>,
        summary: impl Into<String>,
        source: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            id: format!("evt-{}", uuid::Uuid::new_v4()),
            subject: subject.into(),
            category: category.into(),
            payload,
            summary: summary.into(),
            source: source.into(),
            timestamp: now_millis(),
            metadata: HashMap::new(),
        }
    }

    /// Add a metadata entry
    pub fn with_metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }
}

/// A received event with delivery context
#[derive(Debug, Clone)]
pub struct ReceivedEvent {
    /// The event data
    pub event: Event,

    /// Provider-assigned sequence number
    pub sequence: u64,

    /// Number of delivery attempts
    pub num_delivered: u64,

    /// Stream/topic name
    pub stream: String,
}

/// Subscription filter for creating consumers
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubscriptionFilter {
    /// Subscriber identifier (e.g., persona id)
    pub subscriber_id: String,

    /// Subject filter patterns (e.g., ["events.market.>", "events.system.>"])
    pub subjects: Vec<String>,

    /// Whether this is a durable subscription (survives reconnects)
    pub durable: bool,
}

/// Event counts grouped by category
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventCounts {
    /// Counts per category
    pub categories: HashMap<String, u64>,

    /// Total event count
    pub total: u64,
}

/// Current time in Unix milliseconds
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_creation() {
        let event = Event::new(
            "events.market.forex",
            "market",
            "USD/CNY rate change",
            "reuters",
            serde_json::json!({"rate": 7.35}),
        );

        assert!(event.id.starts_with("evt-"));
        assert_eq!(event.subject, "events.market.forex");
        assert_eq!(event.category, "market");
        assert_eq!(event.source, "reuters");
        assert!(event.timestamp > 0);
        assert!(event.metadata.is_empty());
    }

    #[test]
    fn test_event_with_metadata() {
        let event = Event::new(
            "events.system.deploy",
            "system",
            "Deployed v1.2",
            "ci",
            serde_json::json!({}),
        )
        .with_metadata("env", "production")
        .with_metadata("version", "1.2.0");

        assert_eq!(event.metadata.len(), 2);
        assert_eq!(event.metadata["env"], "production");
        assert_eq!(event.metadata["version"], "1.2.0");
    }

    #[test]
    fn test_event_serialization_roundtrip() {
        let event = Event::new(
            "events.market.forex",
            "market",
            "Rate change",
            "reuters",
            serde_json::json!({"rate": 7.35}),
        )
        .with_metadata("region", "asia");

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"subject\":\"events.market.forex\""));
        assert!(json.contains("\"category\":\"market\""));

        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.id, event.id);
        assert_eq!(parsed.subject, event.subject);
        assert_eq!(parsed.metadata["region"], "asia");
    }

    #[test]
    fn test_event_counts_default() {
        let counts = EventCounts::default();
        assert_eq!(counts.total, 0);
        assert!(counts.categories.is_empty());
    }

    #[test]
    fn test_subscription_filter_serialization() {
        let filter = SubscriptionFilter {
            subscriber_id: "financial-analyst".to_string(),
            subjects: vec!["events.market.>".to_string()],
            durable: true,
        };

        let json = serde_json::to_string(&filter).unwrap();
        assert!(json.contains("\"subscriberId\":\"financial-analyst\""));
        assert!(json.contains("\"durable\":true"));

        let parsed: SubscriptionFilter = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.subscriber_id, "financial-analyst");
        assert!(parsed.durable);
    }
}
