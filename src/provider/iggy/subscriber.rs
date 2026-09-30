//! Iggy subscription — poll loop over one or more topics with cursor tracking
//!
//! Durability model: durable subscriptions are consumer groups whose offsets
//! are stored explicitly on ack, pinned to partition 0. Iggy stores the
//! offset of the **last consumed message** (storing `head + 1` is rejected
//! by the server), so this provider resumes with
//! `PollingStrategy::offset(stored + 1)` — a convention owned entirely by
//! this provider, self-consistent across versions.
//!
//! Delivery is at-least-once: an ack that fails to persist (or a crash
//! before it) redelivers on the next subscribe.
//!
//! Ordering: per-topic total order (single-partition topics). No ordering
//! is promised across topics.

use super::config::IggyConfig;
use crate::error::{EventError, Result};
use crate::provider::{PendingEvent, ReceivedEvent, Subscription};
use crate::subject::subject_matches;
use crate::types::Event;
use iggy::clients::client::IggyClient as SdkClient;
use iggy::prelude::{Consumer, Identifier, PollingStrategy};
use iggy::prelude::{ConsumerOffsetClient as _, MessageClient as _};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

/// Everything one subscription polls, in round-robin order
pub(crate) struct TopicCursor {
    pub(crate) topic: Identifier,
    /// Next offset to fetch (server semantics: stored offset = last consumed)
    pub(crate) next_offset: u64,
    /// Unix-millis start for the first poll (DeliverPolicy::ByStartTime);
    /// cleared after use — later polls continue from the last seen offset
    pub(crate) initial_timestamp_ms: Option<u64>,
}

/// A decoded message waiting to be handed to the caller
pub(crate) struct Delivery {
    pub(crate) event: Event,
    pub(crate) offset: u64,
    pub(crate) topic: Identifier,
}

impl Delivery {
    pub(crate) fn new(event: Event, offset: u64, topic: Identifier) -> Self {
        Self {
            event,
            offset,
            topic,
        }
    }
}

/// Subscription handle over Iggy topics
pub struct IggySubscription {
    client: Arc<SdkClient>,
    config: Arc<IggyConfig>,
    stream: Identifier,
    consumer: Consumer,
    durable: bool,
    /// Client-side subject filter (None matches everything)
    filter: Option<String>,
    cursors: VecDeque<TopicCursor>,
    buffer: VecDeque<Delivery>,
    closed: bool,
}

/// Everything needed to construct a positioned subscription
pub(crate) struct SubscriptionSpec {
    /// Positioned per-topic cursors (offsets resolved at subscribe time)
    pub(crate) cursors: Vec<TopicCursor>,
    /// Group (durable) or plain (ephemeral) consumer identity
    pub(crate) consumer: Consumer,
    /// Whether acks persist server-side offsets
    pub(crate) durable: bool,
    /// Client-side subject filter (None matches everything)
    pub(crate) filter: Option<String>,
    /// Deliveries resolved at subscribe time (the head message for `Last`)
    pub(crate) seed: Vec<Delivery>,
}

impl IggySubscription {
    /// Build a subscription over already-positioned topic cursors.
    ///
    /// The client resolves deliver policies, probes partition heads and
    /// reads stored offsets before constructing the subscription, so the
    /// poll loop only ever fetches forward from `next_offset`.
    pub(crate) fn new(
        client: Arc<SdkClient>,
        config: Arc<IggyConfig>,
        stream: Identifier,
        spec: SubscriptionSpec,
    ) -> Self {
        Self {
            client,
            config,
            stream,
            consumer: spec.consumer,
            durable: spec.durable,
            filter: spec.filter,
            cursors: spec.cursors.into(),
            buffer: spec.seed.into(),
            closed: false,
        }
    }

    /// Poll every topic once, buffering matching messages.
    ///
    /// Returns true if any message was seen (delivered or skipped).
    async fn poll_batch(&mut self) -> Result<bool> {
        let batch = self.config.poll_batch_size.max(1);
        let mut activity = false;
        let mut cursors = std::mem::take(&mut self.cursors);

        for cursor in cursors.iter_mut() {
            // A timestamp position (DeliverPolicy::ByStartTime) is consumed
            // only once a poll actually RETURNS messages: a send
            // confirmation does not make the message instantly pollable,
            // and an empty first poll must not fall back to offset(0),
            // which would deliver pre-cutoff events.
            let strategy = match cursor.initial_timestamp_ms {
                Some(millis) => {
                    PollingStrategy::timestamp(iggy::prelude::IggyTimestamp::from(millis * 1_000))
                }
                None => PollingStrategy::offset(cursor.next_offset),
            };

            // Durable (group) consumers poll without an explicit partition:
            // the server routes to one of the member's assigned partitions,
            // which prevents duplicate delivery even during a join race.
            // Ephemeral consumers read partition 0 explicitly.
            let strategy_for = move |_partition: u32| strategy;
            let poll_fut = if self.durable {
                self.client.poll_messages_with_strategy_for(
                    &self.stream,
                    &cursor.topic,
                    None,
                    &self.consumer,
                    &strategy_for,
                    batch,
                    false,
                )
            } else {
                self.client.poll_messages(
                    &self.stream,
                    &cursor.topic,
                    Some(0),
                    &self.consumer,
                    &strategy,
                    batch,
                    false,
                )
            };

            let polled =
                tokio::time::timeout(Duration::from_secs(self.config.poll_timeout_secs), poll_fut)
                    .await
                    .map_err(|_| {
                        EventError::Timeout(format!(
                            "poll timed out after {}s on topic '{}'",
                            self.config.poll_timeout_secs, cursor.topic
                        ))
                    })?
                    .map_err(|e| {
                        EventError::JetStream(format!(
                            "poll failed on topic '{}': {e}",
                            cursor.topic
                        ))
                    })?;

            for msg in &polled.messages {
                let offset = msg.header.offset;
                activity = true;
                cursor.next_offset = offset + 1;
                if let Some(delivery) = self.decode(msg, offset, &cursor.topic) {
                    self.buffer.push_back(delivery);
                }
            }

            // The timestamp positioned us: from here on, continue by offset.
            if !polled.messages.is_empty() {
                cursor.initial_timestamp_ms = None;
            }
        }

        self.cursors = cursors;
        Ok(activity)
    }

