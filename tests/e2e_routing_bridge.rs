//! Cross-bus event bridge via TopicSink — two buses, two providers, one flow
//!
//! Deep scenarios over the routing feature:
//! 1. trigger filter matrix: subject patterns, source, attributes — each
//!    dimension must gate routing independently.
//! 2. the bridge: bus A publishes, a TopicSink backed by bus B's provider
//!    forwards, a subscriber on bus B receives the bridged event with its
//!    envelope intact.

#![cfg(feature = "routing")]

use a3s_event::provider::memory::MemoryProvider;
use a3s_event::sink::{CollectorSink, TopicSink};
use a3s_event::{
    Broker, Event, EventBus, EventProvider, SubscriptionFilter, Trigger, TriggerFilter,
};
use std::sync::Arc;

#[tokio::test]
async fn trigger_filter_matrix_gates_every_dimension() {
    let broker = Arc::new(Broker::new());

    let by_subject = Arc::new(CollectorSink::new("by-subject"));
    let by_source = Arc::new(CollectorSink::new("by-source"));
    let by_attr = Arc::new(CollectorSink::new("by-attr"));

    broker
        .add_trigger(Trigger::new(
            "subject-pattern",
            TriggerFilter::by_subject("events.fx.*"),
            by_subject.clone(),
        ))
        .await;
    broker
        .add_trigger(Trigger::new(
            "source-gate",
            TriggerFilter::by_source("exchange-1"),
            by_source.clone(),
        ))
        .await;
    broker
        .add_trigger(Trigger::new(
            "attr-gate",
            TriggerFilter::by_type("trade.executed").with_attribute("desk", "fx"),
            by_attr.clone(),
        ))
        .await;

    // Matches subject pattern only.
    let r = broker
        .route(&Event::new(
            "events.fx.eur",
            "fx",
            "s1",
            "other",
            serde_json::json!({}),
        ))
        .await;
    assert_eq!((r.matched, r.delivered, r.failed), (1, 1, 0));

    // Matches source only.
    let r = broker
        .route(&Event::new(
            "events.any.x",
            "any",
            "s2",
            "exchange-1",
            serde_json::json!({}),
        ))
        .await;
    assert_eq!((r.matched, r.delivered), (1, 1));

    // Matches type+attribute only.
    let mut trade = Event::typed(
        "events.trades.executed",
        "trades",
        "trade.executed",
        1,
        "s3",
        "desk-system",
        serde_json::json!({}),
    );
    trade.metadata.insert("desk".to_string(), "fx".to_string());
    let r = broker.route(&trade).await;
    assert_eq!((r.matched, r.delivered), (1, 1));

    // Matches nothing.
    let r = broker
        .route(&Event::new(
            "events.other.y",
            "other",
            "s4",
            "nobody",
            serde_json::json!({}),
        ))
        .await;
    assert_eq!((r.matched, r.delivered, r.failed), (0, 0, 0));

    // One event matching several triggers fans out to all of them.
    let mut big = Event::typed(
        "events.fx.executed",
        "fx",
        "trade.executed",
        1,
        "s5",
        "exchange-1",
        serde_json::json!({}),
    );
    big.metadata.insert("desk".to_string(), "fx".to_string());
    let r = broker.route(&big).await;
    assert_eq!(
        (r.matched, r.delivered),
        (3, 3),
        "subject+source+attr all match"
    );

    assert_eq!(by_subject.count().await, 2);
    assert_eq!(by_source.count().await, 2);
    assert_eq!(by_attr.count().await, 2);
}

#[tokio::test]
async fn topic_sink_bridges_events_across_buses() {
    // Bus B (destination) with a live subscriber.
    let dest_provider = Arc::new(MemoryProvider::default());
    let bus_b = EventBus::from_provider(Arc::clone(&dest_provider) as Arc<dyn EventProvider>);
    bus_b
        .update_subscription(SubscriptionFilter {
            subscriber_id: "bridge-receiver".to_string(),
            subjects: vec!["events.bridged.>".to_string()],
            durable: false,
            options: None,
        })
        .await
        .unwrap();
    let mut receiver = bus_b
        .create_subscriber("bridge-receiver")
        .await
        .unwrap()
        .remove(0);

    // Bus A (source) routes everything into bus B's provider via TopicSink.
    let src_provider = Arc::new(MemoryProvider::default());
    let mut bus_a = EventBus::from_provider(Arc::clone(&src_provider) as Arc<dyn EventProvider>);
    let broker = Arc::new(Broker::new());
    broker
        .add_trigger(Trigger::new(
            "bridge-all",
            TriggerFilter::by_subject("events.bridged.>"),
            Arc::new(TopicSink::new(
                "to-bus-b",
                Arc::clone(&dest_provider) as Arc<dyn EventProvider>,
            )),
        ))
        .await;
    bus_a.set_broker(broker);

    let payload = serde_json::json!({"trip": "a-to-b", "nested": {"ok": true}});
    let event = Event::typed(
        "events.bridged.payload",
        "bridged",
        "bridge.message",
        1,
        "cross-bus message",
        "bus-a",
        payload.clone(),
    )
    .with_metadata("hop", "1");
    bus_a.publish_event(&event).await.unwrap();

    // The subscriber on bus B receives the bridged envelope intact.
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.next())
        .await
        .expect("bridge delivery timed out")
        .unwrap()
        .expect("bridged bus must deliver");
    assert_eq!(
        got.event.id, event.id,
        "envelope identity survives the bridge"
    );
    assert_eq!(got.event.event_type, "bridge.message");
    assert_eq!(got.event.payload, payload);
    assert_eq!(got.event.metadata["hop"], "1");
    assert_eq!(got.event.source, "bus-a");

    // Non-matching subjects do not traverse the bridge.
    let off_path = Event::new(
        "events.local.only",
        "local",
        "stays-home",
        "bus-a",
        serde_json::json!({}),
    );
    bus_a.publish_event(&off_path).await.unwrap();
    let silence =
        tokio::time::timeout(std::time::Duration::from_millis(300), receiver.next()).await;
    assert!(silence.is_err(), "non-bridged subject must not arrive");
}
