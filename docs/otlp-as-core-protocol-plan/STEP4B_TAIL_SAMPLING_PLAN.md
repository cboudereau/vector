# Step 4b — Tail Sampling Transform

## Context

Step 4a (load-balancing sink) is complete. The tail_sample transform buffers OTel spans
by trace_id, evaluates sampling policies after a decision wait period, and emits or drops
complete traces. It mirrors the OTel Collector Contrib `tailsamplingprocessor`.

Together with the load-balancing sink, this enables the 3-tier deployment:
Agent → Gateway (LB by traceID) → Sampling Collector (tail_sample).

## Design (following OTel Collector Contrib tailsamplingprocessor)

### Config

```toml
[transforms.sample_traces]
type = "tail_sample"
inputs = ["otel_source"]
decision_wait_secs = 30
num_traces = 50000
max_trace_size_bytes = 10485760

[transforms.sample_traces.decision_cache]
sampled_cache_size = 100000
non_sampled_cache_size = 100000

[[transforms.sample_traces.policies]]
name = "errors"
type = "status_code"
status_codes = ["ERROR"]

[[transforms.sample_traces.policies]]
name = "slow"
type = "latency"
threshold_ms = 5000

[[transforms.sample_traces.policies]]
name = "baseline"
type = "probabilistic"
sampling_percentage = 10.0
```

### Core data structures

- `TailSampling` — main transform state (trace buffer, decision caches, policies)
- `BufferedTrace` — per-trace: spans vec, first_seen Instant, total_bytes
- `TraceId` — `[u8; 16]` type alias
- `SamplingPolicy` trait — `evaluate(&self, trace) -> Decision`
- `Decision` enum — `Sample`, `Drop`, `Pending`

### Event flow

1. Span arrives → extract trace_id from OtelSpan
2. Check decision cache → if sampled: emit; if dropped: discard
3. Buffer span in traces[trace_id]
4. Check max_trace_size_bytes → if exceeded, drop entire trace
5. On tick (1s): for each trace past decision_wait:
   - Evaluate policies (first match wins)
   - Sample → emit all spans, cache as sampled
   - Drop/none → drop, cache as non-sampled
6. Eviction: if traces.len() > num_traces, evict oldest

---

## Phases

### Phase 1: Core types + config + empty transform shell (~200 lines)

- `src/transforms/tail_sample/mod.rs`
- `src/transforms/tail_sample/config.rs`
- `src/transforms/tail_sample/transform.rs`
- Register in `src/transforms/mod.rs` + `Cargo.toml`

### Phase 2: Buffer + decision loop (~300 lines)

- BufferedTrace, TraceId, buffer management
- Decision loop with 1s tick
- Decision cache (sampled + non-sampled LRU)
- Emit/drop logic

### Phase 3: Policy implementations (~500 lines)

- `src/transforms/tail_sample/policies.rs`
- always_sample, status_code, latency, probabilistic, rate_limiting,
  span_count, string_attribute, numeric_attribute, and/not/drop, composite

### Phase 4: Metrics + tests + docs (~200 lines)

- vector_tail_sampling_traces_sampled, trace_dropped_too_early
- Unit tests per policy, buffer eviction, decision cache, late spans
- Update CONSOLIDATED_MIGRATION_PLAN.md
