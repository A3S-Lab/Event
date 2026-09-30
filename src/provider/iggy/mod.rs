//! Apache Iggy event provider
//!
//! Implements `EventProvider` using Apache Iggy for persistent,
//! distributed event streaming. Iggy organizes data as
//! stream → topic → partition; this provider maps the a3s-event
//! subject space onto it with one rule:
//!
//! - everything lives in the single configured stream
//! - each subject **category** (the token after the prefix) is one topic
//! - the full subject rides in the payload and the `a3s-subject` user
//!   header; subscription filters narrow it client-side
//!
//! Durability: durable subscriptions are Iggy consumer groups with
//! explicitly stored offsets (at-least-once, next-to-consume convention).
//! Ephemeral subscriptions keep no server-side state.
//!
//! Ordering: guaranteed within a topic (per-category total order, single
//! partition). No ordering is promised across topics.
//!
//! Consumer identity: a consumer name maps to one group member **per client
//! connection**. Two `subscribe_durable` calls under one name through the
//! same `IggyProvider` share that connection's identity and will each see
//! the topic's messages — use one provider (connection) per group member.
//!
//! Known limitations of this version (documented, fail-closed where the
//! semantics would be a lie):
//! - `PublishOptions::expected_sequence` is rejected
//! - `DeliverPolicy::LastPerSubject` is rejected
//! - `max_deliver` / `backoff_secs` / `max_ack_pending` / `ack_wait_secs`
//!   are accepted and ignored (no per-group redelivery controls in the
//!   low-level polling API)
//! - `IggyPartitioning::Balanced` is reserved and currently behaves like
//!   [`IggyPartitioning::Single`] (topics are created with one partition)

mod client;
mod config;
mod mapping;
mod policy;
mod subscriber;

pub use client::{IggyClient, StreamInfo};
pub use config::{IggyConfig, IggyPartitioning};
pub use subscriber::IggySubscription;

use crate::error::Result;
use crate::provider::{EventProvider, ProviderInfo, Subscription};
use crate::types::{Event, PublishOptions, SubscribeOptions};
use async_trait::async_trait;

/// Apache Iggy event provider
///
/// Wraps [`IggyClient`] and implements the `EventProvider` trait.
pub struct IggyProvider {
    client: IggyClient,
}

impl IggyProvider {
    /// Connect to Iggy and initialize the stream
    pub async fn connect(config: IggyConfig) -> Result<Self> {
        let client = IggyClient::connect(config).await?;
        Ok(Self { client })
    }

    /// Get the underlying Iggy client for advanced usage
    pub fn client(&self) -> &IggyClient {
        &self.client
    }
}

#[async_trait]
impl EventProvider for IggyProvider {
    async fn publish(&self, event: &Event) -> Result<u64> {
        self.client.publish(event).await
    }

    async fn subscribe_durable(
        &self,
        consumer_name: &str,
        filter_subject: &str,
    ) -> Result<Box<dyn Subscription>> {
        let sub = self
            .client
            .subscribe_durable(consumer_name, filter_subject)
            .await?;
        Ok(Box::new(sub))
    }

    async fn subscribe(&self, filter_subject: &str) -> Result<Box<dyn Subscription>> {
        let sub = self.client.subscribe(filter_subject).await?;
        Ok(Box::new(sub))
    }

    async fn history(&self, filter_subject: Option<&str>, limit: usize) -> Result<Vec<Event>> {
        self.client.history(filter_subject, limit).await
    }

    async fn unsubscribe(&self, consumer_name: &str) -> Result<()> {
        self.client.unsubscribe(consumer_name).await
    }

    async fn info(&self) -> Result<ProviderInfo> {
        let info = self.client.stream_info().await?;
        Ok(ProviderInfo {
            provider: "iggy".to_string(),
            messages: info.messages,
            bytes: info.bytes,
            consumers: info.consumer_groups,
        })
    }

    fn subject_prefix(&self) -> &str {
        &self.client.config().subject_prefix
    }

    fn name(&self) -> &str {
        "iggy"
    }

    async fn publish_with_options(&self, event: &Event, opts: &PublishOptions) -> Result<u64> {
        self.client.publish_with_options(event, opts).await
    }

    async fn subscribe_durable_with_options(
        &self,
        consumer_name: &str,
        filter_subject: &str,
        opts: &SubscribeOptions,
    ) -> Result<Box<dyn Subscription>> {
        let sub = self
            .client
            .subscribe_durable_with_options(consumer_name, filter_subject, opts)
            .await?;
        Ok(Box::new(sub))
    }

    async fn subscribe_with_options(
        &self,
        filter_subject: &str,
        opts: &SubscribeOptions,
    ) -> Result<Box<dyn Subscription>> {
        let sub = self
            .client
            .subscribe_with_options(filter_subject, opts)
            .await?;
        Ok(Box::new(sub))
    }
}
