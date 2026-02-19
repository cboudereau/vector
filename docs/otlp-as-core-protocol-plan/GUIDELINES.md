# OTLP Migration: Architectural Guidelines

These principles apply to every PR in the migration. Read this before touching any code.

---

## Dependency Model

```
┌────────────────────────────────────────────────────┐
│                 ADAPTERS (I/O boundary)             │
│  src/sources/datadog_agent/  src/sources/vector/   │
│  src/sinks/opentelemetry/    src/sinks/prometheus/ │
│  src/sinks/influxdb/         src/sinks/kafka/  …   │
│                                                     │
│  Allowed: DD types, Vector proto, AgentDDSketch,   │
│           vendor-specific error paths, DD constants │
└────────────────┬───────────────────────────────────┘
                 │ adapters depend on core
                 ▼
┌────────────────────────────────────────────────────┐
│                      CORE                          │
│  lib/vector-core/   lib/codecs/                    │
│  lib/opentelemetry-proto/                          │
│                                                     │
│  OTel types only.  No DD.  No Vector proto.        │
│  No approximations. No vendor constants.           │
└────────────────────────────────────────────────────┘
```

**The core crate must never depend on adapter code.**
Verify with: `cargo build -p vector-core` — if it compiles, the boundary holds.

---

## Rules

1. **OTel/OTLP is the only core protocol.**
   `lib/vector-core/` and `lib/codecs/` contain only OTel types after Step 5.
   No exceptions.

2. **Vendor logic lives exclusively in adapters.**
   DataDog ingestion quirks, Vector native proto translation, sketch precision notes —
   all of this belongs in `src/sources/` or `src/sinks/`, never in `lib/`.

3. **Core never imports from adapters.**
   Direction of dependency is one-way: adapters depend on `lib/vector-core`, never the reverse.
   Adding `use crate::sinks::datadog` anywhere in `lib/` is a build break.

4. **No vendor type escapes the adapter.**
   `AgentDDSketch`, `dd_metric.proto` types, `dd_trace.proto` types — these are adapter-private.
   They are converted to OTel types at the first line of the source handler and never
   flow further into the pipeline.

5. **No approximations in core without a protocol-documented basis.**
   `ExponentialHistogram` is the OTel-native choice for latency distributions.
   Converting DDSketch → OTel is the DD source adapter's responsibility.
   Prometheus quantile export is the Prometheus sink adapter's responsibility.

6. **gRPC internally, HTTP also supported externally.**
   Inter-Vector communication uses OTLP/gRPC.
   Sources and sinks support both gRPC and HTTP for external integrations.
   No `MessagePack`, no `vector.proto` wire format internally.

7. **Trade-offs are documented at the boundary, not suppressed.**
   If precision is lost during sketch conversion, this is documented in the DataDog source
   adapter's code comment, not hidden in core logic.

8. **Features are preserved, not quietly dropped.**
   APM stats and tail sampling survive as OTel-native transforms.
   Internal pipeline telemetry (`vector_*` metrics) survives, emitted as OTel metrics.

9. **Baby steps: every commit passes all existing tests.**
   No "will fix tests later" commits. The bar is CI green on every PR.

10. **The VRL migration tool ships before Step 5.**
    User VRL programs must not break without warning. `vector vrl-migrate` runs first.

11. **`DatadogMetricOriginMetadata` is an adapter concern after Step 3.**
    Origin data lives in `Resource.attributes["datadog.origin.*"]`.
    No `datadog_origin_metadata` field or `DatadogMetricOriginMetadata` type in `EventMetadata`.

---

## Prohibited in `lib/vector-core/` after each Step

| After Step | Prohibited |
|---|---|
| Step 1 | `use crate::sinks::datadog`, `use crate::sinks::vector` |
| Step 3 | `AgentDDSketch`, `MetricValue::Sketch`, `DatadogMetricOriginMetadata`, `datadog_api_key()`, `DATADOG_API_KEY` |
| Step 5 | `LogEvent`, `TraceEvent` (as internal model), `MetricValue`, `use_otlp_decoding`, `LogNamespace::Legacy` |
| Step 6 | `NativeDeserializer`, `NativeSerializer`, native JSON codec |

---

## PR Checklist

Before submitting any PR in this migration:

- [ ] `cargo build -p vector-core` clean
- [ ] `cargo nextest run -p vector-core` passes
- [ ] `rg "AgentDDSketch|DatadogMetricOriginMetadata|datadog_api_key" lib/vector-core/src/`
  returns empty (from Step 3 onward)
- [ ] No `// TODO: handle sketch` comments without a tracking issue
- [ ] No match arms with `unimplemented!()` or `todo!()` on OTel signal paths
- [ ] Integration test coverage for each changed I/O boundary

---

## Scope Gotchas Found in Source Analysis

1. **`spans.rs` drops `InstrumentationScope`** (Bug, fixed in Step 0b).
   `scope_spans.into_iter().flat_map(|ils| ils.spans)` discards `ils.scope`.
   Never let a conversion function flatten away OTel structural fields silently.

2. **OTel serializer errors on `Event::Metric`** (Blocked, fixed in Step 2).
   `lib/codecs/src/encoding/format/otlp.rs:127`. Any extension that calls this path must
   wait for Step 2 or add a temporary skip.

3. **`use_otlp_decoding = false` is the current default — OTel data is silently lossy.**
   Until Step 5d, assume incoming OTLP metrics are converted with field loss
   (specifically: `ExponentialHistogram` → `AggregatedHistogram` with bucket collapsing).

4. **`DatadogMetricOriginMetadata` is set by `log_to_metric.rs` transform**, not just
   the DataDog source. The transform must stop calling `.with_origin_metadata()` in Step 3.
