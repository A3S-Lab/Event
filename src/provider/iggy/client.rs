//! Iggy client — connect, ensure stream/topic, publish, subscribe, query
//!
//! Wraps the official `iggy` SDK client. All requests use explicit
//! (non-auto-commit) offset handling; durability is owned by
//! [`super::subscriber::IggySubscription`].

use super::config::{IggyConfig, IggyPartitioning};
use super::mapping::{parse_subject, resolve_filter, sanitize_name, FilterRoute};
use super::subscriber::IggySubscription;
use crate::error::{EventError, Result};
use crate::subject::subject_matches;
use crate::types::{DeliverPolicy, Event, PublishOptions, SubscribeOptions};
use iggy::clients::client::IggyClient as SdkClient;
use iggy::clients::client_builder::IggyClientBuilder;
use iggy::prelude::{
    Consumer, ConsumerGroupClient, ConsumerOffsetClient, Identifier, IggyDuration, IggyExpiry,
    IggyMessage, MessageClient, Partitioning, PersonalAccessTokenClient, PollingStrategy,
    StreamClient, TopicClient, TopicCreateOptions, UserClient,
};
use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

/// User header carrying the full event subject
const HEADER_SUBJECT: &str = "a3s-subject";
/// User header carrying the event id
const HEADER_EVENT_ID: &str = "a3s-event-id";
/// User header carrying the caller's dedup id (observability; broker-side
/// idempotence depends on server configuration and numeric message ids)
const HEADER_MSG_ID: &str = "a3s-msg-id";

/// Iggy client
///
/// Low-level client for publishing and subscribing to events via Apache
/// Iggy. Manages the connection and the stream/topic lifecycle: topics are
/// created on demand, one per subject category.
pub struct IggyClient {
    client: Arc<SdkClient>,
    config: Arc<IggyConfig>,
    stream: Identifier,
    /// Topics already verified to exist in the stream
    ensured: Mutex<HashSet<String>>,
    /// Client-side publish sequence (starting at 1)
    sequence: AtomicU64,
}

impl IggyClient {
    /// Connect to Iggy, log in, and ensure the stream exists
    pub async fn connect(config: IggyConfig) -> Result<Self> {
        // The provider owns the connect deadline: the SDK's dial has no
        // bound of its own for a single endpoint, so build + login are
        // wrapped in the configured timeout.
        let connect_fut = async {
            let sdk = IggyClientBuilder::new()
                .with_tcp()
                .with_server_address(config.server_address.clone())
                .build()
                .map_err(|e| EventError::Connection(format!("{}: {e}", config.server_address)))?;

            match &config.token {
                Some(token) => sdk
                    .login_with_personal_access_token(token)
                    .await
                    .map_err(|e| EventError::Connection(format!("token login failed: {e}")))?,
                None => sdk
                    .login_user(config.effective_username(), config.effective_password())
                    .await
                    .map_err(|e| {
                        EventError::Connection(format!(
                            "login failed for '{}': {e}",
                            config.effective_username()
                        ))
                    })?,
            };

            Ok::<SdkClient, EventError>(sdk)
        };

        let sdk = tokio::time::timeout(
            Duration::from_secs(config.connect_timeout_secs),
            connect_fut,
        )
        .await
        .map_err(|_| {
            EventError::Connection(format!(
                "connect to {} timed out after {}s",
                config.server_address, config.connect_timeout_secs
            ))
        })??;

        tracing::info!(server = %config.server_address, "Connected to Iggy");

        let stream = Identifier::named(&config.stream_name)
            .map_err(|e| EventError::Config(format!("invalid stream name: {e}")))?;

        // Ensure the stream up front so config errors surface at connect.
        ensure_stream(&sdk, &stream, &config.stream_name).await?;

        let client = Arc::new(sdk);
        let mut ensured = HashSet::new();
        ensured.insert(config.stream_name.clone());
        let config = Arc::new(config);

        Ok(Self {
            client,
            config,
            stream,
            ensured: Mutex::new(ensured),
            sequence: AtomicU64::new(0),
        })
    }

