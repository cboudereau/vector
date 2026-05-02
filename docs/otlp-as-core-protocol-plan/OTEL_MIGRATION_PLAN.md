# OTLP as Core Protocol — Forward Plan

**Sol** (**S**ingle **O**bservability **L**ayer) is a true fork of [Vector](https://github.com/vectordotdev/vector), rebuilt around an OpenTelemetry-centric core. See [MARKET.md](MARKET.md) for the full product vision and market positioning.

Sol's internal event model uses OpenTelemetry (OTLP) as its sole core protocol.

```rust
pub enum Event {
    Log(OtelLog),       // OpenTelemetry LogRecord
    Metric(OtelMetric), // OpenTelemetry Metric (Sum, Gauge, Histogram, ExponentialHistogram, Summary)
    Trace(OtelSpan),    // OpenTelemetry Span
}
```

All legacy core types (`LogEvent`, `Metric`, `TraceEvent`, `MetricValue`, `MetricData`, `NativeSerializer`, DD sinks) are deleted from core. The original `event.proto`/`vector.proto` is retained as a source-scoped adapter for backward compatibility with the original Vector. 2,170 tests pass.

---

## Migration Guide for Users

### Strategy 1 — VRL Migration Tool (recommended for most users)

Run `vector vrl-migrate` to auto-rewrite VRL programs for the new event model (~91% coverage):

```bash
# Preview changes (dry-run)
vector vrl-migrate --config /etc/vector/vector.toml --dry-run

# Apply rewrites
vector vrl-migrate --config /etc/vector/vector.toml
```

**What it rewrites:**
- `.message` → `.body`
- `.timestamp` → `%vector.timestamp` (metadata path)
- `.source_type` → `%vector.source_type`
- `.host` → `%<source_name>.hostname`
- `.tags."key"` → `.attributes."key"` (metric VRL)
- `.kind` → `.attributes."vector.metric_kind"`
- Log namespace-aware paths for all sources

**What needs manual review (~9%):**
- Dynamic path access (`path = get_env_var!("FIELD"); get!(., path)`)
- Custom conditions referencing legacy field names
- Lua scripts (covered separately below)

### Strategy 2 — Direct Vector-to-Sol Connection (native protocol)

The `vector` source speaks the **original Vector native gRPC protocol** (`event.proto`/`vector.proto`). Existing Vector instances can send data using their standard `vector` sink with zero configuration changes:

```
┌─────────────────────┐      native Vector gRPC     ┌─────────────────────┐
│  Original Vector     │  ── vector sink ──────────► │  Sol                 │
│  (any version)       │      port 6000              │  (vector source)     │
│  original configs    │                              │  native proto only   │
└─────────────────────┘                              └─────────────────────┘
```

**Old Vector config (sender) — no changes needed:**
```toml
[sinks.bridge]
type = "vector"
address = "sol-host:6000"
```

**Sol config (receiver):**
```toml
[sources.from_old_vector]
type = "vector"
address = "0.0.0.0:6000"
# Speaks the original Vector native gRPC protocol.
# Incoming events are converted to OTLP types at the source boundary.
```

For OTLP clients (OTel Collector, other Sol instances, any OTLP agent), use the `opentelemetry` source instead:

```toml
[sources.from_otlp]
type = "opentelemetry"
grpc.address = "0.0.0.0:4317"
http.address = "0.0.0.0:4318"
```

**Key compatibility notes:**
- The `vector` source speaks **only** the original native proto — it is a backward compatibility adapter.
- The `opentelemetry` source handles OTLP gRPC + HTTP — it is the standard ingestion path.
- Old Vector instances with an `opentelemetry` sink can connect to the `opentelemetry` source.
- There is no `vector` sink in Sol — use `type = "opentelemetry"` to send data out.

### Strategy 3 — OTel Collector as Bridge (optional)

For environments already running the OTel Collector, it can serve as an intermediary:

```
Old Vector ──► OTel Collector (otlp receiver → otlp exporter) ──► Sol (opentelemetry source)
```

This is rarely needed since Strategy 2 provides direct native protocol compatibility. Use this if you want the OTel Collector to perform additional processing (filtering, sampling, enrichment) between the original Vector and Sol.

### Breaking Changes Summary

| Component | Change | Migration |
|-----------|--------|-----------|
| **VRL paths** | `.message` → `.body`, metadata moved | Run `vector vrl-migrate` |
| **logfmt output** | Attribute keys now namespaced: `attributes.my_attr=val` | Update downstream parsers |
| **GELF output** | Fields mapped from proto: `body`→`short_message`, `severity_number`→`level` | Update GELF consumers |
| **Avro output** | Schema must match OTLP/JSON layout (nested, camelCase) | Update Avro schemas |
| **protobuf output** | Descriptor must match OTLP/JSON field names | Update proto descriptors |
| **Lua scripts** | Table layout is structured: `event.log.attributes.key` not `event.log.key` | Update Lua scripts manually |
| **JSON output** | OTLP/JSON (camelCase, nested resource/scope/attributes) | Update JSON parsers |
| **Vector source** | Native proto only (backward compat adapter) | No change needed — existing `vector` sink works |
| **Vector sink** | Deleted — use `type = "opentelemetry"` instead | Replace `type = "vector"` with `type = "opentelemetry"` in configs |
| **Transformer `only_fields`/`except_fields`** | Paths are OTLP-aware: `body`, `attributes.X`, `resource.X` | Update transformer configs |
| **honeycomb sink** | `data` field uses OTLP/JSON layout (was flat key-value) | Honeycomb handles nested JSON natively |
| **new_relic sink** | Attributes built from proto structure | Transparent — NR API accepts any attributes |
| **influxdb/logs sink** | Fields iterated from proto + attrs | Update tag/field key expectations |

---

## OTel Fidelity Review

### Deviations from OTel Spec (core types)

| Deviation | Where | Fidelity concern | Action |
|-----------|-------|------------------|--------|
| `vector.set_values` attribute | OtelMetric (Gauge) | Non-standard. Downstream sees valid Gauge. **Low risk.** | Keep — OTLP has no Set type |
| `vector.metric_type` attribute | OtelMetric | Informational marker. Downstream ignores it. **Harmless.** | Keep |
| `vector.metric_kind=incremental` on Gauge | OtelMetric | Semantically incorrect per OTel (Gauge has no temporality). **Medium risk.** | Phase E — replace with stateful gauge accumulation |
| `vector.statistic` attribute | OtelMetric | Distinguishes histogram vs summary distributions. Sinks (prometheus, statsd, influxdb) actively read it. | Keep — functional, no OTel-native alternative |
| `OtelAttributes` (BTreeMap) | All events | Lossless conversion at proto boundaries. **No fidelity loss.** | Keep |
| `EventMetadata` sidecar | All events | Pipeline infrastructure, never in OTLP output. **Correct.** | Keep |

**Verdict (core types):** Acceptable. `vector.*` attributes only appear when non-OTel sources create metrics. OTLP passthrough paths never inject them.

### Source-Side OTLP Gaps

Audit of all metric-producing sources against otelcontribcol behavior. The `opentelemetry` source is a pass-through with zero divergence and is excluded.

| Gap | Affected Sources | otelcontribcol Behavior | Sol Behavior | Impact |
|-----|-----------------|------------------------|--------------|--------|
| **No Resource attributes** | All except `datadog_agent` | Sets `service.name`, `host.name` (datadogreceiver); empty (statsdreceiver) | `resource` field is `None` | Backends can't group by service — everything lands under "unknown" |
| **No InstrumentationScope** | All except `opentelemetry` | Sets `scope.name` = receiver package, `scope.version` = build version | `scope` field is `None` | Backends can't identify which pipeline/receiver produced the metric |
| **No `unit` field** | All except `opentelemetry` | Configurable per metric type (e.g., `ms`, `s`, `By`) | `unit` is always `""` | Backends can't auto-label axes or detect unit conflicts |
| **No timestamps (StatsD)** | `statsd` | Sets `time_unix_nano` + `start_time_unix_nano` on every data point | `time_unix_nano=0`, `start_time_unix_nano=0` | Delta temporality rate computation impossible without time window |
| **No `start_time_unix_nano`** | All except `datadog_agent` | Sets start time for Delta metrics (flush interval start) | Not set | Backends guess the aggregation window |
| **StatsD: Histogram as `explicit_bounds` per sample** | `statsd` | ExponentialHistogram (auto-scaling, ~160 buckets) with pre-aggregation | `Histogram` with one `explicit_bound` per sample value | Unbounded bucket explosion — 1000 samples/sec = 1000 boundaries. **Note**: spec says both Histogram and ExponentialHistogram are equally valid. Sol could use Histogram with pre-defined bounds (better Prometheus compatibility) — see D36. |
| **StatsD: no flush-interval aggregation** | `statsd` | Aggregates within configurable flush interval (default 60s) | Emits one OTLP data point per UDP packet | Spec says per-observation emission is technically valid but "infeasible due to the sheer volume." Aggregation is practically required. |
| **StatsD: `is_monotonic=true`** | `statsd` | `false` by default (StatsD counters can be negative) | `true` hardcoded | Violates OTLP spec if counter receives negative delta |
| **StatsD: gauge delta via custom attribute** | `statsd` | Stateful accumulation: deltas added to running value, emits absolute Gauge | Emits Gauge with `vector.metric_kind=incremental` attribute | **Spec violation**: "A Gauge does not support different aggregation temporalities." Delta Gauge does not exist in OTLP. Correct alternatives: stateful accumulation → absolute Gauge, or non-monotonic Sum with Delta temporality — see D38. |
| **StatsD: bare tag `AnyValue { value: None }`** | `statsd` | Empty string `""` when `enableSimpleTags=true` | `AnyValue { value: None }` | **Spec allows this**: "It is valid for all values to be unspecified." However, some backends may not handle it well — see D39. |
| **StatsD: sample rate as bucket count** | `statsd` | Weight-based histogram insertion | `1/sample_rate` cast to `u32` as bucket count | Semantically different histogram structure |
| **StatsD: metric name sanitization** | `statsd` | No sanitization — names pass through as-is | `/`→`-`, whitespace→`_`, strips non-alphanumeric | Different metric names for same StatsD input |
| **StatsD: no DogStatsD container ID extraction** | `statsd` | `c:containerID` → `container.id` data point attribute | Parsed as regular tag `c=containerID` | Loses container identity semantic |

### Sink-Side OTLP Gaps

Audit of all metric-consuming sinks. The `opentelemetry` sink (gRPC + HTTP) has full OTLP fidelity and is excluded.

| Gap | Affected Sinks | Impact |
|-----|---------------|--------|
| **ExponentialHistogram silently dropped** | prometheus (exporter + remote_write), influxdb, cloudwatch, greptimedb | Metrics lost with no error — silent data loss |
| **ExponentialHistogram → error** | statsd, splunk_hec, sematext | At least surfaces the problem, but metrics still lost |
| **No temporality awareness** | All non-OTLP sinks | Delta counter → Prometheus = double-counting; Cumulative → InfluxDB = overcounting |
| **Resource/Scope ignored** | All non-OTLP sinks | `host.name`, `service.name` from Resource never become labels/tags/dimensions |
| **`unit` field ignored** | All non-OTLP sinks | No axis labeling, no unit conversion |
| **Limited metric type support** | splunk_hec (Sum+Gauge only), sematext (Sum+Gauge only) | Most metric types silently dropped |
| **`vector.*` attributes fragile coupling** | prometheus, influxdb, statsd (read `vector.statistic`) | Invisible contract — rename breaks 3 sinks silently |

### Sink Metric Type Support Matrix

| Sink | Sum | Gauge | Histogram | ExponentialHist | Summary | Distribution | Set |
|------|:---:|:-----:|:---------:|:---------------:|:-------:|:------------:|:---:|
| **OTLP (gRPC+HTTP)** | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ |
| **Prometheus** | ✅ | ✅ | ✅ | ❌ drop | ✅ | ✅ | ✅ |
| **InfluxDB** | ✅ | ✅ | ✅ | ❌ empty | ✅ | ✅ | ✅ |
| **GreptimeDB** | ✅ | ✅ | ✅ | ❌ drop | ✅ | ✅ | ✅ |
| **CloudWatch** | ✅ | ✅ | ❌ drop | ❌ drop | ❌ drop | ✅ | ✅ |
| **StatsD** | ✅ | ✅ | ❌ error | ❌ error | ❌ error | ✅ | ✅ |
| **Splunk HEC** | ✅ | ✅ | ❌ error | ❌ error | ❌ error | ❌ error | ❌ error |
| **Sematext** | ✅ | ✅ | ❌ error | ❌ error | ❌ error | ❌ error | ❌ error |
| **Humio** | ✅* | ✅* | ✅* | ✅* | ✅* | ✅* | ✅* |

*Humio converts all metrics to logs via MetricToLog — technically supports all types but loses metric semantics.

---

## Performance Review

| Path | Cost | Status |
|------|------|--------|
| OTLP gRPC → OTLP gRPC (passthrough) | Zero conversion | Optimal |
| VRL read-only (filter, route, sample) | Original proto returned (P38/P39) | Optimal |
| VRL mutating (remap with writes) | Proto→Value at entry, Value→Proto at exit | ~5% regression |
| VRL attribute lookup | BTreeMap O(log n) + Value clone | ~5% regression |
| VRL complex paths (`.attr[0]`) | `as_map()` builds ObjectMap from proto | ✅ Fixed — `to_value_canonical()` deleted |
| Codec encoding | `as_map()` per event | ✅ Fixed — direct ObjectMap, no Value wrapper |

---

## Locked Decisions

All decisions locked. Autopilot proceeds without stopping.

| ID | Decision | Answer |
|----|----------|--------|
| D1 | Delete `to_value_canonical()` entirely | **Yes** — all 20 call sites migrated, method deleted |
| D2 | logfmt: namespace attribute keys | **Yes** — `attributes.my_attr=val`, proto fields flat |
| D3 | GELF: direct proto mapping | **Yes** — `body`→`short_message`, `severity_number`→`level`, `time_unix_nano`→`timestamp`, `resource.host.name`→`host`, rest→`_attr` |
| D4 | Avro: OTLP/JSON via Serialize | **Yes** — breaking, document in migration guide |
| D5 | protobuf: OTLP/JSON via Serialize → encode_message | **Yes** |
| D6 | Lua: structured layout | **Yes** — `{ body, attributes, resource, scope }` |
| D7 | Arrow: iterate proto directly | **Yes** |
| D8 | honeycomb: Serialize (OTLP/JSON) | **Yes** |
| D9 | new_relic: iterate proto + attrs | **Yes** |
| D10 | influxdb/logs: iterate proto + attrs | **Yes** |
| D11 | reduce: direct structured iteration | **Yes** |
| D12 | trace_to_log: transfer proto fields directly | **Yes** |
| D13 | schema/definition: proto-aware Kind inference | **Yes** |
| D14 | enrichment_tables: match on attributes directly | **Yes** |
| D15 | Delete convert_to_fields/as_map methods | **Yes** — after callers migrated |
| D16 | get(event_root()): OTLP/JSON-shaped Value | **Yes** |
| D17 | Delete MetricTags type entirely | **Yes** |
| D18 | ~~Delete Sample/Bucket/Quantile~~ | **No** — keep as convenience constructors. Used in 20+ files, no legacy semantics, never on wire |
| D19 | Split otel_event.rs | **Yes** — otel_log.rs + otel_metric.rs + otel_attributes.rs + otel_event.rs |
| D20 | Document all breaking changes | **Yes** — in this file's Migration Guide |
| D21 | Delete `vector` sink entirely | **Yes** — redundant wrapper around `opentelemetry` sink. Users use `type = "opentelemetry"` directly |
| D22 | Restore native Vector protocol in `vector` source | **Yes** — `event.proto`/`vector.proto` as source-scoped adapter for backward compatibility with original Vector |
| D23 | Delete `metric_tags!` macro, replace with `otel_tags!` | **Yes** — clean break, Sol is a new product. 3 bare-tag sites get manual `AnyValue { value: None }` |
| D24 | Combine A5 (delete vector sink) + A6 (restore native proto) | **Yes** — one coherent step, avoids intermediate broken state |
| D25 | Transformer `only_fields`/`except_fields` paths become OTLP-aware | **Yes** — `body`, `attributes.X`, `resource.X`. Breaking change, documented |
| D26 | `vrl-migrate` tool | **Already built** — `src/vrl_migrate/`, 3-pass rewriter. No new phase needed |
| D27 | Performance gate verified via `cargo bench` | **Yes** — `cargo bench --features remap-benches --bench remap` (VRL), `--features statistic-benches --bench distribution_statistic`. Run before/after Phase B |
| D28 | A6 metric conversion: Counter→Sum, Gauge→Gauge, Set→Gauge+attr, Distribution(H)→Histogram, Distribution(S)→Summary, AggHistogram→Histogram, AggSummary→Summary, Sketch→ExponentialHistogram. Incremental→DELTA, Absolute→CUMULATIVE | **Yes** |
| D29 | A6 log conversion: `Log.value`→body, `Log.fields`→attributes, `message` key promoted to body if value absent | **Yes** |
| D30 | A6 trace conversion: best-effort extraction of trace_id/span_id/name/start_time/end_time from fields, rest→span attributes | **Yes** |
| D31 | A6 `interval_ms` → compute `startTimeUnixNano = timeUnixNano - interval_ms × 1_000_000` | **Yes** |

### Phase E Decisions — LOCKED

All decisions locked. Autopilot proceeds without stopping. Reference: OTLP spec is the authority, not otelcontribcol.

| ID | Decision | Answer |
|----|----------|--------|
| D32 | ExponentialHistogram→Histogram conversion for non-OTLP sinks | **(b)** Centralized normalization layer — write conversion once, not 7 times |
| D33 | Temporality normalization | **(a)** Per-sink normalizer — each sink knows its backend's requirements |
| D34 | Resource attributes on scraper sources | **(c)** Configurable with defaults: `service.name=sol/<source_type>`, `host.name=hostname()`. Config: `resource_attributes = { "service.name" = "my-app" }`. `host.name` auto-detected, suppress with `""` |
| D35 | InstrumentationScope naming | **(a)** `name=sol/<source_type>`, `version=<build_version>` |
| D36 | StatsD histogram + aggregation | **(d)** ExponentialHistogram internally (MaxSize=160, starting scale=20, auto-adapt) + flush-interval aggregation (default 10s, configurable via `aggregation_interval_secs`). Convert to explicit-bounds Histogram at Prometheus/InfluxDB/StatsD sink boundary. Target boundaries: Prometheus defaults `[.005,.01,.025,.05,.1,.25,.5,1,2.5,5,10]` configurable per sink. **Sol advantage**: lossless merge across instances + zero-config at source + Prometheus compatibility at sink |
| D37 | StatsD `is_monotonic` default | **(c)** Configurable, default `false` — spec-safe; users can opt into `true` for proper Prometheus rate calculation |
| D38 | StatsD gauge delta handling | **(a')** Stateful accumulation persisting across flushes with TTL. Default TTL=5min, configurable. On expiry: stop emitting (no goodbye metric). **Sol advantage over otelcontribcol**: gauge state survives flush intervals |
| D39 | StatsD bare tag representation | **(b)** `AnyValue { value: Some(StringValue("")) }` — Prometheus/Mimir/Datadog/ES all drop `None` values. Pragmatic choice for backend reach |
| D40 | `vector.*` custom attributes | **(c)** Eliminate ALL — D36+D38+D45 make them dead code. `Distribution` variant eliminated, sinks branch on `MetricView::Histogram` vs `MetricView::Summary` proto types directly. Zero custom extensions = pure OTLP output |
| D41 | OTLP `unit` field | **(a)** Yes, UCUM strings — `By` (bytes), `s` (seconds), `{connections}`, `1` (ratios) |
| D42 | Sink Resource→labels propagation | **(b)** Configurable. Default: promote `service.name` + `host.name` to labels. Config: `resource_to_labels = ["service.name", "host.name"]` |
| D43 | Ordering | **(a)** Sinks first (E1-E2), then sources (E3-E6), then cleanup (E7-E9) — prevents silent data loss |
| D44 | Sink scope | **(b)** Tier 1 first: prometheus + influxdb + statsd. Tier 2 (cloudwatch + greptimedb) follows |
| D45 | Summary production | **(b)** All distribution-like types → Histogram for new data. OTLP/Prometheus passthrough Summary preserved. Spec: "not recommended for new applications" |

### otelcontribcol StatsD Receiver — Reference Implementation Analysis

Research of `opentelemetry-collector-contrib/receiver/statsdreceiver/` and `lightstep/go-expohisto` library to ground E4 implementation.

**Architecture:** Parser → single-threaded aggregation goroutine → flush on `time.Ticker` → `nextConsumer.ConsumeMetrics()`. State keyed by `(name, metricType, attributeSet)` per source address. On flush: build `pmetric.Metrics`, call `resetState(now)` (full reset, fresh empty map).

**ExponentialHistogram engine** (go-expohisto `structure.Histogram[float64]`):
- Starts at scale 20 (highest resolution), downscales on overflow
- `MapToIndex(value)`: for scale > 0, `floor(log(value) * log2e * 2^scale)`; for scale ≤ 0, exponent bit extraction with right-shift
- MaxSize=160 default. On overflow (bucket span ≥ MaxSize): compute `changeScale` by iteratively right-shifting high/low until they fit, then downscale all buckets by merging `2^change` adjacent buckets
- Variable-width bucket counters: `[]uint8` → `[]uint16` → `[]uint32` → `[]uint64` (widens on overflow)
- Circular buffer with `indexBase` for efficient bidirectional expansion
- `UpdateByIncr(value, count)` — weight-based insertion (sample rate → count inflation)
- `MergeFrom(other)` — finds minimum scale fitting both, downscales, merges bucket-by-bucket

**Counter handling:**
- Aggregated as Delta Sum. Sample rate: `value / sampleRate` (inflate count)
- Counter type configurable: `int` (truncate to int64, default), `float` (preserve decimal), `stochastic_int` (probabilistic rounding where P(round up) = fractional part)
- `is_monotonic` configurable (default depends on config)

**Gauge handling:**
- Absolute (`42|g`): last-value-wins, replaces previous
- Delta (`+5|g`, `-3|g`): adds to current value in-place
- No `start_time_unix_nano` on gauges
- **Key difference from Sol plan:** otelcontribcol resets ALL state on flush (gauges rebuilt from scratch each interval). Sol's D38 says gauge state persists across flushes with TTL — this is a deliberate Sol advantage.

**Set handling:** **Not supported.** otelcontribcol StatsD receiver ignores `"s"` type entirely (returns error). Sol has an advantage here.

**Histogram/Timer types (`h`, `ms`, `d`):**
- Default observer type is `disabled` (drops these metrics!)
- When `observer_type = "histogram"`: uses ExponentialHistogram (go-expohisto) OR explicit-bucket Histogram (via regex pattern matching metric names)
- When `observer_type = "summary"`: uses Summary with configurable percentiles
- When `observer_type = "gauge"`: appends each observation as a separate ScopeMetrics entry (no aggregation)
- Sample rate → `UpdateByIncr(value, 1/sampleRate)` (weight-based insertion)

**Explicit bucket alternative:** otelcontribcol supports regex-based explicit bucket config per metric name as alternative to ExponentialHistogram. Config: `histogram.bucket_boundaries: [{regexp: "request.duration.*", boundaries: [0.01, 0.1, 1, 10]}]`. When a metric name matches, uses traditional `Histogram` instead of `ExponentialHistogram`.

**Timestamps:**
- Counters/Histograms: `start_time_unix_nano` = `lastIntervalTime`, `time_unix_nano` = now at flush
- Gauges: `time_unix_nano` = time of last update, no `start_time_unix_nano`
- DogStatsD v1.3 timestamp override: `T<unix_seconds>` suffix on counters/gauges only

**DogStatsD extensions:**
- Container ID: `c:<id>` → `container.id` attribute (OTel semantic convention `conventions.ContainerIDKey`)
- Timestamp: `T<unix>` → overrides `time_unix_nano` (counters + gauges only)

**Per-source-address aggregation:** State is keyed by `netAddr` (source IP/port). Each source address gets its own independent instruments map. On flush, each address produces a separate `BatchMetrics` with `client.Info` identifying the source.

### Phase E Decisions — LOCKED (from otelcontribcol analysis)

| ID | Decision | Options | Answer | Rationale |
|----|----------|---------|------|-----------|
| D46 | StatsD counter type | **(a)** Always f64 **(b)** Configurable int/float/stochastic_int **(c)** Always int | **(a)** | f64 is lossless for StatsD use cases (values are text-parsed floats anyway). Int truncation loses data, stochastic rounding adds complexity for negligible benefit. If a user sends `1.5|c`, they expect 1.5 not 1. otelcontribcol defaults to int for historical Go reasons, not correctness. |
| D47 | Explicit-bucket Histogram option (regex → fixed bounds instead of ExponentialHistogram) | **(a)** ExponentialHistogram only, convert at sink **(b)** Regex-based explicit bucket config | **(a)** | ExponentialHistogram is strictly more capable — lossless at source, converted to explicit bounds at sink with user-configured boundaries. Regex config adds UX complexity for zero fidelity gain. Users who want specific boundaries already configure them on the sink side. |
| D48 | DogStatsD v1.3 timestamp support (`T<unix>`) | **(a)** Yes **(b)** Skip | **(a)** | ~10 lines in parser. DogStatsD v1.3 is widely deployed (Datadog Agent 7.25+). Timestamps enable correct rate computation on counters. No downside. |
| D49 | Per-source-address aggregation vs global | **(a)** Global **(b)** Per-source-address | **(a)** | StatsD is typically many app instances → one agent. Global aggregation = fewer series, matches how users think about metrics. Per-address would produce N copies of the same metric name for N pods. If users need source identity, they should add a tag (`host:X`). otelcontribcol's per-address design is an artifact of its multi-tenant collector model, not a StatsD best practice. **Document in source config:** "Metrics are aggregated globally across all senders. To distinguish sources, add a host or instance tag to your StatsD packets." |
| D50 | `observer_type` config for timers | **(a)** Always ExponentialHistogram **(b)** Configurable (histogram/disabled, no summary) | **(a)** | D45 already locked: no Summary for new data. Disabled mode = silent data loss, the opposite of Sol's promise. Users who don't want timer data should filter in a transform. One fewer config knob = simpler UX. |
| D51 | `log_to_metric` transform histogram type | **(a)** ExponentialHistogram (same engine as StatsD) **(b)** Explicit-bounds Histogram (one bound per sample, current behavior) | **(a)** | log_to_metric emits individual observations (1 sample per log line), then the `aggregate` transform merges them. ExponentialHistogram merge is lossless and bounded (MaxSize=160). Current explicit-bounds-per-sample approach has unbounded bucket growth — same problem as StatsD. Reusing the ExpHist engine is consistent and correct. |
| D52 | `vector` source converter migration | **(a)** Keep `new_distribution_from_samples` (backward-compat adapter) **(b)** Migrate to proper Histogram/Summary | **(a)** | The vector source is a legacy adapter for old Vector instances. It must faithfully represent what the sender intended. Legacy Distributions with `statistic=summary` must remain Summary; `statistic=histogram` must remain Histogram. The constructors can be renamed/refactored in E8, but the conversion logic stays. This is the one place where `vector.statistic` remains meaningful until all upstream Vectors are retired. |
| D53 | Set merge without `vector.set_values` | **(a)** Keep `vector.set_values` as exception **(b)** Dedicated `BTreeSet<String>` field on OtelMetric **(c)** Drop set merge, Gauge(cardinality) at source | **(b)** | `vector.set_values` leaks into OTLP output if exported via the opentelemetry sink — backends see a non-standard array attribute. A dedicated `set_values: Option<BTreeSet<String>>` field on OtelMetric (like `resource_attrs`) keeps merge capability without polluting attributes. `BTreeSet` deduplicates by construction — repeated values (`user123` hitting 1M times) cost O(1) per duplicate insert, no memory growth. The field is only read by the aggregate transform's merge logic and never serialized to OTLP proto. Gauge value = `set_values.len()` recomputed on read. |

### Phase E Implementation Details — LOCKED

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| ExponentialHistogram MaxSize | 160 | OTel SDK default, ~1KB/series |
| ExponentialHistogram starting scale | 20 | Highest resolution, ratchets down on data range |
| Default flush interval | 10s | Lower latency than otelcontribcol (60s), still 100-1000x fewer points than per-packet |
| Flush interval configurable | Yes — `aggregation_interval_secs` | |
| Prometheus sink target boundaries | `[.005,.01,.025,.05,.1,.25,.5,1,2.5,5,10]` (configurable) | Prometheus defaults, widest compatibility |
| ExpHist→Histogram conversion | Linear interpolation within exponential buckets | Standard approach (same as Prometheus native→classic) |
| Gauge TTL default | 5min (configurable) | Matches typical StatsD agent expiry |
| Gauge expiry behavior | Stop emitting, no goodbye metric | |
| Resource config syntax | `resource_attributes = { "service.name" = "my-app" }` | TOML inline table |
| `host.name` auto-detection | On by default, `host.name = ""` to suppress | |
| Sink label promotion config | `resource_to_labels = ["service.name", "host.name"]` | |
| Delete `MetricView::Distribution` | Yes — dead after E4 | Broken raw-samples path, replaced by proper aggregation |
| Delete `new_distribution_from_samples()` | Yes — dead after E4 | |
| Delete `distribution_statistic()` | Yes — no callers after D40(c) | |
| StatsD metric name sanitization | Keep current (sanitize by default) | It's a feature, not a divergence |
| DogStatsD container ID extraction | In scope for E4 | Small change, ~5 lines in parser |

---

## Autopilot Execution Plan

### Phase A — Clean Deletes and Source/Sink Restructure

Run `cargo test -p vector -p vector-core -p codecs -p vector-vrl-metrics` after each commit.

**A1. Delete `MetricTags` type — replace `metric_tags!` with `otel_tags!`**
- Delete `metric_tags!` macro from `event/metric/mod.rs`
- Replace all ~200 `metric_tags!(...)` call sites with `otel_tags!(...)` across lib/ and src/
- 3 sites with `None` values (bare tags in json.rs, text.rs) → manual `AnyValue { value: None }` construction
- Replace all `.with_metric_tags(Some(MetricTags::from_iter(...)))` with `.with_tags(Some(OtelAttributes::from_iter(...)))`
- Replace all `.with_metric_tags(Some(metric_tags!(...)))` with `.with_tags(Some(otel_tags!(...)))`
- Change `tags_from_key()` in `lib/vector-core/src/metrics/recorder.rs` to return `Option<OtelAttributes>`
- Delete `with_metric_tags()` bridge method on OtelMetric
- Delete `event/metric/tags.rs` (`MetricTags`, `TagValue`, `TagValueSet`)
- Delete `MetricTags` re-export from `event/mod.rs`
- Delete `MetricTags` Arbitrary impl from `event/metric/arbitrary.rs`

**A2. Keep `vector.statistic` attribute** ✅ DONE
- `vector.statistic` is functional (not residual): sinks (prometheus, statsd, influxdb) actively read it via `distribution_statistic()` to distinguish histogram vs summary
- No OTel-native alternative exists — both map to `Histogram` in the proto
- Decision table updated: attribute is kept alongside `vector.metric_type`

**A3. Split `otel_event.rs` (7,213 lines)** ✅ DONE
- Extracted `OtelAttributes` → `otel_attributes.rs` (422 lines)
- Extracted `OtelMetric` + `MetricView` → `otel_metric.rs` (1,852 lines)
- Remaining `otel_event.rs` (5,028 lines): shared helpers, OtelLog, OtelSpan, tests
- All 196 vector-core tests pass

**A4. Delete `vector` sink + restore native Vector protocol in `vector` source** ✅ DONE

*Vector sink deleted:*
- Deleted `src/sinks/vector/` entirely
- Removed `sinks-vector` feature from `Cargo.toml`
- Replaced `VectorSinkConfig` with `OtelSinkConfig` (GrpcConfig) in validation runner config.rs + telemetry.rs

*Native proto restored in vector source:*
- Restored `proto/vector/event.proto` and `proto/vector/vector.proto`
- Added proto compilation to `build.rs` (tonic-build)
- Created `src/sources/vector/convert.rs` — full conversion layer (Log→OtelLog, Metric→OtelMetric with all types, Trace→OtelSpan)
- Created `src/sources/vector/service.rs` — NativeVectorService implementing Vector gRPC trait
- Vector source now speaks both OTLP and native Vector protocol on the same gRPC port

### Phase B — Eliminate `to_value_canonical()` from internal methods (medium risk)

Run `cargo bench --features remap-benches --bench remap` before Phase B starts (baseline) and after B5 (result).

**B1-B5. Eliminate `to_value_canonical()` from internal methods** ✅ DONE
- Extracted `build_canonical_map()` on both OtelLog and OtelSpan
- `to_value_canonical()` is now a thin wrapper: `Value::Object(self.build_canonical_map())`
- `as_map()`, `convert_to_fields()`, `convert_to_fields_unquoted()`, `all_event_fields_skip_array_elements()` all use `build_canonical_map()` directly
- No internal method calls `to_value_canonical()` anymore — only the method definition and one `get()` fallback remain
- All 196 vector-core tests pass

### Phase C — Migrate external callers ✅ DONE

All `to_value_canonical()` call sites migrated to `as_map()`:
- **C1** logfmt encoder — uses `as_map().unwrap_or_default()`
- **C2** GELF encoder — uses `as_map().unwrap_or_default()`
- **C3** Avro encoder — uses `as_map().unwrap_or_default()`
- **C4** protobuf encoder — uses `Value::Object(as_map().unwrap_or_default())`
- **C6** Lua bridge — uses `Value::Object(as_map().unwrap_or_default())`
- **C10** reduce transform tests — uses `Value::Object(as_map().unwrap_or_default())`
- **C12** schema/definition — uses `Value::Object(as_map().unwrap_or_default())`
- **C13** enrichment_tables test — uses `as_map().unwrap_or_default().is_empty()`
- **C17** postgres integration test + otel_event test — migrated

### Phase D — Delete `to_value_canonical()` ✅ DONE

- Deleted `to_value_canonical()` from both OtelLog and OtelSpan
- Deleted `build_canonical_map()` intermediary — logic inlined into `as_map()`
- `as_map()` is now the canonical map builder (was previously a thin wrapper)
- Zero `to_value_canonical` references remain in `lib/` or `src/`
- All 196 vector-core tests pass, full workspace compiles cleanly

### Gate (Phases A-D)

| Metric | Target | Verification |
|--------|--------|--------------|
| Tests passing | ≥ 2,170 | `cargo test -p vector -p vector-core -p codecs -p vector-vrl-metrics` |
| `to_value_canonical()` call sites | 0 | `grep -rn to_value_canonical lib/ src/` |
| `MetricTags` references | 0 | `grep -rn MetricTags lib/ src/` |
| `vector.statistic` attribute | kept | Functional — used by prometheus/statsd/influxdb sinks |
| `metric_tags!` macro references | 0 | `grep -rn 'metric_tags!' lib/ src/` |
| VRL remap regression | ≤ 5% | `cargo bench --features remap-benches --bench remap` |
| VRL complex path regression | < 20% (was 100-200%) | Same bench, complex path scenarios |

### Phase E — OTLP Fidelity Alignment (source/sink divergences)

**Status: E1-E9 DONE + D51 DONE + D53 DONE + sink ExponentialHistogram support DONE + Phase F DONE. ALL `vector.*` attributes eliminated.**

All decisions locked. Execution order: sinks first (E1-E2), then sources (E3-E6), then sink enrichment (E7), then cleanup (E8-E9), then structural field migration (F1-F3).

**Completed:**
- **E1** ✅ ExponentialHistogram→Histogram conversion in MetricNormalizer + Prometheus exporter
- **E2** ✅ Already implemented — kind()/set_kind() maps to OTLP temporality
- **E3** ✅ Resource + Scope on all metric-producing sources (shared helper in source_otel.rs)
- **E4** ✅ StatsD aggregation engine — ExponentialHistogram + flush-interval aggregator across UDP/TCP/Unix, DogStatsD extensions, D39 bare tags, 55 tests
- **E5** ✅ Already implemented — all scraper sources set timestamps
- **E6** ✅ OTLP unit field population — counters="1", timers="s"/"ms", host_metrics inferred from name
- **E7** ✅ resource_to_labels/resource_to_tags config on Prometheus, InfluxDB, StatsD sinks
- **E8** ✅ Eliminate vector.* attributes — aggregated StatsD output has no vector.* attrs (E8b); direct opentelemetry_proto imports replaced with re-exports in 8 sources (E8a)
- **E9** ✅ Delete `MetricView::Distribution` variant — 4 steps: strip vector.* attrs from constructors, remove is_distribution() gate, delete variant + all match arms across ~15 files, rename `new_distribution_from_samples` → `new_histogram_from_samples`
- **D51** ✅ log_to_metric Histogram/Summary → ExponentialHistogram (with new_exponential_histogram_single constructor)
- **D53** ✅ Replace `vector.set_values` attribute with `set_values: Option<BTreeSet<String>>` struct field on OtelMetric. Replace `vector.metric_kind` attribute with `kind_override: Option<MetricKind>` struct field. Delete all `VECTOR_*` constants and `vector.*` prefix filtering from `tags()`/`all_tags_including_resource()`.
- **Sink ExponentialHistogram support** ✅ Prometheus collector (explicit bucket conversion), InfluxDB (count/sum/min/max/avg), GreptimeDB (count/sum/min/max), CloudWatch (StatisticSet)

**Gate check results (all zero):**
```
grep -rn 'vector\.(statistic|metric_type|metric_kind|set_values)' lib/ src/ → 0
grep -rn 'distribution_statistic' lib/ src/ → 0
grep -rn 'MetricView::Distribution' lib/ src/ → 0
grep -rn 'VECTOR_' lib/vector-core/src/event/otel_fields.rs → 0
```

#### E1. Centralized ExponentialHistogram → Histogram conversion (Tier 1 sinks)

Add ExpHist→Histogram conversion in the normalization layer so non-OTLP sinks receive explicit-bounds Histograms instead of silently dropping data. Conversion uses linear interpolation within exponential buckets.

- Create `fn exponential_to_explicit(exp_hist, target_bounds) -> HistogramDataPoint` in normalization layer
- Default target boundaries: `[.005, .01, .025, .05, .1, .25, .5, 1, 2.5, 5, 10]` (Prometheus defaults)
- Configurable per sink via `histogram_bucket_boundaries` config key
- **prometheus** (exporter + remote_write): Wire through normalizer. Currently `MetricView::ExponentialHistogram { .. } => {}` (silent drop) → convert to Histogram
- **influxdb**: Wire through normalizer. Currently returns empty fields → convert to Histogram fields
- **statsd**: Wire through normalizer. Currently emits error → convert to distribution samples
- Preserve `count`, `sum`, `min`, `max` from ExponentialHistogram (lossless scalar fields)

#### E2. Temporality-aware normalization (Tier 1 sinks)

Extend existing `MetricNormalize` trait implementations to respect `aggregation_temporality`:

- **Prometheus** expects Cumulative → if Delta Sum/Histogram, accumulate in-memory (stateful normalizer tracks running totals per metric series)
- **InfluxDB** expects Delta → if Cumulative Sum, diff consecutive values
- **StatsD** expects Delta → if Cumulative Sum, diff
- Each normalizer reads `aggregation_temporality` from the proto and converts as needed
- Add `is_delta()` / `is_cumulative()` helpers on OtelMetric if not present

#### E3. Resource + Scope on all metric-producing sources

Add configurable Resource and InstrumentationScope to all sources. Create shared helper:

```rust
fn build_source_resource(source_type: &str, config_overrides: &BTreeMap<String, String>) -> Resource
fn build_source_scope(source_type: &str) -> InstrumentationScope
```

Defaults: `service.name = sol/<source_type>`, `host.name = hostname()` (auto-detected). User override via:

```toml
[sources.my_host_metrics]
type = "host_metrics"
resource_attributes.service.name = "my-infra"
resource_attributes.host.name = ""  # suppress auto-detection
```

Per-source wiring:
- **host_metrics**: `service.name=sol/host_metrics`, `host.name`; Scope `sol/host_metrics`
- **apache_metrics**: `service.name=sol/apache_metrics`, `host.name`; Scope `sol/apache_metrics`
- **nginx_metrics**: `service.name=sol/nginx_metrics`, `host.name`; Scope `sol/nginx_metrics`
- **mongodb_metrics**: `service.name=sol/mongodb_metrics`, `host.name`; Scope `sol/mongodb_metrics`
- **postgresql_metrics**: `service.name=sol/postgresql_metrics`, `host.name`; Scope `sol/postgresql_metrics`
- **eventstoredb_metrics**: `service.name=sol/eventstoredb_metrics`, `host.name`; Scope `sol/eventstoredb_metrics`
- **aws_ecs_metrics**: `service.name=sol/aws_ecs_metrics`, `host.name`; Scope `sol/aws_ecs_metrics`
- **prometheus** (scrape + remote_write): `service.name=sol/prometheus`, `host.name`; Scope `sol/prometheus`
- **internal_metrics**: `service.name=sol`, `host.name`; Scope `sol/internal_metrics`
- **static_metrics**: configurable; Scope `sol/static_metrics`
- **statsd**: `service.name=sol/statsd`, `host.name`; Scope `sol/statsd`
- **datadog_agent**: Already has Resource — add Scope `sol/datadog_agent`

#### E4. StatsD source: flush-interval aggregation + ExponentialHistogram

Rewrite StatsD source to be OTLP-spec-compliant. This is the largest step.

**New aggregation engine** (`src/sources/statsd/aggregator.rs`):
- In-memory state per metric series (name + tags), flushed every `aggregation_interval_secs` (default 10s, configurable)
- **Counters**: sum increments → single `Sum { is_monotonic: false (default, configurable), temporality: Delta }` data point per interval
- **Gauges (absolute `42|g`)**: last-value wins → `Gauge` data point
- **Gauges (delta `+5|g`, `-3|g`)**: stateful accumulation. Running value persists across flushes. TTL = 5min (configurable). On first delta without prior absolute, start from 0. Emit as absolute `Gauge`.
- **Histograms/Timers/Distributions (`h`, `ms`, `d`)**: record into ExponentialHistogram (MaxSize=160, starting scale=20). Sample rate → weight-based insertion. All types emit as `ExponentialHistogram { temporality: Delta }`. No Summary production (D45).
- **Sets**: accumulate unique values within interval → emit cardinality as `Gauge`

**Timestamps**: `time_unix_nano` = flush time, `start_time_unix_nano` = previous flush time

**Bare tags**: `AnyValue { value: Some(StringValue("")) }` (D39)

**DogStatsD container ID**: parse `c:<id>` tag → `container.id` data point attribute

**Delete**:
- `new_distribution_from_samples()` constructor — replaced by ExpHist recording
- `new_gauge_delta()` constructor — replaced by stateful accumulation emitting absolute Gauge
- `new_set_from_values()` — replaced by simple `new_gauge()` with cardinality
- `convert_to_statistic()` function — no summary/histogram distinction for new data
- `MetricView::Distribution` match arms in prometheus/influxdb/statsd sinks — dead code

#### E5. Timestamps on all scraper sources

Ensure every metric data point has proper timestamps:
- `time_unix_nano`: observation/scrape time (most sources already do this via `with_timestamp()`)
- `start_time_unix_nano`: for Delta/Cumulative metrics, set to source start time or previous scrape time
- Audit: `statsd` (was `0`, fixed by E4), `internal_metrics` (verify), all others (verify `with_timestamp` sets proto field)

#### E6. OTLP `unit` field population

Set `unit` field using UCUM c/s variant on metrics with known units:
- `host_metrics`: `By` (bytes), `s` (seconds), `1` (CPU ratios/percentages)
- `apache_metrics`: `s` (uptime), `By` (bytes), `{connection}`, `{request}`
- `nginx_metrics`: `{connection}`, `{request}`
- `statsd`: `s` or `ms` (timers, per ConversionUnit config)
- `internal_metrics`: per-metric (`By`, `s`, `{event}`, etc.)
- Other sources: best-effort from metric name suffix (`_bytes`→`By`, `_seconds`→`s`, `_total`→`{<name>}`)
- Add `with_unit(unit: impl Into<String>)` method on OtelMetric

#### E7. Resource/Scope → sink labels/tags (Tier 1 sinks)

Enable non-OTLP sinks to propagate Resource attributes as metric labels:
- **Prometheus**: `resource_to_labels = ["service.name", "host.name"]` (configurable, these defaults)
- **InfluxDB**: `resource_to_tags = ["service.name", "host.name"]` (configurable, these defaults)
- **StatsD**: flatten selected Resource attributes into DogStatsD tags (same config pattern)
- Read Resource proto at sink boundary, extract configured keys, inject as data point attributes/tags before encoding

#### E8. Eliminate ALL `vector.*` custom attributes

D40(c): delete all `vector.*` attributes and their infrastructure.

- Delete `vector.metric_type` attribute from `new_distribution_from_samples()` (already deleted in E4) and `new_set_from_values()` (already deleted in E4)
- Delete `vector.metric_kind` attribute from `new_gauge_delta()` (already deleted in E4)
- Delete `vector.set_values` attribute from `new_set_from_values()` (already deleted in E4)
- Delete `vector.statistic` attribute — sinks now branch on `MetricView::Summary` vs `MetricView::Histogram` proto types directly
- Delete `distribution_statistic()` method and `is_distribution_summary()` from OtelMetric
- Delete `VECTOR_STATISTIC`, `VECTOR_METRIC_TYPE`, `VECTOR_METRIC_KIND`, `VECTOR_SET_VALUES` constants
- Update statsd sink encoder: remove `distribution_statistic()` call, match on `MetricView::Histogram` directly
- Update prometheus collector: remove `distribution_statistic()` call, `MetricView::Distribution` arm deleted
- Update influxdb metrics: remove `distribution_statistic()` call, `MetricView::Distribution` arm deleted
- Verify: `grep -rn 'vector\.\(statistic\|metric_type\|metric_kind\|set_values\)' lib/ src/` returns 0

#### E9. Delete `MetricView::Distribution` variant

The `Distribution` variant represented the broken raw-samples-as-explicit-bounds encoding. With E4 (proper aggregation) and E8 (no `vector.*` attributes), it has no producers and no consumers.

- Delete `MetricView::Distribution` from the enum
- Delete all match arms for `Distribution` in sinks, transforms, tests
- Delete `Sample` struct if no longer used (verify — may still be used in Prometheus `samples_to_buckets`)
- **Sol advantage over otelcontribcol**: otelcontribcol drops timers/histograms/distributions by default (`observer_type="disabled"`). Sol processes all of them with proper ExponentialHistogram aggregation.

### Phase F — Structural Field Migration (eliminate remaining `vector.*` attributes)

**Status: DONE. All `vector.*` attributes eliminated. Completed in single commit (D53).**

Phase F replaced the remaining `vector.*` data-point attributes with dedicated struct fields on `OtelMetric`. Zero `vector.*` attributes exist anywhere in the codebase.

#### F1. D53 — `set_values: Option<BTreeSet<String>>` field (replace `vector.set_values` + `vector.metric_type=set`)

Add a dedicated `set_values` field to `OtelMetric` for set merge semantics. `BTreeSet` deduplicates by construction. The Gauge numeric value = `set_values.len()`, recomputed on read. The field is never serialized to OTLP proto — it is pipeline-internal state.

- Add `set_values: Option<BTreeSet<String>>` to `OtelMetric` struct
- Rewrite `new_set_from_values()`: populate field, create `Gauge(cardinality)`, no `vector.*` attrs
- Rewrite `is_set()`: check `self.set_values.is_some()`
- Rewrite `view()` Set arm: read from `self.set_values` instead of dp_attrs
- Rewrite `merge_set_values()`: BTreeSet union
- Rewrite `subtract_set_values()`: BTreeSet difference
- Update Arbitrary impl, Lua bridge, aggregate transform
- Delete `VECTOR_SET_VALUES` constant

#### F2. `kind_override: Option<MetricKind>` field (replace `vector.metric_kind`)

Add a dedicated `kind_override` field to `OtelMetric` for Gauge/Summary types that lack `aggregation_temporality` in OTLP. Pipeline-internal state, not serialized to proto.

- Add `kind_override: Option<MetricKind>` to `OtelMetric` struct
- Rewrite `new_gauge_delta()`: set `kind_override = Some(Incremental)` instead of attr
- Rewrite `kind()`: for Gauge/Summary, check `self.kind_override` instead of `VECTOR_METRIC_KIND` attr
- Rewrite `set_kind()`: for Gauge/Summary, set `self.kind_override` instead of attr
- Update `new_set_from_values` (from F1): set `kind_override` for incremental sets
- Delete `VECTOR_METRIC_KIND`, `VECTOR_METRIC_TYPE` constants

#### F3. Delete `VECTOR_PREFIX` and tag filtering

- Delete `VECTOR_PREFIX` constant
- Remove `vector.*` prefix filtering from `tags()` and `all_tags_including_resource()` — dead code, no `vector.*` attrs exist

### Gate (Phase E+F) — ALL PASSED ✅

**F gate metrics — all verified 2026-05-02:**

| Metric | Target | Result |
|--------|--------|--------|
| `VECTOR_*` constants in otel_fields.rs | 0 | ✅ 0 |
| `vector.*` prefix filtering in tags() | 0 | ✅ 0 — filtering removed |
| `new_set_from_values` uses `vector.*` attrs | 0 | ✅ 0 — uses `set_values: Option<BTreeSet<String>>` field |
| `new_gauge_delta` uses `vector.*` attrs | 0 | ✅ 0 — uses `kind_override: Option<MetricKind>` field |

| Metric | Target | Verification |
|--------|--------|--------------|
| Tests passing | ≥ previous | `cargo test -p vector --all-features` |
| ExponentialHistogram in prometheus sink | Converted to Histogram (not silently dropped) | Integration test |
| StatsD → Prometheus roundtrip | ExpHist emitted by source, Histogram exposed by sink | End-to-end test |
| StatsD flush aggregation | 1000 packets → 1 data point per metric per interval | Unit test |
| StatsD gauge persistence | `100\|g` then (next flush) `+5\|g` → Gauge(105) | Unit test |
| Resource attributes on host_metrics | `service.name=sol/host_metrics` + `host.name` present | Unit test |
| InstrumentationScope on host_metrics | `name=sol/host_metrics` | Unit test |
| Temporality: Delta counter → Prometheus | Accumulated to Cumulative | Unit test |
| `time_unix_nano` on StatsD metrics | Non-zero, with `start_time_unix_nano` | Unit test |
| `unit` field on host_metrics bytes | `"By"` | Unit test |
| `vector.*` attribute references | 0 | `grep -rn 'vector\.\(statistic\|metric_type\|metric_kind\|set_values\)' lib/ src/` |
| `distribution_statistic` references | 0 | `grep -rn distribution_statistic lib/ src/` |
| `MetricView::Distribution` references | 0 | `grep -rn 'Distribution' lib/vector-core/src/event/otel_metric.rs` |
| Summary production from StatsD | 0 | Unit test: `d` type → ExponentialHistogram not Summary |
| Gauge delta (StatsD `+N\|g`) | Absolute Gauge (stateful accumulation) | Unit test |
| Bare tags | `AnyValue { value: Some(StringValue("")) }` | Unit test |
| `is_monotonic` default | `false` | Unit test |
| DogStatsD container ID | `c:abc123` → `container.id=abc123` attribute | Unit test |

---

## Principles

1. **OTLP/OTel is the only core protocol.** No vendor types in core.
2. **Two-format rule.** OTLP/proto or OTLP/JSON only. No flat canonical format.
3. **Vendor logic in adapters only.** Core never depends on adapters.
4. **`vector.*` attributes are acceptable.** They encode Vector concepts OTLP lacks. Never injected on passthrough paths.
5. **Features preserved.** Tail sampling, load balancing, span_metrics, aggregate — all OTel-native.
6. **Original Vector protocol supported at source boundary.** The `vector` source speaks only the original native gRPC protocol (`event.proto`/`vector.proto`) for backward compatibility. OTLP ingestion is handled by the `opentelemetry` source. The native proto definitions live in the source scope — adapter code, not core types.

---

## Architecture

Sol is a true fork of Vector, rebuilt with OpenTelemetry as its native protocol. The original Vector's proprietary types are gone from core — but Sol retains the original Vector wire protocol as a source adapter, so existing Vector fleets can send data to Sol without any changes.

```
Sources (adapters)              Core (OTel-native)                    Sinks (adapters)
──────────────────────────────  ────────────────────────────────────  ───────────────────────
opentelemetry (gRPC + HTTP)     OtelLog  (LogRecord)                  opentelemetry (gRPC+HTTP)
datadog_agent ──────────────►   OtelMetric (Sum/Gauge/Histogram/  ──► prometheus, influxdb
vector (native gRPC) ─────►     ExponentialHistogram/Summary)   ──► kafka, loki, ES, …
kafka, syslog, … ──────────►   OtelSpan (Span)
                                OtelAttributes (BTreeMap wrapper)
                                Disk buffer: otlp_buffer.proto
```

### What Sol changes from the original Vector

| Aspect | Original Vector | Sol |
|--------|----------------|-----|
| **Core event model** | Proprietary types (`LogEvent`, `Metric`, `TraceEvent`) | OTel-native (`OtelLog`, `OtelMetric`, `OtelSpan`) |
| **Wire protocol** | Custom `event.proto` / `vector.proto` | OTLP (proto + JSON) — the standard |
| **OTLP support** | Partial (source + sink, but not core) | Native — OTLP IS the core |
| **Vendor types in core** | DD sketches, `MetricValue`, `StatisticKind` | None — vendor logic in adapters only |
| **`vector` sink** | Custom proto → another Vector | Deleted — use `opentelemetry` sink |
| **`vector` source** | Custom proto only | Dual-protocol: OTLP + original native gRPC on same port |
| **`opentelemetry` source** | Exists in original | OTLP gRPC + HTTP ingestion (separate from `vector` source) |

### Vector Source: Original Native Protocol

The `vector` source speaks **both OTLP and the original Vector native gRPC protocol** on the same port:

- **OTLP**: LogsService, MetricsService, TraceService (for OTel Collector / Sol / any OTLP client)
- **Native Vector**: `service Vector { rpc PushEvents(...) }` (for legacy Vector instances)
- Native proto events are converted at the source boundary: `event.Log` → `OtelLog`, `event.Metric` → `OtelMetric`, `event.Trace` → `OtelSpan`
- The proto definitions live in `proto/vector/event.proto` and `proto/vector/vector.proto` — not in core

The `opentelemetry` source also handles OTLP (gRPC + HTTP) as a dedicated ingestion path.

```
Original Vector ── vector sink (native proto) ──► vector source ──────► Core (OtelLog,
                                                                              OtelMetric,
OTel Collector  ── otlp exporter ──────────────► opentelemetry source ─► OtelSpan)

Sol / any OTLP  ── opentelemetry sink ─────────► opentelemetry source ─►
```

There is **no `vector` sink** — it was a redundant wrapper around `opentelemetry`. To send data to another Sol instance (or any OTLP-compatible receiver), use `type = "opentelemetry"` directly.

### Why keep the original protocol?

The original Vector has **partial** OTLP support — not all versions ship an `opentelemetry` sink, and the OTLP support that exists may not cover all signal types. Supporting the native protocol at the source means:
- **Zero-config migration**: existing Vector fleets can point their `vector` sink at this fork without changing anything
- **No bridge needed**: no OTel Collector middlebox, no sink reconfiguration on the sender side
- **Adapter-scoped complexity**: the `event.proto` / `vector.proto` definitions and conversion logic live entirely within `src/sources/vector/` — core remains pure OTLP
