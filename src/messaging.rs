//! Cross-session messaging trait for inter-process communication
//!
//! Provides a trait abstraction for sending messages between sessions,
//! supporting different transports (UDS, TCP, HTTP) without coupling
//! to a specific implementation.

use crate::error::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::RwLock;
use std::time::Duration;

/// Message envelope for cross-session communication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Unique message identifier
    pub id: String,
    /// Source session ID
    pub source_id: String,
    /// Target session ID (or broadcast if None)
    pub target_id: Option<String>,
    /// Message type/category
    pub msg_type: String,
    /// Message payload
    pub payload: serde_json::Value,
    /// Timestamp (Unix milliseconds)
    pub timestamp: u64,
}

impl Message {
    /// Create a new message
    pub fn new(source_id: String, msg_type: String, payload: serde_json::Value) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            source_id,
            target_id: None,
            msg_type,
            payload,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }
    }

    /// Create a message targeted at a specific session
    pub fn to_session(mut self, target_id: String) -> Self {
        self.target_id = Some(target_id);
        self
    }

    /// Create a broadcast message (no specific target)
    pub fn broadcast(source_id: String, msg_type: String, payload: serde_json::Value) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            source_id,
            target_id: None,
            msg_type,
            payload,
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64,
        }
    }
}

/// Message handler callback
pub type MessageHandler = Box<dyn Fn(Message) -> BoxFuture<'static, ()> + Send + Sync>;

/// Shared message handler with optional callback
pub struct MessageHandlerRef {
    handler: Option<MessageHandler>,
}

impl MessageHandlerRef {
    pub fn new(handler: MessageHandler) -> Self {
        Self {
            handler: Some(handler),
        }
    }

    pub fn none() -> Self {
        Self { handler: None }
    }

    pub fn handle(&self, msg: Message) -> BoxFuture<'static, ()> {
        if let Some(ref h) = self.handler {
            h(msg)
        } else {
            Box::pin(std::future::ready(()))
        }
    }
}

/// Cross-session messaging port trait
///
/// Abstraction for sending and receiving messages between sessions.
/// Implementations can use UDS, TCP, HTTP, or any other transport.
#[async_trait]
pub trait MessagingPort: Send + Sync {
    /// Send a message to a target session (or broadcast if target_id is None)
    async fn send(&self, msg: &Message) -> Result<()>;

    /// Subscribe to messages matching the given filter
    ///
    /// The filter is a subject pattern (e.g., "session.*", "*").
    /// Returns a stream of messages.
    async fn subscribe(&self, filter: &str) -> Result<Box<dyn MessageStream>>;

    /// Check if the messaging port is connected
    fn is_connected(&self) -> bool;

    /// Get the port name (e.g., "uds", "tcp", "http")
    fn name(&self) -> &str;
}

/// Message stream from a subscription
#[async_trait]
pub trait MessageStream: Send + Sync {
    /// Receive the next message (blocks until available)
    async fn next(&mut self) -> Result<Option<Message>>;

    /// Receive with timeout
    async fn next_timeout(&mut self, timeout: Duration) -> Result<Option<Message>>;
}

/// In-memory message broker for single-process testing
pub struct InMemoryMessaging {
    subscribers: std::sync::Arc<RwLock<Vec<(String, flume::Sender<Message>)>>>,
}

impl InMemoryMessaging {
    pub fn new() -> Self {
        Self {
            subscribers: std::sync::Arc::new(RwLock::new(Vec::new())),
        }
    }
}

impl Default for InMemoryMessaging {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl MessagingPort for InMemoryMessaging {
    async fn send(&self, msg: &Message) -> Result<()> {
        let subscribers = self.subscribers.read().unwrap();

        // Match subscribers by filter pattern
        let pattern = match &msg.target_id {
            Some(target) => format!("session.{}", target),
            None => "*".to_string(),
        };

        for (filter, sender) in subscribers.iter() {
            if matches_pattern(&pattern, filter) {
                let _ = sender.send(msg.clone());
            }
        }

        Ok(())
    }

    async fn subscribe(&self, filter: &str) -> Result<Box<dyn MessageStream>> {
        let (tx, rx) = flume::unbounded();
        {
            let mut subscribers = self.subscribers.write().unwrap();
            subscribers.push((filter.to_string(), tx));
        }
        Ok(Box::new(InMemoryStream { receiver: rx }))
    }

    fn is_connected(&self) -> bool {
        true
    }

    fn name(&self) -> &str {
        "in-memory"
    }
}

/// In-memory message stream
pub struct InMemoryStream {
    receiver: flume::Receiver<Message>,
}

#[async_trait]
impl MessageStream for InMemoryStream {
    async fn next(&mut self) -> Result<Option<Message>> {
        Ok(self.receiver.recv_async().await.ok())
    }

    async fn next_timeout(&mut self, timeout: Duration) -> Result<Option<Message>> {
        let result = tokio::time::timeout(timeout, self.receiver.recv_async()).await;
        match result {
            Ok(Ok(msg)) => Ok(Some(msg)),
            Ok(Err(_)) => Ok(None),
            Err(_) => Ok(None),
        }
    }
}

/// Simple pattern matching (glob-style)
fn matches_pattern(pattern: &str, filter: &str) -> bool {
    if pattern == "*" || filter == "*" {
        return true;
    }

    let pattern_parts: Vec<&str> = pattern.split('.').collect();
    let filter_parts: Vec<&str> = filter.split('.').collect();

    if pattern_parts.len() != filter_parts.len() {
        return false;
    }

    for (p, f) in pattern_parts.iter().zip(filter_parts.iter()) {
        if *p != "*" && *p != *f {
            return false;
        }
    }

    true
}

/// Box future type alias
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

// Re-export
pub use crate::error::Result as MessagingResult;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_send_receive() {
        let messaging = InMemoryMessaging::new();

        let mut stream = messaging.subscribe("session.*").await.unwrap();

        let msg = Message::new(
            "session-1".to_string(),
            "test".to_string(),
            serde_json::json!({}),
        );
        messaging.send(&msg).await.unwrap();

        let received = stream.next().await.unwrap().unwrap();
        assert_eq!(received.source_id, "session-1");
    }