    /// Get the configuration
    pub fn config(&self) -> &IggyConfig {
        &self.config
    }

    /// Publish an event, returning a provider-assigned sequence number.
    ///
    /// The sequence is a client-side counter (starting at 1), mirroring the
    /// in-memory provider. Iggy assigns broker offsets per partition, but
    /// the send confirmation does not reliably carry them, so callers must
    /// not treat this value as a broker offset.
    pub async fn publish(&self, event: &Event) -> Result<u64> {
        self.publish_inner(event, &PublishOptions::default()).await
    }

    /// Publish an event with options.
    ///
    /// - `msg_id`: stored in the `a3s-msg-id` user header. Broker-side
    ///   deduplication is NOT provided in this version.
    /// - `expected_sequence`: unsupported — fails closed with
    ///   [`EventError::Provider`]. Iggy sends have no optimistic-concurrency
    ///   check on the last sequence.
    /// - `timeout_secs`: bounds the send request.
    pub async fn publish_with_options(&self, event: &Event, opts: &PublishOptions) -> Result<u64> {
        if opts.expected_sequence.is_some() {
            return Err(EventError::Provider(
                "expected_sequence is not supported by the iggy provider".to_string(),
            ));
        }
        self.publish_inner(event, opts).await
    }

    async fn publish_inner(&self, event: &Event, opts: &PublishOptions) -> Result<u64> {
        let route = parse_subject(&event.subject, &self.config.subject_prefix)
            .map_err(EventError::Config)?;
        self.ensure_topic(&route.topic).await?;

        let mut headers: BTreeMap<iggy::prelude::HeaderKey, iggy::prelude::HeaderValue> =
            BTreeMap::new();
        if let Ok(key) = HEADER_SUBJECT.parse() {
            if let Ok(value) = event.subject.as_str().parse() {
                headers.insert(key, value);
            }
        }
        if let Ok(key) = HEADER_EVENT_ID.parse() {
            if let Ok(value) = event.id.as_str().parse() {
                headers.insert(key, value);
            }
        }
        if let Some(msg_id) = &opts.msg_id {
            if let (Ok(key), Ok(value)) = (HEADER_MSG_ID.parse(), msg_id.as_str().parse()) {
                headers.insert(key, value);
            }
        }

        let payload = serde_json::to_vec(event)?;
        let message = IggyMessage::builder()
            .payload(bytes::Bytes::from(payload))
            .user_headers(headers)
            .build()
            .map_err(|e| EventError::Publish {
                subject: event.subject.clone(),
                reason: format!("message build failed: {e}"),
            })?;

        let partitioning = match self.config.partitioning {
            // Single partition keeps per-category total order.
            IggyPartitioning::Single => Partitioning::partition_id(0),
            IggyPartitioning::Balanced => Partitioning::balanced(),
        };

        let topic_id = Identifier::named(&route.topic)
            .map_err(|e| EventError::Stream(format!("invalid topic name: {e}")))?;
        let mut messages = [message];
        let send = self
            .client
            .send_messages(&self.stream, &topic_id, &partitioning, &mut messages);

        let response = match opts.timeout_secs {
            Some(secs) => tokio::time::timeout(Duration::from_secs(secs), send)
                .await
                .map_err(|_| {
                    EventError::Timeout(format!(
                        "publish timed out after {secs}s for subject '{}'",
                        event.subject
                    ))
                })?,
            None => send.await,
        }
        .map_err(|e| EventError::Publish {
            subject: event.subject.clone(),
            reason: e.to_string(),
        })?;

        let sequence = self.next_sequence().await;
        tracing::debug!(
            event_id = %event.id,
            subject = %event.subject,
            topic = %route.topic,
            sequence,
            confirmations = response.confirmations.len(),
            "Event published (iggy)"
        );
        Ok(sequence)
    }

