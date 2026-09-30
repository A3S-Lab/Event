# Changelog

All notable changes to this project are documented in this file.
The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] — 2026-09-30

### ⚠️ Operational migrations

- **The default provider is now Iggy.** The default feature set is
  `iggy, encryption, cloudevents, routing`; NATS becomes opt-in via the
  `nats` feature. Consumers that relied on the default providing NATS must
  add `features = ["nats"]`. Note the known upstream Iggy restart issue
  (apache/iggy#4361) before choosing Iggy for production restart-heavy
  deployments.

- **NATS durable consumer names are now sanitized.** `EventBus` used to build
  consumer names as `{subscriber}-{subject with '.'→'-'}`; subjects also
  contain `*` and `>`, which JetStream rejects outright (`error 10103`). The
  name is now built by collapsing every character outside `[a-zA-Z0-9_-]` to
  `-`. **Deployed consumers subscribed under the old naming will see new,
  empty consumers on upgrade** — either drain/retire old subscriptions before
  upgrading, or accept a one-time redelivery from the deliver policy's start.
- **NATS `history()` now scans forward from the start of the retained
  stream** (`DeliverPolicy::All`) instead of `Last`, which returned at most
  one message. Read-side behavior change: `list_events`/`counts` now actually
  return history.

### Added

- **Apache Iggy provider** (`iggy` feature, SDK `iggy` 0.11 / server 0.9):
  stream→topic mapping where each subject category is one topic (full subject
  preserved in payload + `a3s-subject` user header; subscription filters
  narrowed client-side), durable subscriptions as consumer groups with
  explicitly stored offsets (at-least-once, last-consumed convention),
  ephemeral subscriptions, subscribe-time head probing for `New`/`Last`
  positioning, bounded connect timeout owned by the provider, PAT or
  username/password login. Fail-closed where the broker cannot honor the
  contract: `expected_sequence`, `DeliverPolicy::LastPerSubject`. Accepted
  but ignored (per the trait contract): `max_deliver`, `backoff_secs`,
  `max_ack_pending`, `ack_wait_secs`. Single-partition topics only
  (`IggyPartitioning::Balanced` is a documented no-op in this version).
- `EventBus::from_provider(Arc<dyn EventProvider>)` — share one provider
  handle between the bus and its owner.
- `EventBus::set_schema_registry` — setter symmetry with the other optional
  capabilities (`with_schema_registry` was previously the only path).
- **Broker routing failures now reach the DLQ.** `EventBus`'s documented
  "routes failed events to a DlqHandler" contract is actually implemented:
  failed sink deliveries produce a `DeadLetterEvent` (reason
  `broker routing: n of m sink deliveries failed`) and advance the
  `dlq_count` metric.
- Deep end-to-end test assets: a cross-provider conformance suite (tier 0 on
  every provider × 7 scenarios; tier 1 on persistent providers × 5), feature
  e2e suites (pipeline, routing/bridge, cron source, crypto, CloudEvents,
  messaging, DLQ/schema/sinks completion, chaos), and opt-in chaos tests
  (broker restart mid-stream, PAT login) driven by environment variables.

### Known issues (upstream)

- **Iggy server 0.9.0 has an intermittent restart-path panic**: boot replay can
  hit `client_id 0 is reserved for internal use` (`core/consensus/src/client_table.rs`)
  when the persisted client table contains certain sessions, killing the shard
  and the server. Discovered by this crate's opt-in chaos suite (broker restart
  mid-stream). Repro: connect clients, publish, `docker restart` the container;
  sometimes the server exits (1) during boot. A fresh container (recreate, not
  restart) boots clean. Until fixed upstream, Iggy restarts in production need
  a supervisor plus a readiness gate — and this is a reason the `iggy` feature
  should not be considered GA-hardened even when this crate is.

### Fixed

- `InMemoryMessaging::send` prefixed targeted patterns with `session.`,
  producing `session.session.<id>` — no documented filter could ever match a
  targeted send (and `test_subscribe_and_send_to_specific_session` hung
  every full `cargo test` run). Patterns are now the target id itself.
- `matches_pattern` checked wildcards on the pattern side only, so
  subscriber filters like `session.*` never matched targeted messages;
  wildcards are now honored symmetrically.
- Iggy `DeliverPolicy::ByStartTime` positioning was consumed on the first
  poll even when it returned nothing, falling back to `offset(0)` and
  delivering pre-cutoff events; a timestamp position now sticks until a poll
  actually returns messages.
- Several `clippy -D warnings` violations across the crate.

## [0.3.0] — prior release

See git history.
