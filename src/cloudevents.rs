//! CloudEvents v1.0 envelope for A3S events
//!
//! Provides a `CloudEvent` struct conforming to the CloudEvents v1.0 specification,
//! with lossless conversion to/from the internal `Event` type. A3S-specific fields
//! are stored as extension attributes with the `a3s` prefix.
//!
//! Manual implementation — no dependency on `cloudevents-sdk`.

use crate::error::{EventError, Result};
use crate::types::Event;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// CloudEvents specification version
pub const SPEC_VERSION: &str = "1.0";

/// Default data content type for A3S events
pub const DEFAULT_DATA_CONTENT_TYPE: &str = "application/json";

/// CloudEvents v1.0 envelope
///
/// Required attributes: `specversion`, `id`, `source`, `type`.
/// Optional attributes: `datacontenttype`, `dataschema`, `subject`, `time`.
/// Extension attributes stored in `extensions`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CloudEvent {
    // ── Required attributes ──

    /// CloudEvents spec version (always "1.0")
    pub specversion: String,

    /// Event identifier (maps to Event.id)
    pub id: String,

    /// Event source (maps to Event.source)
    pub source: String,

    /// Event type (maps to Event.event_type, or "a3s.event" for untyped)
    #[serde(rename = "type")]
    pub event_type: String,

    // ── Optional attributes ──

    /// Data content type
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub datacontenttype: Option<String>,

    /// Data schema URI
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataschema: Option<String>,

    /// Event subject (maps to Event.subject)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,

    /// Timestamp in RFC 3339 format
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,

    // ── Data ──

    /// Event payload
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,

    // ── Extension attributes ──

    /// Extension attributes (includes a3s-prefixed fields)
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub extensions: HashMap<String, serde_json::Value>,
}

impl CloudEvent {
    /// Create a new CloudEvent with required attributes
    pub fn new(
        id: impl Into<String>,
        source: impl Into<String>,
        event_type: impl Into<String>,
    ) -> Self {
        Self {
            specversion: SPEC_VERSION.to_string(),
            id: id.into(),
            source: source.into(),
            event_type: event_type.into(),
            datacontenttype: None,
            dataschema: None,
            subject: None,
            time: None,
            data: None,
            extensions: HashMap::new(),
        }
    }

    /// Set the data payload
    pub fn with_data(mut self, data: serde_json::Value) -> Self {
        self.datacontenttype = Some(DEFAULT_DATA_CONTENT_TYPE.to_string());
        self.data = Some(data);
        self
    }

    /// Set the subject
    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    /// Set the time
    pub fn with_time(mut self, time: impl Into<String>) -> Self {
        self.time = Some(time.into());
        self
    }

    /// Add an extension attribute
    pub fn with_extension(
        mut self,
        key: impl Into<String>,
        value: impl Into<serde_json::Value>,
    ) -> Self {
        self.extensions.insert(key.into(), value.into());
        self
    }
}

/// Convert an A3S Event to a CloudEvent (lossless)
///
/// A3S-specific fields are stored as extension attributes:
/// - `a3scategory` — Event.category
/// - `a3sversion` — Event.version
/// - `a3ssummary` — Event.summary
/// - `a3stimestamp` — Event.timestamp (Unix ms)
/// - `a3smeta_<key>` — each metadata entry
impl From<Event> for CloudEvent {
    fn from(event: Event) -> Self {
        let event_type = if event.event_type.is_empty() {
            "a3s.event".to_string()
        } else {
            event.event_type.clone()
        };

        let time = millis_to_rfc3339(event.timestamp);

        let mut ce = CloudEvent {
            specversion: SPEC_VERSION.to_string(),
            id: event.id.clone(),
            source: event.source.clone(),
            event_type,
            datacontenttype: Some(DEFAULT_DATA_CONTENT_TYPE.to_string()),
            dataschema: None,
            subject: Some(event.subject.clone()),
            time: Some(time),
            data: Some(event.payload.clone()),
            extensions: HashMap::new(),
        };

        // Store A3S-specific fields as extensions
        ce.extensions.insert(
            "a3scategory".to_string(),
            serde_json::Value::String(event.category.clone()),
        );
        ce.extensions.insert(
            "a3sversion".to_string(),
            serde_json::Value::Number(event.version.into()),
        );
        ce.extensions.insert(
            "a3ssummary".to_string(),
            serde_json::Value::String(event.summary.clone()),
        );
        ce.extensions.insert(
            "a3stimestamp".to_string(),
            serde_json::Value::Number(event.timestamp.into()),
        );

        // Store original event_type if it was set
        if !event.event_type.is_empty() {
            ce.extensions.insert(
                "a3seventtype".to_string(),
                serde_json::Value::String(event.event_type),
            );
        }

        // Store metadata as prefixed extensions
        for (key, value) in &event.metadata {
            ce.extensions.insert(
                format!("a3smeta_{}", key),
                serde_json::Value::String(value.clone()),
            );
        }

        ce
    }
}