    async fn next_sequence(&self) -> u64 {
        self.sequence.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Create a durable subscription (consumer group) over the filter's topics
    pub async fn subscribe_durable(
        &self,
        consumer_name: &str,
        filter_subject: &str,
    ) -> Result<IggySubscription> {
        self.subscribe_durable_impl(consumer_name, filter_subject, &SubscribeOptions::default())
            .await
    }

    /// Create a durable subscription with options
    ///
    /// `max_deliver`, `backoff_secs`, `max_ack_pending` and `ack_wait_secs`
    /// are accepted but ignored: Iggy's low-level polling has no per-group
    /// redelivery controls (the trait contract allows providers to ignore
    /// unsupported options). `LastPerSubject` fails closed — no cheap Iggy
    /// equivalent.
    pub async fn subscribe_durable_with_options(
        &self,
        consumer_name: &str,
        filter_subject: &str,
        opts: &SubscribeOptions,
    ) -> Result<IggySubscription> {
        self.subscribe_durable_impl(consumer_name, filter_subject, opts)
            .await
    }

    async fn subscribe_durable_impl(
        &self,
        consumer_name: &str,
        filter_subject: &str,
        opts: &SubscribeOptions,
    ) -> Result<IggySubscription> {
        if opts.deliver_policy == DeliverPolicy::LastPerSubject {
            return Err(EventError::Provider(
                "DeliverPolicy::LastPerSubject is not supported by the iggy provider".to_string(),
            ));
        }
        if !opts.backoff_secs.is_empty()
            || opts.max_deliver.is_some()
            || opts.max_ack_pending.is_some()
            || opts.ack_wait_secs.is_some()
        {
            tracing::debug!(
                consumer = consumer_name,
                "iggy provider ignores unsupported SubscribeOptions (max_deliver/backoff/max_ack_pending/ack_wait)"
            );
        }

        let topics = self.resolve_topics(filter_subject).await?;
        let sanitized_name = sanitize_name(consumer_name);
        let group = Identifier::named(&sanitized_name)
            .map_err(|e| EventError::Config(format!("invalid consumer name: {e}")))?;

        for topic in &topics {
            // get-or-create the group on this topic
            if let Err(e) = self
                .client
                .create_consumer_group(&self.stream, topic, &sanitized_name)
                .await
            {
                if !is_already_exists(&e) {
                    return Err(EventError::Consumer(format!(
                        "Failed to create consumer group '{consumer_name}' on topic '{topic}': {e}"
                    )));
                }
            }
            self.client
                .join_consumer_group(&self.stream, topic, &group)
                .await
                .map_err(|e| {
                    EventError::Consumer(format!(
                        "Failed to join consumer group '{consumer_name}' on topic '{topic}': {e}"
                    ))
                })?;
        }

        // Resolve starting positions: a stored offset (last-consumed
        // convention → resume at stored + 1) wins over the deliver policy;
        // otherwise the policy positions a fresh consumer at subscribe time.
        let mut cursors = Vec::new();
        let mut seed = Vec::new();
        for topic in &topics {
            let mut resumed = false;
            let mut next_offset = 0u64;
            if let Ok(Some(info)) = self
                .client
                .get_consumer_offset(
                    &Consumer::group(group.clone()),
                    &self.stream,
                    topic,
                    Some(0),
                )
                .await
            {
                next_offset = info.stored_offset + 1;
                resumed = true;
            }

            let plan = super::policy::position_plan(&opts.deliver_policy, resumed);
            if !resumed {
                next_offset = self.apply_probe(topic, &plan, &mut seed).await?;
            }

            cursors.push(super::subscriber::TopicCursor {
                topic: topic.clone(),
                next_offset,
                initial_timestamp_ms: plan.initial_timestamp_ms,
            });
        }

        tracing::info!(
            consumer = consumer_name,
            filter = filter_subject,
            topics = topics.len(),
            "Durable subscription created (iggy)"
        );

        Ok(IggySubscription::new(
            Arc::clone(&self.client),
            Arc::clone(&self.config),
            self.stream.clone(),
            super::subscriber::SubscriptionSpec {
                cursors,
                consumer: Consumer::group(group),
                durable: true,
                filter: Some(filter_subject.to_string()),
                seed,
            },
        ))
    }

    /// Create an ephemeral subscription (no server-side state)
    pub async fn subscribe(&self, filter_subject: &str) -> Result<IggySubscription> {
        self.subscribe_with_options(filter_subject, &SubscribeOptions::default())
            .await
    }

    /// Create an ephemeral subscription with options
    pub async fn subscribe_with_options(
        &self,
        filter_subject: &str,
        opts: &SubscribeOptions,
    ) -> Result<IggySubscription> {
        if opts.deliver_policy == DeliverPolicy::LastPerSubject {
            return Err(EventError::Provider(
                "DeliverPolicy::LastPerSubject is not supported by the iggy provider".to_string(),
            ));
        }

        let topics = self.resolve_topics(filter_subject).await?;
        // Numeric id unique per process; offsets stay client-side.
        let id = crate::types::now_millis() as u32 ^ (std::process::id());
        let consumer = Consumer::new(
            Identifier::numeric(id)
                .map_err(|e| EventError::Config(format!("invalid consumer id: {e}")))?,
        );

        // Position a fresh consumer at subscribe time (race-free).
        let mut cursors = Vec::new();
        let mut seed = Vec::new();
        for topic in &topics {
            let plan = super::policy::position_plan(&opts.deliver_policy, false);
            let next_offset = self.apply_probe(topic, &plan, &mut seed).await?;
            cursors.push(super::subscriber::TopicCursor {
                topic: topic.clone(),
                next_offset,
                initial_timestamp_ms: plan.initial_timestamp_ms,
            });
        }

        tracing::info!(
            filter = filter_subject,
            topics = topics.len(),
            "Ephemeral subscription created (iggy)"
        );

        Ok(IggySubscription::new(
            Arc::clone(&self.client),
            Arc::clone(&self.config),
            self.stream.clone(),
            super::subscriber::SubscriptionSpec {
                cursors,
                consumer,
                durable: false,
                filter: Some(filter_subject.to_string()),
                seed,
            },
        ))
    }

    /// Apply a [`super::policy::PositionPlan`] to one topic: run the
    /// subscribe-time head probe if the plan calls for one, seed the buffer
    /// for `Last`, and return the cursor's next fetch offset.
    async fn apply_probe(
        &self,
        topic: &Identifier,
        plan: &super::policy::PositionPlan,
        seed: &mut Vec<super::subscriber::Delivery>,
    ) -> Result<u64> {
        match plan.probe {
            super::policy::HeadProbe::None => Ok(plan.start_offset),
            super::policy::HeadProbe::SkipToAfterHead | super::policy::HeadProbe::DeliverHead => {
                let head = self.probe_head(topic).await?;
                match head {
                    Some(offset) => {
                        if plan.probe == super::policy::HeadProbe::DeliverHead {
                            if let Some(delivery) = self.read_at(topic, offset).await? {
                                seed.push(delivery);
                            }
                        }
                        Ok(offset + 1)
                    }
                    // Empty partition: park at the next write offset so the
                    // first arriving message is seen.
                    None => Ok(self.partition_head(topic).await?),
                }
            }
        }
    }

    /// Offset of the partition's head message (None when the partition is empty)
    async fn probe_head(&self, topic: &Identifier) -> Result<Option<u64>> {
        let polled = tokio::time::timeout(
            Duration::from_secs(self.config.poll_timeout_secs),
            self.client.poll_messages(
                &self.stream,
                topic,
                Some(0),
                &history_consumer(),
                &PollingStrategy::last(),
                1,
                false,
            ),
        )
        .await
        .map_err(|_| EventError::Timeout(format!("head probe timed out on topic '{topic}'")))?
        .map_err(|e| EventError::JetStream(format!("head probe failed on topic '{topic}': {e}")))?;

        Ok(polled.messages.first().map(|m| m.header.offset))
    }

    /// Fetch and decode one message at an explicit offset
    async fn read_at(
        &self,
        topic: &Identifier,
        offset: u64,
    ) -> Result<Option<super::subscriber::Delivery>> {
        let polled = tokio::time::timeout(
            Duration::from_secs(self.config.poll_timeout_secs),
            self.client.poll_messages(
                &self.stream,
                topic,
                Some(0),
                &history_consumer(),
                &PollingStrategy::offset(offset),
                1,
                false,
            ),
        )
        .await
        .map_err(|_| EventError::Timeout(format!("read timed out on topic '{topic}'")))?
        .map_err(|e| EventError::JetStream(format!("read failed on topic '{topic}': {e}")))?;

        Ok(polled.messages.first().and_then(|m| {
            let offset = m.header.offset;
            serde_json::from_slice::<Event>(&m.payload)
                .ok()
                .map(|event| super::subscriber::Delivery::new(event, offset, topic.clone()))
        }))
    }

    /// Next write offset of partition 0 (partition current offset)
    async fn partition_head(&self, topic: &Identifier) -> Result<u64> {
        let polled = tokio::time::timeout(
            Duration::from_secs(self.config.poll_timeout_secs),
            self.client.poll_messages(
                &self.stream,
                topic,
                Some(0),
                &history_consumer(),
                &PollingStrategy::last(),
                1,
                false,
            ),
        )
        .await
        .map_err(|_| EventError::Timeout(format!("head probe timed out on topic '{topic}'")))?
        .map_err(|e| EventError::JetStream(format!("head probe failed on topic '{topic}': {e}")))?;

        Ok(polled.current_offset)
    }

    /// Fetch historical events, most recent `limit` of the matching set.
    ///
    /// Traversal order is (topic, offset); ordering across topics follows
    /// the stream's topic listing and carries no cross-topic guarantee.
    pub async fn history(&self, filter_subject: Option<&str>, limit: usize) -> Result<Vec<Event>> {
        let topics = match filter_subject {
            Some(filter) => self.resolve_topics(filter).await?,
            None => self.list_topics().await?,
        };
        let filter = filter_subject.map(|s| s.to_string());

        let mut collected: Vec<Event> = Vec::new();
        for topic in &topics {
            let mut offset = 0u64;
            let batch_size = self.config.poll_batch_size.max(1) as usize;
            let mut empty_streak = 0;
            while collected.len() < limit * 4 && empty_streak < 2 {
                let polled = tokio::time::timeout(
                    Duration::from_secs(self.config.poll_timeout_secs),
                    self.client.poll_messages(
                        &self.stream,
                        topic,
                        Some(0),
                        // Plain consumer reading from an explicit offset;
                        // history never touches group state.
                        &history_consumer(),
                        &PollingStrategy::offset(offset),
                        self.config.poll_batch_size.max(1),
                        false,
                    ),
                )
                .await
                .map_err(|_| {
                    EventError::Timeout(format!("history poll timed out on topic '{topic}'"))
                })?
                .map_err(|e| {
                    EventError::JetStream(format!("history fetch failed on topic '{topic}': {e}"))
                })?;

                if polled.messages.is_empty() {
                    empty_streak += 1;
                    continue;
                }
                empty_streak = 0;

                for msg in &polled.messages {
                    offset = msg.header.offset + 1;
                    if let Ok(event) = serde_json::from_slice::<Event>(&msg.payload) {
                        let matches = match &filter {
                            Some(f) => subject_matches(&event.subject, f),
                            None => true,
                        };
                        if matches {
                            collected.push(event);
                        }
                    }
                }
                if polled.messages.len() < batch_size {
                    break;
                }
            }
        }

        let start = collected.len().saturating_sub(limit);
        Ok(collected.split_off(start))
    }

    /// Delete a durable consumer group across the stream's topics.
    ///
    /// Topics or groups that do not exist are ignored.
    pub async fn unsubscribe(&self, consumer_name: &str) -> Result<()> {
        let group = Identifier::named(&sanitize_name(consumer_name))
            .map_err(|e| EventError::Config(format!("invalid consumer name: {e}")))?;
        let topics = self.list_topics().await?;

        for topic in &topics {
            if let Err(e) = self
                .client
                .delete_consumer_group(&self.stream, topic, &group)
                .await
            {
                if !is_not_found(&e) {
                    return Err(EventError::Consumer(format!(
                        "Failed to delete consumer group '{consumer_name}' on topic '{topic}': {e}"
                    )));
                }
            }
        }

        tracing::info!(consumer = consumer_name, "Consumer groups deleted (iggy)");
        Ok(())
    }

    /// Stream statistics
    pub async fn stream_info(&self) -> Result<StreamInfo> {
        let details = self
            .client
            .get_stream(&self.stream)
            .await
            .map_err(|e| EventError::Stream(format!("Failed to get stream info: {e}")))?
            .ok_or_else(|| EventError::NotFound(self.config.stream_name.clone()))?;

        let mut consumer_groups = 0usize;
        for topic in &details.topics {
            let topic_id =
                Identifier::named(&topic.name).map_err(|e| EventError::Stream(e.to_string()))?;
            if let Ok(groups) = self
                .client
                .get_consumer_groups(&self.stream, &topic_id)
                .await
            {
                consumer_groups += groups.len();
            }
        }

        Ok(StreamInfo {
            messages: details.messages_count,
            bytes: details.size.as_bytes_u64(),
            topics: details.topics.len(),
            consumer_groups,
        })
    }

    /// Resolve a filter to the concrete topic identifiers it covers,
    /// creating missing single-topic targets on demand.
    async fn resolve_topics(&self, filter_subject: &str) -> Result<Vec<Identifier>> {
        match resolve_filter(filter_subject, &self.config.subject_prefix)
            .map_err(EventError::Config)?
        {
            FilterRoute::Single { topic } => {
                self.ensure_topic(&topic).await?;
                Ok(vec![Identifier::named(&topic).map_err(|e| {
                    EventError::Stream(format!("invalid topic name: {e}"))
                })?])
            }
            FilterRoute::AllTopics => self.list_topics().await,
        }
    }

    /// All topic identifiers currently in the stream
    async fn list_topics(&self) -> Result<Vec<Identifier>> {
        let topics = self
            .client
            .get_topics(&self.stream)
            .await
            .map_err(|e| EventError::Stream(format!("Failed to list topics: {e}")))?;
        topics
            .into_iter()
            .map(|t| Identifier::named(&t.name).map_err(|e| EventError::Stream(e.to_string())))
            .collect()
    }

    /// Ensure a topic exists, creating it on first use
    async fn ensure_topic(&self, topic: &str) -> Result<()> {
        {
            let ensured = self.ensured.lock().await;
            if ensured.contains(topic) {
                return Ok(());
            }
        }

        let id = Identifier::named(topic)
            .map_err(|e| EventError::Stream(format!("invalid topic name: {e}")))?;
        match self.client.get_topic(&self.stream, &id).await {
            Ok(Some(_)) => {}
            Ok(None) => {
                let options = TopicCreateOptions {
                    partitions_count: Some(self.config.effective_partitions_count()),
                    message_expiry: expiry_from_secs(self.config.max_age_secs),
                    ..Default::default()
                };
                if let Err(e) = self
                    .client
                    .create_topic(&self.stream, topic, &options)
                    .await
                {
                    if !is_already_exists(&e) {
                        return Err(EventError::Stream(format!(
                            "Failed to create topic '{topic}': {e}"
                        )));
                    }
                }
                tracing::info!(topic, stream = %self.config.stream_name, "Iggy topic created");
            }
            Err(e) => {
                return Err(EventError::Stream(format!(
                    "Failed to inspect topic '{topic}': {e}"
                )));
            }
        }

        self.ensured.lock().await.insert(topic.to_string());
        Ok(())
    }
}

/// Plain consumer used only for history reads (never stores offsets)
fn history_consumer() -> Consumer {
    Consumer::new(Identifier::numeric(1).unwrap_or_default())
}

/// Ensure the stream exists, creating it on first use
async fn ensure_stream(sdk: &SdkClient, stream: &Identifier, name: &str) -> Result<()> {
    match sdk.get_stream(stream).await {
        Ok(Some(_)) => Ok(()),
        Ok(None) => sdk
            .create_stream(name)
            .await
            .map(|_| ())
            .map_err(|e| EventError::Stream(format!("Failed to create stream '{name}': {e}"))),
        Err(e) => Err(EventError::Stream(format!(
            "Failed to inspect stream '{name}': {e}"
        ))),
    }
}

/// Summary of stream state
#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub messages: u64,
    pub bytes: u64,
    pub topics: usize,
    pub consumer_groups: usize,
}

