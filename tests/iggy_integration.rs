#![cfg(feature = "iggy")]
//! Apache Iggy integration tests
//!
//! These tests require a running Iggy server with TCP enabled:
//!   docker run --rm --security-opt seccomp=unconfined \
//!     -e IGGY_TCP_ADDRESS=0.0.0.0:5102 -e IGGY_NODE_ADVERTISED_ADDRESS=127.0.0.1 \
//!     -e IGGY_SHARDING_CPU_ALLOCATION=1 -e IGGY_SHARDING_PIN_CORES=false \
//!     -p 5102:5102 apache/iggy:0.9.0
//!
//! Tests are skipped automatically if the server is not available.
//!
//! # Test matrix (derived from the EventProvider contract)
//!
//! Every case pins one claim of the provider contract; the pure decision
//! tables behind routing and positioning live in `provider::iggy::mapping`
//! and `provider::iggy::policy` with their own unit tests. This file holds
//! the claims that need a live broker.
//!
//! | # | Claim (contract) | Test |
//! |---|------------------|------|
//! | 1 | publish → history round-trip | `publish_and_history` |
//! | 2 | payload/metadata/type/version fidelity | `event_payload_fidelity` |
//! | 3 | categories = topics; per-category filters | `publish_multiple_categories` |
//! | 4 | info() reflects stored events | `provider_info` |
//! | 5 | health() true when connected | `health_check` |
//! | 6 | durable subscription delivers | `durable_subscription_receives_events` |
//! | 7 | ack ⇒ offset persists across resubscribe | `durable_offset_persists_across_resubscribe` |
//! | 8 | offset survives a NEW connection (server-side state) | `durable_offset_survives_new_connection` |
//! | 9 | no-ack ⇒ redelivery on rejoin (at-least-once) | `unacked_event_redelivers_on_rejoin` |
//! | 10 | group deletion ⇒ replay from retention (rebuild) | `unsubscribe_and_resubscribe_replays_from_start` |
//! | 11 | ephemeral + All replays history | `ephemeral_subscription_from_history` |
//! | 12 | category-wildcard filters span topics | `all_topics_filter` |
//! | 13 | `New` skips pre-subscribe events | `deliver_policy_new_skips_history` |
//! | 14 | `Last` seeds exactly the head | `deliver_last_seeds_head` |
//! | 15 | `ByStartSequence` pins the start offset | `deliver_by_start_sequence` |
//! | 16 | `ByStartTime` positions the first poll | `deliver_by_start_time` |
//! | 17 | per-topic total order | `ordering_within_topic` |
//! | 18 | sub-wildcard filters narrow within a topic | `sub_wildcard_filter_narrows_topic` |
//! | 19 | name sanitization (categories, consumer names) | `sanitized_category_and_consumer_names` |
//! | 20 | poison-undecodable tolerance (skip, no wedge) | `foreign_poison_message_is_skipped` |
//! | 21 | `expected_sequence` fails closed | `expected_sequence_fails_closed` |
//! | 22 | `LastPerSubject` fails closed | `last_per_subject_fails_closed` |
//! | 23 | subjects outside the prefix are rejected | `subject_outside_prefix_fails` |
//! | 24 | unsubscribe of a missing group is a no-op | `unsubscribe_missing_group_is_ok` |
//! | 25 | unreachable server → fast Connection error | `unreachable_server_fails_fast` |
//! | 26 | history bounded to most recent `limit` | `history_limit_returns_most_recent` |
//! | 27 | history on a fresh stream is empty | `history_on_fresh_stream_is_empty` |
//! | 28 | group offsets are independent per topic | `all_topics_group_offsets_independent` |
//! | 29 | shared group: exactly one member delivers | `shared_group_exactly_one_member_receives` |
//! | 30 | ephemeral subscriptions have independent cursors | `two_ephemeral_subs_independent` |
//! | 31 | info() counts consumer groups | `info_counts_consumer_groups` |
//! | 32 | concurrent publishes all land | `concurrent_publish` |

use a3s_event::provider::iggy::{IggyConfig, IggyPartitioning, IggyProvider};
use a3s_event::{DeliverPolicy, Event, EventBus, EventProvider, PublishOptions, SubscribeOptions};

