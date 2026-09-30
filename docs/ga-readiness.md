# a3s-event 0.4.0 — GA readiness audit

Date: 2026-09-30 · Branch: `feat/iggy-provider-ga` (local, not pushed)

This document is the auditable evidence trail for calling this release
production-ready. It separates what is **verified** from what is **gated on
external action**, and names every known limitation. It is intentionally not
a marketing document.

## 1. Verification evidence

### 1.1 Test matrices (all green, re-verified on a live broker)

| Matrix | Result |
|---|---|
| Default features (nats/encryption/cloudevents/routing) | **280 passed / 0 failed** |
| `nats,iggy` (no default) | **246 passed / 0 failed** — against a live `apache/iggy:0.9.0` container |
| Chaos (opt-in env vars) | iggy restart-resume **passed live**; nats restart-recovery **passed live**; both skip cleanly when unset |

Caveat recorded in the chaos suite: **always check the broker is alive after
running chaos** — suites skip-pass against a dead server (skip-if-unavailable
is the harness contract). The upstream restart bug (§3.1) makes this a real
operational footgun, not a theoretical one.

### 1.2 Depth of coverage

- **Cross-provider conformance** (`tests/conformance.rs`): tier-0 (every
  provider: envelope fidelity across 3 categories × 4 versions, fan-out
  isolation with a 3-subscriber overlap matrix, per-category total order,
  8×10 concurrent publish no-loss/no-dup, tail filters, options plumbing,
  counts/info/health) and tier-1 (persistent providers: unacked redelivery,
  resume across a NEW connection, group-rebuild replay, late-subscriber
  ordered replay, competing consumers exactly-once across connections).
- **Iggy contract matrix**: 32 rows, all implemented — including
  poison-message tolerance (foreign non-JSON frame skipped without wedging)
  and the two fail-closed surfaces (`expected_sequence`,
  `LastPerSubject`).
- **Feature e2e**: EventBus full pipeline (schema gate → encryption at rest
  → broker routing → DLQ capture → state persistence across "restart" →
  metrics audit), routing/bridge (filter matrix + cross-bus TopicSink),
  CronSource lifecycle, crypto key-rotation/tamper, CloudEvents fidelity,
  messaging isolation, DLQ capacity/predicate/SinkDlqHandler notification
  contract, schema compatibility matrix (stepwise), error paths
  (unwritable state store, always-failing provider).
- **Bugs the depth bought** (all fixed, regression-covered): DLQ contract
  unwired, wildcard matching dead code, NATS durable-name rejection,
  NATS history policy, ByStartTime fallback-to-zero, messaging target
  prefix, plus test-semantics fixes (clock-skew midpoint, per-connection
  group identity, ack-wait redelivery windows).

### 1.3 Static quality gates

| Gate | Result |
|---|---|
| `cargo clippy --all-targets -- -D warnings` | clean × 4 feature sets (default, nats, iggy, nats+iggy) |
| `cargo fmt --check` | clean |
| Unit coverage (`cargo llvm-cov --lib`) | **82.9% lines** (broker provider bodies exercised by live e2e, not counted) |
| Cross-compile, minimal core | linux x64/arm64 + windows msvc clean (TLS deps need native or C cross-toolchain — CI runs native per-OS) |

### 1.4 Performance baseline (criterion, crate release profile, Apple Silicon)

Memory provider: publish ~203 µs/100-event batch (~2.0 µs/event), ~1.70 ms
per 1000 (~1.7 µs/event); `history(100)` 12.4 µs / 20.8 µs filtered. Broker
round-trips dominate all networked paths.

### 1.5 Packaging

`cargo publish --dry-run` verifies the packaged crate builds standalone and
ships README/LICENSE/CHANGELOG/docs. The stray root-monorepo `.gitmodules`
is excluded via `.gitignore` (never shipped).

## 2. Known limitations (documented, not hidden)

- iggy provider: no broker-side dedup (`msg_id` is header-only); redelivery
  controls (`max_deliver`/`backoff`/`max_ack_pending`/`ack_wait`) accepted
  and ignored; single-partition topics only (`Balanced` is a documented
  no-op); group membership is per client connection.
- History ordering across providers is not part of the contract (memory is
  newest-first, brokers oldest-first).
- TLS paths compile but have no e2e coverage.

## 3. Gated on external action (the honest remainder)

### 3.1 Upstream defect gating iggy-GA

Iggy server 0.9.0 intermittently panics on restart boot replay
(`client_id 0 is reserved for internal use`,
`core/consensus/src/client_table.rs`), leaving the server unbootable with
the same data directory. Observed twice locally. Issue draft:
`docs/upstream-iggy-restart-panic.md` (not filed — needs authorization).
**Until fixed upstream, the `iggy` feature must not be called
GA-hardened**, regardless of this crate's own quality.

### 3.2 Owner decisions

- Push `feat/iggy-provider-ga`, wire CI to the remote, first remote run.
- `cargo publish` for real (0.4.0; migration warnings in CHANGELOG §0.4.0).
- How this crate rejoins the a3s monorepo (in-tree vs submodule re-pin) —
  the root `.gitmodules` deletion is mid-conversion and is an owners' call.
- Filing the upstream iggy issue.

### 3.3 Time-gated

- Production soak: weeks of real load. No substitute exists.

## 4. Verdict

For the **memory and nats** feature sets: engineering GA criteria are met
(tests, gates, coverage, packaging, docs, migration notes) pending §3.2's
publish/CI wiring. For the **iggy** feature set: same crate-level criteria
are met, but the feature is explicitly **not GA** until §3.1 is resolved
upstream — this is stated in the CHANGELOG and is not negotiable by test
count.
