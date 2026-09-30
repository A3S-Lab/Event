//! Chaos & resilience e2e — broker restart mid-stream, and PAT credentials
//!
//! These scenarios are OPT-IN: restarting a broker is an infrastructure
//! operation, so the restart command arrives through an environment variable
//! and the tests skip cleanly when it is unset (no overfitting to one
//! machine's docker setup):
//!
//!   A3S_EVENT_IGGY_RESTART="docker restart a3s-iggy-test" \
//!   A3S_EVENT_NATS_RESTART="docker restart a3s-nats" \
//!   cargo test --test e2e_chaos_resilience -- --nocapture
//!
//! # What each restart proves
//!
//! - **Iggy**: the server's stream/topic/offset state lives in its data
//!   directory, which survives a container RESTART (not recreate). After the
//!   broker comes back: a brand-new connection resumes the consumer group
//!   from its committed offset — no replay of acked events, in-flight tail
//!   still delivered. This is the "broker is not business truth, the owner
//!   can rebuild" contract from EVENT-R3.
//! - **NATS**: the dev server runs JetStream without a persistence volume,
//!   so a restart legitimately LOSES stream state. The library-level claim
//!   is narrower and still valuable: after a restart the provider reconnects
//!   and a fresh stream/subscription pipeline works end to end.

// Per-test feature gates below (iggy and/or nats).

#[cfg(feature = "iggy")]
use a3s_event::provider::iggy::{IggyConfig, IggyPartitioning, IggyProvider};
#[cfg(feature = "iggy")]
use a3s_event::SubscribeOptions;
use a3s_event::{Event, EventProvider};
use std::time::Duration;

/// Broker restarts are binary-global side effects: every test in this file
/// that talks to Iggy must hold this lock so a restart never races another
/// test's connection.
static BROKER_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(feature = "iggy")]
fn iggy_config(tag: &str) -> IggyConfig {
    IggyConfig {
        server_address: "127.0.0.1:5102".to_string(),
        stream_name: format!("chaos_{tag}_{}", std::process::id()),
        subject_prefix: format!("chaos.{tag}"),
        partitioning: IggyPartitioning::Single,
        max_age_secs: 300,
        poll_batch_size: 50,
        poll_interval_ms: 20,
        ..Default::default()
    }
}

#[cfg(feature = "iggy")]
async fn connect_iggy(tag: &str) -> Option<IggyProvider> {
    match IggyProvider::connect(iggy_config(tag)).await {
        Ok(p) => Some(p),
        Err(e) => {
            eprintln!("Iggy unavailable ({e}), skipping chaos test");
            None
        }
    }
}

/// Run the operator-provided restart command; fail the test (not skip) when
/// the command exists but fails — a chaos test that silently ignores a
/// failed restart proves nothing.
fn restart_broker(env_var: &str) -> Option<()> {
    let cmd = std::env::var(env_var).ok()?;
    eprintln!("chaos: {env_var} = {cmd}");

    let mut parts = cmd.split_whitespace();
    let program = parts.next().expect("non-empty restart command");
    let args: Vec<&str> = parts.collect();
    match std::process::Command::new(program).args(&args).output() {
        Ok(out) if out.status.success() => Some(()),
        Ok(out) => panic!(
            "restart command failed ({}): {}",
            out.status,
            String::from_utf8_lossy(&out.stderr)
        ),
        Err(e) => panic!("could not run restart command {cmd:?}: {e}"),
    }
}

/// Wait until a fresh Iggy connection succeeds again (bounded).
#[cfg(feature = "iggy")]
async fn wait_iggy_back(tag: &str, timeout: Duration) -> Option<IggyProvider> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(p) = connect_iggy(tag).await {
            return Some(p);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
}

