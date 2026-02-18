# A3S Event

<p align="center">
  <strong>Pluggable Event System for A3S</strong>
</p>

<p align="center">
  <em>Provider-agnostic event publish, subscribe, and persistence — swap backends without changing application code</em>
</p>

<p align="center">
  <a href="https://crates.io/crates/a3s-event"><img src="https://img.shields.io/crates/v/a3s-event.svg" alt="crates.io"></a>
  <a href="https://docs.rs/a3s-event"><img src="https://docs.rs/a3s-event/badge.svg" alt="docs.rs"></a>
  <a href="#license"><img src="https://img.shields.io/crates/l/a3s-event.svg" alt="MIT"></a>
</p>

<p align="center">
  <a href="#quick-start">Quick Start</a> •
  <a href="#feature-flags">Feature Flags</a> •
  <a href="#providers">Providers</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#api-reference">API Reference</a> •
  <a href="#custom-providers">Custom Providers</a> •
  <a href="#development">Development</a>
</p>

---

## Overview

**A3S Event** provides a provider-agnostic API for event subscription, dispatch, and persistence. All backends implement the `EventProvider` trait — swap between NATS JetStream, in-memory, or any custom provider without changing application code.

```rust
use a3s_event::{EventBus, Event};
use a3s_event::provider::memory::MemoryProvider;

#[tokio::main]
async fn main() -> a3s_event::Result<()> {
    let bus = EventBus::new(MemoryProvider::default());

    // Publish
    let event = bus.publish(
        "market", "forex.usd_cny",
        "USD/CNY broke through 7.35", "reuters",
        serde_json::json!({"rate": 7.3521}),
    ).await?;

    // Query
    let events = bus.list_events(Some("market"), 50).await?;
    println!("{} market events", events.len());
    Ok(())
}
```

## Feature Flags

All optional modules are behind feature gates. The minimal core (types, memory provider, EventBus, schema, DLQ, metrics) compiles with zero optional dependencies.

| Feature | Default | Description |
|---------|---------|-------------|
| `nats` | ✅ | NATS JetStream provider (`async-nats`, `futures-util`, `time`) |
| `encryption` | ✅ | AES-256-GCM payload encryption (`aes-gcm`, `base64`) |
| `cloudevents` | ✅ | CloudEvents v1.0 conversion (`chrono`) |
| `routing` | ✅ | Broker/Trigger event routing + Sink DLQ |
| `full` | — | All features |

```toml
# Full (default)
a3s-event = "0.3"

# Minimal core — no NATS, no encryption, no CloudEvents, no routing
a3s-event = { version = "0.3", default-features = false }

# Pick what you need
a3s-event = { version = "0.3", default-features = false, features = ["nats", "encryption"] }
```

## Providers

| Provider | Use Case | Persistence | Distribution |
|----------|----------|-------------|--------------|
| `MemoryProvider` | Testing, development, single-process | In-process only | Single process |
| `NatsProvider` | Production, multi-service | JetStream (file/memory) | Distributed |

### Memory Provider

Zero-dependency, in-process event bus using `tokio::sync::broadcast`.

```rust
use a3s_event::provider::memory::{MemoryProvider, MemoryConfig};

let provider = MemoryProvider::new(MemoryConfig {
    subject_prefix: "events".to_string(),
    max_events: 100_000,
    channel_capacity: 10_000,
});

// Or use defaults
let provider = MemoryProvider::default();
```

### NATS JetStream Provider

Requires `nats` feature. Distributed event streaming with persistent storage, durable consumers, and at-least-once delivery.

```rust
use a3s_event::provider::nats::{NatsProvider, NatsConfig, StorageType};

let provider = NatsProvider::connect(NatsConfig {
    url: "nats://127.0.0.1:4222".to_string(),
    stream_name: "A3S_EVENTS".to_string(),
    subject_prefix: "events".to_string(),
    storage: StorageType::File,
    max_events: 100_000,
    max_age_secs: 604_800,  // 7 days
    ..Default::default()
}).await?;
```

## Architecture