/// Convert a CloudEvent back to an A3S Event
///
/// Extracts A3S-specific fields from extension attributes.
/// Fails if required A3S extensions are missing.
impl TryFrom<CloudEvent> for Event {
    type Error = EventError;

    fn try_from(ce: CloudEvent) -> Result<Self> {
        let category = ce
            .extensions
            .get("a3scategory")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let version = ce
            .extensions
            .get("a3sversion")
            .and_then(|v| v.as_u64())
            .unwrap_or(1) as u32;

        let summary = ce
            .extensions
            .get("a3ssummary")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let timestamp = ce
            .extensions
            .get("a3stimestamp")
            .and_then(|v| v.as_u64())
            .unwrap_or_else(|| {
                // Fall back to parsing RFC 3339 time
                ce.time
                    .as_deref()
                    .and_then(rfc3339_to_millis)
                    .unwrap_or(0)
            });

        // Recover original event_type
        let event_type = ce
            .extensions
            .get("a3seventtype")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| {
                if ce.event_type == "a3s.event" {
                    String::new()
                } else {
                    ce.event_type.clone()
                }
            });

        let subject = ce.subject.unwrap_or_default();
        let payload = ce.data.unwrap_or(serde_json::Value::Null);

        // Recover metadata from a3smeta_ prefixed extensions
        let mut metadata = HashMap::new();
        for (key, value) in &ce.extensions {
            if let Some(meta_key) = key.strip_prefix("a3smeta_") {
                if let Some(meta_val) = value.as_str() {
                    metadata.insert(meta_key.to_string(), meta_val.to_string());
                }
            }
        }

        Ok(Event {
            id: ce.id,
            subject,
            category,
            event_type,
            version,
            payload,
            summary,
            source: ce.source,
            timestamp,
            metadata,
        })
    }
}

/// Convert Unix milliseconds to RFC 3339 string
fn millis_to_rfc3339(millis: u64) -> String {
    let secs = (millis / 1000) as i64;
    let nanos = ((millis % 1000) * 1_000_000) as u32;
    let dt = chrono::DateTime::from_timestamp(secs, nanos);
    match dt {
        Some(dt) => dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
        None => String::new(),
    }
}

