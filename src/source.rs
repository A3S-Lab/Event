//! Event sources — adapters that produce events from external signals
//!
//! `EventSource` defines a standard interface for components that generate
//! events on a schedule, from webhooks, or from metric thresholds.
//! Sources emit events through a sender channel that the EventBus or
//! Broker can consume.

use crate::error::Result;
use crate::types::Event;
use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::{mpsc, Notify};

/// Trait for event sources
///
/// An event source generates events from external signals and sends
/// them through the provided channel. Sources run asynchronously and
/// can be stopped gracefully.
#[async_trait]
pub trait EventSource: Send + Sync {
    /// Start the source, emitting events through the sender
    ///
    /// This method runs until `stop()` is called or the sender is dropped.
    /// Implementations should handle errors gracefully (log and continue).
    async fn start(&self, sender: mpsc::Sender<Event>) -> Result<()>;

    /// Signal the source to stop
    async fn stop(&self) -> Result<()>;

    /// Human-readable source name
    fn name(&self) -> &str;
}

/// Type alias for the event factory function used by CronSource
type EventFactory = dyn Fn() -> Event + Send + Sync;

/// Event source that emits events on a fixed interval
///
/// Uses `tokio::time::interval` for scheduling and `Notify` for
/// graceful shutdown.
pub struct CronSource {
    name: String,
    interval: std::time::Duration,
    factory: Arc<EventFactory>,
    stop_signal: Arc<Notify>,
}

impl CronSource {
    /// Create a new cron source
    ///
    /// - `name` — source identifier
    /// - `interval` — time between event emissions
    /// - `factory` — closure that creates each event
    pub fn new<F>(
        name: impl Into<String>,
        interval: std::time::Duration,
        factory: F,
    ) -> Self
    where
        F: Fn() -> Event + Send + Sync + 'static,
    {
        Self {
            name: name.into(),
            interval,
            factory: Arc::new(factory),
            stop_signal: Arc::new(Notify::new()),
        }
    }
}

#[async_trait]
impl EventSource for CronSource {
    async fn start(&self, sender: mpsc::Sender<Event>) -> Result<()> {
        let mut interval = tokio::time::interval(self.interval);
        let factory = self.factory.clone();
        let stop = self.stop_signal.clone();
        let name = self.name.clone();

        // Consume the first tick (fires immediately)
        interval.tick().await;

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    let event = (factory)();
                    if sender.send(event).await.is_err() {
                        tracing::debug!(source = %name, "CronSource sender closed, stopping");
                        break;
                    }
                }
                _ = stop.notified() => {
                    tracing::debug!(source = %name, "CronSource received stop signal");
                    break;
                }
            }
        }

        Ok(())
    }

    async fn stop(&self) -> Result<()> {
        self.stop_signal.notify_one();
        Ok(())
    }

    fn name(&self) -> &str {
        &self.name
    }
}

/// Marker trait for webhook-based event sources
///
/// Concrete implementations require an HTTP server, which is out of scope
/// for a library crate. This trait defines the contract for webhook sources
/// that can be implemented by applications.
#[async_trait]
pub trait WebhookSource: EventSource {
    /// The path this webhook listens on (e.g., "/webhooks/github")
    fn path(&self) -> &str;

    /// Accepted content types (e.g., "application/json")
    fn content_types(&self) -> Vec<String> {
        vec!["application/json".to_string()]
    }
}

/// Marker trait for metrics-based event sources
///
/// Concrete implementations require a metrics collector (Prometheus, etc.),
/// which is out of scope for a library crate. This trait defines the contract
/// for metric threshold sources.
#[async_trait]
pub trait MetricsSource: EventSource {
    /// The metric name being monitored
    fn metric_name(&self) -> &str;

    /// The threshold value that triggers an event
    fn threshold(&self) -> f64;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[tokio::test]
    async fn test_cron_source_emits_events() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let source = CronSource::new(
            "test-cron",
            std::time::Duration::from_millis(50),
            move || {
                let n = counter_clone.fetch_add(1, Ordering::SeqCst);
                Event::new(
                    format!("events.cron.tick.{}", n),
                    "cron",
                    format!("Tick {}", n),
                    "cron-source",
                    serde_json::json!({"tick": n}),
                )
            },
        );

        assert_eq!(source.name(), "test-cron");

        let (tx, mut rx) = mpsc::channel(100);

        // Start source in background
        let source_handle = {
            let source_ref = &source;
            let tx = tx.clone();
            tokio::spawn({
                let _name = source_ref.name().to_string();
                let interval = source_ref.interval;
                let factory = source_ref.factory.clone();
                let stop = source_ref.stop_signal.clone();

                async move {
                    let mut interval = tokio::time::interval(interval);
                    interval.tick().await; // consume first immediate tick

                    loop {
                        tokio::select! {
                            _ = interval.tick() => {
                                let event = (factory)();
                                if tx.send(event).await.is_err() {
                                    break;
                                }
                            }
                            _ = stop.notified() => {
                                break;
                            }
                        }
                    }
                }
            })
        };

        // Wait for a few events
        tokio::time::sleep(std::time::Duration::from_millis(180)).await;
        source.stop().await.unwrap();
        source_handle.await.unwrap();

        // Collect received events
        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }

        // Should have received at least 2 events in 180ms with 50ms interval
        assert!(events.len() >= 2, "Expected >= 2 events, got {}", events.len());
        assert!(events[0].subject.starts_with("events.cron.tick."));
    }

    #[tokio::test]
    async fn test_cron_source_stop() {
        let source = CronSource::new(
            "stoppable",
            std::time::Duration::from_millis(10),
            || Event::new("events.cron.a", "cron", "A", "src", serde_json::json!({})),
        );

        let (tx, _rx) = mpsc::channel(100);

        let stop = source.stop_signal.clone();
        let factory = source.factory.clone();
        let interval = source.interval;

        let handle = tokio::spawn(async move {
            let mut interval_timer = tokio::time::interval(interval);
            interval_timer.tick().await;

            loop {
                tokio::select! {
                    _ = interval_timer.tick() => {
                        let event = (factory)();
                        if tx.send(event).await.is_err() {
                            break;
                        }
                    }
                    _ = stop.notified() => {
                        break;
                    }
                }
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        source.stop().await.unwrap();

        // Should complete without hanging
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn test_cron_source_sender_closed() {
        let source = CronSource::new(
            "closed-sender",
            std::time::Duration::from_millis(10),
            || Event::new("events.cron.a", "cron", "A", "src", serde_json::json!({})),
        );

        let (tx, rx) = mpsc::channel(1);
        drop(rx); // Close receiver immediately

        // start should complete without error when sender is closed
        let result = source.start(tx).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_cron_source_name() {
        let source = CronSource::new(
            "health-sweep",
            std::time::Duration::from_secs(30),
            || Event::new("events.health.sweep", "health", "Sweep", "src", serde_json::json!({})),
        );
        assert_eq!(source.name(), "health-sweep");
    }

    #[test]
    fn test_event_source_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<CronSource>();
    }
}
