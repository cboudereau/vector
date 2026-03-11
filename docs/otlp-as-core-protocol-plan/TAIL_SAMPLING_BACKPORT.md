# Tail Sampling, Trace-Aware Load Balancing, and Span-Level Routing

This document specifies the `tail_sample` transform, the trace-aware load-balancing
sink mode, and span-level routing for the OTLP-core migration. It replaces the
DataDog-specific APM sampling pipeline with a general-purpose, OTel-native
implementation aligned with the
[OpenTelemetry Collector deployment patterns](https://opentelemetry.io/docs/collector/deploy/).

---

## 1. Why Vector Has No Tail Sampling Today

Vector's current architecture processes events one at a time through a pipeline. Tail
sampling requires **buffering a complete trace** (all spans sharing a `trace_id`)
before a sampling decision can be made. This is fundamentally at odds with the
streaming model.

Additionally, tail sampling is a **stateful** operation: all spans for a given trace
must arrive at the **same** processor instance. In a horizontally-scaled deployment,
a trace-aware routing layer must sit in front of the sampling tier to guarantee this
invariant — otherwise each instance sees a partial trace and makes incorrect decisions.

The OTel Collector Contrib project solved this with a two-component architecture:
1. A **`loadbalancingexporter`** that consistent-hashes on `traceID` to route spans
2. A **`tailsamplingprocessor`** that buffers and evaluates policies

Vector should adopt the same pattern.

---

## 2. Reference Architecture: OTel 3-Role Deployment

The [OTel Collector deployment docs](https://opentelemetry.io/docs/collector/deploy/gateway/)
and the [Grafana scaling guide](https://grafana.com/docs/opentelemetry/collector/sampling/scale)
describe a 3-role architecture for production tail sampling:

```
┌─────────────────────────────┐
│  Agent (DaemonSet/sidecar)  │  Role: per-host enrichment, batching
│                             │
│  Stateless. Scales freely.  │
│  Forwards everything.       │
└──────────┬──────────────────┘
           │  OTLP/gRPC → standard L4/L7 LB (K8s Service, NGINX, Envoy)
           ▼
┌─────────────────────────────┐
│  Gateway (Deployment)       │  Role: trace-aware routing ONLY
│                             │
│  Runs loadbalancingexporter │
│  Consistent-hash on traceID │
│  Stateless. Scales freely.  │
│  Multiple replicas produce  │
│  identical routing.         │
└──────────┬──────────────────┘
           │  Consistent-hash routed OTLP/gRPC
           ▼
┌─────────────────────────────┐
│  Sampling Collector (SS)    │  Role: tail sampling + span metrics
│                             │
│  STATEFUL. Each instance    │
│  receives a deterministic   │
│  subset of traces.          │
│  Buffers → evaluates →      │
│  emits or drops.            │
└──────────┬──────────────────┘
           │  OTLP
           ▼
        Backend
```

### Why 3 roles instead of 2

The OTel docs explicitly recommend separating the Gateway and Sampling Collector
layers "in order to have better failure isolation." Specific reasons:

| Concern | 3-role (OTel reference) | 2-role (Agent with built-in LB → Sampler) |
|---------|------------------------|-------------------------------------------|
| Failure isolation | Gateway crash ≠ Agent crash ≠ Sampler crash | Agent crash = routing failure |
| Scaling | Each layer scales independently | Agent count tied to host count, not routing load |
| Consistency | Multiple Gateway replicas produce identical hash → same backend | Each agent resolves DNS independently → brief inconsistency windows |
| Debugging | Routing problems isolated to Gateway logs | Routing and enrichment failures mixed |
| Resource usage | Higher (3 deployments) | Lower (2 deployments) |
| Latency | One extra hop (~2-5ms) | One fewer hop |

For production use, the 3-role model is preferred. For small/dev deployments, a single
Vector instance running `tail_sample` directly (no load balancing) is sufficient.

### Mapping to Vector

| OTel role | Vector equivalent | Configuration |
|-----------|------------------|---------------|
| Agent | Vector DaemonSet | `[sources.otel] → [transforms.enrich] → [sinks.otel_to_gateway]` |
| Gateway | Vector Deployment | `[sources.otel] → [sinks.otel_lb]` with `load_balancing.routing_key = "traceID"` |
| Sampling Collector | Vector StatefulSet/Deployment | `[sources.otel] → [transforms.tail_sample] → [transforms.apm_stats] → [sinks.otel_to_backend]` |

Each role is a standard Vector TOML config — no special "mode" flag needed.

---

## 3. Component 1: Trace-Aware Load-Balancing Sink

### 3.1 What it does

A routing mode on the `opentelemetry` gRPC sink that consistent-hashes spans by
`trace_id` (or `service.name`) across a set of backend endpoints. This is the Vector
equivalent of the OTel Collector Contrib `loadbalancingexporter`.

### 3.2 Configuration

```toml
[sinks.trace_router]
type = "opentelemetry"
inputs = ["otel_source"]

[sinks.trace_router.protocol.grpc]

[sinks.trace_router.load_balancing]
routing_key = "traceID"   # or "service"

# Backend discovery — exactly one resolver must be configured
[sinks.trace_router.load_balancing.resolver.static]
hostnames = ["sampler-0:4317", "sampler-1:4317", "sampler-2:4317"]

# OR: DNS resolver (Kubernetes headless service)
# [sinks.trace_router.load_balancing.resolver.dns]
# hostname = "sampler-headless.ns.svc.cluster.local"
# port = 4317
# interval = "5s"

# OR: Kubernetes resolver (EndpointSlice watcher, fastest convergence)
# [sinks.trace_router.load_balancing.resolver.k8s]
# service = "sampler-svc.ns"
# ports = [4317]
```

### 3.3 Routing keys

| `routing_key` | Applies to | Hash input | Use case |
|---------------|-----------|-----------|----------|
| `traceID` (default) | traces | `span.trace_id` | Tail sampling — all spans for a trace → same backend |
| `service` | traces, metrics | `resource.service.name` | Span metrics — all spans for a service → same backend |

### 3.4 Internal design

- **Consistent hash ring**: maps a 128-bit trace ID (or service name hash) to one of N
  backends. Standard ring with virtual nodes for even distribution.
- **One OTLP/gRPC sub-connection per backend**: each with its own queue, retry, and
  timeout settings (matching otel-col-contrib's model).
- **Resolver**: periodically refreshes the backend list. On change, ~R/N routes shift
  (R = total routes, N = backends).
- **Deterministic**: multiple Gateway instances with the same config and resolver
  produce identical routing decisions — no coordination required.

### 3.5 Metrics

| Metric | Description |
|--------|-------------|
| `vector_lb_num_backends` | Current number of backends in the hash ring |
| `vector_lb_num_resolutions` | Total resolver refreshes, tagged `success=true/false` |
| `vector_lb_backend_latency` | Per-backend export latency histogram |
| `vector_lb_backend_outcome` | Per-backend success/failure count |

### 3.6 Estimated effort

~600 lines: consistent hash ring (~100), resolver abstraction + DNS/static/k8s
implementations (~250), sub-connection management (~150), config + tests (~100).

---

## 4. Component 2: The `tail_sample` Transform

### 4.1 Configuration

```toml
[transforms.sample_traces]
type = "tail_sample"
inputs = ["otel_source"]

# Wall-clock time since the FIRST span of a trace before making a decision.
# Matches otel-col-contrib's `decision_wait` semantic.
decision_wait_secs = 30

# Maximum number of traces buffered in memory. Oldest evicted when full.
num_traces = 50000

# Per-trace size limit (bytes, protobuf-estimated). Traces exceeding this
# are dropped immediately to protect memory.
max_trace_size_bytes = 10485760   # 10 MB, optional

# Decision cache — remembers keep/drop decisions for late-arriving spans
decision_cache.sampled_cache_size = 100000
decision_cache.non_sampled_cache_size = 100000

# Policies (evaluated in order, first match wins with `sample_on_first_match`)
[[transforms.sample_traces.policies]]
name = "keep-errors"
type = "status_code"
status_code.status_codes = ["ERROR"]

[[transforms.sample_traces.policies]]
name = "keep-slow"
type = "latency"
latency.threshold_ms = 5000

[[transforms.sample_traces.policies]]
name = "sample-rest"
type = "probabilistic"
probabilistic.sampling_percentage = 10
```

### 4.2 Execution model

1. Spans arrive as individual OTel `Span` events.
2. The transform groups them by `trace_id` into an in-memory circular buffer.
3. After `decision_wait_secs` since the **first** span for a given `trace_id`, the
   trace is considered complete and policies are evaluated.
4. Policy evaluation produces a decision: **sample** (emit all spans), **drop**
   (discard all spans), or **no decision** (continue to next policy).
5. If no policy matches, the trace is **dropped** (same as otel-col-contrib default).
6. Late-arriving spans (after decision) inherit the cached decision if in the LRU
   cache, or are re-buffered as a new trace if evicted.

Memory bound: `num_traces` circular buffer. When full, the oldest incomplete trace
is evaluated immediately with whatever spans are available.

### 4.3 Built-in policy types

Aligned with the OTel Collector Contrib `tailsamplingprocessor`:

| Policy type | Description | Vector-specific note |
|-------------|-------------|---------------------|
| `always_sample` | Sample all traces | |
| `latency` | Sample if trace duration (earliest start → latest end) exceeds threshold | `threshold_ms`, optional `upper_threshold_ms` |
| `status_code` | Sample if any span has matching status | `status_codes: ["ERROR", "OK", "UNSET"]` |
| `numeric_attribute` | Sample by numeric attribute range | `key`, `min_value`, `max_value` |
| `string_attribute` | Sample by string attribute match | `key`, `values`, `enabled_regex_matching` |
| `probabilistic` | Hash-based probabilistic sampling | `sampling_percentage` |
| `rate_limiting` | Token-bucket rate limit | `spans_per_second` |
| `span_count` | Sample by span count in trace | `min_spans`, `max_spans` |
| `and` | All sub-policies must sample | `and_sub_policy: [...]` |
| `not` | Invert a sub-policy | `not_sub_policy: {...}` |
| `drop` | Explicitly drop matching traces | `drop_sub_policy: [...]` |
| `composite` | Rate-allocated composite | `max_total_spans_per_second`, `policy_order`, `rate_allocation` |
| `vrl` | Evaluate a VRL expression against the full trace | Vector-specific, replaces `ottl_condition` |

The `vrl` policy type is Vector's equivalent of otel-col-contrib's `ottl_condition`.
It receives the same context as the current spec's VRL policies (`.spans`, `.trace_id`,
`.span_count`, `.root_span`).

### 4.4 VRL policy context

Inside a `vrl` policy, the following fields are available:

| Field | Type | Description |
|-------|------|-------------|
| `.spans` | `array<object>` | All OTel `Span` objects in the trace |
| `.trace_id` | `string` | The trace ID (hex-encoded) |
| `.span_count` | `integer` | Number of spans buffered |
| `.root_span` | `object \| null` | The span with no `parent_span_id`, if present |

Each span object follows the OTel `Span` schema:
`.name`, `.attributes`, `.status.code`, `.duration_nano`, `.start_time_unix_nano`, etc.

### 4.5 VRL policy examples

```toml
# Keep traces with any error span
[[transforms.sample_traces.policies]]
name = "keep-errors"
type = "vrl"
vrl.condition = 'exists(.spans, |_i, s| s.status.code == 2)'
```

```toml
# Keep traces where the root span exceeds 2 seconds
[[transforms.sample_traces.policies]]
name = "slow-root"
type = "vrl"
vrl.condition = '''
  root = filter(.spans, |_i, s| s.parent_span_id == null)[0] ?? null
  root != null && root.duration_nano > 2000000000
'''
```

### 4.6 Metrics

| Metric | Description |
|--------|-------------|
| `vector_tail_sampling_traces_sampled{policy,sampled}` | Per-policy decision count |
| `vector_tail_sampling_trace_dropped_too_early` | Traces evicted before `decision_wait` |
| `vector_tail_sampling_late_span_age` | Histogram of late span arrival time after decision |
| `vector_tail_sampling_decision_latency` | Time to evaluate policies for a batch |

### 4.7 Estimated effort

~1,200 lines: trace buffer with circular eviction (~300), policy engine with 12+
built-in types (~500), VRL policy adapter (~100), config/wiring/metrics (~150),
tests (~150).

---

## 5. Span-Level Routing

Spans can be routed to different outputs based on span attributes using the standard
`route` transform. No special transform is needed — OTel spans are plain events.

```toml
[transforms.route_spans]
type = "route"
inputs = ["otel_source"]

[transforms.route_spans.route]
errors   = '.status.code == "STATUS_CODE_ERROR"'
slow     = '.duration_nano > 1000000000'  # > 1 second
database = '.attributes."db.system" != null'
```

For trace-level routing (route the entire trace based on any span's attributes), use
`tail_sample` with a policy that routes on the buffered `.spans` array.

---

## 6. Pipeline Telemetry (Span Metrics)

Pipeline telemetry — including the `spanmetricsconnector`-equivalent that converts
traces into RED metrics — is specified separately in `APM_STATS_OTLP_BACKPORT.md`
and deferred to after Step 5 (core event model → OTel types).

The `span_metrics` transform (replacement for the cancelled DD-specific `apm_stats`)
is a **stateful** component that requires all spans for a given `service.name` to
reach the same instance. When deployed at the Sampling Collector tier (behind the
trace-aware LB with `routing_key = "service"`), this invariant is satisfied
automatically.

---

## 7. Full Deployment Examples

### 7.1 Single instance (dev / low volume)

```toml
[sources.otel]
type = "opentelemetry"

[transforms.sample]
type = "tail_sample"
inputs = ["otel.traces"]
decision_wait_secs = 10
num_traces = 10000
[[transforms.sample.policies]]
name = "keep-errors"
type = "status_code"
status_code.status_codes = ["ERROR"]
[[transforms.sample.policies]]
name = "sample-rest"
type = "probabilistic"
probabilistic.sampling_percentage = 10

[sinks.backend]
type = "opentelemetry"
inputs = ["sample"]
protocol.grpc.endpoint = "http://tempo:4317"
```

### 7.2 Production 3-tier (matches OTel guidelines)

**Agent (DaemonSet):**
```toml
[sources.otel]
type = "opentelemetry"

[sinks.to_gateway]
type = "opentelemetry"
inputs = ["otel"]
protocol.grpc.endpoint = "http://gateway.ns.svc.cluster.local:4317"
```

**Gateway (Deployment, behind K8s Service LB):**
```toml
[sources.otel]
type = "opentelemetry"

[sinks.to_samplers]
type = "opentelemetry"
inputs = ["otel"]

[sinks.to_samplers.load_balancing]
routing_key = "traceID"
[sinks.to_samplers.load_balancing.resolver.dns]
hostname = "sampler-headless.ns.svc.cluster.local"
port = 4317
interval = "5s"
```

**Sampling Collector (StatefulSet/Deployment):**
```toml
[sources.otel]
type = "opentelemetry"

[transforms.apm]
type = "apm_stats"
inputs = ["otel.traces"]

[transforms.sample]
type = "tail_sample"
inputs = ["otel.traces"]
decision_wait_secs = 30
num_traces = 50000

[[transforms.sample.policies]]
name = "keep-errors"
type = "status_code"
status_code.status_codes = ["ERROR"]
[[transforms.sample.policies]]
name = "keep-slow"
type = "latency"
latency.threshold_ms = 5000
[[transforms.sample.policies]]
name = "sample-rest"
type = "probabilistic"
probabilistic.sampling_percentage = 10

[sinks.traces_out]
type = "opentelemetry"
inputs = ["sample"]
protocol.grpc.endpoint = "http://tempo:4317"

[sinks.metrics_out]
type = "opentelemetry"
inputs = ["apm.stats"]
protocol.grpc.endpoint = "http://mimir:4317"

[sinks.spans_passthrough]
type = "opentelemetry"
inputs = ["apm.spans"]
protocol.grpc.endpoint = "http://tempo:4317"
```

---

## 8. Implementation Checklist

| Task | Location | Component |
|------|----------|-----------|
| `tail_sample` transform skeleton | `src/transforms/tail_sample/` | Sampler |
| Trace buffer (circular, group by `trace_id`, wall-clock eviction) | `src/transforms/tail_sample/buffer.rs` | Sampler |
| Built-in policy types (12+ types matching otel-col-contrib) | `src/transforms/tail_sample/policy.rs` | Sampler |
| VRL policy adapter | `src/transforms/tail_sample/vrl_policy.rs` | Sampler |
| Decision cache (sampled + non-sampled LRU) | `src/transforms/tail_sample/cache.rs` | Sampler |
| Load-balancing sink mode: consistent hash ring | `src/sinks/opentelemetry/load_balancing.rs` | LB Sink |
| Resolver abstraction + static/dns/k8s implementations | `src/sinks/opentelemetry/resolver.rs` | LB Sink |
| Per-backend sub-connection management | `src/sinks/opentelemetry/grpc.rs` | LB Sink |
| `apm_stats` transform | `src/transforms/apm_stats/` | APM Stats |
| Integration test: 3-tier tail sampling pipeline | `tests/` | All |
| Integration test: apm_stats metric output | `tests/` | APM Stats |
| Deployment guide with K8s manifests | `docs/` | Docs |

---

## 9. Comparison with OTel Collector Contrib

| Feature | otel-col-contrib | Vector (this spec) | Notes |
|---------|-----------------|-------------------|-------|
| Load-balancing exporter | `loadbalancingexporter` | `opentelemetry` sink with `load_balancing` | Same consistent-hash approach |
| Routing keys | `traceID`, `service`, `metric`, `resource`, `streamID`, `attributes` | `traceID`, `service` (initially) | Others can be added later |
| Resolvers | static, DNS, k8s, AWS CloudMap | static, DNS, k8s (initially) | AWS CloudMap can be added later |
| Tail sampling processor | `tailsamplingprocessor` | `tail_sample` transform | Same buffering model |
| Policy types | 14+ built-in | 12+ built-in + VRL | VRL replaces OTTL |
| Decision timing | `decision_wait` + `decision_wait_after_root_received` | `decision_wait_secs` (initially) | Root-aware wait can be added later |
| Decision cache | sampled + non-sampled LRU | sampled + non-sampled LRU | Same model |
| Per-trace size limit | `maximum_trace_size_bytes` | `max_trace_size_bytes` | Same |
| Span metrics | `spanmetricsconnector` | `apm_stats` transform | Same purpose, Vector-native |
| Group-by-trace | Built-in to tail sampler | Built-in to `tail_sample` | Same — no separate component needed |
