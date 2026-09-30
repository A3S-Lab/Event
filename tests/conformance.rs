//! Cross-provider conformance suite — one set of deep scenarios, every backend
//!
//! First-principles structure: the `EventProvider` trait makes a *contract*,
//! and the contract has capability tiers, not per-provider quirks:
//!
//! - **Tier 0 — every provider** (memory included): publish/history
//!   round-trips with full envelope fidelity, subscription fan-out and
//!   filter isolation, sequential per-category ordering, concurrent-publish
//!   no-loss/no-dup, tail filtering, options plumbing, counts/info/health.
//! - **Tier 1 — providers with server-side persistence** (nats, iggy):
//!   unacked redelivery, resume across a NEW connection, consumer rebuild
//!   replay, late-subscriber full replay, competing consumers across
//!   connections with exactly-once delivery per event.
//!
//! Scenarios are prefix-agnostic: all subjects and filters are built through
//! `build_subject`/`category_subject`, so the same code runs against any
//! backend regardless of its subject namespace.
//!
//! Each scenario is its own `#[tokio::test]` per provider. Backends that are
//! not compiled in (feature gates) or not running (server down) skip
//! cleanly.

use a3s_event::{
    DeliverPolicy, Event, EventBus, EventProvider, PublishOptions, SubscribeOptions,
    SubscriptionFilter,
};
use std::sync::Arc;
use std::time::Duration;

/// Factory: creates a provider bound to a per-scenario namespace; `None` = skip.
/// Called repeatedly for tier-1 scenarios: each call is a fresh connection to
/// the SAME underlying stream.
type ProviderFactory = Box<
    dyn Fn(&str) -> Pin<Box<dyn Future<Output = Option<Arc<dyn EventProvider>>> + Send>>
        + Send
        + Sync,
>;

/// A scenario's view of the backend under test
struct Suite {
    create: ProviderFactory,
}

impl Suite {
    async fn provider(&self, tag: &str) -> Provider {
        match (self.create)(tag).await {
            Some(p) => Provider::Some(p),
            None => Provider::Skip,
        }
    }
}

enum Provider {
    Some(Arc<dyn EventProvider>),
    Skip,
}

use std::future::Future;
use std::pin::Pin;

/// Skip-guard helper: expands to an early `return` on `Provider::Skip`
macro_rules! some {
    ($p:expr) => {
        match $p {
            Provider::Some(p) => p,
            Provider::Skip => return,
        }
    };
}

// ---------------------------------------------------------------------------
// Tier 0 scenarios
// ---------------------------------------------------------------------------

/// T0.1 — publish → history round-trip with full envelope fidelity across
/// several categories, plus subject format, uniqueness, and counts.
async fn scenario_lifecycle_and_fidelity(suite: &Suite) {
    let p = some!(suite.provider("t0lifecycle").await);
    let bus = EventBus::from_provider(Arc::clone(&p));
    let prefix = p.subject_prefix().to_string();

    let mut published_ids = Vec::new();
    for cat in ["market", "system", "fleet"] {
        for i in 0..4 {
            let event = Event::typed(
                p.build_subject(cat, &format!("tick.{i}")),
                cat,
                format!("{cat}.tick"),
                2,
                format!("{cat} tick {i}"),
                "conformance",
                serde_json::json!({"i": i, "cat": cat}),
            )
            .with_metadata("run", "t0")
            .with_metadata("idx", i.to_string());
            bus.publish_event(&event).await.unwrap();
            published_ids.push(event.id);
        }
    }

    // Per-category history: exactly this category's events, fully faithful.
    for cat in ["market", "system", "fleet"] {
        let events = bus.list_events(Some(cat), 100).await.unwrap();
        assert_eq!(events.len(), 4, "category {cat} history");
        // History ordering across providers is NOT part of the contract
        // (memory returns newest-first, brokers oldest-first); assert the
        // set faithfully round-trips.
        let mut seen_indices = Vec::new();
        for event in &events {
            assert_eq!(event.category, cat);
            assert_eq!(event.event_type, format!("{cat}.tick"));
            assert_eq!(event.version, 2);
            assert_eq!(event.source, "conformance");
            assert_eq!(event.payload["cat"], cat);
            assert_eq!(event.metadata["run"], "t0");
            assert!(event.subject.starts_with(&format!("{prefix}.{cat}.")));
            seen_indices.push(event.payload["i"].as_u64().expect("payload i"));
        }
        seen_indices.sort();
        assert_eq!(seen_indices, vec![0, 1, 2, 3], "category {cat} set");
    }

    // Full history: every published id exactly once.
    let all = bus.list_events(None, 100).await.unwrap();
    let mut ids: Vec<&str> = all.iter().map(|e| e.id.as_str()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), published_ids.len(), "no loss, no duplication");

    // Counts aggregate to the same total.
    let counts = bus.counts(100).await.unwrap();
    assert_eq!(counts.total as usize, published_ids.len());
    assert_eq!(
        counts.categories.values().sum::<u64>() as usize,
        published_ids.len()
    );
}

