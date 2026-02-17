//! Scaling event types for Gateway ↔ Box coordination
//!
//! Defines standardized event types for autoscaler communication between
//! the Gateway (traffic brain) and Box (instance executor). In standalone
//! mode, these events are the primary coordination channel. In K8s mode,
//! they complement native mechanisms (HPA, Endpoints watch) with richer
//! application-level signals.

use crate::types::Event;
use serde::{Deserialize, Serialize};

// ── Event type constants ─────────────────────────────────────────

/// Gateway requests Box to add instances
pub const SCALE_UP: &str = "a3s.gateway.scale.up";

/// Gateway requests Box to remove instances
pub const SCALE_DOWN: &str = "a3s.gateway.scale.down";

/// Box reports a new instance is ready to receive traffic
pub const INSTANCE_READY: &str = "a3s.box.instance.ready";

/// Box reports an instance has been terminated
pub const INSTANCE_STOPPED: &str = "a3s.box.instance.stopped";

/// Box reports instance health metrics
pub const INSTANCE_HEALTH: &str = "a3s.box.instance.health";

// ── Payload structs ──────────────────────────────────────────────

/// Payload for `a3s.gateway.scale.up` events
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ScaleUpPayload {
    /// Service identifier to scale
    pub service: String,

    /// Desired number of replicas
    pub desired_replicas: u32,

    /// Human-readable reason for scaling
    pub reason: String,
}

/// Payload for `a3s.gateway.scale.down` events
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ScaleDownPayload {
    /// Service identifier to scale
    pub service: String,

    /// Target number of replicas after scaling down
    pub target_replicas: u32,

    /// Grace period in seconds for draining connections
    pub drain_timeout_secs: u64,
}

/// Payload for `a3s.box.instance.ready` events
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InstanceReadyPayload {
    /// Service this instance belongs to
    pub service: String,

    /// Unique instance identifier
    pub instance_id: String,

    /// Endpoint address (e.g., "10.0.0.5:8080")
    pub endpoint: String,
}

/// Payload for `a3s.box.instance.stopped` events
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InstanceStoppedPayload {
    /// Service this instance belonged to
    pub service: String,

    /// Unique instance identifier
    pub instance_id: String,
}

/// Payload for `a3s.box.instance.health` events
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InstanceHealthPayload {
    /// Unique instance identifier
    pub instance_id: String,

    /// CPU usage percentage (0.0 - 100.0)
    pub cpu_percent: f64,

    /// Memory usage in bytes
    pub memory_bytes: u64,

    /// Number of in-flight requests
    pub in_flight: u32,
}

// ── ScalingEvent trait ───────────────────────────────────────────

/// Trait for typed scaling events with conversion to `Event`
pub trait ScalingEvent: Serialize {
    /// The event type constant (e.g., `SCALE_UP`)
    fn event_type() -> &'static str;

    /// The event source (e.g., "gateway", "box")
    fn event_source() -> &'static str;

    /// The subject category
    fn category() -> &'static str;

    /// Convert this payload to an `Event`
    fn to_event(&self, summary: impl Into<String>) -> Event {
        let payload = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        let event_type = Self::event_type();
        let source = Self::event_source();
        let category = Self::category();

        // Build subject from event type: a3s.gateway.scale.up -> events.gateway.scale.up
        let subject = event_type.replacen("a3s.", "events.", 1);

        Event::typed(subject, category, event_type, 1, summary, source, payload)
    }
}

impl ScalingEvent for ScaleUpPayload {
    fn event_type() -> &'static str {
        SCALE_UP
    }
    fn event_source() -> &'static str {
        "gateway"
    }
    fn category() -> &'static str {
        "gateway"
    }
}

impl ScalingEvent for ScaleDownPayload {
    fn event_type() -> &'static str {
        SCALE_DOWN
    }
    fn event_source() -> &'static str {
        "gateway"
    }
    fn category() -> &'static str {
        "gateway"
    }
}

impl ScalingEvent for InstanceReadyPayload {
    fn event_type() -> &'static str {
        INSTANCE_READY
    }
    fn event_source() -> &'static str {
        "box"
    }
    fn category() -> &'static str {
        "box"
    }
}

impl ScalingEvent for InstanceStoppedPayload {
    fn event_type() -> &'static str {
        INSTANCE_STOPPED
    }
    fn event_source() -> &'static str {
        "box"
    }
    fn category() -> &'static str {
        "box"
    }
}

