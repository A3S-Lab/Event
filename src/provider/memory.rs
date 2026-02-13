//! In-memory event provider for testing and lightweight usage
//!
//! Stores events in memory with no external dependencies.
//! Events are lost on process restart.

use crate::error::Result;
use crate::provider::{EventProvider, PendingEvent, ProviderInfo, Subscription};
use crate::types::{Event, ReceivedEvent};
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

/// In-memory event provider configuration
#[derive(Debug, Clone)]
pub struct MemoryConfig {
    /// Subject prefix for events (default: "events")
    pub subject_prefix: String,
    /// Maximum events to retain (0 = unlimited)
    pub max_events: usize,
    /// Broadcast channel capacity
    pub channel_capacity: usize,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            subject_prefix: "events".to_string(),
            max_events: 100_000,
            channel_capacity: 10_000,
        }
    }
}

/// In-memory event provider
///
/// Uses `tokio::sync::broadcast` for pub/sub and a `Vec` for persistence.
/// Suitable for testing, development, and single-process deployments.
pub struct MemoryProvider {
    config: MemoryConfig,
    events: Arc<RwLock<Vec<Event>>>,
    sender: broadcast::Sender<Event>,
    sequence: Arc<std::sync::atomic::AtomicU64>,
}

impl MemoryProvider {
    /// Create a new in-memory provider
    pub fn new(config: MemoryConfig) -> Self {
        let (sender, _) = broadcast::channel(config.channel_capacity);
        Self {
            config,
            events: Arc::new(RwLock::new(Vec::new())),
            sender,
            sequence: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }
}

impl Default for MemoryProvider {
    fn default() -> Self {
        Self::new(MemoryConfig::default())
    }
}

#[async_trait]
impl EventProvider for MemoryProvider {
    async fn publish(&self, event: &Event) -> Result<u64> {
        let seq = self
            .sequence
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        {
            let mut events = self.events.write().await;
            events.push(event.clone());

            // Enforce max_events limit
            if self.config.max_events > 0 && events.len() > self.config.max_events {
                let drain_count = events.len() - self.config.max_events;
                events.drain(..drain_count);
            }
        }

        // Broadcast to subscribers (ignore send errors — no receivers is fine)
        let _ = self.sender.send(event.clone());

        tracing::debug!(
            event_id = %event.id,
            subject = %event.subject,
            sequence = seq,
            "Event published (memory)"
        );

        Ok(seq)
    }

    async fn subscribe_durable(
        &self,
        _consumer_name: &str,
        filter_subject: &str,
    ) -> Result<Box<dyn Subscription>> {
        // In-memory provider treats durable same as ephemeral
        self.subscribe(filter_subject).await
    }

    async fn subscribe(
        &self,
        filter_subject: &str,
    ) -> Result<Box<dyn Subscription>> {
        let receiver = self.sender.subscribe();
        Ok(Box::new(MemorySubscription {
            receiver,
            filter: filter_subject.to_string(),
        }))
    }

    async fn history(
        &self,
        filter_subject: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Event>> {
        let events = self.events.read().await;
        let filtered: Vec<Event> = events
            .iter()
            .rev()
            .filter(|e| {
                if let Some(filter) = filter_subject {
                    subject_matches(&e.subject, filter)
                } else {
                    true
                }
            })
            .take(limit)
            .cloned()
            .collect();
        Ok(filtered)
    }

    async fn unsubscribe(&self, _consumer_name: &str) -> Result<()> {
        // No-op for in-memory provider
        Ok(())
    }

    async fn info(&self) -> Result<ProviderInfo> {
        let events = self.events.read().await;
        let bytes: u64 = events
            .iter()
            .map(|e| serde_json::to_vec(e).map(|v| v.len() as u64).unwrap_or(0))
            .sum();

        Ok(ProviderInfo {
            provider: "memory".to_string(),
            messages: events.len() as u64,
            bytes,
            consumers: self.sender.receiver_count(),
        })
    }

    fn build_subject(&self, category: &str, topic: &str) -> String {
        format!("{}.{}.{}", self.config.subject_prefix, category, topic)
    }

    fn category_subject(&self, category: &str) -> String {
        format!("{}.{}.>", self.config.subject_prefix, category)
    }

    fn name(&self) -> &str {
        "memory"
    }
}

/// In-memory subscription backed by broadcast channel
struct MemorySubscription {
    receiver: broadcast::Receiver<Event>,
    filter: String,
}

#[async_trait]
impl Subscription for MemorySubscription {
    async fn next(&mut self) -> Result<Option<ReceivedEvent>> {
        loop {
            match self.receiver.recv().await {
                Ok(event) => {
                    if subject_matches(&event.subject, &self.filter) {
                        return Ok(Some(ReceivedEvent {
                            event,
                            sequence: 0,
                            num_delivered: 1,
                            stream: "memory".to_string(),
                        }));
                    }
                    // Skip non-matching events
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "Memory subscriber lagged, skipped events");
                    // Continue receiving
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return Ok(None);
                }
            }
        }
    }

