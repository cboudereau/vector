# Step 4a — Load-Balancing Sink for OTel gRPC

## Context

Steps 0–7 complete. The OTel gRPC sink sends all events to a single endpoint. To enable
the 3-tier deployment pattern (Agent → Gateway → Sampling Collector), we need consistent-hash
load balancing that routes events by traceID or service name to multiple backends.

This mirrors the OTel Collector Contrib's `loadbalancingexporter`:
- CRC32 consistent hash ring with virtual nodes (100 per endpoint)
- Per-backend OTLP sub-exporter with independent retry/timeout
- Static, DNS, and K8s resolvers for backend discovery

## Design (following OTel Collector Contrib pattern)

### Config

```toml
[sinks.trace_router]
type = "opentelemetry"
[sinks.trace_router.protocol.grpc]
# endpoint NOT set — resolver provides backends

[sinks.trace_router.protocol.grpc.load_balancing]
routing_key = "traceID"   # or "service"

[sinks.trace_router.protocol.grpc.load_balancing.resolver.static]
hostnames = ["sampler-0:4317", "sampler-1:4317"]

# OR dns resolver:
# [sinks.trace_router.protocol.grpc.load_balancing.resolver.dns]
# hostname = "sampler-headless.ns.svc.cluster.local"
# port = 4317
# interval = "5s"

# OR k8s resolver:
# [sinks.trace_router.protocol.grpc.load_balancing.resolver.k8s]
# service = "sampler-svc.ns"
# ports = [4317]
```

### Architecture

```
Events → extract routing key → consistent hash → per-backend batch → per-backend OtlpGrpcService
                                     ↓
                              Hash Ring (CRC32, 100 vnodes/endpoint)
                                     ↓
                    ┌────────────────┼────────────────┐
                    ▼                ▼                ▼
              Backend 0         Backend 1         Backend 2
           (OtlpGrpcService) (OtlpGrpcService) (OtlpGrpcService)
```

### Consistent Hash Ring

Follow OTel Collector Contrib: CRC32 (IEEE) hash, ring 0–36000, 100 virtual nodes per
endpoint, binary search for lookup. ~60 lines. No external crate needed — CRC32 is in Rust
stdlib (`std::hash`) or use `crc32fast` (already in dependency tree via `flate2`).

### Routing Key Extraction

- `traceID`: from `OtelSpan.span.trace_id` (bytes → CRC32 hash)
- `service`: from `OtelMetric/OtelLog/OtelSpan.resource.attributes["service.name"]` (string → CRC32 hash)
- Non-trace events (logs, metrics): hash on service.name or send to all backends

### Resolver Trait

```rust
#[async_trait]
trait Resolver: Send + Sync {
    async fn resolve(&mut self) -> Vec<String>;
}
```

Implementations:
- `StaticResolver` — returns fixed list
- `DnsResolver` — periodic `tokio::net::lookup_host()` with configurable interval
- `K8sResolver` — EndpointSlice watcher via `kube` crate (already in dep tree at 0.93)

### Sub-Service Management

Each backend gets its own `OtlpGrpcService` (Channel + 3 tonic clients). When the resolver
refreshes and the backend list changes:
1. Create new services for added backends
2. Shutdown removed backends (drain queue, then drop)
3. Rebuild hash ring atomically

---

## Phased Implementation

### Phase 1: Hash ring + config types (~150 lines)

**Files to create:**
- `src/sinks/opentelemetry/load_balancing.rs` — `LoadBalancingConfig`, `RoutingKey` enum,
  `ResolverConfig` enum, `ConsistentHashRing` struct

**ConsistentHashRing:**
- `new(endpoints: &[String], vnodes: u32)` — build ring
- `get(key: &[u8]) -> &str` — lookup endpoint for key
- CRC32 IEEE hash, 36000-point ring, binary search

**Tests:** ring distribution, determinism, endpoint add/remove stability

### Phase 2: Resolver implementations (~200 lines)

**Add to `load_balancing.rs`:**
- `StaticResolver` — trivial, returns config hostnames
- `DnsResolver` — spawns background task, resolves on interval, updates via `watch::channel`
- `K8sResolver` — EndpointSlice watcher via `kube::runtime::watcher`, watches
  `discovery.k8s.io/v1/EndpointSlice` with label `kubernetes.io/service-name={svc}`.
  Gated behind `kubernetes` feature flag. Uses `kube` 0.93 already in Cargo.toml.

**Tests:** static resolution, DNS mock, K8s mock (using `kube::Client::try_default()` test pattern)

### Phase 3: Load-balanced sink (~300 lines)

**Files to modify:**
- `src/sinks/opentelemetry/grpc.rs` — add `load_balancing` field to `GrpcConfig`
- `src/sinks/opentelemetry/mod.rs` — wire up module

**New type: `LoadBalancedOtlpGrpcSink`:**
- Holds: hash ring (behind `RwLock`), map of endpoint → `OtlpGrpcService`, resolver handle
- Event flow:
  1. Extract routing key from event
  2. Hash → lookup endpoint
  3. Route to per-backend batch buffer
  4. Each backend batch → `OtlpGrpcService::call()`
- Uses `tokio::select!` to drive resolver refresh + event processing concurrently

**Integration with `GrpcConfig::build()`:**
- If `load_balancing` is `Some`: build `LoadBalancedOtlpGrpcSink`
- If `None`: current single-endpoint behavior (unchanged)

**Tests:**
- 2-backend deterministic routing (same traceID → same backend)
- Backend add/remove → minimal reshuffling
- Config serialization/deserialization

### Phase 4: Metrics + docs (~50 lines)

- `vector_lb_num_backends` gauge
- `vector_lb_num_resolutions` counter (success/fail)
- Update `CONSOLIDATED_MIGRATION_PLAN.md`

---

## Verification

- `cargo test -p vector --lib` — all tests pass
- `cargo check -p vector-core` — clean
- Determinism test: same events + same config → same routing decisions
- Integration: 2-backend test server, verify trace routing by traceID