```text
┌─────────────────────────────────────────────────────────────┐
│                        EventBus                             │
│  High-level API: publish, subscribe, history, manage subs   │
│                                                             │
│  ┌───────────────────────────────────────────────────────┐  │
│  │              dyn EventProvider                        │  │
│  │  publish() | subscribe() | history() | info()        │  │
│  └───────────────────────────────────────────────────────┘  │
│         │                │                │                  │
│  ┌──────┴──────┐  ┌──────┴──────┐  ┌──────┴──────┐        │
│  │   Memory    │  │    NATS     │  │   Custom    │        │
│  │  Provider   │  │  Provider   │  │  Provider   │        │
│  │ (broadcast) │  │ (JetStream) │  │ (your impl) │        │
│  └─────────────┘  └─────────────┘  └─────────────┘        │
└─────────────────────────────────────────────────────────────┘
```

### Subject Hierarchy

Events follow a dot-separated naming convention:

```
events.<category>.<topic>[.<subtopic>...]

Examples:
  events.market.forex.usd_cny     — forex rate change
  events.system.deploy.gateway    — service deployment
  events.task.completed           — task completion
```

Wildcard patterns:
- `events.market.>` — all market events (any depth)
- `events.*.forex` — forex events from any category

### Core Types

| Type | Description |
|------|-------------|
| `EventProvider` | Core trait — all backends implement this |
| `EventBus` | High-level API with subscription management |
| `Event` | Message envelope (id, subject, category, event_type, version, payload) |
| `ReceivedEvent` | Event with delivery context (sequence, num_delivered, stream) |
| `Subscription` | Async event stream from any provider |
| `PendingEvent` | Event with ack/nak callbacks for manual acknowledgement |
| `ProviderInfo` | Backend status (message count, bytes, consumers) |
| `SchemaRegistry` | Event type validation with compatibility checks |
| `StateStore` | Trait for persisting subscription state |
| `EventMetrics` | Lock-free atomic counters for publish, subscribe, error, latency |
| `DlqHandler` | Dead letter queue trait + `MemoryDlqHandler` |

Optional types (behind feature gates):

| Type | Feature | Description |
|------|---------|-------------|
| `Aes256GcmEncryptor` | `encryption` | AES-256-GCM encryptor with key rotation |
| `CloudEvent` | `cloudevents` | CloudEvents v1.0 envelope with lossless conversion |
| `Broker` / `Trigger` | `routing` | Knative-inspired event routing with filters |
| `EventSink` | `routing` | Delivery targets: `TopicSink`, `InProcessSink`, `LogSink` |
| `SinkDlqHandler` | `routing` | DLQ handler that forwards dead letters through sinks |
| `NatsProvider` | `nats` | NATS JetStream distributed provider |

## API Reference

### EventBus

```rust
use a3s_event::{EventBus, SubscriptionFilter};
use a3s_event::provider::memory::MemoryProvider;

let bus = EventBus::new(MemoryProvider::default());

// Publish
let event = bus.publish("market", "forex", "Rate change", "reuters", payload).await?;
let seq = bus.publish_event(&event).await?;

// Query
let events = bus.list_events(Some("market"), 50).await?;
let counts = bus.counts(1000).await?;

// Subscriptions
bus.update_subscription(SubscriptionFilter {
    subscriber_id: "analyst".to_string(),
    subjects: vec!["events.market.>".to_string()],
    durable: true,
    options: None,
}).await?;
let subs = bus.create_subscriber("analyst").await?;
bus.remove_subscription("analyst").await?;

// Info & health
let info = bus.info().await?;
let healthy = bus.health().await?;
let metrics = bus.metrics();
```

### EventProvider Trait

```rust
use a3s_event::provider::EventProvider;

provider.publish(&event).await?;
provider.subscribe("events.market.>").await?;
provider.subscribe_durable("consumer-1", "events.market.>").await?;
provider.history(Some("events.market.>"), 100).await?;
provider.unsubscribe("consumer-1").await?;
provider.info().await?;
provider.build_subject("market", "forex.usd");  // → "events.market.forex.usd"
provider.category_subject("market");             // → "events.market.>"
provider.name();                                 // → "memory" | "nats"
```

