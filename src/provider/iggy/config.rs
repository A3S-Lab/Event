//! Configuration for the Apache Iggy event provider

use serde::{Deserialize, Serialize};

/// Partitioning strategy applied when publishing to a topic
///
/// Iggy topics can hold multiple partitions; a partition is an append-only
/// log with its own total order. The default keeps every topic at one
/// partition so each category is a single totally-ordered log — the closest
/// match to JetStream's per-stream ordering.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IggyPartitioning {
    /// All events of a topic append to partition 0 (total order per category)
    #[default]
    Single,
    /// Reserved. Currently behaves like [`IggyPartitioning::Single`]:
    /// this provider's offset tracking is per-topic and its topics are
    /// created with one partition, so multi-partition publishes would be
    /// invisible to subscriptions.
    Balanced,
}

/// Iggy connection and stream configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IggyConfig {
    /// Iggy server TCP address (e.g. "127.0.0.1:5102")
    pub server_address: String,

    /// Username for login (server default: "iggy")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,

    /// Password for login (server default: "iggy")
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,

    /// Personal access token (preferred over username/password when set)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Iggy stream holding every event topic
    pub stream_name: String,

    /// Subject prefix for events (default: "events")
    pub subject_prefix: String,

    /// Partitioning strategy for publishes
    pub partitioning: IggyPartitioning,

    /// Partitions a new topic is created with.
    ///
    /// Always 1 in this version: the subscription model is per-topic
    /// cursors, which is only complete for single-partition topics. The
    /// field exists so a future multi-partition provider can opt in
    /// without a config break.
    pub partitions_count: u32,

    /// Maximum age of events in seconds (0 = server default)
    pub max_age_secs: u64,

    /// Messages fetched per poll
    pub poll_batch_size: u32,

    /// Idle sleep between empty polls, in milliseconds
    pub poll_interval_ms: u64,

    /// Client-side guard on a single poll request, in seconds
    pub poll_timeout_secs: u64,

    /// TCP connection timeout in seconds
    pub connect_timeout_secs: u64,
}

impl Default for IggyConfig {
    fn default() -> Self {
        Self {
            server_address: "127.0.0.1:5102".to_string(),
            username: None,
            password: None,
            token: None,
            stream_name: "a3s_events".to_string(),
            subject_prefix: "events".to_string(),
            partitioning: IggyPartitioning::Single,
            partitions_count: 1,
            max_age_secs: 604_800, // 7 days
            poll_batch_size: 100,
            poll_interval_ms: 50,
            poll_timeout_secs: 5,
            connect_timeout_secs: 5,
        }
    }
}

impl IggyConfig {
    /// Effective login username (server default when unset)
    pub fn effective_username(&self) -> &str {
        self.username.as_deref().unwrap_or("iggy")
    }

    /// Effective login password (server default when unset)
    pub fn effective_password(&self) -> &str {
        self.password.as_deref().unwrap_or("iggy")
    }

    /// Partitions a new topic is created with (always 1 in this version)
    pub fn effective_partitions_count(&self) -> u32 {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = IggyConfig::default();
        assert_eq!(config.server_address, "127.0.0.1:5102");
        assert_eq!(config.stream_name, "a3s_events");
        assert_eq!(config.subject_prefix, "events");
        assert_eq!(config.partitioning, IggyPartitioning::Single);
        assert_eq!(config.max_age_secs, 604_800);
        assert_eq!(config.poll_batch_size, 100);
        assert_eq!(config.connect_timeout_secs, 5);
        assert!(config.username.is_none());
        assert!(config.token.is_none());
    }

    #[test]
    fn test_effective_credentials_default_to_server_root() {
        let config = IggyConfig::default();
        assert_eq!(config.effective_username(), "iggy");
        assert_eq!(config.effective_password(), "iggy");

        let config = IggyConfig {
            username: Some("alice".to_string()),
            password: Some("secret".to_string()),
            ..Default::default()
        };
        assert_eq!(config.effective_username(), "alice");
        assert_eq!(config.effective_password(), "secret");
    }

    #[test]
    fn test_effective_partitions_count_is_single_partition() {
        // v1 subscriptions own per-topic cursors — topics stay single-partition
        // regardless of the (reserved) partitioning knob.
        let config = IggyConfig {
            partitioning: IggyPartitioning::Balanced,
            partitions_count: 8,
            ..Default::default()
        };
        assert_eq!(config.effective_partitions_count(), 1);
    }

    #[test]
    fn test_config_serialization() {
        let config = IggyConfig {
            token: Some("pat-123".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"serverAddress\":\"127.0.0.1:5102\""));
        assert!(json.contains("\"partitioning\":\"single\""));
        assert!(!json.contains("username")); // None fields skipped

        let parsed: IggyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.token.as_deref(), Some("pat-123"));
        assert_eq!(parsed.partitioning, IggyPartitioning::Single);
    }

    #[test]
    fn test_partitioning_serialization() {
        assert_eq!(
            serde_json::to_string(&IggyPartitioning::Single).unwrap(),
            "\"single\""
        );
        assert_eq!(
            serde_json::to_string(&IggyPartitioning::Balanced).unwrap(),
            "\"balanced\""
        );
    }
}
