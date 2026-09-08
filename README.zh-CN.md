# A3S Event

<p align="center">
  <strong>语言 / Language:</strong>
  <a href="README.md">English</a> ·
  <a href="README.zh-CN.md">中文</a>
</p>

<p align="center">
  <strong>A3S 的可插拔事件系统</strong>
</p>

<p align="center">
  <em>与 provider 无关的事件发布、订阅和持久化 — 切换后端而不更改应用程序代码</em>
</p>

<p align="center">
  <a href="https://crates.io/crates/a3s-event"><img src="https://img.shields.io/crates/v/a3s-event.svg" alt="crates.io"></a>
  <a href="https://docs.rs/a3s-event"><img src="https://docs.rs/a3s-event/badge.svg" alt="docs.rs"></a>
  <a href="#license"><img src="https://img.shields.io/crates/l/a3s-event.svg" alt="MIT"></a>
</p>

<p align="center">
  <a href="#quick-start">快速开始</a> •
  <a href="#feature-flags">Feature Flags</a> •
  <a href="#providers">提供商</a> •
  <a href="#architecture">架构</a> •
  <a href="#api-reference">API 参考</a> •
  <a href="#custom-providers">自定义提供商</a> •
  <a href="#development">开发</a>
</p>

---

## 概述

**A3S Event** 提供与 provider 无关的 API，用于事件订阅、分派和持久化。所有后端均实现 `EventProvider` 特性 — 在 NATS JetStream、内存中或任何自定义provider之间交换，而无需更改应用程序代码。

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

所有可选模块都位于功能门后面。最小核心（类型、内存provider、EventBus、架构、DLQ、指标）以零可选依赖项进行编译。

|特色|默认|描述 |
|---------|---------|-------------|
| `nats` | ✅ | NATS JetStream 提供商（`async-nats`、`futures-util`、`time`）|
| `encryption` | ✅ | AES-256-GCM 有效负载加密（`aes-gcm`、`base64`）|
| `cloudevents` | ✅ | CloudEvents v1.0 转换 (`chrono`) |
| `routing` | ✅ | Broker/Trigger 事件路由 + Sink DLQ |
| `full` | — |所有功能 |

```toml
# Full (default)
a3s-event = "0.3"

# Minimal core — no NATS, no encryption, no CloudEvents, no routing
a3s-event = { version = "0.3", default-features = false }

# Pick what you need
a3s-event = { version = "0.3", default-features = false, features = ["nats", "encryption"] }
```

## 提供商

|供应商|使用案例|坚持|分销|
|----------|----------|--------------|--------------|
| `MemoryProvider` |测试、开发、单流程|仅在进程中 |单进程|
| `NatsProvider` |生产、多项服务 | JetStream（文件/内存）|分布式|

### 内存提供者

使用 `tokio::sync::broadcast` 的零依赖、进程内事件总线。

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

### NATS JetStream 提供商

需要`nats`功能。具有持久存储、持久消费者和至少一次传递的分布式事件流。

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

## 架构

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

### 主题层次结构

事件遵循点分隔的命名约定：

```
events.<category>.<topic>[.<subtopic>...]

Examples:
  events.market.forex.usd_cny     — forex rate change
  events.system.deploy.gateway    — service deployment
  events.task.completed           — task completion
```

通配符模式：
- `events.market.>` — 所有市场事件（任何深度）
- `events.*.forex` — 任何类别的外汇事件

### 核心类型

|类型 |描述 |
|------|-------------|
| `EventProvider` |核心特征——所有后端都实现这个 |
| `EventBus` |具有订阅管理功能的高级 API |
| `Event` |消息信封（id、主题、类别、事件类型、版本、有效负载）|
| `ReceivedEvent` |具有传递上下文的事件（序列、num_delivered、流）|
| `Subscription` |来自任何提供商的异步事件流 |
| `PendingEvent` |用于手动确认的带有 ack/nak 回调的事件 |
| `ProviderInfo` |后端状态（消息数、字节数、消费者）|
| `SchemaRegistry` |具有兼容性检查的事件类型验证 |
| `StateStore` |持久订阅状态的特征 |
| `EventMetrics` |用于发布、订阅、错误、延迟的无锁原子计数器 |
| `DlqHandler` |死信队列特征 + `MemoryDlqHandler` |