### Subscription

```rust
// Auto-ack mode
let mut sub = provider.subscribe("events.>").await?;
while let Some(received) = sub.next().await? {
    println!("{}: {}", received.event.id, received.event.summary);
}

// Manual ack mode
while let Some(pending) = sub.next_manual_ack().await? {
    match process(&pending.received.event) {
        Ok(_) => pending.ack().await?,
        Err(_) => pending.nak().await?,  // request redelivery
    }
}
```

## Custom Providers

Implement `EventProvider` and `Subscription` to add any backend:

```rust
use a3s_event::provider::{EventProvider, Subscription, PendingEvent, ProviderInfo};
use a3s_event::types::{Event, ReceivedEvent};
use a3s_event::Result;
use async_trait::async_trait;

pub struct RedisProvider { /* ... */ }

#[async_trait]
impl EventProvider for RedisProvider {
    async fn publish(&self, event: &Event) -> Result<u64> { todo!() }

    async fn subscribe_durable(
        &self, consumer_name: &str, filter_subject: &str,
    ) -> Result<Box<dyn Subscription>> { todo!() }

    async fn subscribe(&self, filter_subject: &str) -> Result<Box<dyn Subscription>> { todo!() }

    async fn history(
        &self, filter_subject: Option<&str>, limit: usize,
    ) -> Result<Vec<Event>> { todo!() }

    async fn unsubscribe(&self, consumer_name: &str) -> Result<()> { todo!() }
    async fn info(&self) -> Result<ProviderInfo> { todo!() }

    // Only subject_prefix() is required — build_subject() and
    // category_subject() have default implementations.
    fn subject_prefix(&self) -> &str { "events" }
    fn name(&self) -> &str { "redis" }
}
```

Then use it like any other provider:

```rust
let bus = EventBus::new(RedisProvider::new(config));
bus.publish("market", "forex", "Rate change", "source", payload).await?;
```

## Responsibility Boundary

A3S Event does NOT re-implement capabilities that providers already offer natively.

| Capability | Owner | Notes |
|------------|-------|-------|
| Retry / backoff | **Provider** | NATS: `MaxDeliver` + `BackOff`. Kafka: consumer retry topic. |
| Backpressure | **Provider** | NATS: pull consumer + `MaxAckPending`. Kafka: consumer poll. |
| Connection resilience | **Provider** | NATS: async-nats auto-reconnect. |
| Partitioning / sharding | **Provider** | NATS: subject-based routing. Kafka: partition key. |
| Transport encryption | **Provider** | NATS/Kafka: TLS configuration. |
| Event versioning / schema | **A3S Event** | Provider-agnostic, application-level concern. |
| Payload encryption | **A3S Event** | Application-level encrypt/decrypt before publish. |
| Dead letter queue | **A3S Event** | Unified DLQ abstraction across providers. |
| State persistence | **A3S Event** | Subscription filter durability across restarts. |
| Observability | **A3S Event** | Application-level metrics and tracing. |
| Provider config passthrough | **A3S Event** | Expose provider-native knobs (`MaxDeliver`, `BackOff`, etc.) |

## Development

### Prerequisites

- Rust 1.75+
- NATS Server with JetStream (for NATS tests): `nats-server -js`

### Commands

```bash
just build              # Build the project
just test               # Run all tests
just test-integration   # NATS integration tests (requires nats-server -js)
just bench              # Performance benchmarks
just lint               # Run clippy
just fmt                # Format code
just ci                 # Full CI check (fmt + lint + test)
just doc                # Generate and open docs
```

### Test Coverage

176 unit tests + 29 memory integration tests + 9 NATS integration tests + 2 doc tests across 15 modules.

```bash
# Unit tests (no external dependencies)
just test

# NATS integration tests
nats-server -js
just test-integration
```

## License

MIT License — see [LICENSE](LICENSE) for details.