/// Try to connect to Iggy. Returns None if server is unavailable.
async fn try_iggy_provider(stream_suffix: &str) -> Option<IggyProvider> {
    // Per-process stream: repeated suite runs against a live server must
    // not see each other's events or consumer groups.
    let config = IggyConfig {
        server_address: "127.0.0.1:5102".to_string(),
        stream_name: format!("test_events_{}_{}", stream_suffix, std::process::id()),
        subject_prefix: format!("test.{}", stream_suffix),
        partitioning: IggyPartitioning::Single,
        max_age_secs: 300,
        poll_batch_size: 50,
        poll_interval_ms: 20,
        ..Default::default()
    };

    // One bounded retry: a just-booted server can still be settling its
    // listener; a second attempt a second later separates "booting" from
    // "not running" without ever tolerating a real outage.
    match IggyProvider::connect(config.clone()).await {
        Ok(provider) => Some(provider),
        Err(first) => {
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            match IggyProvider::connect(config).await {
                Ok(provider) => {
                    eprintln!("Iggy settled after one retry (first: {first})");
                    Some(provider)
                }
                Err(e) => {
                    // CI sets A3S_EVENT_REQUIRE_BROKERS=1 so a dead service
                    // container FAILS the build instead of skip-passing.
                    if std::env::var("A3S_EVENT_REQUIRE_BROKERS").is_ok() {
                        panic!("Iggy required but unreachable: {e}");
                    }
                    eprintln!("Iggy not available, skipping integration test");
                    None
                }
            }
        }
    }
}

/// Helper to create an EventBus with Iggy, or skip the test
macro_rules! iggy_bus {
    ($suffix:expr) => {
        match try_iggy_provider($suffix).await {
            Some(p) => EventBus::new(p),
            None => return,
        }
    };
}

#[tokio::test]
async fn test_iggy_publish_and_history() {
    let bus = iggy_bus!("pub_hist");

    let event = bus
        .publish(
            "market",
            "forex",
            "USD/CNY rate change",
            "reuters",
            serde_json::json!({"rate": 7.35}),
        )
        .await
        .unwrap();

    assert!(event.id.starts_with("evt-"));
    assert_eq!(event.category, "market");

    let events = bus.list_events(Some("market"), 10).await.unwrap();
    assert!(!events.is_empty());
    assert!(events.iter().any(|e| e.id == event.id));
}

#[tokio::test]
async fn test_iggy_event_payload_fidelity() {
    let provider = match try_iggy_provider("fidelity").await {
        Some(p) => p,
        None => return,
    };

    let event = Event::typed(
        "test.fidelity.market.forex",
        "market",
        "forex.rate_change",
        3,
        "Typed event",
        "reuters",
        serde_json::json!({"rate": 7.3521, "nested": {"a": [1, 2, 3]}}),
    )
    .with_metadata("region", "asia")
    .with_metadata("env", "test");

    provider.publish(&event).await.unwrap();

    let history = provider
        .history(Some("test.fidelity.market.>"), 10)
        .await
        .unwrap();
    let round_tripped = history
        .iter()
        .find(|e| e.id == event.id)
        .expect("published event must come back from history");

    assert_eq!(round_tripped.event_type, "forex.rate_change");
    assert_eq!(round_tripped.version, 3);
    assert_eq!(
        round_tripped.payload["nested"]["a"],
        serde_json::json!([1, 2, 3])
    );
    assert_eq!(round_tripped.metadata["region"], "asia");
    assert_eq!(round_tripped.metadata["env"], "test");
}

#[tokio::test]
async fn test_iggy_publish_multiple_categories() {
    let bus = iggy_bus!("multi_cat");

    bus.publish("market", "forex", "A", "test", serde_json::json!({}))
        .await
        .unwrap();
    bus.publish("system", "deploy", "B", "test", serde_json::json!({}))
        .await
        .unwrap();
    bus.publish("market", "crypto", "C", "test", serde_json::json!({}))
        .await
        .unwrap();

    let all = bus.list_events(None, 100).await.unwrap();
    assert!(all.len() >= 3);

    let market = bus.list_events(Some("market"), 100).await.unwrap();
    assert!(market.iter().all(|e| e.category == "market"));
    assert!(market.iter().any(|e| e.summary == "A"));
    assert!(market.iter().any(|e| e.summary == "C"));
}

#[tokio::test]
async fn test_iggy_provider_info() {
    let bus = iggy_bus!("info");

    bus.publish("test", "a", "Info test", "test", serde_json::json!({}))
        .await
        .unwrap();

    let info = bus.info().await.unwrap();
    assert_eq!(info.provider, "iggy");
    assert!(info.messages >= 1);
}

#[tokio::test]
async fn test_iggy_health_check() {
    let bus = iggy_bus!("health");
    assert!(bus.health().await.unwrap());
}

#[tokio::test]
async fn test_iggy_durable_subscription_receives_events() {
    let provider = match try_iggy_provider("durable_recv").await {
        Some(p) => p,
        None => return,
    };

    let mut sub = provider
        .subscribe_durable("recv-consumer", "test.durable_recv.market.>")
        .await
        .unwrap();

    let event = Event::new(
        "test.durable_recv.market.forex",
        "market",
        "Durable delivery",
        "test",
        serde_json::json!({"k": 1}),
    );
    provider.publish(&event).await.unwrap();

    let received = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next_manual_ack())
        .await
        .expect("timed out waiting for delivery")
        .unwrap()
        .expect("subscription must yield the event");

    assert_eq!(received.received.event.id, event.id);
    received.ack().await.unwrap();

    let _ = provider.unsubscribe("recv-consumer").await;
}