/// Parse RFC 3339 string to Unix milliseconds
fn rfc3339_to_millis(s: &str) -> Option<u64> {
    let dt = chrono::DateTime::parse_from_rfc3339(s).ok()?;
    Some(dt.timestamp_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cloudevent_creation() {
        let ce = CloudEvent::new("evt-123", "test-source", "test.type");
        assert_eq!(ce.specversion, "1.0");
        assert_eq!(ce.id, "evt-123");
        assert_eq!(ce.source, "test-source");
        assert_eq!(ce.event_type, "test.type");
        assert!(ce.subject.is_none());
        assert!(ce.data.is_none());
    }

    #[test]
    fn test_cloudevent_builder() {
        let ce = CloudEvent::new("evt-1", "src", "type.a")
            .with_data(serde_json::json!({"key": "value"}))
            .with_subject("events.test.a")
            .with_time("2024-01-01T00:00:00.000Z")
            .with_extension("custom", serde_json::json!("ext-value"));

        assert_eq!(ce.subject.as_deref(), Some("events.test.a"));
        assert_eq!(ce.data.as_ref().unwrap()["key"], "value");
        assert_eq!(
            ce.datacontenttype.as_deref(),
            Some("application/json")
        );
        assert_eq!(ce.time.as_deref(), Some("2024-01-01T00:00:00.000Z"));
        assert_eq!(ce.extensions["custom"], "ext-value");
    }

    #[test]
    fn test_cloudevent_serialization_roundtrip() {
        let ce = CloudEvent::new("evt-1", "src", "type.a")
            .with_data(serde_json::json!({"rate": 7.35}))
            .with_subject("events.market.forex");

        let json = serde_json::to_string(&ce).unwrap();
        assert!(json.contains("\"specversion\":\"1.0\""));
        assert!(json.contains("\"type\":\"type.a\""));

        let parsed: CloudEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ce);
    }

    #[test]
    fn test_event_to_cloudevent_typed() {
        let event = Event::typed(
            "events.market.forex",
            "market",
            "forex.rate_change",
            2,
            "USD/CNY rate change",
            "reuters",
            serde_json::json!({"rate": 7.35}),
        )
        .with_metadata("region", "asia");

        let ce: CloudEvent = event.clone().into();

        assert_eq!(ce.specversion, "1.0");
        assert_eq!(ce.id, event.id);
        assert_eq!(ce.source, "reuters");
        assert_eq!(ce.event_type, "forex.rate_change");
        assert_eq!(ce.subject.as_deref(), Some("events.market.forex"));
        assert_eq!(ce.data.as_ref().unwrap()["rate"], 7.35);
        assert_eq!(ce.extensions["a3scategory"], "market");
        assert_eq!(ce.extensions["a3sversion"], 2);
        assert_eq!(ce.extensions["a3ssummary"], "USD/CNY rate change");
        assert_eq!(ce.extensions["a3smeta_region"], "asia");
        assert!(ce.time.is_some());
    }

    #[test]
    fn test_event_to_cloudevent_untyped() {
        let event = Event::new(
            "events.test.a",
            "test",
            "Test event",
            "test-src",
            serde_json::json!({}),
        );

        let ce: CloudEvent = event.into();
        assert_eq!(ce.event_type, "a3s.event");
        // Untyped events should NOT have a3seventtype extension
        assert!(!ce.extensions.contains_key("a3seventtype"));
    }

    #[test]
    fn test_cloudevent_to_event_roundtrip_typed() {
        let original = Event::typed(
            "events.market.forex",
            "market",
            "forex.rate_change",
            2,
            "Rate change",
            "reuters",
            serde_json::json!({"rate": 7.35}),
        )
        .with_metadata("env", "prod");

        let ce: CloudEvent = original.clone().into();
        let recovered: Event = ce.try_into().unwrap();

        assert_eq!(recovered.id, original.id);
        assert_eq!(recovered.subject, original.subject);
        assert_eq!(recovered.category, original.category);
        assert_eq!(recovered.event_type, original.event_type);
        assert_eq!(recovered.version, original.version);
        assert_eq!(recovered.summary, original.summary);
        assert_eq!(recovered.source, original.source);
        assert_eq!(recovered.timestamp, original.timestamp);
        assert_eq!(recovered.payload, original.payload);
        assert_eq!(recovered.metadata["env"], "prod");
    }

    #[test]
    fn test_cloudevent_to_event_roundtrip_untyped() {
        let original = Event::new(
            "events.test.a",
            "test",
            "Test",
            "src",
            serde_json::json!({"key": "val"}),
        );

        let ce: CloudEvent = original.clone().into();
        let recovered: Event = ce.try_into().unwrap();

        assert_eq!(recovered.id, original.id);
        assert_eq!(recovered.event_type, ""); // restored to empty
        assert_eq!(recovered.version, 1);
    }

    #[test]
    fn test_cloudevent_from_external_source() {
        // Simulate a CloudEvent not created from an A3S Event
        let ce = CloudEvent::new("ext-1", "external-service", "com.example.order.created")
            .with_data(serde_json::json!({"order_id": "ORD-123"}))
            .with_subject("orders")
            .with_time("2024-06-15T10:30:00.000Z");

        let event: Event = ce.try_into().unwrap();

        assert_eq!(event.id, "ext-1");
        assert_eq!(event.source, "external-service");
        assert_eq!(event.event_type, "com.example.order.created");
        assert_eq!(event.subject, "orders");
        assert_eq!(event.category, ""); // no a3scategory extension
        assert_eq!(event.version, 1); // default
        assert_eq!(event.payload["order_id"], "ORD-123");
    }

    #[test]
    fn test_cloudevent_no_data() {
        let ce = CloudEvent::new("evt-1", "src", "type.a");
        let event: Event = ce.try_into().unwrap();
        assert_eq!(event.payload, serde_json::Value::Null);
    }

    #[test]
    fn test_millis_to_rfc3339() {
        let time = millis_to_rfc3339(1700000000000);
        assert!(time.contains("2023-11-14"));
    }

    #[test]
    fn test_rfc3339_to_millis() {
        let millis = rfc3339_to_millis("2023-11-14T22:13:20.000Z").unwrap();
        assert_eq!(millis, 1700000000000);
    }

    #[test]
    fn test_rfc3339_roundtrip() {
        let original_ms = 1700000000123u64;
        let rfc = millis_to_rfc3339(original_ms);
        let recovered = rfc3339_to_millis(&rfc).unwrap();
        assert_eq!(recovered, original_ms);
    }

    #[test]
    fn test_cloudevent_multiple_metadata_roundtrip() {
        let original = Event::new(
            "events.test.a",
            "test",
            "Test",
            "src",
            serde_json::json!({}),
        )
        .with_metadata("key1", "val1")
        .with_metadata("key2", "val2")
        .with_metadata("key3", "val3");

        let ce: CloudEvent = original.clone().into();
        let recovered: Event = ce.try_into().unwrap();

        assert_eq!(recovered.metadata.len(), 3);
        assert_eq!(recovered.metadata["key1"], "val1");
        assert_eq!(recovered.metadata["key2"], "val2");
        assert_eq!(recovered.metadata["key3"], "val3");
    }

    #[test]
    fn test_cloudevent_json_wire_format() {
        let ce = CloudEvent::new("evt-wire", "a3s", "a3s.gateway.scale.up")
            .with_data(serde_json::json!({"replicas": 3}))
            .with_subject("scaling.gateway");

        let json = serde_json::to_value(&ce).unwrap();
        // Verify CE required fields present at top level
        assert_eq!(json["specversion"], "1.0");
        assert_eq!(json["id"], "evt-wire");
        assert_eq!(json["source"], "a3s");
        assert_eq!(json["type"], "a3s.gateway.scale.up");
        assert_eq!(json["data"]["replicas"], 3);
    }

    #[test]
    fn test_spec_version_constant() {
        assert_eq!(SPEC_VERSION, "1.0");
        assert_eq!(DEFAULT_DATA_CONTENT_TYPE, "application/json");
    }
}
