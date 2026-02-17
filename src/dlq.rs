//! Dead Letter Queue — handle events that exceed max delivery attempts
//!
//! Provides a `DlqHandler` trait for routing failed events. This is an
//! application-level concern — providers handle retry/backoff natively,
//! but DLQ routing lives above the provider layer.

use crate::error::Result;
use crate::sink::EventSink;
use crate::types::{Event, ReceivedEvent};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::RwLock;

/// A failed event with context about why it ended up in the DLQ
#[derive(Debug, Clone)]
pub struct DeadLetterEvent {
    /// The original received event
    pub event: ReceivedEvent,

    /// Reason the event was sent to DLQ
    pub reason: String,

    /// Unix timestamp in milliseconds when the event was dead-lettered
    pub dead_lettered_at: u64,

    /// Original subject the event was published to (for routing context)
    pub original_subject: Option<String>,

    /// Number of delivery attempts before dead-lettering
    pub delivery_attempts: Option<u64>,

    /// Unix timestamp in milliseconds of the first delivery failure
    pub first_failure_at: Option<u64>,
}

/// Trait for dead letter queue handlers
///
/// Implementations decide what to do with events that exceed
/// max delivery attempts or fail processing permanently.
#[async_trait]
pub trait DlqHandler: Send + Sync {
    /// Handle a dead-lettered event
    ///
    /// Called when an event exceeds max delivery attempts or is
    /// explicitly rejected. Implementations may log, store, forward,
    /// or alert on the failed event.
    async fn handle(&self, event: DeadLetterEvent) -> Result<()>;

    /// Get the number of events currently in the DLQ
    async fn count(&self) -> Result<usize>;

    /// List recent dead-lettered events
    async fn list(&self, limit: usize) -> Result<Vec<DeadLetterEvent>>;
}

/// In-memory DLQ handler for development and testing
///
/// Stores dead-lettered events in a `Vec` with configurable max capacity.
pub struct MemoryDlqHandler {
    events: Arc<RwLock<Vec<DeadLetterEvent>>>,
    max_events: usize,
}

impl MemoryDlqHandler {
    /// Create a new in-memory DLQ handler
    pub fn new(max_events: usize) -> Self {
        Self {
            events: Arc::new(RwLock::new(Vec::new())),
            max_events,
        }
    }
}

impl Default for MemoryDlqHandler {
    fn default() -> Self {
        Self::new(10_000)
    }
}

#[async_trait]
impl DlqHandler for MemoryDlqHandler {
    async fn handle(&self, event: DeadLetterEvent) -> Result<()> {
        tracing::warn!(
            event_id = %event.event.event.id,
            subject = %event.event.event.subject,
            num_delivered = event.event.num_delivered,
            reason = %event.reason,
            "Event dead-lettered"
        );

        let mut events = self.events.write().await;
        events.push(event);

        // Enforce max capacity
        if self.max_events > 0 && events.len() > self.max_events {
            let drain_count = events.len() - self.max_events;
            events.drain(..drain_count);
        }

        Ok(())
    }

    async fn count(&self) -> Result<usize> {
        let events = self.events.read().await;
        Ok(events.len())
    }

    async fn list(&self, limit: usize) -> Result<Vec<DeadLetterEvent>> {
        let events = self.events.read().await;
        let result: Vec<DeadLetterEvent> = events.iter().rev().take(limit).cloned().collect();
        Ok(result)
    }
}

