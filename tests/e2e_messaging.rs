//! MessagingPort end-to-end: targeted vs broadcast delivery, wildcard
//! subscriptions, multiple concurrent subscribers, handler refs, timeouts.

use a3s_event::{InMemoryMessaging, Message, MessagingPort};
use std::time::Duration;

#[tokio::test]
async fn targeted_send_reaches_only_matching_filters() {
    let messaging = InMemoryMessaging::new();

    let mut exact = messaging.subscribe("session.abc").await.unwrap();
    let mut star = messaging.subscribe("session.*").await.unwrap();
    let mut other = messaging.subscribe("session.def").await.unwrap();

    let msg = Message::new(
        "src".to_string(),
        "chat".to_string(),
        serde_json::json!({"n": 1}),
    )
    .to_session("session.abc".to_string());
    messaging.send(&msg).await.unwrap();

    let got_exact = tokio::time::timeout(Duration::from_secs(2), exact.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(got_exact.target_id, Some("session.abc".to_string()));

    let got_star = tokio::time::timeout(Duration::from_secs(2), star.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(got_star.id, msg.id);

    // The non-matching subscriber stays silent.
    let silent = tokio::time::timeout(Duration::from_millis(300), other.next()).await;
    assert!(silent.is_err(), "def filter must not receive abc traffic");
}

#[tokio::test]
async fn broadcast_reaches_every_subscriber() {
    let messaging = InMemoryMessaging::new();
    let mut subs = Vec::new();
    for f in ["*", "session.*", "chat"] {
        subs.push((f, messaging.subscribe(f).await.unwrap()));
    }

    let msg = Message::broadcast(
        "src".to_string(),
        "alert".to_string(),
        serde_json::json!({"lvl": 1}),
    );
    messaging.send(&msg).await.unwrap();

    for (name, sub) in subs.iter_mut() {
        let got = tokio::time::timeout(Duration::from_secs(2), sub.next())
            .await
            .unwrap_or_else(|_| panic!("{name} timed out"))
            .unwrap()
            .unwrap();
        assert_eq!(got.msg_type, "alert", "{name} must receive the broadcast");
        assert_eq!(got.target_id, None);
    }
}

#[tokio::test]
async fn timeout_yields_none_without_messages() {
    let messaging = InMemoryMessaging::new();
    let mut stream = messaging.subscribe("quiet.*").await.unwrap();
    let r = stream
        .next_timeout(Duration::from_millis(80))
        .await
        .unwrap();
    assert!(r.is_none(), "no traffic → None on timeout");
}

#[tokio::test]
async fn subscribers_are_isolated_streams() {
    let messaging = InMemoryMessaging::new();
    let mut a = messaging.subscribe("*").await.unwrap();
    let mut b = messaging.subscribe("*").await.unwrap();

    let m1 = Message::broadcast("s".to_string(), "one".to_string(), serde_json::json!({}));
    messaging.send(&m1).await.unwrap();

    // a consumes its copy; b's copy is independent.
    let got_a = a.next().await.unwrap().unwrap();
    assert_eq!(got_a.msg_type, "one");

    let m2 = Message::broadcast("s".to_string(), "two".to_string(), serde_json::json!({}));
    messaging.send(&m2).await.unwrap();

    let got_b1 = b.next().await.unwrap().unwrap();
    assert_eq!(got_b1.msg_type, "one", "b has its own backlog");
    let got_b2 = b.next().await.unwrap().unwrap();
    assert_eq!(got_b2.msg_type, "two");
    assert_ne!(got_a.id, got_b2.id);
}