可选类型（功能门后面）：

|类型 |特色 |描述 |
|------|---------|-------------|
| `Aes256GcmEncryptor` | `encryption` |具有密钥轮换功能的 AES-256-GCM 加密器 |
| `CloudEvent` | `cloudevents` |具有无损转换功能的 CloudEvents v1.0 信封 |
| `Broker` / `Trigger` | `routing` |带有过滤器的受 Knative 启发的事件路由 |
| `EventSink` | `routing` |交付目标：`TopicSink`、`InProcessSink`、`LogSink` |
| `SinkDlqHandler` | `routing` |通过接收器转发死信的 DLQ 处理程序
| `NatsProvider` | `nats` | NATS JetStream 分布式提供商 |

## API 参考

### 事件总线

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

### 事件提供者特征

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

### 订阅

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

## 定制提供商

实现 `EventProvider` 和 `Subscription` 添加任何后端：

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

然后像任何其他提供者一样使用它：

```rust
let bus = EventBus::new(RedisProvider::new(config));
bus.publish("market", "forex", "Rate change", "source", payload).await?;
```

## 责任边界

A3S Event 不会重新实现提供商已原生提供的功能。

|能力|业主|笔记|
|------------|---------|--------|
|重试/退避| **提供商** | NATS：`MaxDeliver` + `BackOff`。 Kafka：消费者重试主题。 |
|背压| **提供商** | NATS：拉消费者+`MaxAckPending`。卡夫卡：消费者民意调查。 |
|连接弹性 | **提供商** | NATS：async-nats 自动重新连接。 |
|分区/分片| **提供商** | NATS：基于主题的路由。卡夫卡：分区键。 |
|传输加密| **提供商** | NATS/Kafka：TLS 配置。 |
|事件版本控制/架构 | **A3S Event** |与 provider 无关的应用程序级别的关注点。 |
|有效负载加密 | **A3S Event** |发布前应用程序级加密/解密。 |
|死信队列 | **A3S Event** |跨提供商的统一 DLQ 抽象。 |
|状态持久化| **A3S Event** |订阅过滤器在重新启动后的持久性。 |
|可观察性| **A3S Event** |应用程序级指标和跟踪。 |
|提供者配置直通 | **A3S Event** |公开提供者本机旋钮（`MaxDeliver`、`BackOff` 等）|

## 开发

### 先决条件

- 铁锈 1.75+
- 带有 JetStream 的 NATS 服务器（用于 NATS 测试）：`nats-server -js`

### 命令

```bash
just build              # Build the project
just test               # Run all tests
just test-integration   # NATS integration tests (fails if JetStream is unavailable)
just bench              # Performance benchmarks
just lint               # Run clippy
just fmt                # Format code
just ci                 # Full CI check (fmt + lint + test)
just doc                # Generate and open docs
```

### 测试覆盖率

196 个单元测试 + 29 个内存集成测试 + 11 个 NATS 集成测试 + 2 个跨 15 个模块的文档测试。

```bash
# Unit tests (no external dependencies)
just test

# NATS integration tests
nats-server -js
just test-integration
```

`just test-integration` 设置 `A3S_EVENT_REQUIRE_NATS=1`，因此缺失或
配置错误的 JetStream 服务器会失败，而不是默默地跳过套件。
每个拉取请求都会针对固定的官方运行相同的失败闭合（fail-closed）套件
NATS 2.11.8 图像。

## 社区

加入我们的 [Discord](https://discord.gg/XVg6Hu6H)，了解问题、讨论和更新。

## 许可证

MIT 许可证 — 请参阅 [许可证](LICENSE) 了解详细信息。