/// T0.2 — three subscribers with overlapping-interest filters: market-only,
/// system-only, and all-categories. Isolation of matches, fan-out of the
/// catch-all, subscription registry lifecycle, unknown-subscriber error.
async fn scenario_fanout_isolation(suite: &Suite) {
    let p = some!(suite.provider("t0fanout").await);
    let bus = EventBus::from_provider(Arc::clone(&p));

    let (market_subj, system_subj) = (p.category_subject("market"), p.category_subject("system"));

    for (id, subjects) in [
        ("market-only", vec![market_subj.clone()]),
        ("system-only", vec![system_subj.clone()]),
        ("everything", vec![market_subj.clone(), system_subj.clone()]),
    ] {
        bus.update_subscription(SubscriptionFilter {
            subscriber_id: id.to_string(),
            subjects,
            durable: false,
            options: None,
        })
        .await
        .unwrap();
    }

    // Registry state is queryable before anything flows.
    assert_eq!(bus.list_subscriptions().await.len(), 3);
    assert!(bus.get_subscription("market-only").await.is_some());

    let market_event = Event::new(
        p.build_subject("market", "forex"),
        "market",
        "fan-market",
        "test",
        serde_json::json!({}),
    );
    let system_event = Event::new(
        p.build_subject("system", "deploy"),
        "system",
        "fan-system",
        "test",
        serde_json::json!({}),
    );

    // Open the receivers BEFORE publishing: broadcast-only backends
    // (memory) deliver nothing to subscriptions created after the publish.
    let mut subs_market = bus.create_subscriber("market-only").await.unwrap();
    let mut subs_system = bus.create_subscriber("system-only").await.unwrap();
    let mut subs_every = bus.create_subscriber("everything").await.unwrap();
    assert_eq!(subs_every.len(), 2, "one subscription per filter subject");

    bus.publish_event(&market_event).await.unwrap();
    bus.publish_event(&system_event).await.unwrap();

    let deadline = Duration::from_secs(5);
    let mut m1 = subs_market.remove(0);
    let mut s1 = subs_system.remove(0);
    let (mut e1, mut e2) = (subs_every.remove(0), subs_every.remove(0));

    let (got_m, got_s, got_e) = tokio::join!(
        recv_summary(&mut m1, "fan-market", deadline),
        recv_summary(&mut s1, "fan-system", deadline),
        collect_summaries(&mut e1, &mut e2, deadline),
    );
    assert_eq!(got_m, vec!["fan-market"], "market-only gets only market");
    assert_eq!(got_s, vec!["fan-system"], "system-only gets only system");
    let mut all_every = got_e;
    all_every.sort();
    assert_eq!(
        all_every,
        vec!["fan-market", "fan-system"],
        "catch-all fans out"
    );

    bus.remove_subscription("market-only").await.unwrap();
    assert!(bus.get_subscription("market-only").await.is_none());
    let err = match bus.create_subscriber("market-only").await {
        Ok(_) => panic!("unknown subscriber must not resolve"),
        Err(e) => e,
    };
    assert!(
        err.to_string().contains("not found"),
        "unknown subscriber: {err}"
    );
}

async fn recv_summary(
    sub: &mut Box<dyn a3s_event::Subscription>,
    want: &str,
    deadline: Duration,
) -> Vec<String> {
    let got = tokio::time::timeout(deadline, sub.next()).await;
    match got {
        Ok(Ok(Some(received))) if received.event.summary == want => vec![want.to_string()],
        other => panic!("expected {want}, got {other:?}"),
    }
}

