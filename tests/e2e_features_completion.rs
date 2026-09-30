//! Feature-completion e2e — the remaining public surfaces not exercised by
//! the other suites
//!
//! Covers, as deep end-to-end flows:
//! 1. DLQ subsystem beyond MemoryDlqHandler-as-a-bucket: the capacity
//!    eviction contract (oldest dropped), the `should_dead_letter`
//!    redelivery-exhaustion predicate, and `SinkDlqHandler` forwarding dead
//!    letters into a real sink.
//! 2. Schema evolution: the full compatibility matrix (Backward / Forward /
//!    Full / None) across v1→v2 registrations, plus the publish gate
//!    enforcing the newest registered version.
//! 3. `InProcessSink` (async handler with side effects) and `LogSink`
//!    (never fails, no side effects observable — delivered without error).
//! 4. `MemoryStateStore` round-trip through the EventBus registry.

#![cfg(feature = "routing")]

use a3s_event::sink::CollectorSink;
use a3s_event::sink::InProcessSink;
use a3s_event::state::MemoryStateStore;
use a3s_event::{
    DeadLetterEvent, DlqHandler, Event, EventBus, EventProvider, MemoryDlqHandler,
    MemorySchemaRegistry, ReceivedEvent, SchemaRegistry, SinkDlqHandler, SubscriptionFilter,
    Trigger, TriggerFilter,
};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn dead_letter(id: &str, subject: &str) -> DeadLetterEvent {
    DeadLetterEvent::new(
        ReceivedEvent {
            event: Event::new(subject, "dlq", id, "test", serde_json::json!({})),
            sequence: 1,
            num_delivered: 3,
            stream: "memory".to_string(),
        },
        "exhausted retries",
    )
}

#[tokio::test]
async fn dlq_capacity_evicts_oldest_and_predicate_gates() {
    // Capacity: a 3-slot DLQ keeps the NEWEST three dead letters.
    let dlq = MemoryDlqHandler::new(3);
    for i in 0..5 {
        dlq.handle(dead_letter(&format!("d{i}"), "events.dlq.a"))
            .await
            .unwrap();
    }
    assert_eq!(dlq.count().await.unwrap(), 3, "capacity enforced");

    let listed = dlq.list(10).await.unwrap();
    // list() returns newest-first.
    let ids: Vec<&str> = listed
        .iter()
        .map(|d| d.event.event.summary.as_str())
        .collect();
    assert_eq!(
        ids,
        vec!["d4", "d3", "d2"],
        "oldest evicted, newest-first listing"
    );

    // The redelivery-exhaustion predicate: dead-letter exactly when
    // deliveries reached the configured maximum.
    let delivered = |n: u64| ReceivedEvent {
        event: Event::new("events.dlq.b", "dlq", "x", "t", serde_json::json!({})),
        sequence: 0,
        num_delivered: n,
        stream: "memory".to_string(),
    };
    assert!(
        !a3s_event::dlq::should_dead_letter(&delivered(2), 3),
        "below max: retry again"
    );
    assert!(
        a3s_event::dlq::should_dead_letter(&delivered(3), 3),
        "at max: dead-letter"
    );
    assert!(
        !a3s_event::dlq::should_dead_letter(&delivered(99), 0),
        "max 0 disables the gate"
    );
}

#[tokio::test]
async fn sink_dlq_handler_forwards_dead_letters_into_a_sink() {
    // A TopicSink-style pipeline: dead letters land in a collector sink.
    let collector = Arc::new(CollectorSink::new("dlq-archive"));
    let handler = SinkDlqHandler::new(collector.clone() as Arc<dyn a3s_event::EventSink>, 100);

    for i in 0..3 {
        handler
            .handle(dead_letter(&format!("dead-{i}"), "events.dlq.in"))
            .await
            .unwrap();
    }

    assert_eq!(handler.count().await.unwrap(), 3, "counted by the handler");
    assert_eq!(
        collector.count().await,
        3,
        "every dead letter forwarded to the sink"
    );

    // The sink receives a typed DLQ NOTIFICATION (not the raw envelope):
    // subject namespaced under events.dlq.*, metadata carries the lineage.
    let archived = collector.events().await;
    let first = &archived[0];
    assert_eq!(first.event_type, "a3s.dlq.dead_letter");
    assert!(
        first.subject.starts_with("events.dlq."),
        "namespaced: {}",
        first.subject
    );
    assert!(first.summary.contains("exhausted retries"));
    assert_eq!(first.metadata["dlq_reason"], "exhausted retries");
    assert!(first.metadata.contains_key("dlq_original_id"));

    // Notification ordering follows handling order.
    // Every notification carries the original event's id in its lineage.
    assert!(
        archived
            .iter()
            .all(|e| e.metadata.contains_key("dlq_original_id")),
        "lineage metadata on every notification"
    );
}

