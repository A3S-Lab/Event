//! CloudEvents conversion end-to-end: attribute mapping, extensions,
//! deterministic defaults, and serde wire round-trips.

#![cfg(feature = "cloudevents")]

use a3s_event::{CloudEvent, Event};

#[tokio::test]
async fn event_to_cloudevent_preserves_the_envelope() {
    let event = Event::typed(
        "events.market.forex",
        "market",
        "forex.rate_change",
        3,
        "USD/CNY move",
        "reuters",
        serde_json::json!({"rate": 7.3521}),
    )
    .with_metadata("region", "asia");

    let ce = CloudEvent::from(event.clone());

    assert_eq!(ce.id, event.id);
    assert_eq!(ce.specversion, "1.0", "CloudEvents 1.0 spec version");
    assert_eq!(ce.event_type, "forex.rate_change");
    assert_eq!(ce.source, "reuters");
    assert_eq!(ce.subject.as_deref(), Some(event.subject.as_str()));
    assert_eq!(ce.data.as_ref(), Some(&event.payload));
    assert_eq!(ce.datacontenttype.as_deref(), Some("application/json"));

    // Time is RFC 3339 derived from the event's millis timestamp.
    let time = ce.time.as_deref().expect("time set");
    assert!(
        time.contains('T') && (time.contains('Z') || time.contains('+')),
        "RFC3339: {time}"
    );

    // A3S fields ride as extensions.
    assert_eq!(
        ce.extensions.get("a3scategory"),
        Some(&serde_json::json!("market"))
    );
    assert_eq!(ce.extensions.get("a3sversion"), Some(&serde_json::json!(3)));

    let _ = event.metadata; // metadata themselves are not required in CE form
}

#[tokio::test]
async fn untyped_events_get_a_default_type() {
    let event = Event::new(
        "events.misc.note",
        "misc",
        "no type",
        "somewhere",
        serde_json::json!({}),
    );
    let ce = CloudEvent::from(event);
    assert_eq!(
        ce.event_type, "a3s.event",
        "untyped events fall back to a3s.event"
    );
}

#[tokio::test]
async fn cloudevent_serde_wire_round_trip() {
    let event = Event::typed(
        "events.wire.round",
        "wire",
        "wire.ping",
        2,
        "wire test",
        "e2e",
        serde_json::json!({"n": 1, "arr": [1, 2, 3]}),
    )
    .with_metadata("k", "v");
    let ce = CloudEvent::from(event);

    let json = serde_json::to_string(&ce).unwrap();
    // Wire format carries the CloudEvents required attributes.
    assert!(json.contains("\"specversion\":\"1.0\""));
    assert!(json.contains("\"type\":\"wire.ping\""));
    assert!(json.contains("\"source\":\"e2e\""));

    let parsed: CloudEvent = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, ce, "serde round-trip is lossless");
}