async fn collect_summaries(
    a: &mut Box<dyn a3s_event::Subscription>,
    b: &mut Box<dyn a3s_event::Subscription>,
    deadline: Duration,
) -> Vec<String> {
    let mut out = Vec::new();
    let start = std::time::Instant::now();
    while out.len() < 2 && start.elapsed() < deadline {
        let remaining = deadline.saturating_sub(start.elapsed());
        let r = tokio::time::timeout(remaining, a.next()).await;
        if let Ok(Ok(Some(received))) = r {
            out.push(received.event.summary.clone());
            continue;
        }
        let r = tokio::time::timeout(remaining, b.next()).await;
        if let Ok(Ok(Some(received))) = r {
            out.push(received.event.summary.clone());
        }
    }
    out
}

/// T0.3 — sequential publishes on one category arrive in publish order.
async fn scenario_sequential_ordering(suite: &Suite) {
    let p = some!(suite.provider("t0order").await);
    let filter = p.category_subject("orders");

    let mut sub = p.subscribe_durable("order-check", &filter).await.unwrap();

    let expected: Vec<String> = (0..10).map(|i| format!("seq-{i}")).collect();
    for summary in &expected {
        let e = Event::new(
            p.build_subject("orders", "line"),
            "orders",
            summary,
            "test",
            serde_json::json!({}),
        );
        p.publish(&e).await.unwrap();
    }

    let deadline = Duration::from_secs(5);
    let start = std::time::Instant::now();
    let mut received = Vec::new();
    while received.len() < expected.len() && start.elapsed() < deadline {
        let got = tokio::time::timeout(deadline.saturating_sub(start.elapsed()), sub.next())
            .await
            .expect("ordering receive timed out")
            .unwrap()
            .expect("subscription yields");
        received.push(got.event.summary);
    }
    assert_eq!(received, expected, "per-category total order");
}