    #[tokio::test]
    async fn test_broadcast() {
        let messaging = InMemoryMessaging::new();

        let mut stream1 = messaging.subscribe("*").await.unwrap();
        let mut stream2 = messaging.subscribe("*").await.unwrap();

        let msg = Message::broadcast(
            "session-1".to_string(),
            "alert".to_string(),
            serde_json::json!({"msg": "hi"}),
        );
        messaging.send(&msg).await.unwrap();

        // Both subscribers should receive
        let received1 = stream1.next().await.unwrap().unwrap();
        let received2 = stream2.next().await.unwrap().unwrap();

        assert_eq!(received1.source_id, "session-1");
        assert_eq!(received2.source_id, "session-1");
    }

    #[test]
    fn test_message_to_session() {
        let msg = Message::new(
            "s1".to_string(),
            "private".to_string(),
            serde_json::json!({}),
        )
        .to_session("s2".to_string());

        assert_eq!(msg.target_id, Some("s2".to_string()));
    }

    #[test]
    fn test_pattern_matching() {
        assert!(matches_pattern("session.*", "session.123"));
        assert!(matches_pattern("*", "anything"));
        assert!(!matches_pattern("session.123", "session.456"));
    }

    #[test]
    fn test_pattern_matching_exact() {
        assert!(matches_pattern("a.b.c", "a.b.c"));
        assert!(!matches_pattern("a.b.c", "a.b.d"));
    }

    #[test]
    fn test_pattern_matching_wildcard_at_end() {
        assert!(matches_pattern("session.*", "session.abc"));
        assert!(matches_pattern("session.*", "session.123"));
    }

    #[test]
    fn test_pattern_matching_star_matches_all() {
        assert!(matches_pattern("*", "anything"));
        assert!(matches_pattern("*", "hello"));
        assert!(matches_pattern("*", "x"));
    }

    #[test]
    fn test_pattern_matching_length_mismatch() {
        assert!(!matches_pattern("a.b", "a.b.c"));
        assert!(!matches_pattern("a.b.c", "a.b"));
    }

    #[tokio::test]
    async fn test_in_memory_messaging_is_connected() {
        let messaging = InMemoryMessaging::new();
        assert!(messaging.is_connected());
    }

    #[tokio::test]
    async fn test_in_memory_messaging_name() {
        let messaging = InMemoryMessaging::new();
        assert_eq!(messaging.name(), "in-memory");
    }

    #[tokio::test]
    async fn test_message_new_has_id() {
        let msg = Message::new("s1".to_string(), "test".to_string(), serde_json::json!({}));
        assert!(!msg.id.is_empty());
    }

    #[tokio::test]
    async fn test_message_new_has_timestamp() {
        let msg = Message::new("s1".to_string(), "test".to_string(), serde_json::json!({}));
        assert!(msg.timestamp > 0);
    }

    #[tokio::test]
    async fn test_message_broadcast_has_no_target() {
        let msg = Message::broadcast("s1".to_string(), "alert".to_string(), serde_json::json!({}));
        assert!(msg.target_id.is_none());
    }

    #[tokio::test]
    async fn test_message_serialize_roundtrip() {
        let msg = Message::new(
            "s1".to_string(),
            "test".to_string(),
            serde_json::json!({"key": "value"}),
        );
        let serialized = serde_json::to_string(&msg).unwrap();
        let deserialized: Message = serde_json::from_str(&serialized).unwrap();
        assert_eq!(deserialized.id, msg.id);
        assert_eq!(deserialized.source_id, msg.source_id);
        assert_eq!(deserialized.msg_type, msg.msg_type);
    }

    #[tokio::test]
    async fn test_message_clone() {
        let msg = Message::new("s1".to_string(), "test".to_string(), serde_json::json!({}));
        let cloned = msg.clone();
        assert_eq!(cloned.id, msg.id);
        assert_eq!(cloned.source_id, msg.source_id);
    }

    #[tokio::test]
    async fn test_subscribe_and_send_to_specific_session() {
        let messaging = InMemoryMessaging::new();

        let mut stream = messaging.subscribe("session.123").await.unwrap();

        let msg = Message::new(
            "session-1".to_string(),
            "test".to_string(),
            serde_json::json!({}),
        )
        .to_session("session.123".to_string());
        messaging.send(&msg).await.unwrap();

        let received = stream.next().await.unwrap().unwrap();
        assert_eq!(received.source_id, "session-1");
    }

    #[tokio::test]
    async fn test_next_timeout_returns_none_on_timeout() {
        let messaging = InMemoryMessaging::new();
        let mut stream = messaging.subscribe("test.*").await.unwrap();

        let result = stream
            .next_timeout(std::time::Duration::from_millis(50))
            .await
            .unwrap();
        assert!(result.is_none());
    }

    #[tokio::test]
    async fn test_message_handler_ref_none() {
        let handler = MessageHandlerRef::none();
        let msg = Message::new("s1".to_string(), "test".to_string(), serde_json::json!({}));
        handler.handle(msg); // Should not panic
    }

    #[test]
    fn test_message_debug() {
        let msg = Message::new("s1".to_string(), "test".to_string(), serde_json::json!({}));
        let debug_str = format!("{:?}", msg);
        assert!(debug_str.contains("Message"));
    }
}
