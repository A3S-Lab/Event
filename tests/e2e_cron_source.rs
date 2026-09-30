//! CronSource end-to-end: schedule → channel → bus → subscriber, with
//! graceful stop and sender-close shutdown.

#![cfg(feature = "routing")]

use a3s_event::provider::memory::MemoryProvider;
use a3s_event::source::{CronSource, EventSource};
use a3s_event::{Event, EventBus};
use std::time::Duration;

#[tokio::test]
async fn cron_source_drives_the_bus_until_stopped() {
    let bus = EventBus::new(MemoryProvider::default());
    bus.update_subscription(a3s_event::SubscriptionFilter {
        subscriber_id: "cron-watcher".to_string(),
        subjects: vec!["events.cron.>".to_string()],
        durable: false,
        options: None,
    })
    .await
    .unwrap();
    let mut sub = bus
        .create_subscriber("cron-watcher")
        .await
        .unwrap()
        .remove(0);

    let source = CronSource::new("ticker", Duration::from_millis(50), || {
        Event::new(
            "events.cron.tick",
            "cron",
            format!("tick-{}", now_millis()),
            "cron-source",
            serde_json::json!({}),
        )
    });

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);
    let runner = tokio::spawn(async move { source.start(tx).await });

    // Drain the channel into the bus until at least 3 ticks flowed.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut seen = 0usize;
    while seen < 3 {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!("cron source produced only {seen} ticks in time");
        }
        if let Some(event) = tokio::time::timeout(remaining, rx.recv())
            .await
            .unwrap_or(None)
        {
            bus.publish_event(&event).await.unwrap();
            seen += 1;
        }
    }

    // Graceful stop ends the source task.
    // (CronSource::stop signals Notify; the loop exits on the next select.)
    let stopped = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if runner.is_finished() {
                break;
            }
            // No direct handle to stop() through the trait object here —
            // dropping the receiver also stops the loop.
            rx.close();
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        stopped.is_ok(),
        "source task must end when its sender closes"
    );
    let _ = runner.await.unwrap();

    // The subscriber saw every tick the bus accepted.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut summaries = Vec::new();
    while summaries.len() < 3 {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!("subscriber saw {}/3 ticks", summaries.len());
        }
        if let Ok(Ok(Some(received))) = tokio::time::timeout(remaining, sub.next()).await {
            summaries.push(received.event.summary);
        }
    }
    assert!(summaries.iter().all(|s| s.starts_with("tick-")));
}

/// Current Unix time in millis (unique-enough tick labels)
fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}