/// T0.4 — concurrent publishers: every event lands exactly once.
async fn scenario_concurrent_no_loss_no_dup(suite: &Suite) {
    let p = some!(suite.provider("t0concurrent").await);
    let bus = EventBus::from_provider(Arc::clone(&p));
    let p2 = Arc::clone(&p);

    let mut handles = Vec::new();
    for worker in 0..8 {
        let p = Arc::clone(&p2);
        handles.push(tokio::spawn(async move {
            for i in 0..10 {
                let e = Event::new(
                    p.build_subject("load", &format!("w{worker}")),
                    "load",
                    format!("w{worker}-{i}"),
                    "test",
                    serde_json::json!({"worker": worker, "i": i}),
                );
                p.publish(&e).await.unwrap();
            }
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let events = bus.list_events(Some("load"), 1000).await.unwrap();
    assert_eq!(events.len(), 80, "all concurrent publishes land");

    let mut ids: Vec<&str> = events.iter().map(|e| e.id.as_str()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 80, "no duplicates under concurrency");
}

/// T0.5 — tail filters narrow within one category: a forex-tail filter
/// must not deliver crypto-tail events sharing the same topic.
async fn scenario_tail_filters(suite: &Suite) {
    let p = some!(suite.provider("t0tails").await);
    let cat_subject = p.build_subject("prices", "base");

    let forex = Event::new(
        format!("{cat_subject}.forex"),
        "prices",
        "tail-forex",
        "test",
        serde_json::json!({}),
    );
    let crypto = Event::new(
        format!("{cat_subject}.crypto"),
        "prices",
        "tail-crypto",
        "test",
        serde_json::json!({}),
    );
    // Subscribe first: broadcast-only backends (memory) deliver nothing to
    // subscriptions created after the publish.
    let mut sub = p.subscribe(&format!("{cat_subject}.forex")).await.unwrap();
    p.publish(&crypto).await.unwrap();
    p.publish(&forex).await.unwrap();

    let got = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("tail filter receive timed out")
        .unwrap()
        .expect("matching tail must deliver");
    assert_eq!(got.event.summary, "tail-forex");
}

/// T0.6 — publish/subscribe options are accepted end-to-end (providers may
/// attach semantics like dedup, but must never reject the standard fields
/// other than the documented unsupported ones).
async fn scenario_options_plumbing(suite: &Suite) {
    let p = some!(suite.provider("t0options").await);

    let event = Event::new(
        p.build_subject("opts", "a"),
        "opts",
        "with-options",
        "test",
        serde_json::json!({}),
    );
    let mut sub = p
        .subscribe_with_options(
            &p.category_subject("opts"),
            &SubscribeOptions {
                deliver_policy: DeliverPolicy::All,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let seq = p
        .publish_with_options(
            &event,
            &PublishOptions {
                msg_id: Some("conf-msg-1".to_string()),
                timeout_secs: Some(5),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(seq > 0);
    let got = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("options receive timed out")
        .unwrap()
        .expect("subscription with options delivers");
    assert_eq!(got.event.summary, "with-options");
}

/// T0.7 — counts/info/health agree with what was published.
async fn scenario_counts_info_health(suite: &Suite) {
    let p = some!(suite.provider("t0stats").await);
    let bus = EventBus::from_provider(Arc::clone(&p));

    assert!(bus.health().await.unwrap(), "healthy after connect");

    for i in 0..5 {
        let e = Event::new(
            p.build_subject("stats", "a"),
            "stats",
            format!("s-{i}"),
            "test",
            serde_json::json!({}),
        );
        bus.publish_event(&e).await.unwrap();
    }

    let info = bus.info().await.unwrap();
    assert_eq!(info.provider, p.name());
    assert!(info.messages >= 5, "info reflects stored events");

    let counts = bus.counts(50).await.unwrap();
    assert_eq!(counts.total, 5);
}

// ---------------------------------------------------------------------------
// Tier 1 scenarios (persistent providers only)
// ---------------------------------------------------------------------------

/// T1.1 — an unacked delivery redelivers on rejoin (at-least-once).
async fn scenario_unacked_redelivery(suite: &Suite) {
    let p = some!(suite.provider("t1redeliver").await);
    let filter = p.category_subject("jobs");

    let e = Event::new(
        p.build_subject("jobs", "work"),
        "jobs",
        "redeliver-me",
        "test",
        serde_json::json!({}),
    );
    p.publish(&e).await.unwrap();

    // A short ack wait makes "unacked ⇒ redelivered" observable quickly on
    // backends that track in-flight state server-side (JetStream); offset
    // backends (iggy) redeliver on rejoin regardless and ignore the field.
    let worker_opts = SubscribeOptions {
        ack_wait_secs: Some(1),
        ..Default::default()
    };

    {
        let mut sub = p
            .subscribe_durable_with_options("worker-1", &filter, &worker_opts)
            .await
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), sub.next_manual_ack())
            .await
            .expect("first delivery timed out")
            .unwrap()
            .expect("must deliver");
        assert_eq!(got.received.event.summary, "redeliver-me");
        // dropped without ack
    }

    // Let the in-flight lease expire before rejoining.
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let mut sub = p
        .subscribe_durable_with_options("worker-1", &filter, &worker_opts)
        .await
        .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), sub.next_manual_ack())
        .await
        .expect("redelivery timed out")
        .unwrap()
        .expect("unacked must redeliver on rejoin");
    assert_eq!(got.received.event.summary, "redeliver-me");
    got.ack().await.unwrap();

    let _ = p.unsubscribe("worker-1").await;
}

/// T1.2 — acked progress persists across a brand-new connection: a consumer
/// name reconnecting to the same stream does not replay what it already
/// acked, and keeps flowing for events published after the reconnect.
/// (The complementary "unacked tail redelivers" property is T1.1.)
async fn scenario_resume_across_reconnect(suite: &Suite) {
    let worker_opts = SubscribeOptions {
        ack_wait_secs: Some(1),
        ..Default::default()
    };
    // The namespace must outlive the first connection; derive it up front.
    let (filter, step0_subject) = {
        let p = some!(suite.provider("t1resume").await);
        (
            p.category_subject("pipeline"),
            p.build_subject("pipeline", "step"),
        )
    };

    // First connection: consume and ack step-0. The PROVIDER (the
    // connection itself) must drop too — group membership is per
    // connection, and a still-attached old member keeps the partition
    // assignment away from the reconnecting one.
    {
        let p = some!(suite.provider("t1resume").await);
        let step0 = Event::new(
            step0_subject,
            "pipeline",
            "step-0",
            "test",
            serde_json::json!({"i": 0}),
        );
        p.publish(&step0).await.unwrap();

        let mut sub = p
            .subscribe_durable_with_options("pipeline-worker", &filter, &worker_opts)
            .await
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), sub.next_manual_ack())
            .await
            .expect("resume: first delivery timed out")
            .unwrap()
            .expect("must deliver step-0");
        assert_eq!(got.received.event.summary, "step-0");
        got.ack().await.unwrap();
        drop(sub);
        drop(p); // connection closed: membership released
    }

    // Brand-new connection, same consumer name, same stream.
    //
    // Server contract (Iggy): a dead member's partitions are reassigned
    // after consumer_group.rebalancing_timeout (default 30s; the test
    // server runs with 2s). JetStream durable pull consumers have no
    // sticky assignment and resume immediately.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let p = some!(suite.provider("t1resume").await);
    let mut sub = p
        .subscribe_durable_with_options("pipeline-worker", &filter, &worker_opts)
        .await
        .unwrap();

    // Events published after the reconnect must flow — and the acked
    // step-0 must NOT replay first.
    let step1 = Event::new(
        p.build_subject("pipeline", "step"),
        "pipeline",
        "step-1",
        "test",
        serde_json::json!({"i": 1}),
    );
    p.publish(&step1).await.unwrap();

    let got = tokio::time::timeout(Duration::from_secs(5), sub.next_manual_ack())
        .await
        .expect("resume: delivery timed out")
        .unwrap()
        .expect("resumed consumer keeps flowing");
    assert_eq!(
        got.received.event.summary, "step-1",
        "acked events must not replay on the new connection"
    );
    got.ack().await.unwrap();

    // And a second publish flows too (subscription remains healthy).
    let step2 = Event::new(
        p.build_subject("pipeline", "step"),
        "pipeline",
        "step-2",
        "test",
        serde_json::json!({"i": 2}),
    );
    p.publish(&step2).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), sub.next_manual_ack())
        .await
        .expect("resume: second delivery timed out")
        .unwrap()
        .expect("subscription healthy after resume");
    assert_eq!(got.received.event.summary, "step-2");
    got.ack().await.unwrap();

    let _ = p.unsubscribe("pipeline-worker").await;
}

