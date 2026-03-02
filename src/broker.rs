//! Broker/Trigger event routing
//!
//! Implements the Knative-inspired Broker/Trigger pattern:
//! - Publishers emit events to a **Broker**
//! - **Triggers** filter events by type, source, subject pattern, and metadata
//! - Matching events are delivered to the Trigger's **EventSink** in parallel
//!
//! Delivery is fire-and-forget — errors are logged but do not break the
//! publish path.

use crate::sink::EventSink;
use crate::subject::subject_matches;
use crate::types::Event;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Filter criteria for a Trigger
///
/// All non-None fields must match (AND logic). A filter with all fields
/// set to None matches every event.
#[derive(Debug, Clone, Default)]
pub struct TriggerFilter {
    /// Match events with this exact event_type
    pub event_type: Option<String>,

    /// Match events from this source
    pub source: Option<String>,

    /// Match events whose subject matches this pattern (supports `>` and `*` wildcards)
    pub subject_pattern: Option<String>,

    /// Match events that contain all of these metadata key-value pairs
    pub attributes: Vec<(String, String)>,
}

impl TriggerFilter {
    /// Create a filter matching a specific event type
    pub fn by_type(event_type: impl Into<String>) -> Self {
        Self {
            event_type: Some(event_type.into()),
            ..Default::default()
        }
    }

    /// Create a filter matching a specific source
    pub fn by_source(source: impl Into<String>) -> Self {
        Self {
            source: Some(source.into()),
            ..Default::default()
        }
    }

    /// Create a filter matching a subject pattern
    pub fn by_subject(pattern: impl Into<String>) -> Self {
        Self {
            subject_pattern: Some(pattern.into()),
            ..Default::default()
        }
    }

    /// Add a required metadata attribute
    pub fn with_attribute(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.attributes.push((key.into(), value.into()));
        self
    }

    /// Check if an event matches this filter
    pub fn matches(&self, event: &Event) -> bool {
        // Check event_type
        if let Some(ref et) = self.event_type {
            if event.event_type != *et {
                return false;
            }
        }

        // Check source
        if let Some(ref src) = self.source {
            if event.source != *src {
                return false;
            }
        }

        // Check subject pattern
        if let Some(ref pattern) = self.subject_pattern {
            if !subject_matches(&event.subject, pattern) {
                return false;
            }
        }

        // Check metadata attributes (AND logic)
        for (key, value) in &self.attributes {
            match event.metadata.get(key) {
                Some(v) if v == value => {}
                _ => return false,
            }
        }

        true
    }
}

/// A Trigger pairs a filter with a delivery sink
pub struct Trigger {
    /// Trigger name for identification and logging
    pub name: String,

    /// Filter criteria — events must match to be delivered
    pub filter: TriggerFilter,

    /// Delivery target for matching events
    pub sink: Arc<dyn EventSink>,
}

impl Trigger {
    /// Create a new trigger
    pub fn new(name: impl Into<String>, filter: TriggerFilter, sink: Arc<dyn EventSink>) -> Self {
        Self {
            name: name.into(),
            filter,
            sink,
        }
    }
}

/// Event broker — receives events and routes them through matching triggers
///
/// The Broker evaluates all registered triggers against each incoming event.
/// Matching events are delivered to their triggers' sinks in parallel.
/// Delivery errors are logged but do not propagate — the broker is
/// fire-and-forget to avoid blocking publishers.
pub struct Broker {
    triggers: Arc<RwLock<Vec<Trigger>>>,
}