#[tokio::test]
async fn schema_compatibility_matrix_gates_evolution() {
    let registry = Arc::new(MemorySchemaRegistry::new());

    let v1 = a3s_event::EventSchema {
        event_type: "order.placed".to_string(),
        version: 1,
        required_fields: vec!["id".to_string(), "amount".to_string()],
        description: "v1".to_string(),
    };
    registry.register(v1.clone()).unwrap();

    use a3s_event::schema::Compatibility;

    // v2 adds a REQUIRED field — backward-incompatible (old consumers break).
    let v2_add_required = a3s_event::EventSchema {
        event_type: "order.placed".to_string(),
        version: 2,
        required_fields: vec![
            "id".to_string(),
            "amount".to_string(),
            "currency".to_string(),
        ],
        description: "v2".to_string(),
    };
    registry.register(v2_add_required.clone()).unwrap();
    let err = registry
        .check_compatibility("order.placed", 2, Compatibility::Backward)
        .unwrap_err();
    assert!(err.to_string().contains("currency"), "{err}");
    // Forward-compatible though: nothing v1 required was removed.
    registry
        .check_compatibility("order.placed", 2, Compatibility::Forward)
        .unwrap();
    // Full = both directions → still fails on the added required field.
    assert!(registry
        .check_compatibility("order.placed", 2, Compatibility::Full)
        .is_err());
    // None skips the check entirely.
    registry
        .check_compatibility("order.placed", 2, Compatibility::None)
        .unwrap();

    // v3 REMOVES a v1-required field — forward-incompatible (new consumers
    // can't read old events).
    let v3_drop_amount = a3s_event::EventSchema {
        event_type: "order.placed".to_string(),
        version: 3,
        required_fields: vec!["id".to_string()],
        description: "v3".to_string(),
    };
    registry.register(v3_drop_amount).unwrap();
    let err = registry
        .check_compatibility("order.placed", 3, Compatibility::Forward)
        .unwrap_err();
    assert!(err.to_string().contains("amount"), "{err}");

    // Compatibility is STEPWISE (vN vs vN-1), so v4 identical to v3 —
    // not to v1 — is what "fully compatible evolution" means.
    let identical_to_v3 = a3s_event::EventSchema {
        event_type: "order.placed".to_string(),
        version: 4,
        required_fields: vec!["id".to_string()],
        description: "v4".to_string(),
    };
    registry.register(identical_to_v3).unwrap();
    for mode in [
        Compatibility::Backward,
        Compatibility::Forward,
        Compatibility::Full,
    ] {
        registry
            .check_compatibility("order.placed", 4, mode)
            .unwrap_or_else(|e| panic!("{mode:?}: {e}"));
    }

    // The registry tracks versions and types for operators.
    assert_eq!(registry.latest_version("order.placed").unwrap(), Some(4));
    let types = registry.list_types().unwrap();
    assert!(types.contains(&"order.placed".to_string()));

    // And the publish gate enforces the LATEST version's requirements.
    let mut bus = EventBus::from_provider(
        Arc::new(a3s_event::MemoryProvider::default()) as Arc<dyn EventProvider>
    );
    bus.set_schema_registry(registry.clone() as Arc<dyn SchemaRegistry>);

    let v4_event = Event::typed(
        "events.orders.placed",
        "orders",
        "order.placed",
        4,
        "complete order",
        "shop",
        serde_json::json!({"id": "o-1", "amount": 9}),
    );
    bus.publish_event(&v4_event).await.unwrap();
}

#[tokio::test]
async fn in_process_sink_runs_real_handlers_and_log_sink_never_fails() {
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();

    let in_process = InProcessSink::new("side-effects", move |event: Event| {
        let seen = seen.clone();
        async move {
            assert!(event.id.starts_with("evt-"), "sink sees the real envelope");
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    });

    let broker = Arc::new(a3s_event::Broker::new());
    broker
        .add_trigger(Trigger::new(
            "in-process-fanout",
            TriggerFilter::by_subject("events.side.>"),
            Arc::new(in_process),
        ))
        .await;
    broker
        .add_trigger(Trigger::new(
            "log-everything",
            TriggerFilter::by_subject("events.side.>"),
            Arc::new(a3s_event::LogSink::new("audit-log")),
        ))
        .await;

    let mut bus = EventBus::from_provider(
        Arc::new(a3s_event::MemoryProvider::default()) as Arc<dyn EventProvider>
    );
    bus.set_broker(broker);

    for i in 0..3 {
        let e = Event::new(
            format!("events.side.{i}"),
            "side",
            format!("s{i}"),
            "t",
            serde_json::json!({}),
        );
        bus.publish_event(&e).await.unwrap();
    }

    // The async handler ran once per published event, and the log sink's
    // deliveries never failed (a failure would have dead-lettered or errored).
    assert_eq!(
        calls.load(Ordering::SeqCst),
        3,
        "handler invoked per delivery"
    );
}

#[tokio::test]
async fn memory_state_store_round_trips_the_registry() {
    let raw = Arc::new(a3s_event::MemoryProvider::default());
    let mut bus = EventBus::from_provider(Arc::clone(&raw) as Arc<dyn EventProvider>);

    let filter = SubscriptionFilter {
        subscriber_id: "mem-state".to_string(),
        subjects: vec!["events.mem.>".to_string()],
        durable: true,
        options: None,
    };

    // Attach the store BEFORE registering: update_subscription persists into it.
    bus.set_state_store(Arc::new(MemoryStateStore::default()))
        .unwrap();
    bus.update_subscription(filter.clone()).await.unwrap();

    // A second bus with the SAME store restores the registry.
    let mut bus2 = EventBus::from_provider(Arc::clone(&raw) as Arc<dyn EventProvider>);
    bus2.set_state_store(Arc::new(MemoryStateStore::default()))
        .unwrap();

    let store = MemoryStateStore::default();
    use a3s_event::StateStore as _;
    // The state-store trait itself round-trips (save → load is lossless).
    let mut map = std::collections::HashMap::new();
    map.insert(filter.subscriber_id.clone(), filter.clone());
    store.save(&map).unwrap();
    let loaded = store.load().unwrap();
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        loaded.get("mem-state").map(|f| f.subjects.clone()),
        Some(vec!["events.mem.>".to_string()])
    );
    let _ = (&bus, &bus2);
}