/// T1.3 — deleting the consumer rebuilds it: replay from retention.
async fn scenario_group_rebuild_replay(suite: &Suite) {
    let p = some!(suite.provider("t1rebuild").await);
    let filter = p.category_subject("audit");

    let e = Event::new(
        p.build_subject("audit", "entry"),
        "audit",
        "rebuild-entry",
        "test",
        serde_json::json!({}),
    );
    p.publish(&e).await.unwrap();

    let mut sub = p.subscribe_durable("auditor", &filter).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), sub.next_manual_ack())
        .await
        .expect("rebuild: delivery timed out")
        .unwrap()
        .expect("must deliver");
    assert_eq!(got.received.event.summary, "rebuild-entry");
    got.ack().await.unwrap();

    p.unsubscribe("auditor").await.unwrap();

    let mut sub = p.subscribe_durable("auditor", &filter).await.unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), sub.next_manual_ack())
        .await
        .expect("rebuild: replay timed out")
        .unwrap()
        .expect("rebuilt consumer replays retained events");
    assert_eq!(got.received.event.summary, "rebuild-entry");
    got.ack().await.unwrap();

    let _ = p.unsubscribe("auditor").await;
}

/// T1.4 — a subscriber attaching AFTER publication replays everything in
/// order (deliver policy All on a fresh consumer).
async fn scenario_late_subscriber_full_replay(suite: &Suite) {
    let p = some!(suite.provider("t1late").await);
    let filter = p.category_subject("ledger");

    for i in 0..3 {
        let e = Event::new(
            p.build_subject("ledger", "line"),
            "ledger",
            format!("ledger-{i}"),
            "test",
            serde_json::json!({}),
        );
        p.publish(&e).await.unwrap();
    }

    let mut sub = p
        .subscribe_durable_with_options(
            "late-reader",
            &filter,
            &SubscribeOptions {
                deliver_policy: DeliverPolicy::All,
                ..Default::default()
            },
        )
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut got = Vec::new();
    while got.len() < 3 {
        let r = tokio::time::timeout(
            deadline.saturating_duration_since(std::time::Instant::now()),
            sub.next_manual_ack(),
        )
        .await
        .expect("late replay timed out")
        .unwrap()
        .expect("replay yields");
        got.push(r.received.event.summary.clone());
        r.ack().await.unwrap();
    }
    assert_eq!(
        got,
        vec!["ledger-0", "ledger-1", "ledger-2"],
        "ordered replay"
    );

    let _ = p.unsubscribe("late-reader").await;
}

