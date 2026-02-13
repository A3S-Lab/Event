//! NATS JetStream subscription — implements the `Subscription` trait

use crate::error::{EventError, Result};
use crate::provider::{PendingEvent, Subscription};
use crate::types::{Event, ReceivedEvent};
use async_trait::async_trait;

/// NATS JetStream subscription backed by a pull consumer
pub struct NatsSubscription {
    messages: async_nats::jetstream::consumer::pull::Stream,
    stream_name: String,
}

impl NatsSubscription {
    /// Create a new subscription from a JetStream pull consumer message stream
    pub(crate) fn new(
        messages: async_nats::jetstream::consumer::pull::Stream,
        stream_name: String,
    ) -> Self {
        Self {
            messages,
            stream_name,
        }
    }
}

#[async_trait]
impl Subscription for NatsSubscription {
    async fn next(&mut self) -> Result<Option<ReceivedEvent>> {
        use futures::StreamExt;

        match self.messages.next().await {
            Some(Ok(msg)) => {
                let event: Event = serde_json::from_slice(&msg.payload)?;
                let info = msg.info().map_err(|e| {
                    EventError::JetStream(format!("Failed to get message info: {}", e))
                })?;

                let received = ReceivedEvent {
                    event,
                    sequence: info.stream_sequence,
                    num_delivered: info.delivered as u64,
                    stream: self.stream_name.clone(),
                };

                // Auto-ack on successful deserialization
                msg.ack().await.map_err(|e| {
                    EventError::Ack(format!(
                        "Failed to ack message seq {}: {}",
                        info.stream_sequence, e
                    ))
                })?;

                Ok(Some(received))
            }
            Some(Err(e)) => Err(EventError::JetStream(format!(
                "Error receiving message: {}",
                e
            ))),
            None => Ok(None),
        }
    }

    async fn next_manual_ack(&mut self) -> Result<Option<PendingEvent>> {
        use futures::StreamExt;

        match self.messages.next().await {
            Some(Ok(msg)) => {
                let event: Event = serde_json::from_slice(&msg.payload)?;
                let info = msg.info().map_err(|e| {
                    EventError::JetStream(format!("Failed to get message info: {}", e))
                })?;

                let received = ReceivedEvent {
                    event,
                    sequence: info.stream_sequence,
                    num_delivered: info.delivered as u64,
                    stream: self.stream_name.clone(),
                };

                let ack_msg = msg.clone();
                let nak_msg = msg;

                Ok(Some(PendingEvent::new(
                    received,
                    move || {
                        Box::pin(async move {
                            ack_msg.ack().await.map_err(|e| {
                                EventError::Ack(format!("Failed to ack: {}", e))
                            })
                        })
                    },
                    move || {
                        Box::pin(async move {
                            nak_msg
                                .ack_with(async_nats::jetstream::AckKind::Nak(None))
                                .await
                                .map_err(|e| {
                                    EventError::Ack(format!("Failed to nak: {}", e))
                                })
                        })
                    },
                )))
            }
            Some(Err(e)) => Err(EventError::JetStream(format!(
                "Error receiving message: {}",
                e
            ))),
            None => Ok(None),
        }
    }
}
