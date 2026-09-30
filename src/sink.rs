//! Event sink — delivery targets for event routing
//!
//! `EventSink` defines where events are delivered. Implementations include
//! publishing to a topic, calling an in-process handler, or logging for
//! debugging. Used by the Broker/Trigger pattern for event routing.

use crate::error::{EventError, Result};
use crate::provider::EventProvider;
use crate::types::{BoxFuture, Event};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Trait for event delivery targets
///
/// Sinks receive events from the Broker when a Trigger's filter matches.
/// Implementations decide how to deliver the event — publish to a topic,
/// call a handler, log it, etc.
#[async_trait]
pub trait EventSink: Send + Sync {
    /// Deliver an event to this sink
    async fn deliver(&self, event: &Event) -> Result<()>;

    /// Human-readable sink name for logging
    fn name(&self) -> &str;
}

/// Sink that publishes events to an EventProvider topic
///
/// Re-publishes matched events to the underlying provider, enabling
/// event forwarding and fan-out patterns.
pub struct TopicSink {
    provider: Arc<dyn EventProvider>,
    name: String,
}

impl TopicSink {
    /// Create a new topic sink backed by a provider
    pub fn new(name: impl Into<String>, provider: Arc<dyn EventProvider>) -> Self {
        Self {
            provider,
            name: name.into(),
        }
    }
}

#[async_trait]
impl EventSink for TopicSink {
    async fn deliver(&self, event: &Event) -> Result<()> {
        self.provider.publish(event).await?;
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// Type alias for the async handler function used by InProcessSink
type HandlerFn = dyn Fn(Event) -> BoxFuture<'static, Result<()>> + Send + Sync;

/// Sink that calls an in-process async handler
///
/// Useful for direct event processing without going through a provider.
pub struct InProcessSink {
    handler: Arc<HandlerFn>,
    name: String,
}

impl InProcessSink {
    /// Create a new in-process sink with an async handler
    pub fn new<F, Fut>(name: impl Into<String>, handler: F) -> Self
    where
        F: Fn(Event) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = Result<()>> + Send + 'static,
    {
        let handler = Arc::new(move |event: Event| -> BoxFuture<'static, Result<()>> {
            Box::pin(handler(event))
        }) as Arc<HandlerFn>;

        Self {
            handler,
            name: name.into(),
        }
    }
}

#[async_trait]
impl EventSink for InProcessSink {
    async fn deliver(&self, event: &Event) -> Result<()> {
        (self.handler)(event.clone()).await
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// Sink that logs events via tracing (for debugging)
///
/// Does not perform any delivery — just logs the event at info level.
pub struct LogSink {
    name: String,
}

impl LogSink {
    /// Create a new log sink
    pub fn new(name: impl Into<String>) -> Self {
        Self { name: name.into() }
    }
}

impl Default for LogSink {
    fn default() -> Self {
        Self::new("log-sink")
    }
}

#[async_trait]
impl EventSink for LogSink {
    async fn deliver(&self, event: &Event) -> Result<()> {
        tracing::info!(
            sink = %self.name,
            event_id = %event.id,
            subject = %event.subject,
            event_type = %event.event_type,
            "Event delivered to log sink"
        );
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// Sink that collects events in memory (for testing)
pub struct CollectorSink {
    events: Arc<Mutex<Vec<Event>>>,
    name: String,
}

impl CollectorSink {
    /// Create a new collector sink
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            events: Arc::new(Mutex::new(Vec::new())),
            name: name.into(),
        }
    }

    /// Get collected events
    pub async fn events(&self) -> Vec<Event> {
        self.events.lock().await.clone()
    }

    /// Get count of collected events
    pub async fn count(&self) -> usize {
        self.events.lock().await.len()
    }
}

#[async_trait]
impl EventSink for CollectorSink {
    async fn deliver(&self, event: &Event) -> Result<()> {
        self.events.lock().await.push(event.clone());
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// Sink that always fails delivery (for testing error paths)
pub struct FailingSink {
    name: String,
    reason: String,
}

impl FailingSink {
    /// Create a new failing sink
    pub fn new(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            reason: reason.into(),
        }
    }
}

#[async_trait]
impl EventSink for FailingSink {
    async fn deliver(&self, _event: &Event) -> Result<()> {
        Err(EventError::SinkDelivery {
            sink: self.name.clone(),
            reason: self.reason.clone(),
        })
    }

    fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::memory::MemoryProvider;

    fn test_event() -> Event {
        Event::new(
            "events.test.a",
            "test",
            "Test event",
            "test-src",
            serde_json::json!({"key": "value"}),
        )
    }

    #[tokio::test]
    async fn test_topic_sink_delivers() {
        let provider = Arc::new(MemoryProvider::default());
        let sink = TopicSink::new("test-topic-sink", provider.clone());

        assert_eq!(sink.name(), "test-topic-sink");

        let event = test_event();
        sink.deliver(&event).await.unwrap();

        let history = provider.history(None, 10).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, event.id);
    }

    #[tokio::test]
    async fn test_in_process_sink_calls_handler() {
        let received = Arc::new(Mutex::new(Vec::new()));
        let received_clone = received.clone();

        let sink = InProcessSink::new("test-handler", move |event: Event| {
            let received = received_clone.clone();
            async move {
                received.lock().await.push(event);
                Ok(())
            }
        });

        assert_eq!(sink.name(), "test-handler");

        let event = test_event();
        sink.deliver(&event).await.unwrap();

        let events = received.lock().await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, event.id);
    }

    #[tokio::test]
    async fn test_log_sink_succeeds() {
        let sink = LogSink::default();
        assert_eq!(sink.name(), "log-sink");

        let event = test_event();
        // Should not error
        sink.deliver(&event).await.unwrap();
    }

    #[tokio::test]
    async fn test_log_sink_custom_name() {
        let sink = LogSink::new("debug-sink");
        assert_eq!(sink.name(), "debug-sink");
    }

    #[tokio::test]
    async fn test_collector_sink() {
        let sink = CollectorSink::new("collector");
        assert_eq!(sink.name(), "collector");
        assert_eq!(sink.count().await, 0);

        let e1 = test_event();
        let e2 = Event::new("events.test.b", "test", "B", "src", serde_json::json!({}));

        sink.deliver(&e1).await.unwrap();
        sink.deliver(&e2).await.unwrap();

        assert_eq!(sink.count().await, 2);
        let events = sink.events().await;
        assert_eq!(events[0].id, e1.id);
        assert_eq!(events[1].id, e2.id);
    }

    #[tokio::test]
    async fn test_failing_sink_returns_error() {
        let sink = FailingSink::new("bad-sink", "connection refused");
        assert_eq!(sink.name(), "bad-sink");

        let event = test_event();
        let err = sink.deliver(&event).await.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("bad-sink"));
        assert!(msg.contains("connection refused"));
    }

    #[tokio::test]
    async fn test_in_process_sink_error_propagation() {
        let sink = InProcessSink::new("err-handler", |_event: Event| async {
            Err(EventError::SinkDelivery {
                sink: "err-handler".to_string(),
                reason: "processing failed".to_string(),
            })
        });

        let event = test_event();
        assert!(sink.deliver(&event).await.is_err());
    }

    #[tokio::test]
    async fn test_dyn_event_sink_trait_object() {
        let sinks: Vec<Box<dyn EventSink>> = vec![
            Box::new(LogSink::default()),
            Box::new(CollectorSink::new("collector")),
        ];

        let event = test_event();
        for sink in &sinks {
            sink.deliver(&event).await.unwrap();
        }

        assert_eq!(sinks.len(), 2);
    }
}