/// T1.5 — competing consumers on separate connections: six events, two
/// members, each event delivered exactly once across the group.
async fn scenario_competing_consumers(suite: &Suite) {
    let pa = some!(suite.provider("t1compete").await);
    let pb = some!(suite.provider("t1compete").await);
    let filter = pa.category_subject("tasks");

    for i in 0..6 {
        let e = Event::new(
            pa.build_subject("tasks", "item"),
            "tasks",
            format!("task-{i}"),
            "test",
            serde_json::json!({}),
        );
        pa.publish(&e).await.unwrap();
    }

    let mut a = pa
        .subscribe_durable("competing-workers", &filter)
        .await
        .unwrap();
    let mut b = pb
        .subscribe_durable("competing-workers", &filter)
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut delivered = Vec::new();
    let mut turn = false;
    while delivered.len() < 6 {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!(
                "competing consumers timed out with {}/6 delivered",
                delivered.len()
            );
        }
        // Alternate short pull slices so both members are exercised without
        // letting an unassigned (idle) member burn the whole budget; the
        // group routes each event to exactly one of them.
        let slice = remaining.min(Duration::from_millis(200));
        let r = if turn {
            tokio::time::timeout(slice, b.next_manual_ack()).await
        } else {
            tokio::time::timeout(slice, a.next_manual_ack()).await
        };
        turn = !turn;
        if let Ok(Ok(Some(pending))) = r {
            delivered.push(pending.received.event.summary.clone());
            pending.ack().await.unwrap();
        }
    }

    delivered.sort();
    let expected: Vec<String> = (0..6).map(|i| format!("task-{i}")).collect();
    assert_eq!(delivered, expected, "each event delivered exactly once");

    let _ = pa.unsubscribe("competing-workers").await;
}

// ---------------------------------------------------------------------------
// Providers under test
// ---------------------------------------------------------------------------

fn memory_suite() -> Suite {
    Suite {
        create: Box::new(|_tag| {
            Box::pin(async {
                Some(Arc::new(a3s_event::MemoryProvider::default()) as Arc<dyn EventProvider>)
            })
        }),
    }
}

#[cfg(feature = "nats")]
fn nats_suite() -> Suite {
    use a3s_event::provider::nats::{NatsConfig, NatsProvider, StorageType};

    Suite {
        create: Box::new(|tag| {
            // pid in the FIRST token: fresh subjects per process run can
            // never overlap a previous run's `test.<suffix>.>` wildcards.
            let tag = format!("conf_{}_{}", tag, std::process::id());
            Box::pin(async move {
                let config = NatsConfig {
                    url: "nats://127.0.0.1:4222".to_string(),
                    stream_name: format!("CONF_{tag}"),
                    subject_prefix: format!("conf.{tag}"),
                    storage: StorageType::Memory,
                    max_events: 50_000,
                    max_age_secs: 300,
                    ..Default::default()
                };
                match NatsProvider::connect(config).await {
                    Ok(p) => Some(Arc::new(p) as Arc<dyn EventProvider>),
                    Err(e) => {
                        eprintln!("NATS unavailable ({e}), skipping conformance");
                        None
                    }
                }
            })
        }),
    }
}