/// Check if a received event should be dead-lettered based on max delivery count
pub fn should_dead_letter(event: &ReceivedEvent, max_deliver: u64) -> bool {
    max_deliver > 0 && event.num_delivered >= max_deliver
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl DeadLetterEvent {
    /// Create a new dead letter event
    pub fn new(event: ReceivedEvent, reason: impl Into<String>) -> Self {
        Self {
            event,
            reason: reason.into(),
            dead_lettered_at: now_millis(),
            original_subject: None,
            delivery_attempts: None,
            first_failure_at: None,
        }
    }

    /// Set the original subject
    pub fn with_original_subject(mut self, subject: impl Into<String>) -> Self {
        self.original_subject = Some(subject.into());
        self
    }

    /// Set the number of delivery attempts
    pub fn with_delivery_attempts(mut self, attempts: u64) -> Self {
        self.delivery_attempts = Some(attempts);
        self
    }

    /// Set the timestamp of the first failure
    pub fn with_first_failure_at(mut self, timestamp: u64) -> Self {
        self.first_failure_at = Some(timestamp);
        self
    }
}

/// DLQ handler that forwards dead-lettered events to an EventSink
///
/// Wraps a dead-lettered event as a new `Event` with DLQ metadata
/// and delivers it through the configured sink. Useful for routing
/// failed events to external systems (logging, alerting, reprocessing).
pub struct SinkDlqHandler {
    sink: Arc<dyn EventSink>,
    events: Arc<RwLock<Vec<DeadLetterEvent>>>,
    max_events: usize,
}

impl SinkDlqHandler {
    /// Create a new sink-based DLQ handler
    pub fn new(sink: Arc<dyn EventSink>, max_events: usize) -> Self {
        Self {
            sink,
            events: Arc::new(RwLock::new(Vec::new())),
            max_events,
        }
    }

    /// Convert a dead-lettered event into a regular Event with DLQ metadata
    fn to_dlq_event(dle: &DeadLetterEvent) -> Event {
        let mut event = Event::typed(
            format!("events.dlq.{}", dle.event.event.subject),
            "dlq",
            "a3s.dlq.dead_letter",
            1,
            format!("Dead letter: {}", dle.reason),
            "dlq-handler",
            dle.event.event.payload.clone(),
        )
        .with_metadata("dlq_reason", &dle.reason)
        .with_metadata("dlq_original_id", &dle.event.event.id)
        .with_metadata(
            "dlq_dead_lettered_at",
            dle.dead_lettered_at.to_string(),
        );

        if let Some(ref subj) = dle.original_subject {
            event = event.with_metadata("dlq_original_subject", subj);
        }
        if let Some(attempts) = dle.delivery_attempts {
            event = event.with_metadata("dlq_delivery_attempts", attempts.to_string());
        }
        if let Some(first_fail) = dle.first_failure_at {
            event = event.with_metadata("dlq_first_failure_at", first_fail.to_string());
        }

        event
    }
}

#[async_trait]
impl DlqHandler for SinkDlqHandler {
    async fn handle(&self, event: DeadLetterEvent) -> Result<()> {
        // Forward to sink as a regular event
        let dlq_event = Self::to_dlq_event(&event);
        self.sink.deliver(&dlq_event).await?;

        // Also store locally for listing
        let mut events = self.events.write().await;
        events.push(event);

        if self.max_events > 0 && events.len() > self.max_events {
            let drain_count = events.len() - self.max_events;
            events.drain(..drain_count);
        }

        Ok(())
    }

    async fn count(&self) -> Result<usize> {
        let events = self.events.read().await;
        Ok(events.len())
    }

    async fn list(&self, limit: usize) -> Result<Vec<DeadLetterEvent>> {
        let events = self.events.read().await;
        let result: Vec<DeadLetterEvent> = events.iter().rev().take(limit).cloned().collect();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_received_event(num_delivered: u64) -> ReceivedEvent {
        ReceivedEvent {
            event: Event::new(
                "events.test.a",
                "test",
                "Test event",
                "test",
                serde_json::json!({}),
            ),
            sequence: 1,
            num_delivered,
            stream: "test".to_string(),
        }
    }

    #[test]
    fn test_should_dead_letter() {
        assert!(!should_dead_letter(&test_received_event(1), 5));
        assert!(!should_dead_letter(&test_received_event(4), 5));
        assert!(should_dead_letter(&test_received_event(5), 5));
        assert!(should_dead_letter(&test_received_event(10), 5));
    }

    #[test]
    fn test_should_dead_letter_zero_max() {
        // max_deliver=0 means unlimited
        assert!(!should_dead_letter(&test_received_event(100), 0));
    }

    #[test]
    fn test_dead_letter_event_creation() {
        let received = test_received_event(5);
        let dle = DeadLetterEvent::new(received.clone(), "Max retries exceeded");
        assert_eq!(dle.reason, "Max retries exceeded");
        assert_eq!(dle.event.event.id, received.event.id);
        assert!(dle.dead_lettered_at > 0);
    }

    #[tokio::test]
    async fn test_memory_dlq_handle_and_count() {
        let dlq = MemoryDlqHandler::default();
        assert_eq!(dlq.count().await.unwrap(), 0);

        let dle = DeadLetterEvent::new(test_received_event(5), "failed");
        dlq.handle(dle).await.unwrap();

        assert_eq!(dlq.count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_memory_dlq_list() {
        let dlq = MemoryDlqHandler::default();

        for i in 0..5 {
            let mut received = test_received_event(3);
            received.sequence = i;
            let dle = DeadLetterEvent::new(received, format!("reason {}", i));
            dlq.handle(dle).await.unwrap();
        }

        let list = dlq.list(3).await.unwrap();
        assert_eq!(list.len(), 3);
        // Most recent first
        assert_eq!(list[0].reason, "reason 4");
        assert_eq!(list[2].reason, "reason 2");
    }

    #[tokio::test]
    async fn test_memory_dlq_max_capacity() {
        let dlq = MemoryDlqHandler::new(3);

        for i in 0..5 {
            let dle = DeadLetterEvent::new(test_received_event(1), format!("reason {}", i));
            dlq.handle(dle).await.unwrap();
        }

        assert_eq!(dlq.count().await.unwrap(), 3);
        let list = dlq.list(10).await.unwrap();
        // Oldest events drained
        assert_eq!(list[0].reason, "reason 4");
        assert_eq!(list[2].reason, "reason 2");
    }

    #[test]
    fn test_dead_letter_event_builder_methods() {
        let received = test_received_event(5);
        let dle = DeadLetterEvent::new(received, "timeout")
            .with_original_subject("events.payment.process")
            .with_delivery_attempts(5)
            .with_first_failure_at(1700000000000);

        assert_eq!(dle.original_subject.as_deref(), Some("events.payment.process"));
        assert_eq!(dle.delivery_attempts, Some(5));
        assert_eq!(dle.first_failure_at, Some(1700000000000));
    }

    #[test]
    fn test_dead_letter_event_optional_fields_default_none() {
        let received = test_received_event(3);
        let dle = DeadLetterEvent::new(received, "failed");

        assert!(dle.original_subject.is_none());
        assert!(dle.delivery_attempts.is_none());
        assert!(dle.first_failure_at.is_none());
    }

    #[tokio::test]
    async fn test_sink_dlq_handler() {
        use crate::sink::CollectorSink;

        let collector = Arc::new(CollectorSink::new("dlq-collector"));
        let dlq = SinkDlqHandler::new(collector.clone(), 100);

        let received = test_received_event(5);
        let dle = DeadLetterEvent::new(received, "processing error")
            .with_original_subject("events.order.process")
            .with_delivery_attempts(5);

        dlq.handle(dle).await.unwrap();

        assert_eq!(dlq.count().await.unwrap(), 1);

        // Verify event was forwarded to sink
        let events = collector.events().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, "a3s.dlq.dead_letter");
        assert_eq!(events[0].category, "dlq");
        assert_eq!(events[0].metadata["dlq_reason"], "processing error");
        assert_eq!(
            events[0].metadata["dlq_original_subject"],
            "events.order.process"
        );
        assert_eq!(events[0].metadata["dlq_delivery_attempts"], "5");
    }

    #[tokio::test]
    async fn test_sink_dlq_handler_list() {
        use crate::sink::CollectorSink;

        let collector = Arc::new(CollectorSink::new("dlq-collector"));
        let dlq = SinkDlqHandler::new(collector, 100);

        for i in 0..3 {
            let dle = DeadLetterEvent::new(
                test_received_event(1),
                format!("error {}", i),
            );
            dlq.handle(dle).await.unwrap();
        }

        assert_eq!(dlq.count().await.unwrap(), 3);
        let list = dlq.list(2).await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].reason, "error 2");
    }

    #[tokio::test]
    async fn test_sink_dlq_handler_max_capacity() {
        use crate::sink::CollectorSink;

        let collector = Arc::new(CollectorSink::new("dlq-collector"));
        let dlq = SinkDlqHandler::new(collector, 2);

        for i in 0..5 {
            let dle = DeadLetterEvent::new(
                test_received_event(1),
                format!("error {}", i),
            );
            dlq.handle(dle).await.unwrap();
        }

        assert_eq!(dlq.count().await.unwrap(), 2);
    }
}
