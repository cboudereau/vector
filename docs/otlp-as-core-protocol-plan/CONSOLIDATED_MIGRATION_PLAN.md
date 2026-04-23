# OTLP as Core Protocol — Migration Complete

Vector's internal event model and wire protocol have been fully replaced with
OpenTelemetry (OTLP/OTel) as the sole core protocol and in-memory representation.

---

## Result

The `Event` enum has exactly 3 variants — all OTLP-native:

```rust
pub enum Event {
    Log(OtelLog),       // OpenTelemetry LogRecord
    Metric(OtelMetric), // OpenTelemetry Metric (Sum, Gauge, Histogram, ExponentialHistogram, Summary)
    Trace(OtelSpan),    // OpenTelemetry Span
}
```

- `LogEvent`, `Metric`, `TraceEvent` — deleted.
- `NativeSerializer`, `NativeDeserializer`, `NativeJsonSerializer`, `NativeJsonDeserializer` — deleted.
- `AgentDDSketch` — moved to DD source adapter, not in core.
- `DatadogMetricOriginMetadata` — no longer in core event metadata.
- `use_otlp_decoding` flag — eliminated (source always emits OTel-native).
- DD sinks (`src/sinks/datadog/`) and Vector sink (`src/sinks/vector/`) — removed.
- DD source — clean OTel adapter: emits `OtelMetric`/`OtelSpan` directly.
- VRL migration tool (`vector vrl-migrate`) — ships with ~91% auto-rewrite coverage.
- OTLP HTTP JSON ingestion — native support in the `opentelemetry` source.
- Zero-conversion OTLP path: OTel source → OTel sink (gRPC + HTTP) for all 3 signals.

---

## Architecture

```
Sources (input adapters)       Core (OTel-native only)              Sinks (output adapters)
─────────────────────────────  ────────────────────────────────────  ───────────────────────────
opentelemetry (gRPC + HTTP)    OTel LogRecord                        opentelemetry (gRPC + HTTP)
datadog_agent  ─────────────►  OTel Metric                     ────► prometheus
  DD proto → OTel at boundary    (Sum/Gauge/Histogram/           ────► influxdb, loki, kafka, …
vector (gRPC)  ─────────────►    ExponentialHistogram/Summary)
  native → OTel at boundary    OTel Span
kafka, syslog, …  ──────────►  Resource + InstrumentationScope
                               Disk buffer: OtlpBufferBatch proto
```

---

## Guiding Principles (preserved for future contributors)

1. **Baby steps, always green.** Every PR leaves all existing tests passing.
2. **OTLP/OTel is the only core protocol.** No vendor types in core.
3. **Vendor logic lives exclusively in adapters.** Adapters depend on core; core never depends on adapters.
4. **The compiler enforces the boundary.** `cargo build -p vector-core` clean = boundary correct.
5. **No approximations in core.** `ExponentialHistogram` is the correct OTel type. Sketch conversion happens in the DD source adapter.
6. **gRPC internally, HTTP also supported.** OTLP/gRPC for inter-Vector. HTTP at sources and sinks for external integrations.
7. **Features are preserved, not dropped.** APM stats → pipeline telemetry (`span_metrics`), tail sampling, load balancing — all re-implemented with OTel types.

---

## Key Design Decisions

| Decision | Resolution |
|----------|-----------|
| DDSketch vs ExponentialHistogram | ExponentialHistogram in core (tighter error at scale 7: ±0.27% vs ±0.78%). DDSketch only in DD adapter. |
| Core value type | `AnyValue` (OTel proto). `Value` is VRL-boundary only. |
| `EventMetadata` | Retained as pipeline sidecar (finalizers, source_id, schema). Not merged into `Resource.attributes`. |
| `Vec<KeyValue>` attribute lookup | O(n) acceptable for non-VRL paths. VRL adapter copies to `BTreeMap` during program execution. |
| Migration strategy | Wrapper types (incremental, always-compilable) over big-bang replacement. |
| DD sink | Not re-added — DD accepts OTLP natively. Users point OTel sink at DD's OTLP endpoint. |
| Native codecs | Deleted. `proto/vector/vector.proto` retained for Vector source backward compat. |

---

## Steps Completed