#[tokio::test]
async fn test_iggy_durable_offset_persists_across_resubscribe() {
    let provider = match try_iggy_provider("durable_offset").await {
        Some(p) => p,
        None => return,
    };
    let filter = "test.durable_offset.market.>";

    // First cycle: consume two events with acks.
    let mut sub = provider
        .subscribe_durable("offset-consumer", filter)
        .await
        .unwrap();
    let a = Event::new(
        "test.durable_offset.market.a",
        "market",
        "event-a",
        "test",
        serde_json::json!({}),
    );
    let b = Event::new(
        "test.durable_offset.market.b",
        "market",
        "event-b",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&a).await.unwrap();
    provider.publish(&b).await.unwrap();

    let first = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next_manual_ack())
        .await
        .expect("timed out waiting for a")
        .unwrap()
        .expect("must deliver a");
    assert_eq!(first.received.event.summary, "event-a");
    first.ack().await.unwrap();

    let second = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next_manual_ack())
        .await
        .expect("timed out waiting for b")
        .unwrap()
        .expect("must deliver b");
    assert_eq!(second.received.event.summary, "event-b");
    second.ack().await.unwrap();

    // Second cycle: a fresh subscription under the same consumer name must
    // resume after b — the stored offset survives the disconnect.
    drop(sub);
    let c = Event::new(
        "test.durable_offset.market.c",
        "market",
        "event-c",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&c).await.unwrap();

    let mut resumed = provider
        .subscribe_durable("offset-consumer", filter)
        .await
        .unwrap();
    let next = tokio::time::timeout(std::time::Duration::from_secs(5), resumed.next_manual_ack())
        .await
        .expect("timed out waiting for c")
        .unwrap()
        .expect("must deliver c");

    assert_eq!(
        next.received.event.summary, "event-c",
        "acked events must not be redelivered after resubscribe"
    );
    next.ack().await.unwrap();

    let _ = provider.unsubscribe("offset-consumer").await;
}

#[tokio::test]
async fn test_iggy_unsubscribe_and_resubscribe_replays_from_start() {
    let provider = match try_iggy_provider("group_rebuild").await {
        Some(p) => p,
        None => return,
    };
    let filter = "test.group_rebuild.market.>";

    let event = Event::new(
        "test.group_rebuild.market.x",
        "market",
        "rebuild-me",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&event).await.unwrap();

    let mut sub = provider
        .subscribe_durable("rebuild-consumer", filter)
        .await
        .unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next_manual_ack())
        .await
        .expect("timed out")
        .unwrap()
        .expect("must deliver");
    assert_eq!(got.received.event.summary, "rebuild-me");
    got.ack().await.unwrap();

    // Deleting the group drops its offsets; a same-name group starts over.
    provider.unsubscribe("rebuild-consumer").await.unwrap();

    let mut fresh = provider
        .subscribe_durable("rebuild-consumer", filter)
        .await
        .unwrap();
    let replayed = tokio::time::timeout(std::time::Duration::from_secs(5), fresh.next_manual_ack())
        .await
        .expect("timed out")
        .unwrap()
        .expect("rebuilt group replays retained events");
    assert_eq!(replayed.received.event.summary, "rebuild-me");
    replayed.ack().await.unwrap();

    let _ = provider.unsubscribe("rebuild-consumer").await;
}

#[tokio::test]
async fn test_iggy_ephemeral_subscription_from_history() {
    let provider = match try_iggy_provider("ephemeral").await {
        Some(p) => p,
        None => return,
    };

    // Per-run topic keeps the assertion exact across re-runs on a live server.
    let category = format!("mkt{}", std::process::id());
    let event = Event::new(
        format!("test.ephemeral.{category}.tick"),
        &category,
        "ephemeral-payload",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&event).await.unwrap();

    let mut sub = provider
        .subscribe(&format!("test.ephemeral.{category}.>"))
        .await
        .unwrap();
    let received = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for delivery")
        .unwrap()
        .expect("ephemeral subscription with All policy replays history");

    assert_eq!(received.event.id, event.id);
    assert_eq!(
        received.sequence, 0,
        "offset starts at 0 for the first event"
    );
}