#[cfg(feature = "iggy")]
fn iggy_suite() -> Suite {
    use a3s_event::provider::iggy::{IggyConfig, IggyPartitioning, IggyProvider};

    Suite {
        create: Box::new(|tag| {
            let tag = format!("conf_{}_{}", tag, std::process::id());
            Box::pin(async move {
                let config = IggyConfig {
                    server_address: "127.0.0.1:5102".to_string(),
                    stream_name: format!("conf_{tag}"),
                    subject_prefix: format!("conf.{tag}"),
                    partitioning: IggyPartitioning::Single,
                    max_age_secs: 300,
                    poll_batch_size: 50,
                    poll_interval_ms: 20,
                    ..Default::default()
                };
                match IggyProvider::connect(config).await {
                    Ok(p) => Some(Arc::new(p) as Arc<dyn EventProvider>),
                    Err(e) => {
                        eprintln!("Iggy unavailable ({e}), skipping conformance");
                        None
                    }
                }
            })
        }),
    }
}

// ---------------------------------------------------------------------------
// Generated tests — tier 0 on every provider, tier 1 on persistent ones
// ---------------------------------------------------------------------------

#[tokio::test]
async fn memory_lifecycle_and_fidelity() {
    scenario_lifecycle_and_fidelity(&memory_suite()).await;
}
#[tokio::test]
async fn memory_fanout_isolation() {
    scenario_fanout_isolation(&memory_suite()).await;
}
#[tokio::test]
async fn memory_sequential_ordering() {
    scenario_sequential_ordering(&memory_suite()).await;
}
#[tokio::test]
async fn memory_concurrent_no_loss_no_dup() {
    scenario_concurrent_no_loss_no_dup(&memory_suite()).await;
}
#[tokio::test]
async fn memory_tail_filters() {
    scenario_tail_filters(&memory_suite()).await;
}
#[tokio::test]
async fn memory_options_plumbing() {
    scenario_options_plumbing(&memory_suite()).await;
}
#[tokio::test]
async fn memory_counts_info_health() {
    scenario_counts_info_health(&memory_suite()).await;
}

#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_lifecycle_and_fidelity() {
    scenario_lifecycle_and_fidelity(&nats_suite()).await;
}
#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_fanout_isolation() {
    scenario_fanout_isolation(&nats_suite()).await;
}
#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_sequential_ordering() {
    scenario_sequential_ordering(&nats_suite()).await;
}
#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_concurrent_no_loss_no_dup() {
    scenario_concurrent_no_loss_no_dup(&nats_suite()).await;
}
#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_tail_filters() {
    scenario_tail_filters(&nats_suite()).await;
}
#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_options_plumbing() {
    scenario_options_plumbing(&nats_suite()).await;
}
#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_counts_info_health() {
    scenario_counts_info_health(&nats_suite()).await;
}

#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_unacked_redelivery() {
    scenario_unacked_redelivery(&nats_suite()).await;
}
#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_resume_across_reconnect() {
    scenario_resume_across_reconnect(&nats_suite()).await;
}
#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_group_rebuild_replay() {
    scenario_group_rebuild_replay(&nats_suite()).await;
}
#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_late_subscriber_full_replay() {
    scenario_late_subscriber_full_replay(&nats_suite()).await;
}
#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_competing_consumers() {
    scenario_competing_consumers(&nats_suite()).await;
}

#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_lifecycle_and_fidelity() {
    scenario_lifecycle_and_fidelity(&iggy_suite()).await;
}
#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_fanout_isolation() {
    scenario_fanout_isolation(&iggy_suite()).await;
}
#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_sequential_ordering() {
    scenario_sequential_ordering(&iggy_suite()).await;
}
#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_concurrent_no_loss_no_dup() {
    scenario_concurrent_no_loss_no_dup(&iggy_suite()).await;
}
#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_tail_filters() {
    scenario_tail_filters(&iggy_suite()).await;
}
#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_options_plumbing() {
    scenario_options_plumbing(&iggy_suite()).await;
}
#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_counts_info_health() {
    scenario_counts_info_health(&iggy_suite()).await;
}

#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_unacked_redelivery() {
    scenario_unacked_redelivery(&iggy_suite()).await;
}
#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_resume_across_reconnect() {
    scenario_resume_across_reconnect(&iggy_suite()).await;
}
#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_group_rebuild_replay() {
    scenario_group_rebuild_replay(&iggy_suite()).await;
}
#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_late_subscriber_full_replay() {
    scenario_late_subscriber_full_replay(&iggy_suite()).await;
}
#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_competing_consumers() {
    scenario_competing_consumers(&iggy_suite()).await;
}
