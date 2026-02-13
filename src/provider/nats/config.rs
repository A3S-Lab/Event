//! Configuration for the NATS event system

use serde::{Deserialize, Serialize};

/// NATS connection and JetStream configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NatsConfig {
    /// NATS server URL (e.g., "nats://127.0.0.1:4222")
    pub url: String,

    /// Optional authentication token
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,

    /// Optional credentials file path (for NKey or JWT auth)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials_path: Option<String>,

    /// JetStream stream name
    pub stream_name: String,

    /// Subject prefix for events (default: "events")
    pub subject_prefix: String,

    /// JetStream storage type
    pub storage: StorageType,

    /// Maximum number of events to retain in the stream
    pub max_events: i64,

    /// Maximum age of events in seconds (0 = unlimited)
    pub max_age_secs: u64,

    /// Maximum bytes for the stream (0 = unlimited)
    pub max_bytes: i64,

    /// Connection timeout in seconds
    pub connect_timeout_secs: u64,

    /// Request timeout in seconds
    pub request_timeout_secs: u64,
}

/// JetStream storage backend
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StorageType {
    /// File-based storage (persistent across restarts)
    File,
    /// In-memory storage (faster, lost on restart)
    Memory,
}

impl Default for NatsConfig {
    fn default() -> Self {
        Self {
            url: "nats://127.0.0.1:4222".to_string(),
            token: None,
            credentials_path: None,
            stream_name: "A3S_EVENTS".to_string(),
            subject_prefix: "events".to_string(),
            storage: StorageType::File,
            max_events: 100_000,
            max_age_secs: 604_800, // 7 days
            max_bytes: 0,          // unlimited
            connect_timeout_secs: 5,
            request_timeout_secs: 10,
        }
    }
}

impl NatsConfig {
    /// Build the full subject pattern for the stream (e.g., "events.>")
    pub fn stream_subjects(&self) -> Vec<String> {
        vec![format!("{}.>", self.subject_prefix)]
    }

    /// Build a full subject from category and topic
    ///
    /// Example: prefix="events", category="market", topic="forex.usd_cny"
    /// → "events.market.forex.usd_cny"
    pub fn build_subject(&self, category: &str, topic: &str) -> String {
        format!("{}.{}.{}", self.subject_prefix, category, topic)
    }

    /// Build a wildcard subject for a category (e.g., "events.market.>")
    pub fn category_subject(&self, category: &str) -> String {
        format!("{}.{}.>", self.subject_prefix, category)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = NatsConfig::default();
        assert_eq!(config.url, "nats://127.0.0.1:4222");
        assert_eq!(config.stream_name, "A3S_EVENTS");
        assert_eq!(config.subject_prefix, "events");
        assert_eq!(config.storage, StorageType::File);
        assert_eq!(config.max_events, 100_000);
        assert_eq!(config.max_age_secs, 604_800);
        assert_eq!(config.connect_timeout_secs, 5);
    }

    #[test]
    fn test_stream_subjects() {
        let config = NatsConfig::default();
        assert_eq!(config.stream_subjects(), vec!["events.>"]);
    }

    #[test]
    fn test_build_subject() {
        let config = NatsConfig::default();
        assert_eq!(
            config.build_subject("market", "forex.usd_cny"),
            "events.market.forex.usd_cny"
        );
        assert_eq!(
            config.build_subject("system", "deploy"),
            "events.system.deploy"
        );
    }

    #[test]
    fn test_category_subject() {
        let config = NatsConfig::default();
        assert_eq!(config.category_subject("market"), "events.market.>");
    }

    #[test]
    fn test_config_serialization() {
        let config = NatsConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("\"streamName\":\"A3S_EVENTS\""));
        assert!(!json.contains("token")); // None fields skipped

        let parsed: NatsConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stream_name, "A3S_EVENTS");
        assert!(parsed.token.is_none());
    }

    #[test]
    fn test_storage_type_serialization() {
        let file_json = serde_json::to_string(&StorageType::File).unwrap();
        assert_eq!(file_json, "\"file\"");

        let mem_json = serde_json::to_string(&StorageType::Memory).unwrap();
        assert_eq!(mem_json, "\"memory\"");

        let parsed: StorageType = serde_json::from_str("\"memory\"").unwrap();
        assert_eq!(parsed, StorageType::Memory);
    }
}