#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_server_restart_preserves_offsets_and_resumes() {
    let _guard = BROKER_LOCK.lock().await;
    if restart_broker("A3S_EVENT_IGGY_RESTART").is_none() {
        eprintln!("A3S_EVENT_IGGY_RESTART unset, skipping (opt-in chaos test)");
        return;
    }
    // The restart above applied to a warm server; reconnect and prove the
    // state survived. (Restarting BEFORE any traffic also proves state
    // bootstrap, which is the weaker claim; we take the stronger path of
    // restarting mid-stream below by publishing first in the NEXT phase.)

    let tag = format!("restart{}", std::process::id());
    let filter = format!("chaos.{tag}.work.>");
    let subject = format!("chaos.{tag}.work.item");

    // Phase 1 — establish state BEFORE a mid-stream restart: publish three,
    // consume and ack the first.
    {
        let p = wait_iggy_back(&tag, Duration::from_secs(30))
            .await
            .expect("server back after warm-up restart");
        for i in 0..3 {
            p.publish(&Event::new(
                &subject,
                "work",
                format!("item-{i}"),
                "chaos",
                serde_json::json!({"i": i}),
            ))
            .await
            .unwrap();
        }
        let mut sub = p
            .subscribe_durable_with_options(
                "chaos-worker",
                &filter,
                &SubscribeOptions {
                    ack_wait_secs: Some(1),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let got = tokio::time::timeout(Duration::from_secs(5), sub.next_manual_ack())
            .await
            .expect("phase-1 delivery timed out")
            .unwrap()
            .expect("item-0 delivered");
        assert_eq!(got.received.event.summary, "item-0");
        got.ack().await.unwrap();
        // Connection dropped with item-1 and item-2 unacked.
    }

    // Phase 2 — restart the broker MID-STREAM: state (stream, topic,
    // consumer group, committed offset) must survive.
    restart_broker("A3S_EVENT_IGGY_RESTART").expect("mid-stream restart configured");

    let p = wait_iggy_back(&tag, Duration::from_secs(30))
        .await
        .unwrap_or_else(|| {
            panic!(
                "Iggy did not come back after restart. Known upstream defect: \
             iggy 0.9.0 can panic during boot replay ('client_id 0 is reserved \
             for internal use', core/consensus/src/client_table.rs) when the \
             persisted client table contains certain sessions. Check the \
             server container logs; a fresh container (recreate, not restart) \
             boots clean. This failure is a REAL availability finding, not a \
             test-environment issue."
            )
        });

    // Dead-member eviction is gated by consumer_group.rebalancing_timeout
    // (the test server runs 2s); wait it out before rejoining.
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // Phase 3 — rejoin under the same consumer name and drain the unacked
    // tail, then keep flowing for post-restart publishes.
    let mut sub = p
        .subscribe_durable_with_options(
            "chaos-worker",
            &filter,
            &SubscribeOptions {
                ack_wait_secs: Some(1),
                ..Default::default()
            },
        )
        .await
        .unwrap();

    // 3 published, 1 acked → the unacked tail is exactly 2 events.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let mut resumed = Vec::new();
    while resumed.len() < 2 {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            panic!("post-restart resume timed out with {resumed:?}/2");
        }
        let got = tokio::time::timeout(remaining, sub.next_manual_ack())
            .await
            .expect("post-restart delivery timed out")
            .unwrap()
            .expect("post-restart delivery yields");
        resumed.push(got.received.event.summary.clone());
        got.ack().await.unwrap();
    }
    assert!(
        resumed.contains(&"item-1".to_string()) && resumed.contains(&"item-2".to_string()),
        "unacked tail redelivered after restart: {resumed:?}"
    );
    assert!(
        !resumed.contains(&"item-0".to_string()),
        "acked offset survived the restart — item-0 must not replay: {resumed:?}"
    );

    // Post-restart publishes keep flowing through the same subscription.
    p.publish(&Event::new(
        &subject,
        "work",
        "item-post-restart",
        "chaos",
        serde_json::json!({}),
    ))
    .await
    .unwrap();
    let got = tokio::time::timeout(Duration::from_secs(5), sub.next_manual_ack())
        .await
        .expect("post-restart publish delivery timed out")
        .unwrap()
        .expect("subscription healthy after restart");
    assert_eq!(got.received.event.summary, "item-post-restart");
    got.ack().await.unwrap();

    let _ = p.unsubscribe("chaos-worker").await;
}