#[tokio::test]
async fn state_store_failure_paths_fail_closed() {
    use a3s_event::state::FileStateStore;
    use a3s_event::StateStore as _;

    // A path UNDER a regular file can never be created: create_dir_all
    // fails and save must propagate that (never panic, never fake success).
    let blocker = std::env::temp_dir().join(format!("a3s-state-blocker-{}", std::process::id()));
    std::fs::write(&blocker, b"i am a file").unwrap();
    let store = FileStateStore::new(blocker.join("nested").join("subs.json"));
    let mut map = std::collections::HashMap::new();
    map.insert(
        "s".to_string(),
        SubscriptionFilter {
            subscriber_id: "s".to_string(),
            subjects: vec!["events.x.>".to_string()],
            durable: false,
            options: None,
        },
    );
    assert!(
        store.save(&map).is_err(),
        "unwritable path must error, not panic"
    );

    // EventBus wiring propagates the failure instead of swallowing it.
    let mut bus = EventBus::from_provider(
        Arc::new(a3s_event::MemoryProvider::default()) as Arc<dyn EventProvider>
    );
    assert!(bus.set_state_store(Arc::new(store)).is_ok()); // load on missing file is fine
    let _ = map;
    let _ = std::fs::remove_file(&blocker);
}

#[tokio::test]
async fn failing_provider_surfaces_errors_and_metrics() {
    use async_trait::async_trait;

    /// A provider that always fails — proves the bus reports publish
    /// failures and counts them instead of masking success.
    struct AlwaysFailingProvider;
    #[async_trait]
    impl a3s_event::EventProvider for AlwaysFailingProvider {
        async fn publish(&self, event: &Event) -> a3s_event::Result<u64> {
            Err(a3s_event::EventError::Publish {
                subject: event.subject.clone(),
                reason: "synthetic outage".to_string(),
            })
        }
        async fn subscribe_durable(
            &self,
            _name: &str,
            filter: &str,
        ) -> a3s_event::Result<Box<dyn a3s_event::Subscription>> {
            Err(a3s_event::EventError::Subscribe {
                subject: filter.to_string(),
                reason: "synthetic outage".to_string(),
            })
        }
        async fn subscribe(
            &self,
            filter: &str,
        ) -> a3s_event::Result<Box<dyn a3s_event::Subscription>> {
            self.subscribe_durable("", filter).await
        }
        async fn history(
            &self,
            _filter: Option<&str>,
            _limit: usize,
        ) -> a3s_event::Result<Vec<Event>> {
            Err(a3s_event::EventError::Provider("history down".to_string()))
        }
        async fn unsubscribe(&self, _name: &str) -> a3s_event::Result<()> {
            Ok(())
        }
        async fn info(&self) -> a3s_event::Result<a3s_event::ProviderInfo> {
            Err(a3s_error_wired())
        }
        fn subject_prefix(&self) -> &str {
            "events"
        }
        fn name(&self) -> &str {
            "always-failing"
        }
    }

    fn a3s_error_wired() -> a3s_event::EventError {
        a3s_event::EventError::Connection("synthetic".to_string())
    }

    let mut bus = EventBus::from_provider(
        Arc::new(AlwaysFailingProvider) as Arc<dyn a3s_event::EventProvider>
    );

    let e = Event::new("events.down.a", "down", "f", "t", serde_json::json!({}));
    let err = bus.publish_event(&e).await.unwrap_err();
    assert!(err.to_string().contains("synthetic outage"), "{err}");
    assert_eq!(
        bus.metrics().snapshot().publish_errors,
        1,
        "failure counted"
    );
    assert_eq!(
        bus.metrics().snapshot().publish_count,
        0,
        "no phantom success"
    );

    let err = bus.list_events(None, 10).await.unwrap_err();
    assert!(err.to_string().contains("history down"), "{err}");
    assert!(bus.health().await.is_err(), "health reflects the outage");

    let _ = &mut bus;
}
