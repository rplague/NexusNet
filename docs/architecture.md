# Architecture and internals

This document describes how a NexusNet node is put together at runtime. For the
protocol contracts see [sidecar.cddl](./sidecar.cddl), [service-registry.md](./service-registry.md),
[nat-relay.md](./nat-relay.md) and [auth.md](./auth.md).

## Process model

A node is a microkernel surrounded by sidecars:

| Component | Responsibility |
|---|---|
| `main` / `runtime` | Startup orchestration and signal handling. `runtime::start_node` is shared by the desktop binary and mobile hosts. |
| `SwarmActor` | The only task that owns the libp2p `Swarm`. All network commands are serialized through it. |
| `NodeController` | Single event loop that handles network events, backend commands, service ticks, relay ticks and shutdown. |
| `ServiceDispatcher` | Owns backend TCP connections, readiness, reconnects and request forwarding. |
| `ServiceRegistryActor` | Pure state machine plus bounded DHT jobs for announcements, withdrawals and the type index. |
| `AuthGate` | Verifies signed authorization records and applies fail-closed decisions. |
| `DhtCache` | Read-through cache with per-key single-flight refresh. |

The desktop binary only adds signal handling and logging mode around
`runtime::start_node`; mobile bridges call the same runtime with explicit paths
and a log sink.

## Startup

```text
start_node(options)
  ├─ install log sink (optional)
  ├─ read config.toml, filling defaults and writing them back
  ├─ load or create keypair.bin (Ed25519 identity)
  ├─ load keypair.pq.bin when PQ is enabled (optional)
  ├─ wait for configured address families (desktop)
  ├─ probe public addresses and update config (desktop)
  ├─ Network::start → build Swarm, spawn SwarmActor
  ├─ dial bootstrap peers
  ├─ spawn ServiceDispatcher
  └─ spawn NodeController
       tokio::select! {
           event_rx          → network events
           to_controller_rx  → backend commands and readiness
           service_tick      → due withdrawals / retries (1 s)
           relay_tick        → relay top-up and bootstrap announce
           shutdown          → graceful exit
       }
```

## Event loop and backpressure

Every queue is bounded. Overload produces an explicit error instead of unbounded
memory growth.

| Queue | Bound | Behavior when full |
|---|---|---|
| Authentication workers | 4 workers × 32 | Inbound request returns `authentication overloaded` |
| Dispatcher inbound | 256 | Inbound request returns `backend overloaded` |
| Controller control | 128 | Control command fails or waits, depending on op |
| Per-backend command / event | 64 each | Backend task applies backpressure or errors |
| Service registry commands | 256 | State commands wait; `Tick`/`RefreshTypes`/`Reannounce` drop or report busy |

DHT work is isolated from the controller:

- Dispatcher DHT commands run in a bounded worker pool (`services.dispatcher.max_inflight_dht`, default 4, range 1–16).
- Registry DHT jobs run in a `JoinSet` capped by `services.registry.max_inflight_dht`.
- `whoami`, `relay_status`, `pq_status`, `auth_status` and `list_services` never wait on DHT.

## Service registration lifecycle

A local service becomes visible only when the node can actually serve it:

1. The backend completes the `hello` handshake. `ServiceDispatcher` publishes the single `backend_ready` set over a watch channel.
2. DHT bootstrap completes.
3. The node is reachable. A node that needs a relay does not announce until at least one circuit is active.

Announcements are reconciled by `ServiceRegistryActor` in this priority order:
due withdrawals, due retries, new announcements, then type-index reconciliation.
Backend disconnects enter a short grace window (about 3–4 seconds, checked on a
1-second tick); a reconnect cancels the withdrawal. A successful announcement is
recorded in `announced`; failures retry with exponential backoff (5 s → 60 s).

`BootstrapCompleted` and `reannounce_services` clear the `announced` cache so the
node re-announces after a DHT rebuild instead of staying in a stale state.

## Relay gating

`needs_relay` is true unless the node is dual-stack **and** has a public address.
When a relay is required:

- Before the first active relay: no announcements are scheduled.
- First relay becomes active: `announced` is cleared and services are announced again.
- Relay count changes but stays above zero: no re-announcement.
- Last relay disappears: every announced service is withdrawn and further announcements are blocked until a relay returns.

Provider records are maintained by libp2p's periodic provider job. Active
providers republish every `provider_publication_interval_seconds` (default
1800 s); records disappear at most about one `provider_record_ttl_seconds`
(default 3600 s) after a provider stops.

## Request path

```text
backend → control(service_request)
  → ServiceDispatcher backend_read_loop
  → ControlRequest → NodeController
  → discover_providers (DHT get_providers)
  → rank candidates by RTT
  → send_request_to_peer
  → on failure: get_closest_peers(peer) to resolve relay addresses, retry
  → CBOR request-response
  → remote NodeController → InboundServiceRequest
  → remote ServiceDispatcher → backend task
  → request frame → remote backend → reply travels back
```

`service_request` prefers a ready local backend and skips DHT, PQ and the auth
gate. `service_request_to(self, service)` also stays local. If the local backend
is not ready, `service_request` falls back to a remote provider, while
`service_request_to(self)` returns `backend_not_ready`.

## DHT read cache

`DhtCache` is read-through with one in-flight refresh per key:

- A previous result is returned immediately and a background refresh starts if none is running.
- The refresh overwrites the cached value; a successful "not found" overwrites with an empty result.
- A cold key waits for the first query and returns its result or a real error.
- Refresh failures keep the previous value and are logged; the next read retries.
- There is no freshness window, global permit, cache timeout or failure backoff.

Authorization uses the same cache but still checks `expires_at` and `version` on
every decision, so expired or rolled-back documents are rejected even when served
from cache. `/oahd/service/types` bypasses the cache and is handled by the
registry lease logic.

## Authorization path

For each inbound request the auth gate checks, in order:

1. `auth.network == "none"` → allow.
2. Service does not require auth → allow.
3. Index record missing or expired → deny.
4. Service absent from the index → allow this request with a warning.
5. Whitelist missing → deny; peer not in members → deny.

Documents are COSE_Sign1 with EdDSA, bound to their DHT key through
`external_aad`. Versions must increase and hashes must match the index entry.
See [auth.md](./auth.md) for the byte-level contract.

## Post-quantum layer

When enabled, service calls use `/oahd/service_req/2.0.0`: the caller encapsulates
to the responder's hybrid KEM public key, and both sides derive one shared key
used for the request and the response. The plaintext protocol is used only when
the peer is confirmed not to support PQ; unknown capability returns a retryable
error rather than silently downgrading. `crypto.pq_required` rejects non-PQ peers
outright.

`pq_identity_enabled` additionally requires ML-DSA-65 signatures and binds the
signing identity to the transport PeerId.

## Size limits

- `services.dispatcher.max_request_bytes` / `max_response_bytes`: default 16 MiB, range 1–64 MiB per message.
- `compat.max_record_bytes`: default 14 KiB, keeps DHT records inside the legacy 16 KiB Kademlia packet.
- `sidecar_protocol::MAX_FRAME`: 16 MiB per backend frame.
- PQ requests add roughly 8 KiB of KEM, nonce and optional identity data.

Relay servers size `max_circuit_bytes` as `max_request_bytes + max_response_bytes`,
so bootstrap limits must be at least as large as their clients'.