#[tokio::test]
async fn test_iggy_all_topics_filter() {
    let provider = match try_iggy_provider("all_topics").await {
        Some(p) => p,
        None => return,
    };

    // Per-run categories keep the exact-count assertion stable on re-runs.
    let run = std::process::id();
    let market_cat = format!("market{run}");
    let system_cat = format!("system{run}");
    let market = Event::new(
        format!("test.all_topics.{market_cat}.m1"),
        &market_cat,
        "from-market",
        "test",
        serde_json::json!({}),
    );
    let system = Event::new(
        format!("test.all_topics.{system_cat}.s1"),
        &system_cat,
        "from-system",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&market).await.unwrap();
    provider.publish(&system).await.unwrap();

    // Category-wildcard filter spans every topic; client-side matching
    // narrows to our two subjects.
    let mut sub = provider.subscribe("test.all_topics.>").await.unwrap();
    let mut summaries = Vec::new();
    for _ in 0..2 {
        let mut received = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
            .await
            .expect("timed out waiting for delivery")
            .unwrap()
            .expect("both topics deliver");
        // Skip deliveries from earlier runs on the same server.
        while received.event.summary != "from-market" && received.event.summary != "from-system" {
            received = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
                .await
                .expect("timed out waiting for delivery")
                .unwrap()
                .expect("both topics deliver");
        }
        summaries.push(received.event.summary);
    }
    summaries.sort();
    assert_eq!(summaries, vec!["from-market", "from-system"]);
}

#[tokio::test]
async fn test_iggy_deliver_policy_new_skips_history() {
    let provider = match try_iggy_provider("policy_new").await {
        Some(p) => p,
        None => return,
    };

    let old = Event::new(
        "test.policy_new.market.old",
        "market",
        "historical",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&old).await.unwrap();

    let mut sub = provider
        .subscribe_with_options(
            "test.policy_new.market.>",
            &SubscribeOptions {
                deliver_policy: DeliverPolicy::New,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let fresh = Event::new(
        "test.policy_new.market.fresh",
        "market",
        "after-subscribe",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&fresh).await.unwrap();

    let received = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
        .await
        .expect("timed out waiting for delivery")
        .unwrap()
        .expect("New policy delivers the post-subscribe event");
    assert_eq!(received.event.summary, "after-subscribe");
}

#[tokio::test]
async fn test_iggy_concurrent_publish() {
    let bus = std::sync::Arc::new(iggy_bus!("concurrent"));
    // Per-run category keeps the exact-count assertion stable on re-runs.
    let category = format!("load{}", std::process::id());
    let mut handles = Vec::new();

    for i in 0..20 {
        let bus = bus.clone();
        let category = category.clone();
        handles.push(tokio::spawn(async move {
            bus.publish(
                &category,
                &format!("topic.{i}"),
                &format!("Event {i}"),
                "test",
                serde_json::json!({"index": i}),
            )
            .await
            .unwrap()
        }));
    }

    for handle in handles {
        handle.await.unwrap();
    }

    let events = bus.list_events(Some(&category), 100).await.unwrap();
    assert_eq!(events.len(), 20);
}

#[tokio::test]
async fn test_iggy_expected_sequence_fails_closed() {
    let provider = match try_iggy_provider("fail_closed").await {
        Some(p) => p,
        None => return,
    };

    let event = Event::new(
        "test.fail_closed.market.x",
        "market",
        "rejected",
        "test",
        serde_json::json!({}),
    );
    let opts = PublishOptions {
        expected_sequence: Some(42),
        ..Default::default()
    };

    let err = provider
        .publish_with_options(&event, &opts)
        .await
        .expect_err("expected_sequence must be rejected, not silently ignored");
    assert!(err.to_string().contains("expected_sequence"));
}

#[tokio::test]
async fn test_iggy_last_per_subject_fails_closed() {
    let provider = match try_iggy_provider("lps").await {
        Some(p) => p,
        None => return,
    };

    let result = provider
        .subscribe_durable_with_options(
            "lps-consumer",
            "test.lps.market.>",
            &SubscribeOptions {
                deliver_policy: DeliverPolicy::LastPerSubject,
                ..Default::default()
            },
        )
        .await;
    let err = match result {
        Ok(_) => panic!("LastPerSubject must be rejected, not silently ignored"),
        Err(e) => e,
    };
    assert!(err.to_string().contains("LastPerSubject"));
}

#[tokio::test]
async fn test_iggy_subject_outside_prefix_fails() {
    let provider = match try_iggy_provider("prefix_guard").await {
        Some(p) => p,
        None => return,
    };

    let event = Event::new(
        "elsewhere.market.x",
        "market",
        "wrong prefix",
        "test",
        serde_json::json!({}),
    );
    let err = provider
        .publish(&event)
        .await
        .expect_err("subjects outside the prefix must be rejected");
    assert!(err.to_string().contains("prefix"));
}

#[tokio::test]
async fn test_iggy_unsubscribe_missing_group_is_ok() {
    let provider = match try_iggy_provider("unsub_missing").await {
        Some(p) => p,
        None => return,
    };

    provider
        .unsubscribe("never-created-consumer")
        .await
        .expect("deleting a group that never existed must be a no-op");
}

/// Helper: unique category per test run (re-run safety on a live server)
fn run_tag() -> String {
    format!("t{}", std::process::id())
}

/// Helper: wait for the next delivery with a timeout, skipping stale
/// events from earlier suite runs (matched by expected summary).
macro_rules! next_matching {
    ($sub:expr, $want:expr) => {{
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for {:?}", $want);
            }
            let received = tokio::time::timeout(remaining, $sub.next_manual_ack())
                .await
                .expect("timed out waiting for delivery")
                .unwrap()
                .expect("subscription must yield an event");
            if received.received.event.summary == $want {
                break received;
            }
        }
    }};
}

