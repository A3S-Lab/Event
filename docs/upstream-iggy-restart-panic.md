# Upstream issue draft — apache/iggy

> Status: DRAFT, not filed. Filing needs the repo maintainer's authorization
> (outward-facing action). Repro material below is ready to paste.

**Title:** Server 0.9.0 intermittently panics on restart boot replay: `client_id 0 is reserved for internal use`

**Component:** server / consensus (client table boot replay)

**Version:** `apache/iggy:0.9.0` (Docker), SDK `iggy` 0.11.0

## Summary

Restarting a single-node server (`docker restart`, i.e. process restart with
the data directory preserved) intermittently kills the shard during boot
replay:

```
thread 'shard-0' panicked at core/consensus/src/client_table.rs:1129:9:
client_id 0 is reserved for internal use
ERROR shard-0 server::boot::threads: message pump died instead of draining
(task panicked: client_id 0 is reserved for internal use); committed journal
tail may not have flushed
Error: ShardJoinFailures { failures: [ShardJoinFailure { shard_id: 0, kind:
Error(ShardPumpDied { shard_id: 0, reason: "task panicked: client_id 0 is
reserved for internal use" }) }] }
```

The process exits (1) and cannot boot again with the same data directory.
Recreating the container (fresh data) boots clean. The panic is
state-dependent: the same workload sometimes restarts cleanly.

## Repro

Docker run (macOS host, Docker Desktop; also seen on linux CI runners):

```bash
docker run -d --name iggy --security-opt seccomp=unconfined -p 5102:5102 \
  -e IGGY_ROOT_USERNAME=iggy -e IGGY_ROOT_PASSWORD=iggy \
  -e IGGY_TCP_ADDRESS=0.0.0.0:5102 -e IGGY_NODE_ADVERTISED_ADDRESS=127.0.0.1 \
  -e IGGY_SHARDING_CPU_ALLOCATION=1 -e IGGY_SHARDING_PIN_CORES=false \
  apache/iggy:0.9.0
```

Then, repeatedly (via the Rust SDK):

1. `login_user("iggy", "iggy")`
2. create a stream + topic, send a few messages
3. create/join a consumer group, poll a batch, `store_consumer_offset`
4. drop the client connection
5. `docker restart iggy`

Observed: roughly every few cycles, boot replay panics as above and the
server stays down.

## Suspicion

The persisted client table replays a session whose (reconstructed or
replayed) client id collides with the reserved internal id 0 — likely a
client that was mid-registration when the process stopped, or a session
whose id was never durably assigned before the kill. Boot treats the
collision as a panic instead of rejecting/ignoring the stale entry, turning
a recoverable restart into a hard outage requiring manual data-dir reset.

## Impact

Any production single-node deployment that restarts (deploy, node bounce,
OOM kill) can become permanently unbootable with the same data directory.