    /// Decode an Iggy message and apply the subject filter.
    ///
    /// Undecodable payloads are skipped with a warning — a poison message
    /// must not wedge the consumer (its offset still advances).
    fn decode(
        &self,
        msg: &iggy::prelude::IggyMessage,
        offset: u64,
        topic: &Identifier,
    ) -> Option<Delivery> {
        let event: Event = match serde_json::from_slice(&msg.payload) {
            Ok(event) => event,
            Err(e) => {
                tracing::warn!(
                    topic = %topic,
                    offset,
                    "Skipping undecodable Iggy message: {e}"
                );
                return None;
            }
        };

        if let Some(filter) = &self.filter {
            if !subject_matches(&event.subject, filter) {
                return None;
            }
        }

        Some(Delivery {
            event,
            offset,
            topic: topic.clone(),
        })
    }

    /// Persist the last-consumed offset for a processed message.
    ///
    /// Iggy only accepts offsets within the partition's existing range, and
    /// this is always the offset of a message we just handed out.
    async fn commit(&self, topic: &Identifier, consumed_offset: u64) {
        if !self.durable {
            return;
        }
        if let Err(e) = self
            .client
            .store_consumer_offset(
                &self.consumer,
                &self.stream,
                topic,
                Some(0),
                consumed_offset,
            )
            .await
        {
            // At-least-once: a failed commit redelivers later.
            tracing::warn!(
                topic = %topic,
                consumed_offset,
                "Failed to store Iggy consumer offset: {e}"
            );
        }
    }

    /// Fetch more deliveries into the buffer, sleeping when idle.
    async fn refill(&mut self) -> Result<()> {
        let activity = self.poll_batch().await?;
        if !activity {
            tokio::time::sleep(Duration::from_millis(self.config.poll_interval_ms.max(1))).await;
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Subscription for IggySubscription {
    async fn next(&mut self) -> Result<Option<ReceivedEvent>> {
        loop {
            if let Some(delivery) = self.buffer.pop_front() {
                self.commit(&delivery.topic, delivery.offset).await;
                return Ok(Some(ReceivedEvent {
                    event: delivery.event,
                    sequence: delivery.offset,
                    num_delivered: 1,
                    stream: self.config.stream_name.clone(),
                }));
            }
            if self.closed {
                return Ok(None);
            }
            self.refill().await?;
        }
    }

    async fn next_manual_ack(&mut self) -> Result<Option<PendingEvent>> {
        loop {
            if let Some(delivery) = self.buffer.pop_front() {
                let client = Arc::clone(&self.client);
                let stream = self.stream.clone();
                let topic = delivery.topic.clone();
                let consumer = self.consumer.clone();
                let durable = self.durable;
                let consumed_offset = delivery.offset;

                let received = ReceivedEvent {
                    event: delivery.event,
                    sequence: delivery.offset,
                    num_delivered: 1,
                    stream: self.config.stream_name.clone(),
                };

                return Ok(Some(PendingEvent::new(
                    received,
                    // ack: persist the last-consumed offset
                    move || {
                        let client = Arc::clone(&client);
                        let stream = stream.clone();
                        let topic = topic.clone();
                        let consumer = consumer.clone();
                        Box::pin(async move {
                            if durable {
                                client
                                    .store_consumer_offset(
                                        &consumer,
                                        &stream,
                                        &topic,
                                        Some(0),
                                        consumed_offset,
                                    )
                                    .await
                                    .map_err(|e| {
                                        EventError::Ack(format!("offset store failed: {e}"))
                                    })?;
                            }
                            Ok(())
                        })
                    },
                    // nak: do nothing — the uncommitted offset redelivers
                    move || Box::pin(async { Ok(()) }),
                )));
            }
            if self.closed {
                return Ok(None);
            }
            self.refill().await?;
        }
    }
}