impl ScalingEvent for InstanceHealthPayload {
    fn event_type() -> &'static str {
        INSTANCE_HEALTH
    }
    fn event_source() -> &'static str {
        "box"
    }
    fn category() -> &'static str {
        "box"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_type_constants() {
        assert_eq!(SCALE_UP, "a3s.gateway.scale.up");
        assert_eq!(SCALE_DOWN, "a3s.gateway.scale.down");
        assert_eq!(INSTANCE_READY, "a3s.box.instance.ready");
        assert_eq!(INSTANCE_STOPPED, "a3s.box.instance.stopped");
        assert_eq!(INSTANCE_HEALTH, "a3s.box.instance.health");
    }

    #[test]
    fn test_scale_up_payload_serialization() {
        let payload = ScaleUpPayload {
            service: "web-api".to_string(),
            desired_replicas: 5,
            reason: "RPS exceeded threshold".to_string(),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["service"], "web-api");
        assert_eq!(json["desiredReplicas"], 5);
        assert_eq!(json["reason"], "RPS exceeded threshold");

        let parsed: ScaleUpPayload = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, payload);
    }

    #[test]
    fn test_scale_down_payload_serialization() {
        let payload = ScaleDownPayload {
            service: "web-api".to_string(),
            target_replicas: 2,
            drain_timeout_secs: 30,
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["targetReplicas"], 2);
        assert_eq!(json["drainTimeoutSecs"], 30);

        let parsed: ScaleDownPayload = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, payload);
    }

    #[test]
    fn test_instance_ready_payload_serialization() {
        let payload = InstanceReadyPayload {
            service: "web-api".to_string(),
            instance_id: "inst-abc123".to_string(),
            endpoint: "10.0.0.5:8080".to_string(),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["instanceId"], "inst-abc123");
        assert_eq!(json["endpoint"], "10.0.0.5:8080");

        let parsed: InstanceReadyPayload = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, payload);
    }

    #[test]
    fn test_instance_stopped_payload_serialization() {
        let payload = InstanceStoppedPayload {
            service: "web-api".to_string(),
            instance_id: "inst-abc123".to_string(),
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["service"], "web-api");
        assert_eq!(json["instanceId"], "inst-abc123");

        let parsed: InstanceStoppedPayload = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, payload);
    }

    #[test]
    fn test_instance_health_payload_serialization() {
        let payload = InstanceHealthPayload {
            instance_id: "inst-abc123".to_string(),
            cpu_percent: 75.5,
            memory_bytes: 1_073_741_824,
            in_flight: 42,
        };

        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["cpuPercent"], 75.5);
        assert_eq!(json["memoryBytes"], 1_073_741_824u64);
        assert_eq!(json["inFlight"], 42);

        let parsed: InstanceHealthPayload = serde_json::from_value(json).unwrap();
        assert_eq!(parsed, payload);
    }

    #[test]
    fn test_scale_up_to_event() {
        let payload = ScaleUpPayload {
            service: "web-api".to_string(),
            desired_replicas: 5,
            reason: "High load".to_string(),
        };

        let event = payload.to_event("Scale up web-api to 5 replicas");

        assert!(event.id.starts_with("evt-"));
        assert_eq!(event.event_type, SCALE_UP);
        assert_eq!(event.source, "gateway");
        assert_eq!(event.category, "gateway");
        assert_eq!(event.subject, "events.gateway.scale.up");
        assert_eq!(event.summary, "Scale up web-api to 5 replicas");
        assert_eq!(event.payload["service"], "web-api");
        assert_eq!(event.payload["desiredReplicas"], 5);
    }

    #[test]
    fn test_scale_down_to_event() {
        let payload = ScaleDownPayload {
            service: "worker".to_string(),
            target_replicas: 1,
            drain_timeout_secs: 60,
        };

        let event = payload.to_event("Scale down worker");

        assert_eq!(event.event_type, SCALE_DOWN);
        assert_eq!(event.source, "gateway");
        assert_eq!(event.subject, "events.gateway.scale.down");
    }

    #[test]
    fn test_instance_ready_to_event() {
        let payload = InstanceReadyPayload {
            service: "web-api".to_string(),
            instance_id: "inst-1".to_string(),
            endpoint: "10.0.0.5:8080".to_string(),
        };

        let event = payload.to_event("Instance inst-1 ready");

        assert_eq!(event.event_type, INSTANCE_READY);
        assert_eq!(event.source, "box");
        assert_eq!(event.category, "box");
        assert_eq!(event.subject, "events.box.instance.ready");
        assert_eq!(event.payload["instanceId"], "inst-1");
    }

    #[test]
    fn test_instance_stopped_to_event() {
        let payload = InstanceStoppedPayload {
            service: "web-api".to_string(),
            instance_id: "inst-1".to_string(),
        };

        let event = payload.to_event("Instance inst-1 stopped");

        assert_eq!(event.event_type, INSTANCE_STOPPED);
        assert_eq!(event.source, "box");
        assert_eq!(event.subject, "events.box.instance.stopped");
    }

    #[test]
    fn test_instance_health_to_event() {
        let payload = InstanceHealthPayload {
            instance_id: "inst-1".to_string(),
            cpu_percent: 45.0,
            memory_bytes: 512_000_000,
            in_flight: 10,
        };

        let event = payload.to_event("Health report");

        assert_eq!(event.event_type, INSTANCE_HEALTH);
        assert_eq!(event.source, "box");
        assert_eq!(event.subject, "events.box.instance.health");
        assert_eq!(event.payload["cpuPercent"], 45.0);
    }

    #[test]
    fn test_scaling_event_trait_type_associations() {
        assert_eq!(ScaleUpPayload::event_type(), SCALE_UP);
        assert_eq!(ScaleUpPayload::event_source(), "gateway");
        assert_eq!(ScaleUpPayload::category(), "gateway");

        assert_eq!(ScaleDownPayload::event_type(), SCALE_DOWN);
        assert_eq!(InstanceReadyPayload::event_type(), INSTANCE_READY);
        assert_eq!(InstanceStoppedPayload::event_type(), INSTANCE_STOPPED);
        assert_eq!(InstanceHealthPayload::event_type(), INSTANCE_HEALTH);

        assert_eq!(InstanceReadyPayload::event_source(), "box");
        assert_eq!(InstanceReadyPayload::category(), "box");
    }
}