| Step | Description | Key Metric |
|------|-------------|------------|
| **0** | Foundations — buffer toggle, isolation test, span scope fix | — |
| **2** | OTel metric encoder (prerequisite for Step 1) | ~400 lines |
| **1** | DD + Vector sinks removed; OTel sink gRPC added | −10,673 lines |
| **3** | DD source rewritten as clean OTel adapter; DD types leave core | −1,637 lines (DDSketch from core) |
| **5a** | Introduce OTel wrapper types (additive, zero breakage) | +740 lines |
| **5b** | Migrate traces: OTel source/sink emit/accept OtelSpan | +200 lines |
| **5c** | Migrate logs: full pipeline (batch 1 + batch 2, 5c²a–5c²h) | ~55 files |
| **5d** | Migrate metrics: full pipeline (batch 1 + batch 2) | — |
| **5e** | Remove `use_otlp_decoding` flag + legacy deserializer paths | −464 lines |
| **5e²** | OTLP serializer encodes OTel-native events (HTTP sink) | +227 lines |
| **5f** | Ship VRL migration tool (`vector vrl-migrate`) | +1,229 lines |
| **5g** | Rename OtelXxxEvent → OtelXxx + type alias cleanup | net +5 lines |
| **5h** | OTLP HTTP JSON ingestion + dependency upgrades (prost 0.13, tonic 0.12) | +1,464/−559 lines |
| **6a** | Migrate log sources → emit `OtelLog` | 40 files |
| **6b** | Migrate metric sources → emit `OtelMetric` | `From<Metric> for Event` produces OtelMetric |
| **6c** | Verify trace source → `OtelSpan` exclusive | Already done |
| **6d** | Migrate transforms off legacy types | ~14 transforms |
| **6e** | Migrate sinks off legacy types | Via coercion helpers from 5c² |
| **6f** | Remove legacy types from core; rename OtelLog→Log | −4,500/+1,200 lines |
| **6g** | Delete native codecs + ~9,220 test fixtures | −9,400 lines |
| **6h** | Fix remaining test failures | ~103 assertion fixes, 0 failures |
| **7** | DD source metrics/traces → OtelMetric/OtelSpan directly | 47/47 DD tests pass |
| **4a** | Load-balancing sink (consistent hash, static/DNS/K8s resolvers) | — |
| **4b** | Tail sampling transform (8 policy types, decision cache) | 10 tests |
| **4c** | Pipeline telemetry — `span_metrics` transform (RED metrics) | 6 tests |

**Net code change:** ~−11,500 lines (removed ~19,500, added ~8,000).

---

## Remaining Infrastructure (intentionally kept)

| Component | Why it stays |
|-----------|-------------|
| `LogEvent` / `TraceEvent` types | Used by bridge methods (`from_log_event`/`to_log_event`) for backward-compat serializers. Remove once all serializers use OTLP natively. |
| `LogSchema` struct + `log_schema()` | `host_key` (2 callers: internal_metrics, dedupe), `metadata_key` (13 callers: remap error handling). User config + merge logic. |
| `proto/vector/vector.proto` | Vector source backward compat with older upstream instances. |

---

## Deferred to Future Release

1. Startup deprecation warning if user config sets `host_key`, `source_type_key`, or `message_key` explicitly.
2. Delete unused `LogSchema` fields (`message_key`, `timestamp_key`, `source_type_key`).
3. Migrate remaining serializers to OTLP-native (remove `to_log_event()`/`to_legacy_metric()` bridges).
4. Document the migration in release notes.

---

## Reference Documents

| Document | Purpose |
|----------|---------|
| `GUIDELINES.md` | Architectural principles for contributors |
| `VRL_MIGRATION_TOOL.md` | VRL migration tool specification and rewrite rule catalogue |
| `VRL_OTEL_NATIVE_TARGETS.md` | OTel-native VRL target design for all 3 signals |
| `VRL_DATAPOINT_CONTEXT.md` | VRL broadcast for metric data points (`.attributes` shorthand) |
| `PERFORMANCE_AND_TRADEOFFS.md` | Performance analysis, DDSketch vs ExponentialHistogram, otel-collector-contrib comparison |
| `PROTOCOL_GAP_ANALYSIS.md` | Field-by-field gap: Vector native protocol vs OTLP |
| `MIGRATION_STUDY.md` | Component-by-component complexity analysis (historical reference) |
| `MARKET.md` | Market study — observability SaaS competitive landscape (separate concern) |
