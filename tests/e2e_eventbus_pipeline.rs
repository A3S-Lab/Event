//! EventBus end-to-end pipeline — the full composition the bus composes
//! from its optional capabilities
//!
//! Deep scenarios over the memory provider (the bus's own composition is
//! the system under test, not the broker):
//!
//! 1. schema-gated publish: typed events validated against a registered
//!    schema, invalid payloads rejected BEFORE hitting the provider, the
//!    validation-error metric advancing, and untyped events passing through.
//! 2. encrypted publish → encrypted-at-rest in the provider → automatic
//!    decrypt on read, with the encrypt/decrypt metrics advancing.
//! 3. publish → broker → trigger → sink routing, and a failing sink paired
//!    with a DLQ handler capturing the dead letter.
//! 4. subscription registry persistence across a "process restart" via
//!    FileStateStore, and the registry's effect on create_subscriber.
//! 5. metrics snapshot as a cross-cutting audit of everything above.

#![cfg(feature = "routing")]

use a3s_event::provider::memory::MemoryProvider;
use a3s_event::sink::CollectorSink;
use a3s_event::state::FileStateStore;
use a3s_event::{
    Aes256GcmEncryptor, Broker, EncryptedPayload, Event, EventBus, EventEncryptor,
    MemoryDlqHandler, MemorySchemaRegistry, SchemaRegistry, SubscriptionFilter, Trigger,
    TriggerFilter,
};
use a3s_event::{DlqHandler, EventProvider};
use std::sync::Arc;

#[tokio::test]
async fn schema_gated_publish_rejects_before_provider() {
    let registry = Arc::new(MemorySchemaRegistry::new());
    registry
        .register(a3s_event::EventSchema {
            event_type: "trade.executed".to_string(),
            version: 1,
            required_fields: vec!["symbol".to_string(), "qty".to_string()],
            description: "a trade".to_string(),
        })
        .unwrap();

    // The raw provider is kept so the test can prove rejections never reach it.
    let raw = Arc::new(MemoryProvider::default());
    let mut bus = EventBus::from_provider(Arc::clone(&raw) as Arc<dyn a3s_event::EventProvider>);
    bus.set_schema_registry(registry.clone() as Arc<dyn SchemaRegistry>);

    // Valid typed event passes and is stored.
    let good = Event::typed(
        "events.trades.executed",
        "trades",
        "trade.executed",
        1,
        "buy 100 AAPL",
        "oms",
        serde_json::json!({"symbol": "AAPL", "qty": 100}),
    );
    bus.publish_event(&good).await.unwrap();

    // Invalid typed event is rejected with a schema error and never stored.
    let bad = Event::typed(
        "events.trades.executed",
        "trades",
        "trade.executed",
        1,
        "missing qty",
        "oms",
        serde_json::json!({"symbol": "MSFT"}),
    );
    let err = match bus.publish_event(&bad).await {
        Err(e) => e,
        Ok(_) => panic!("missing required field must be rejected"),
    };
    assert!(err.to_string().contains("Schema validation"), "{err}");

    // Untyped events bypass validation entirely.
    let untyped = Event::new(
        "events.trades.note",
        "trades",
        "no schema for this",
        "oms",
        serde_json::json!({"anything": true}),
    );
    bus.publish_event(&untyped).await.unwrap();

    // Provider saw exactly the two accepted events.
    let stored = raw.history(None, 100).await.unwrap();
    assert_eq!(stored.len(), 2);
    let ids: Vec<&str> = stored.iter().map(|e| e.id.as_str()).collect();
    assert!(ids.contains(&good.id.as_str()));
    assert!(ids.contains(&untyped.id.as_str()));

    // The validation-error metric advanced exactly once.
    let snap = bus.metrics().snapshot();
    assert_eq!(snap.validation_errors, 1, "one schema rejection");
}