/// PAT credentials: login with a personal access token instead of
/// username/password, and prove the session can publish and consume.
#[cfg(feature = "iggy")]
#[tokio::test]
async fn iggy_personal_access_token_login_flows_end_to_end() {
    use iggy::prelude::{
        IggyClientBuilder, PersonalAccessTokenClient, PersonalAccessTokenExpiry, UserClient,
    };
    let _guard = BROKER_LOCK.lock().await;

    // Mint a PAT through the SDK with root credentials.
    let admin = IggyClientBuilder::new()
        .with_tcp()
        .with_server_address("127.0.0.1:5102".to_string())
        .build()
        .unwrap();
    if let Err(e) = admin.login_user("iggy", "iggy").await {
        eprintln!("Iggy unavailable ({e}), skipping PAT test");
        return;
    }
    let pat = match admin
        .create_personal_access_token(
            &format!("a3s-e2e-{}", std::process::id()),
            PersonalAccessTokenExpiry::ExpireDuration(iggy::prelude::IggyDuration::from(
                Duration::from_secs(300),
            )),
        )
        .await
    {
        Ok(pat) => pat,
        Err(e) => {
            eprintln!("could not mint a PAT ({e}), skipping PAT test");
            return;
        }
    };

    // Connect the provider with ONLY the token — no username/password.
    let tag = format!("pat{}", std::process::id());
    let provider = match IggyProvider::connect(IggyConfig {
        token: Some(pat.token.to_string()),
        ..iggy_config(&tag)
    })
    .await
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("PAT login failed: {e}");
            panic!("PAT login must work when the token is valid");
        }
    };

    // The PAT session is fully functional.
    let e = Event::new(
        format!("chaos.{tag}.market.tick"),
        "market",
        "pat-tick",
        "chaos",
        serde_json::json!({}),
    );
    provider.publish(&e).await.unwrap();
    let history = provider
        .history(Some(&format!("chaos.{tag}.>")), 10)
        .await
        .unwrap();
    assert!(
        history.iter().any(|ev| ev.id == e.id),
        "PAT session can read back"
    );
}

/// NATS restart: the dev server runs JetStream WITHOUT a persistence volume,
/// so a restart legitimately loses stream state — this test pins the
/// library-level contract only: after a broker restart, a fresh provider
/// connection builds a working stream/subscription pipeline again.
/// (Persistent JetStream deployments retain streams across restarts; that
/// is infrastructure configuration, not a library property.)
#[cfg(feature = "nats")]
#[tokio::test]
async fn nats_server_restart_allows_fresh_pipelines() {
    use a3s_event::provider::nats::{NatsConfig, NatsProvider, StorageType};
    use a3s_event::Subscription;

    let _guard = BROKER_LOCK.lock().await;
    if restart_broker("A3S_EVENT_NATS_RESTART").is_none() {
        eprintln!("A3S_EVENT_NATS_RESTART unset, skipping (opt-in chaos test)");
        return;
    }

    // Pre-restart pipeline works.
    let mk_config = |gen: u32| NatsConfig {
        url: "nats://127.0.0.1:4222".to_string(),
        stream_name: format!("CHAOS_NATS_{gen}_{}", std::process::id()),
        subject_prefix: format!("chaos.n{gen}.{}", std::process::id()),
        storage: StorageType::Memory,
        max_events: 10_000,
        max_age_secs: 300,
        ..Default::default()
    };
    let gen_one = std::process::id() ^ 0x5a5a;
    let first = match NatsProvider::connect(mk_config(gen_one)).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("NATS unavailable ({e}), skipping chaos test");
            return;
        }
    };
    let e0 = Event::new(
        format!("chaos.n{gen_one}.{}.{}", std::process::id(), "work.tick"),
        "work",
        "pre-restart",
        "chaos",
        serde_json::json!({}),
    );
    first.publish(&e0).await.unwrap();
    drop(first);

    // Restart the broker mid-pipeline.
    restart_broker("A3S_EVENT_NATS_RESTART").expect("mid-stream restart configured");

    // Bounded wait for the server to accept connections again.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let second = loop {
        let gen_two = std::process::id() ^ 0xa5a5;
        match NatsProvider::connect(mk_config(gen_two)).await {
            Ok(p) => break p,
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            Err(e) => panic!("NATS did not come back after restart: {e}"),
        }
    };

    // Post-restart pipeline: subscribe, publish, receive — end to end.
    let gen_two = std::process::id() ^ 0xa5a5;
    let filter = format!("chaos.n{gen_two}.{}.>", std::process::id());
    let mut sub: Box<dyn Subscription> = second.subscribe(&filter).await.unwrap();
    let e1 = Event::new(
        format!("chaos.n{gen_two}.{}.{}", std::process::id(), "work.tick"),
        "work",
        "post-restart",
        "chaos",
        serde_json::json!({}),
    );
    second.publish(&e1).await.unwrap();

    let got = tokio::time::timeout(Duration::from_secs(5), sub.next())
        .await
        .expect("post-restart delivery timed out")
        .unwrap()
        .expect("fresh pipeline delivers after restart");
    assert_eq!(got.event.summary, "post-restart");
}