    async fn next_manual_ack(&mut self) -> Result<Option<PendingEvent>> {
        match self.next().await? {
            Some(received) => Ok(Some(PendingEvent::new(
                received,
                || Box::pin(async { Ok(()) }),
                || Box::pin(async { Ok(()) }),
            ))),
            None => Ok(None),
        }
    }
}

/// Check if a subject matches a filter pattern
///
/// Supports `>` (match rest) and `*` (match one token) wildcards.
fn subject_matches(subject: &str, filter: &str) -> bool {
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
    fn test_subject_matches() {
        assert!(subject_matches("events.market.forex", "events.market.forex"));
        assert!(subject_matches("events.market.forex", "events.market.>"));
        assert!(subject_matches("events.market.forex.usd", "events.market.>"));
        assert!(subject_matches("events.market.forex", "events.*.forex"));
        assert!(!subject_matches("events.market.forex", "events.system.>"));
        assert!(!subject_matches("events.market", "events.market.forex"));
        assert!(subject_matches("events.market.forex", "events.>"));
    }

    #[test]
    fn test_memory_config_default() {
        let config = MemoryConfig::default();
        assert_eq!(config.subject_prefix, "events");
        assert_eq!(config.max_events, 100_000);
        assert_eq!(config.channel_capacity, 10_000);
    }

    #[tokio::test]
    async fn test_publish_and_history() {
        let provider = MemoryProvider::default();

        let event = Event::new(
            "events.market.forex",
            "market",
            "Rate change",
            "test",
            serde_json::json!({}),
        );
        let seq = provider.publish(&event).await.unwrap();
        assert!(seq > 0);

        let history = provider.history(None, 10).await.unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].id, event.id);
    }

    #[tokio::test]
    async fn test_history_with_filter() {
        let provider = MemoryProvider::default();

        let e1 = Event::new("events.market.forex", "market", "A", "test", serde_json::json!({}));
        let e2 = Event::new("events.system.deploy", "system", "B", "test", serde_json::json!({}));
        provider.publish(&e1).await.unwrap();
        provider.publish(&e2).await.unwrap();

        let market = provider.history(Some("events.market.>"), 10).await.unwrap();
        assert_eq!(market.len(), 1);
        assert_eq!(market[0].category, "market");

        let all = provider.history(None, 10).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn test_max_events_limit() {
        let provider = MemoryProvider::new(MemoryConfig {
            max_events: 3,
            ..Default::default()
        });

        for i in 0..5 {
            let e = Event::new(
                format!("events.test.{}", i),
                "test",
                format!("Event {}", i),
                "test",
                serde_json::json!({}),
            );
            provider.publish(&e).await.unwrap();
        }

        let history = provider.history(None, 10).await.unwrap();
        assert_eq!(history.len(), 3);
    }

    #[tokio::test]
    async fn test_subscribe_and_receive() {
        let provider = MemoryProvider::default();
        let mut sub = provider.subscribe("events.market.>").await.unwrap();

        let event = Event::new(
            "events.market.forex",
            "market",
            "Rate change",
            "test",
            serde_json::json!({}),
        );

        // Publish in background
        let provider_clone = {
            let events = provider.events.clone();
            let sender = provider.sender.clone();
            let seq = provider.sequence.clone();
            (events, sender, seq)
        };

        let event_clone = event.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let _ = provider_clone.1.send(event_clone);
        });

        let received = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            sub.next(),
        )
        .await
        .unwrap()
        .unwrap()
        .unwrap();

        assert_eq!(received.event.id, event.id);
    }

    #[tokio::test]
    async fn test_provider_info() {
        let provider = MemoryProvider::default();

        let e = Event::new("events.test.a", "test", "A", "test", serde_json::json!({}));
        provider.publish(&e).await.unwrap();

        let info = provider.info().await.unwrap();
        assert_eq!(info.provider, "memory");
        assert_eq!(info.messages, 1);
        assert!(info.bytes > 0);
    }

    #[test]
    fn test_build_subject() {
        let provider = MemoryProvider::default();
        assert_eq!(
            provider.build_subject("market", "forex.usd"),
            "events.market.forex.usd"
        );
    }

    #[test]
    fn test_category_subject() {
        let provider = MemoryProvider::default();
        assert_eq!(provider.category_subject("market"), "events.market.>");
    }

    #[test]
    fn test_provider_name() {
        let provider = MemoryProvider::default();
        assert_eq!(provider.name(), "memory");
    }
}