#[tokio::test]
async fn encrypted_publish_stores_ciphertext_reads_plaintext() {
    let key: [u8; 32] = [7u8; 32];
    let encryptor = Arc::new(Aes256GcmEncryptor::new("k1", &key));

    let raw = Arc::new(MemoryProvider::default());
    let mut bus = EventBus::from_provider(Arc::clone(&raw) as Arc<dyn a3s_event::EventProvider>);
    bus.set_encryptor(encryptor.clone() as Arc<dyn EventEncryptor>);

    let secret = serde_json::json!({
        "account": "ACC-1",
        "balance": 42.5,
        "nested": {"key": "value"}
    });
    let event = Event::new(
        "events.vault.balance",
        "vault",
        "balance snapshot",
        "core",
        secret.clone(),
    );
    bus.publish_event(&event).await.unwrap();

    // At rest in the provider: the payload is an encrypted envelope, not
    // the plaintext, and the envelope carries the key id.
    let at_rest = raw.history(None, 10).await.unwrap();
    assert_eq!(at_rest.len(), 1);
    assert!(
        EncryptedPayload::is_encrypted(&at_rest[0].payload),
        "stored payload must be an encrypted envelope"
    );
    assert!(
        !at_rest[0].payload.to_string().contains("ACC-1"),
        "plaintext must not be recoverable from the stored payload"
    );

    // Through the bus: read path decrypts transparently.
    let read_back = bus.list_events(None, 10).await.unwrap();
    assert_eq!(read_back.len(), 1);
    assert_eq!(read_back[0].payload, secret);

    let snap = bus.metrics().snapshot();
    assert_eq!(snap.encrypt_count, 1);
    assert_eq!(snap.decrypt_count, 1);
}

#[tokio::test]
async fn publish_routes_through_broker_and_dlq_captures_failures() {
    let raw = Arc::new(MemoryProvider::default());
    let mut bus = EventBus::from_provider(Arc::clone(&raw) as Arc<dyn a3s_event::EventProvider>);

    let broker = Arc::new(Broker::new());
    let good_sink = Arc::new(CollectorSink::new("collector"));
    let failing_sink = Arc::new(a3s_event::FailingSink::new("broken", "simulated outage"));

    broker
        .add_trigger(Trigger::new(
            "audit-trades",
            TriggerFilter::by_type("trade.executed"),
            good_sink.clone(),
        ))
        .await;
    broker
        .add_trigger(Trigger::new(
            "alert-path",
            TriggerFilter::by_type("trade.executed"),
            failing_sink.clone(),
        ))
        .await;
    // A trigger that must NOT match.
    broker
        .add_trigger(Trigger::new(
            "deploy-watcher",
            TriggerFilter::by_type("deploy.completed"),
            Arc::new(CollectorSink::new("deploys")),
        ))
        .await;
    bus.set_broker(broker.clone());
    assert_eq!(broker.trigger_count().await, 3);

    let dlq = Arc::new(MemoryDlqHandler::new(100));
    bus.set_dlq_handler(dlq.clone() as Arc<dyn a3s_event::DlqHandler>);

    let trade = Event::typed(
        "events.trades.executed",
        "trades",
        "trade.executed",
        1,
        "routed event",
        "oms",
        serde_json::json!({"symbol": "GOOG"}),
    );
    bus.publish_event(&trade).await.unwrap();

    // Delivered to the matching sink, not the non-matching one.
    assert_eq!(good_sink.count().await, 1);
    let collected = good_sink.events().await;
    assert_eq!(collected[0].id, trade.id);

    // A publish that matches no trigger routes nowhere, without error.
    let other = Event::new(
        "events.misc.noise",
        "misc",
        "unrouted",
        "test",
        serde_json::json!({}),
    );
    bus.publish_event(&other).await.unwrap();
    assert_eq!(
        good_sink.count().await,
        1,
        "unmatched events are not routed"
    );

    // The failing sink's delivery is recorded as a dead letter.
    let dlq_events = dlq.list(10).await.unwrap();
    assert!(
        dlq_events.iter().any(|d| d.event.event.id == trade.id),
        "failed sink delivery must land in the DLQ"
    );
    let failed = dlq_events
        .iter()
        .find(|d| d.event.event.id == trade.id)
        .unwrap();
    assert!(
        failed.reason.contains("broker routing"),
        "reason must identify the failed routing: {}",
        failed.reason
    );

    let snap = bus.metrics().snapshot();
    assert!(snap.dlq_count >= 1, "dlq metric advanced");

    // Removing a trigger stops its routing.
    assert!(broker.remove_trigger("alert-path").await);
    assert!(
        !broker.remove_trigger("alert-path").await,
        "second remove is a no-op"
    );
    assert_eq!(broker.trigger_count().await, 2);

    let second = Event::typed(
        "events.trades.executed",
        "trades",
        "trade.executed",
        1,
        "routed again",
        "oms",
        serde_json::json!({"symbol": "AMZN"}),
    );
    bus.publish_event(&second).await.unwrap();
    assert_eq!(good_sink.count().await, 2);
    assert_eq!(
        dlq.count().await.unwrap(),
        1,
        "no new dead letters after removal"
    );
}