/// True when the error is a stream/topic/group already-exists error
fn is_already_exists(e: &iggy::prelude::IggyError) -> bool {
    matches!(
        e,
        iggy::prelude::IggyError::StreamNameAlreadyExists(_)
            | iggy::prelude::IggyError::TopicNameAlreadyExists(..)
            | iggy::prelude::IggyError::ConsumerGroupNameAlreadyExists(..)
    )
}

/// True when the error means the resource was not there
fn is_not_found(e: &iggy::prelude::IggyError) -> bool {
    matches!(
        e,
        iggy::prelude::IggyError::ResourceNotFound(_)
            | iggy::prelude::IggyError::StreamIdNotFound(_)
            | iggy::prelude::IggyError::TopicIdNotFound(..)
            | iggy::prelude::IggyError::ConsumerGroupIdNotFound(..)
            | iggy::prelude::IggyError::ConsumerGroupNameNotFound(..)
    )
}

/// Map `max_age_secs` to a topic message expiry
fn expiry_from_secs(secs: u64) -> Option<IggyExpiry> {
    if secs == 0 {
        Some(IggyExpiry::NeverExpire)
    } else {
        Some(IggyExpiry::ExpireDuration(IggyDuration::from(
            Duration::from_secs(secs),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iggy::prelude::IggyError;

    #[test]
    fn already_exists_matches_stream_topic_group_variants() {
        assert!(is_already_exists(&IggyError::StreamNameAlreadyExists(
            "s".to_string()
        )));
        assert!(is_already_exists(&IggyError::TopicNameAlreadyExists(
            "t".to_string(),
            Identifier::named("t").unwrap(),
        )));
        assert!(is_already_exists(
            &IggyError::ConsumerGroupNameAlreadyExists(
                "g".to_string(),
                Identifier::named("g").unwrap(),
            )
        ));
    }

    #[test]
    fn already_exists_rejects_other_errors() {
        assert!(!is_already_exists(&IggyError::InvalidConfiguration));
        assert!(!is_already_exists(&IggyError::ResourceNotFound(
            "s".to_string()
        )));
    }

    #[test]
    fn not_found_matches_resource_variants() {
        assert!(is_not_found(&IggyError::ResourceNotFound("x".to_string())));
        assert!(is_not_found(&IggyError::StreamIdNotFound(
            Identifier::named("s").unwrap(),
        )));
        assert!(is_not_found(&IggyError::TopicIdNotFound(
            Identifier::named("s").unwrap(),
            Identifier::named("t").unwrap(),
        )));
        assert!(is_not_found(&IggyError::ConsumerGroupIdNotFound(
            Identifier::named("s").unwrap(),
            Identifier::named("g").unwrap(),
        )));
        assert!(is_not_found(&IggyError::ConsumerGroupNameNotFound(
            "g".to_string(),
            Identifier::named("g").unwrap(),
        )));
    }

    #[test]
    fn not_found_rejects_other_errors() {
        assert!(!is_not_found(&IggyError::InvalidConfiguration));
        assert!(!is_not_found(&IggyError::StreamNameAlreadyExists(
            "s".to_string()
        )));
    }

    #[test]
    fn history_consumer_is_numeric_and_stable() {
        let c1 = history_consumer();
        let c2 = history_consumer();
        assert_eq!(c1.kind, c2.kind);
        assert_eq!(
            c1.id.get_u32_value().unwrap(),
            c2.id.get_u32_value().unwrap()
        );
    }
}