#[tokio::test]
async fn test_iggy_ordering_within_topic() {
    let provider = match try_iggy_provider("ordering").await {
        Some(p) => p,
        None => return,
    };
    let tag = run_tag();

    let mut sub = provider
        .subscribe_durable("order-consumer", &format!("test.ordering.{tag}.>"))
        .await
        .unwrap();

    let expected: Vec<String> = (0..10).map(|i| format!("seq-{i}")).collect();
    for summary in &expected {
        let e = Event::new(
            format!("test.ordering.{tag}.tick"),
            &tag,
            summary,
            "test",
            serde_json::json!({"i": 1}),
        );
        provider.publish(&e).await.unwrap();
    }

    for summary in &expected {
        let got = next_matching!(sub, summary.clone());
        got.ack().await.unwrap();
    }

    let _ = provider.unsubscribe("order-consumer").await;
}

#[tokio::test]
async fn test_iggy_durable_offset_survives_new_connection() {
    let filter = "test.reconn.market.>";

    // First connection: consume and ack one event, then drop everything.
    {
        let provider = match try_iggy_provider("reconn").await {
            Some(p) => p,
            None => return,
        };
        let mut sub = provider
            .subscribe_durable("reconn-consumer", filter)
            .await
            .unwrap();
        let a = Event::new(
            "test.reconn.market.a",
            "market",
            "reconn-a",
            "test",
            serde_json::json!({}),
        );
        provider.publish(&a).await.unwrap();
        let got = next_matching!(sub, "reconn-a");
        got.ack().await.unwrap();
        // provider + subscription dropped here: connection closed
    }

    // Second connection, brand-new provider: the offset lives on the server.
    let provider = match try_iggy_provider("reconn").await {
        Some(p) => p,
        None => return,
    };
    let b = Event::new(
        "test.reconn.market.b",
        "market",
        "reconn-b",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&b).await.unwrap();

    let mut sub = provider
        .subscribe_durable("reconn-consumer", filter)
        .await
        .unwrap();
    let got = next_matching!(sub, "reconn-b");
    got.ack().await.unwrap();

    let _ = provider.unsubscribe("reconn-consumer").await;
}

#[tokio::test]
async fn test_iggy_unacked_event_redelivers_on_rejoin() {
    let provider = match try_iggy_provider("redelivery").await {
        Some(p) => p,
        None => return,
    };
    let filter = "test.redelivery.market.>";
    let tag = run_tag();

    let e = Event::new(
        format!("test.redelivery.market.{tag}"),
        "market",
        "needs-redelivery",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&e).await.unwrap();

    // First subscription: receive but never ack.
    {
        let mut sub = provider
            .subscribe_durable("redeliver-consumer", filter)
            .await
            .unwrap();
        let got = next_matching!(sub, "needs-redelivery");
        assert_eq!(got.received.event.id, e.id);
        // dropped without ack — offset must not have advanced
    }

    // Rejoin under the same consumer name: at-least-once redelivery.
    let mut sub = provider
        .subscribe_durable("redeliver-consumer", filter)
        .await
        .unwrap();
    let got = next_matching!(sub, "needs-redelivery");
    assert_eq!(got.received.event.id, e.id);
    got.ack().await.unwrap();

    let _ = provider.unsubscribe("redeliver-consumer").await;
}