impl Broker {
    /// Create an empty broker
    pub fn new() -> Self {
        Self {
            triggers: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Add a trigger to the broker
    pub async fn add_trigger(&self, trigger: Trigger) {
        self.triggers.write().await.push(trigger);
    }

    /// Remove a trigger by name, returns true if found
    pub async fn remove_trigger(&self, name: &str) -> bool {
        let mut triggers = self.triggers.write().await;
        let len_before = triggers.len();
        triggers.retain(|t| t.name != name);
        triggers.len() < len_before
    }

    /// Get the number of registered triggers
    pub async fn trigger_count(&self) -> usize {
        self.triggers.read().await.len()
    }

    /// Route an event through all matching triggers
    ///
    /// Evaluates each trigger's filter. For matching triggers, delivers
    /// the event to the sink. All deliveries happen in parallel.
    /// Errors are logged but do not propagate.
    pub async fn route(&self, event: &Event) -> RouteResult {
        let triggers = self.triggers.read().await;
        let mut delivered = 0usize;
        let mut failed = 0usize;

        // Collect matching triggers and their sinks
        let matching: Vec<(&str, Arc<dyn EventSink>)> = triggers
            .iter()
            .filter(|t| t.filter.matches(event))
            .map(|t| (t.name.as_str(), t.sink.clone()))
            .collect();

        let matched = matching.len();

        if matching.is_empty() {
            return RouteResult {
                matched: 0,
                delivered: 0,
                failed: 0,
            };
        }

        // Deliver to all matching sinks in parallel
        let mut handles = Vec::with_capacity(matching.len());
        for (name, sink) in matching {
            let event = event.clone();
            let trigger_name = name.to_string();
            handles.push(tokio::spawn(async move {
                match sink.deliver(&event).await {
                    Ok(()) => {
                        tracing::debug!(
                            trigger = %trigger_name,
                            event_id = %event.id,
                            sink = %sink.name(),
                            "Event delivered via trigger"
                        );
                        true
                    }
                    Err(e) => {
                        tracing::warn!(
                            trigger = %trigger_name,
                            event_id = %event.id,
                            sink = %sink.name(),
                            error = %e,
                            "Trigger delivery failed"
                        );
                        false
                    }
                }
            }));
        }

        for handle in handles {
            match handle.await {
                Ok(true) => delivered += 1,
                Ok(false) => failed += 1,
                Err(e) => {
                    tracing::warn!(error = %e, "Trigger delivery task panicked");
                    failed += 1;
                }
            }
        }

        RouteResult {
            matched,
            delivered,
            failed,
        }
    }
}

impl Default for Broker {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of routing an event through the broker
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteResult {
    /// Number of triggers whose filter matched
    pub matched: usize,
    /// Number of successful deliveries
    pub delivered: usize,
    /// Number of failed deliveries
    pub failed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::{CollectorSink, FailingSink, LogSink};

    fn test_event(event_type: &str, source: &str, subject: &str) -> Event {
        Event::typed(
            subject,
            "test",
            event_type,
            1,
            "Test",
            source,
            serde_json::json!({}),
        )
    }

    // ─── TriggerFilter tests ─────────────────────────────────────

    #[test]
    fn test_filter_empty_matches_all() {
        let filter = TriggerFilter::default();
        let event = test_event("any.type", "any-src", "events.any.subject");
        assert!(filter.matches(&event));
    }

    #[test]
    fn test_filter_by_type() {
        let filter = TriggerFilter::by_type("a3s.gateway.scale.up");
        assert!(filter.matches(&test_event("a3s.gateway.scale.up", "gw", "events.scale.up")));
        assert!(!filter.matches(&test_event(
            "a3s.gateway.scale.down",
            "gw",
            "events.scale.down"
        )));
    }

    #[test]
    fn test_filter_by_source() {
        let filter = TriggerFilter::by_source("gateway");
        assert!(filter.matches(&test_event("any", "gateway", "events.a")));
        assert!(!filter.matches(&test_event("any", "box", "events.a")));
    }

    #[test]
    fn test_filter_by_subject_exact() {
        let filter = TriggerFilter::by_subject("events.market.forex");
        assert!(filter.matches(&test_event("t", "s", "events.market.forex")));
        assert!(!filter.matches(&test_event("t", "s", "events.market.crypto")));
    }

    #[test]
    fn test_filter_by_subject_wildcard() {
        let filter = TriggerFilter::by_subject("events.market.>");
        assert!(filter.matches(&test_event("t", "s", "events.market.forex")));
        assert!(filter.matches(&test_event("t", "s", "events.market.crypto.btc")));
        assert!(!filter.matches(&test_event("t", "s", "events.system.deploy")));
    }

    #[test]
    fn test_filter_by_subject_single_wildcard() {
        let filter = TriggerFilter::by_subject("events.*.forex");
        assert!(filter.matches(&test_event("t", "s", "events.market.forex")));
        assert!(!filter.matches(&test_event("t", "s", "events.market.crypto")));
    }

    #[test]
    fn test_filter_with_attributes() {
        let filter = TriggerFilter::default()
            .with_attribute("env", "prod")
            .with_attribute("region", "us-east");

        let event = test_event("t", "s", "events.a")
            .with_metadata("env", "prod")
            .with_metadata("region", "us-east");
        assert!(filter.matches(&event));

        let partial = test_event("t", "s", "events.a").with_metadata("env", "prod");
        assert!(!filter.matches(&partial));

        let wrong = test_event("t", "s", "events.a")
            .with_metadata("env", "staging")
            .with_metadata("region", "us-east");
        assert!(!filter.matches(&wrong));
    }

    #[test]
    fn test_filter_combined() {
        let filter = TriggerFilter {
            event_type: Some("scale.up".to_string()),
            source: Some("gateway".to_string()),
            subject_pattern: Some("events.scaling.>".to_string()),
            attributes: vec![("priority".to_string(), "high".to_string())],
        };

        let good = Event::typed(
            "events.scaling.web",
            "test",
            "scale.up",
            1,
            "Scale",
            "gateway",
            serde_json::json!({}),
        )
        .with_metadata("priority", "high");
        assert!(filter.matches(&good));

        // Wrong type
        let bad_type = Event::typed(
            "events.scaling.web",
            "test",
            "scale.down",
            1,
            "Scale",
            "gateway",
            serde_json::json!({}),
        )
        .with_metadata("priority", "high");
        assert!(!filter.matches(&bad_type));
    }

    // ─── Broker tests ────────────────────────────────────────────

    #[tokio::test]
    async fn test_broker_add_remove_triggers() {
        let broker = Broker::new();
        assert_eq!(broker.trigger_count().await, 0);

        let sink = Arc::new(LogSink::default());
        broker
            .add_trigger(Trigger::new("t1", TriggerFilter::default(), sink.clone()))
            .await;
        broker
            .add_trigger(Trigger::new("t2", TriggerFilter::by_type("x"), sink))
            .await;

        assert_eq!(broker.trigger_count().await, 2);

        assert!(broker.remove_trigger("t1").await);
        assert_eq!(broker.trigger_count().await, 1);

        assert!(!broker.remove_trigger("nonexistent").await);
    }

    #[tokio::test]
    async fn test_broker_route_to_matching_sink() {
        let broker = Broker::new();
        let collector = Arc::new(CollectorSink::new("matched"));

        broker
            .add_trigger(Trigger::new(
                "scale-trigger",
                TriggerFilter::by_type("a3s.gateway.scale.up"),
                collector.clone(),
            ))
            .await;

        // Matching event
        let event = test_event("a3s.gateway.scale.up", "gateway", "events.scaling.up");
        let result = broker.route(&event).await;
        assert_eq!(result.matched, 1);
        assert_eq!(result.delivered, 1);
        assert_eq!(result.failed, 0);
        assert_eq!(collector.count().await, 1);

        // Non-matching event
        let other = test_event("a3s.box.instance.ready", "box", "events.instance.ready");
        let result = broker.route(&other).await;
        assert_eq!(result.matched, 0);
        assert_eq!(result.delivered, 0);
        assert_eq!(collector.count().await, 1); // unchanged
    }

    #[tokio::test]
    async fn test_broker_route_multiple_triggers() {
        let broker = Broker::new();
        let sink1 = Arc::new(CollectorSink::new("sink1"));
        let sink2 = Arc::new(CollectorSink::new("sink2"));
        let sink3 = Arc::new(CollectorSink::new("sink3"));

        broker
            .add_trigger(Trigger::new(
                "t1",
                TriggerFilter::by_type("scale.up"),
                sink1.clone(),
            ))
            .await;
        broker
            .add_trigger(Trigger::new(
                "t2",
                TriggerFilter::by_source("gateway"),
                sink2.clone(),
            ))
            .await;
        broker
            .add_trigger(Trigger::new(
                "t3",
                TriggerFilter::by_type("scale.down"),
                sink3.clone(),
            ))
            .await;

        let event = test_event("scale.up", "gateway", "events.a");
        let result = broker.route(&event).await;

        // t1 and t2 match, t3 does not
        assert_eq!(result.matched, 2);
        assert_eq!(result.delivered, 2);
        assert_eq!(sink1.count().await, 1);
        assert_eq!(sink2.count().await, 1);
        assert_eq!(sink3.count().await, 0);
    }

    #[tokio::test]
    async fn test_broker_route_with_failing_sink() {
        let broker = Broker::new();
        let good_sink = Arc::new(CollectorSink::new("good"));
        let bad_sink = Arc::new(FailingSink::new("bad", "network error"));

        broker
            .add_trigger(Trigger::new(
                "good-trigger",
                TriggerFilter::default(),
                good_sink.clone(),
            ))
            .await;
        broker
            .add_trigger(Trigger::new(
                "bad-trigger",
                TriggerFilter::default(),
                bad_sink,
            ))
            .await;

        let event = test_event("any", "any", "events.a");
        let result = broker.route(&event).await;

        assert_eq!(result.matched, 2);
        assert_eq!(result.delivered, 1);
        assert_eq!(result.failed, 1);
        assert_eq!(good_sink.count().await, 1); // good sink still delivered
    }

    #[tokio::test]
    async fn test_broker_route_no_triggers() {
        let broker = Broker::new();
        let event = test_event("any", "any", "events.a");
        let result = broker.route(&event).await;

        assert_eq!(result.matched, 0);
        assert_eq!(result.delivered, 0);
        assert_eq!(result.failed, 0);
    }

    #[tokio::test]
    async fn test_broker_default() {
        let broker = Broker::default();
        assert_eq!(broker.trigger_count().await, 0);
    }

    #[tokio::test]
    async fn test_route_result_equality() {
        let a = RouteResult {
            matched: 2,
            delivered: 1,
            failed: 1,
        };
        let b = RouteResult {
            matched: 2,
            delivered: 1,
            failed: 1,
        };
        assert_eq!(a, b);
    }
}