#[tokio::test]
async fn subscriptions_persist_across_restart_via_file_state_store() {
    let dir = std::env::temp_dir().join(format!("a3s-event-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let state_path = dir.join("subscriptions.json");

    let filter = SubscriptionFilter {
        subscriber_id: "restart-survivor".to_string(),
        subjects: vec!["events.persistence.>".to_string()],
        durable: true,
        options: None,
    };

    // "Process 1": register the subscription, persist via state store.
    {
        let mut bus = EventBus::new(MemoryProvider::default());
        bus.set_state_store(Arc::new(FileStateStore::new(&state_path)))
            .unwrap();
        bus.update_subscription(filter.clone()).await.unwrap();
        // update_subscription auto-saves; drop = "process exit"
    }

    // "Process 2": a fresh bus restores the registry from the file and can
    // materialize the subscriber without re-registering.
    let raw = Arc::new(MemoryProvider::default());
    let mut bus = EventBus::from_provider(Arc::clone(&raw) as Arc<dyn a3s_event::EventProvider>);
    // set_state_store restores persisted subscriptions immediately
    bus.set_state_store(Arc::new(FileStateStore::new(&state_path)))
        .unwrap();

    let restored = bus
        .get_subscription("restart-survivor")
        .await
        .expect("restored");
    assert_eq!(restored.subjects, filter.subjects);
    assert!(restored.durable);

    let subs = bus.create_subscriber("restart-survivor").await.unwrap();
    assert_eq!(subs.len(), 1);

    // And the restored subscription actually receives.
    let e = Event::new(
        "events.persistence.tick",
        "persistence",
        "post-restart",
        "test",
        serde_json::json!({}),
    );
    bus.publish_event(&e).await.unwrap();
    let mut sub = subs.into_iter().next().unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
        .await
        .expect("post-restart receive timed out")
        .unwrap()
        .expect("restored subscription receives");
    assert_eq!(got.event.summary, "post-restart");

    let _ = std::fs::remove_file(&state_path);
    let _ = std::fs::remove_dir(&dir);
}

#[tokio::test]
async fn metrics_snapshot_is_a_cross_cutting_audit() {
    let mut bus = EventBus::new(MemoryProvider::default());
    let dlq = Arc::new(MemoryDlqHandler::new(10));
    bus.set_dlq_handler(dlq as Arc<dyn a3s_event::DlqHandler>);

    let before = bus.metrics().snapshot();
    assert_eq!(before.publish_count, 0);

    for i in 0..5 {
        let e = Event::new(
            format!("events.audit.{i}"),
            "audit",
            format!("e{i}"),
            "test",
            serde_json::json!({}),
        );
        bus.publish_event(&e).await.unwrap();
    }

    bus.update_subscription(SubscriptionFilter {
        subscriber_id: "auditor".to_string(),
        subjects: vec!["events.audit.>".to_string()],
        durable: false,
        options: None,
    })
    .await
    .unwrap();
    let _ = bus.create_subscriber("auditor").await.unwrap();
    bus.remove_subscription("auditor").await.unwrap();

    let snap = bus.metrics().snapshot();
    assert_eq!(snap.publish_count, 5);
    assert_eq!(snap.subscribe_count, 1);
    assert_eq!(snap.unsubscribe_count, 1);
    assert!(snap.avg_publish_latency_us > 0 || snap.max_publish_latency_us > 0);

    // Explicit dead-letter recording through the handler flows into metrics
    // only via bus plumbing; record it the way the bus would:
    bus.metrics().record_dlq();
    let snap = bus.metrics().snapshot();
    assert_eq!(snap.dlq_count, 1);

    // A publish error path (provider failure is not injectable on memory,
    // so verify the counter through the public recorder).
    bus.metrics().record_publish_error();
    let snap = bus.metrics().snapshot();
    assert_eq!(snap.publish_errors, 1);
}

// Keep the state-store import honest when routing is the only enabled
// extra feature (MemoryStateStore is exercised in state.rs unit tests).
#[test]
fn memory_state_store_is_available() {
    let store = a3s_event::state::MemoryStateStore::default();
    use a3s_event::StateStore as _;
    assert!(store.load().unwrap().is_empty());
}