#[tokio::test]
async fn test_iggy_deliver_last_seeds_head() {
    let provider = match try_iggy_provider("last_seed").await {
        Some(p) => p,
        None => return,
    };
    let tag = run_tag();

    for i in 0..3 {
        let e = Event::new(
            format!("test.last_seed.market.{tag}.{i}"),
            "market",
            format!("old-{i}"),
            "test",
            serde_json::json!({}),
        );
        provider.publish(&e).await.unwrap();
    }

    let mut sub = provider
        .subscribe_with_options(
            &format!("test.last_seed.market.{tag}.>"),
            &SubscribeOptions {
                deliver_policy: DeliverPolicy::Last,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // Very first delivery is the head (old-2); nothing older comes first.
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
        .await
        .expect("timed out")
        .unwrap()
        .expect("Last must seed the head");
    assert_eq!(got.event.summary, "old-2");

    // And the subscription keeps flowing.
    let fresh = Event::new(
        format!("test.last_seed.market.{tag}.fresh"),
        "market",
        "fresh-after-last",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&fresh).await.unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
        .await
        .expect("timed out")
        .unwrap()
        .expect("subscription must keep flowing after the seed");
    assert_eq!(got.event.summary, "fresh-after-last");
}

#[tokio::test]
async fn test_iggy_deliver_by_start_sequence() {
    let provider = match try_iggy_provider("by_seq").await {
        Some(p) => p,
        None => return,
    };
    let tag = run_tag();

    for i in 0..3 {
        let e = Event::new(
            format!("test.by_seq.market.{tag}.{i}"),
            "market",
            format!("seq-{i}"),
            "test",
            serde_json::json!({}),
        );
        provider.publish(&e).await.unwrap();
    }

    let mut sub = provider
        .subscribe_with_options(
            &format!("test.by_seq.market.{tag}.>"),
            &SubscribeOptions {
                deliver_policy: DeliverPolicy::ByStartSequence { sequence: 1 },
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let got = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
        .await
        .expect("timed out")
        .unwrap()
        .expect("ByStartSequence must deliver from the pinned offset");
    assert_eq!(got.event.summary, "seq-1", "offset 1 is the second event");
    assert_eq!(got.sequence, 1);
}

#[tokio::test]
async fn test_iggy_deliver_by_start_time() {
    let provider = match try_iggy_provider("by_time").await {
        Some(p) => p,
        None => return,
    };
    let tag = run_tag();

    let early = Event::new(
        format!("test.by_time.market.{tag}.early"),
        "market",
        "early",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&early).await.unwrap();

    // The timestamp filter compares against SERVER-side receive stamps
    // (microsecond resolution), while the cut comes from the host clock —
    // a containerized server can skew a few milliseconds from the host. A
    // cut taken at the MIDPOINT of a 1.5s gap leaves ~750ms of margin on
    // both sides, far beyond clock skew, so the early/late split is
    // deterministic in both directions.
    tokio::time::sleep(std::time::Duration::from_millis(750)).await;
    let cut = crate_timestamp_now();
    tokio::time::sleep(std::time::Duration::from_millis(750)).await;

    let late = Event::new(
        format!("test.by_time.market.{tag}.late"),
        "market",
        "late",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&late).await.unwrap();

    let mut sub = provider
        .subscribe_with_options(
            &format!("test.by_time.market.{tag}.>"),
            &SubscribeOptions {
                deliver_policy: DeliverPolicy::ByStartTime { timestamp: cut },
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let got = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
        .await
        .expect("timed out")
        .unwrap()
        .expect("ByStartTime must deliver post-cutoff events");
    assert_eq!(
        got.event.summary, "late",
        "events before the cutoff are skipped"
    );
}

/// Current Unix time in millis (matches `Event::timestamp` semantics)
fn crate_timestamp_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test]
async fn test_iggy_sub_wildcard_filter_narrows_topic() {
    let provider = match try_iggy_provider("narrow").await {
        Some(p) => p,
        None => return,
    };
    let tag = run_tag();

    // Same topic (category `market`), different tails.
    let forex = Event::new(
        format!("test.narrow.market.{tag}.forex"),
        "market",
        "forex-tick",
        "test",
        serde_json::json!({}),
    );
    let crypto = Event::new(
        format!("test.narrow.market.{tag}.crypto"),
        "market",
        "crypto-tick",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&crypto).await.unwrap();
    provider.publish(&forex).await.unwrap();

    // Narrow filter: only the forex tail matches.
    let mut sub = provider
        .subscribe(&format!("test.narrow.market.{tag}.forex"))
        .await
        .unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
        .await
        .expect("timed out")
        .unwrap()
        .expect("narrow filter must still deliver matching events");
    assert_eq!(got.event.summary, "forex-tick");
}

#[tokio::test]
async fn test_iggy_sanitized_category_and_consumer_names() {
    let provider = match try_iggy_provider("sanitize").await {
        Some(p) => p,
        None => return,
    };
    let tag = run_tag();

    // Category with characters Iggy forbids; the provider sanitizes both
    // the publish route and the filter route identically.
    let e = Event::new(
        format!("test.sanitize.{tag}/usd cny.rate"),
        format!("{tag}/usd cny"),
        "sanitized-route",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&e).await.unwrap();

    let history = provider
        .history(Some(&format!("test.sanitize.{tag}/usd cny.>")), 10)
        .await
        .unwrap();
    assert!(
        history.iter().any(|ev| ev.id == e.id),
        "sanitized category must route publish and filter to the same topic"
    );

    // Consumer names with dots are sanitized symmetrically.
    let mut sub = provider
        .subscribe_durable(
            "sanitize.consumer.v1",
            &format!("test.sanitize.{tag}/usd cny.>"),
        )
        .await
        .unwrap();
    let got = next_matching!(sub, "sanitized-route");
    got.ack().await.unwrap();
    provider.unsubscribe("sanitize.consumer.v1").await.unwrap();
}

#[tokio::test]
async fn test_iggy_unreachable_server_fails_fast() {
    let start = std::time::Instant::now();
    let result = IggyProvider::connect(IggyConfig {
        server_address: "127.0.0.1:1".to_string(), // closed port
        connect_timeout_secs: 3,
        ..Default::default()
    })
    .await;

    match result {
        Err(err) => {
            let msg = err.to_string();
            assert!(
                msg.contains("127.0.0.1:1"),
                "connection errors must name the server: {msg}"
            );
        }
        Ok(_) => panic!("connect to a closed port must fail"),
    }
    assert!(
        start.elapsed() < std::time::Duration::from_secs(15),
        "connection failure must be fast, took {:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn test_iggy_history_limit_returns_most_recent() {
    let provider = match try_iggy_provider("hist_limit").await {
        Some(p) => p,
        None => return,
    };
    let tag = run_tag();

    for i in 0..5 {
        let e = Event::new(
            format!("test.hist_limit.market.{tag}.{i}"),
            "market",
            format!("h-{i}"),
            "test",
            serde_json::json!({}),
        );
        provider.publish(&e).await.unwrap();
    }

    let recent = provider
        .history(Some(&format!("test.hist_limit.market.{tag}.>")), 3)
        .await
        .unwrap();
    assert_eq!(recent.len(), 3);
    let summaries: Vec<String> = recent.iter().map(|e| e.summary.clone()).collect();
    assert_eq!(
        summaries,
        vec!["h-2", "h-3", "h-4"],
        "must keep the most recent tail"
    );
}

#[tokio::test]
async fn test_iggy_history_on_fresh_stream_is_empty() {
    let provider = match try_iggy_provider("hist_empty").await {
        Some(p) => p,
        None => return,
    };
    let history = provider.history(None, 10).await.unwrap();
    assert!(history.is_empty(), "a fresh stream has no history");
}

#[tokio::test]
async fn test_iggy_all_topics_group_offsets_independent() {
    let provider = match try_iggy_provider("per_topic").await {
        Some(p) => p,
        None => return,
    };
    let tag = run_tag();
    let filter = "test.per_topic.>"; // AllTopics: category token is a wildcard

    let m = Event::new(
        format!("test.per_topic.market.{tag}"),
        "market",
        "per-topic-market",
        "test",
        serde_json::json!({}),
    );
    let s = Event::new(
        format!("test.per_topic.system.{tag}"),
        "system",
        "per-topic-system",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&m).await.unwrap();
    provider.publish(&s).await.unwrap();

    // One group over both topics: consume and ack each exactly once.
    {
        let mut sub = provider
            .subscribe_durable("per-topic-consumer", filter)
            .await
            .unwrap();
        let first = next_matching!(sub, "per-topic-market");
        first.ack().await.unwrap();
        let second = next_matching!(sub, "per-topic-system");
        second.ack().await.unwrap();
    }

    // Resubscribe: both topic offsets persisted — nothing redelivers; the
    // next fresh event on either topic is the next delivery.
    let fresh = Event::new(
        format!("test.per_topic.market.{tag}.fresh"),
        "market",
        "per-topic-fresh",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&fresh).await.unwrap();

    let mut sub = provider
        .subscribe_durable("per-topic-consumer", filter)
        .await
        .unwrap();
    let got = next_matching!(sub, "per-topic-fresh");
    got.ack().await.unwrap();

    let _ = provider.unsubscribe("per-topic-consumer").await;
}

#[tokio::test]
async fn test_iggy_shared_group_exactly_one_member_receives() {
    // Group membership is per client connection: real-world group members
    // are separate processes/connections, so the test uses two providers.
    let provider_a = match try_iggy_provider("shared_group").await {
        Some(p) => p,
        None => return,
    };
    let provider_b = match try_iggy_provider("shared_group").await {
        Some(p) => p,
        None => return,
    };
    let tag = run_tag();
    let filter = format!("test.shared_group.market.{tag}.>");

    let mut member_a = provider_a
        .subscribe_durable("shared-workers", &filter)
        .await
        .unwrap();
    let mut member_b = provider_b
        .subscribe_durable("shared-workers", &filter)
        .await
        .unwrap();

    let e = Event::new(
        format!("test.shared_group.market.{tag}.one"),
        "market",
        "single-delivery",
        "test",
        serde_json::json!({}),
    );
    provider_a.publish(&e).await.unwrap();

    // Exactly one member gets the message; the other idles without error
    // (NO_ASSIGNED_PARTITION sentinel, no duplicate delivery).
    let got_a = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        member_a.next_manual_ack(),
    )
    .await;
    let got_b = tokio::time::timeout(
        std::time::Duration::from_secs(4),
        member_b.next_manual_ack(),
    )
    .await;

    let deliveries = [got_a, got_b]
        .into_iter()
        .filter_map(|r| match r {
            Ok(Ok(Some(pending))) => Some(pending),
            _ => None,
        })
        .count();

    assert_eq!(
        deliveries, 1,
        "a single event must be delivered to exactly one group member"
    );

    let _ = provider_a.unsubscribe("shared-workers").await;
}

#[tokio::test]
async fn test_iggy_two_ephemeral_subs_independent() {
    let provider = match try_iggy_provider("eph_indep").await {
        Some(p) => p,
        None => return,
    };
    let tag = run_tag();
    let filter = format!("test.eph_indep.market.{tag}.>");

    // Both subscribe BEFORE the publish: each cursor is independent, so
    // each receives its own copy.
    let mut sub1 = provider.subscribe(&filter).await.unwrap();
    let mut sub2 = provider.subscribe(&filter).await.unwrap();

    let e = Event::new(
        format!("test.eph_indep.market.{tag}.x"),
        "market",
        "fan-out",
        "test",
        serde_json::json!({}),
    );
    provider.publish(&e).await.unwrap();

    for (name, sub) in [("sub1", &mut sub1), ("sub2", &mut sub2)] {
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
            .await
            .unwrap_or_else(|_| panic!("{name} timed out"))
            .unwrap()
            .expect("{name} must receive the event");
        assert_eq!(got.event.id, e.id, "{name} got the wrong event");
    }
}
#[tokio::test]
async fn test_iggy_info_counts_consumer_groups() {
    let provider = match try_iggy_provider("info_groups").await {
        Some(p) => p,
        None => return,
    };
    let tag = run_tag();

    let before = provider.info().await.unwrap();

    let _guard = provider
        .subscribe_durable(
            "info-group-consumer",
            &format!("test.info_groups.market.{tag}.>"),
        )
        .await
        .unwrap();

    let after = provider.info().await.unwrap();
    assert!(
        after.consumers > before.consumers,
        "creating a durable subscription must register a consumer group (before {}, after {})",
        before.consumers,
        after.consumers
    );

    let _ = provider.unsubscribe("info-group-consumer").await;
}

#[tokio::test]
async fn test_iggy_foreign_poison_message_is_skipped() {
    use iggy::prelude::{
        IggyClientBuilder, IggyMessage, MessageClient as _, Partitioning, UserClient as _,
    };

    let provider = match try_iggy_provider("poison").await {
        Some(p) => p,
        None => return,
    };
    let tag = run_tag();

    // A good event first, so the topic exists through the provider's mapping.
    let good = Event::new(
        format!("test.poison.market.{tag}.good"),
        "market",
        "good-event",
        "test",
        serde_json::json!({"n": 1}),
    );
    provider.publish(&good).await.unwrap();

    // Foreign writer: raw SDK writes a NON-JSON payload into the same
    // stream + topic ("market" category) — bytes no Event can decode from.
    {
        let foreign = IggyClientBuilder::new()
            .with_tcp()
            .with_server_address("127.0.0.1:5102".to_string())
            .build()
            .unwrap();
        foreign.login_user("iggy", "iggy").await.unwrap();
        let stream =
            iggy::prelude::Identifier::named(&format!("test_events_poison_{}", std::process::id()))
                .unwrap();
        let topic = iggy::prelude::Identifier::named("market").unwrap();
        let poison = IggyMessage::builder()
            .payload(bytes::Bytes::from_static(b"\x00\x81not-json{{"))
            .build()
            .unwrap();
        foreign
            .send_messages(
                &stream,
                &topic,
                &Partitioning::partition_id(0),
                &mut [poison],
            )
            .await
            .unwrap();
    }

    // A good event AFTER the poison: history must return both good events
    // and skip the undecodable frame entirely.
    let good2 = Event::new(
        format!("test.poison.market.{tag}.good2"),
        "market",
        "good-event-2",
        "test",
        serde_json::json!({"n": 2}),
    );
    provider.publish(&good2).await.unwrap();

    let history = provider
        .history(Some(&format!("test.poison.market.{tag}.>")), 10)
        .await
        .unwrap();
    let summaries: Vec<&str> = history.iter().map(|e| e.summary.as_str()).collect();
    assert_eq!(
        summaries,
        vec!["good-event", "good-event-2"],
        "poison skipped in history"
    );

    // And a live subscription flows past the poison without wedging.
    let mut sub = provider
        .subscribe(&format!("test.poison.market.{tag}.>"))
        .await
        .unwrap();
    let got = tokio::time::timeout(std::time::Duration::from_secs(5), sub.next())
        .await
        .expect("subscription wedged on poison message")
        .unwrap()
        .expect("good events flow past the poison");
    assert!(got.event.summary.starts_with("good-event"));
}
