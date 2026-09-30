//! AES-256-GCM encryptor end-to-end: multi-key lifecycle, rotation,
//! tamper detection, and envelope interop with the EventBus read path.

#![cfg(feature = "encryption")]

use a3s_event::crypto::{Aes256GcmEncryptor, EncryptedPayload, EventEncryptor};

#[tokio::test]
async fn multi_key_lifecycle_and_rotation() {
    let key_v1: [u8; 32] = [1u8; 32];
    let key_v2: [u8; 32] = [2u8; 32];

    let mut enc = Aes256GcmEncryptor::new("v1", &key_v1);
    assert_eq!(enc.active_key_id(), "v1");

    // Encrypt under v1.
    let secret = serde_json::json!({"pan": "4111-1111", "cvv": "123"});
    let envelope_v1 = enc.encrypt(&secret).unwrap();
    assert!(EncryptedPayload::is_encrypted(&envelope_v1));
    assert!(!envelope_v1.to_string().contains("4111"));

    // Add v2 and rotate: new envelopes use v2, old ones still decrypt.
    enc.add_key("v2", &key_v2).unwrap();
    enc.rotate_to("v2").unwrap();
    assert_eq!(enc.active_key_id(), "v2");
    let mut key_ids = enc.key_ids();
    key_ids.sort();
    assert_eq!(
        key_ids,
        vec!["v1".to_string(), "v2".to_string()],
        "both keys registered"
    );

    let envelope_v2 = enc.encrypt(&secret).unwrap();
    assert_ne!(
        envelope_v1, envelope_v2,
        "different keys produce different envelopes"
    );

    // Cross-generation decryption.
    assert_eq!(
        enc.decrypt(&envelope_v1).unwrap(),
        secret,
        "v1 envelope decrypts after rotation"
    );
    assert_eq!(
        enc.decrypt(&envelope_v2).unwrap(),
        secret,
        "v2 envelope decrypts"
    );

    // Wrong key fails closed.
    let wrong: [u8; 32] = [9u8; 32];
    let other = Aes256GcmEncryptor::new("other", &wrong);
    assert!(
        other.decrypt(&envelope_v2).is_err(),
        "unrelated key must not decrypt"
    );

    // Tampered ciphertext fails the GCM tag check.
    let mut tampered = envelope_v2.clone();
    if let Some(obj) = tampered.as_object_mut() {
        for (_k, v) in obj.iter_mut() {
            if let Some(s) = v.as_str() {
                if s.len() > 4 {
                    let chars = s.chars().collect::<Vec<_>>();
                    let flipped: String = chars
                        .into_iter()
                        .enumerate()
                        .map(|(i, c)| {
                            if i == 0 {
                                char::from_u32(c as u32 ^ 1).unwrap_or(c)
                            } else {
                                c
                            }
                        })
                        .collect();
                    *v = serde_json::Value::String(flipped);
                    break;
                }
            }
        }
    }
    if EncryptedPayload::is_encrypted(&tampered) {
        assert!(
            enc.decrypt(&tampered).is_err(),
            "tampering must break the GCM tag"
        );
    }

    // Plaintext is not an envelope.
    assert!(!EncryptedPayload::is_encrypted(
        &serde_json::json!({"plain": true})
    ));
}

#[tokio::test]
async fn encryptor_round_trips_through_event_bus_storage() {
    use a3s_event::provider::memory::MemoryProvider;
    use a3s_event::{Event, EventBus, EventProvider};
    use std::sync::Arc;

    let key: [u8; 32] = [5u8; 32];
    let encryptor = Arc::new(Aes256GcmEncryptor::new("ops-key", &key));
    let raw = Arc::new(MemoryProvider::default());
    let mut bus = EventBus::from_provider(Arc::clone(&raw) as Arc<dyn EventProvider>);
    bus.set_encryptor(encryptor.clone() as Arc<dyn a3s_event::EventEncryptor>);

    let original = serde_json::json!({"employee": "E-77", "salary": 120_000});
    let event = Event::new(
        "events.hr.salary",
        "hr",
        "salary record",
        "hr-core",
        original.clone(),
    );
    bus.publish_event(&event).await.unwrap();

    // Bus read path decrypts; raw provider path still holds ciphertext that
    // the encryptor itself can decrypt.
    let via_bus = bus.list_events(None, 10).await.unwrap();
    assert_eq!(via_bus[0].payload, original);

    let at_rest = raw.history(None, 10).await.unwrap();
    assert!(EncryptedPayload::is_encrypted(&at_rest[0].payload));
    assert_eq!(encryptor.decrypt(&at_rest[0].payload).unwrap(), original);
}
