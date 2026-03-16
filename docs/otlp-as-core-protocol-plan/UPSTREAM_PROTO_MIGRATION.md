# Migrate to Upstream `opentelemetry-proto` Crate + OTLP HTTP JSON Support

Companion plan to `CONSOLIDATED_MIGRATION_PLAN.md`. Covers the replacement of
Vector's two local proto crates with the upstream `opentelemetry-proto` Rust
crate and the addition of OTLP HTTP JSON ingestion support.

---

## Motivation

Vector currently maintains **two** local crates generated from the same
`.proto` files:

| Crate | Purpose |
|---|---|
| `lib/opentelemetry-proto` | prost codegen + `build.rs`, gRPC service stubs, conversion logic (`logs.rs`, `spans.rs`, `metrics.rs`, `buffer_codec.rs`, `common.rs`) |
| `lib/otel-proto-types` | Separate prost codegen (no gRPC stubs), used by `OtelLog` / `OtelSpan` / `OtelMetric` in `vector-core` |

This dual-crate structure exists because `vector-core` cannot depend on
`opentelemetry-proto` (circular dependency). The two crates generate
**identical protobuf wire types** from the same `.proto` sources, but with
different Rust module paths. Converting between them requires
encode→decode round-trips (see `proto_convert_*` functions in `logs.rs`,
`spans.rs`, `metrics.rs`).

### Problems

1. **Redundant codegen** — same `.proto` files compiled twice.
2. **Encode→decode round-trips** — wasteful and fragile; relies on the
   assumption that both crates stay in sync.
3. **No serde support** — neither crate generates `serde::Serialize` /
   `serde::Deserialize` impls, so OTLP HTTP JSON (`Content-Type:
   application/json`) cannot be accepted by the `opentelemetry` source.
4. **Maintenance burden** — any OTLP proto update must be applied to both
   crates manually.

### Solution

Replace both crates with the upstream
[`opentelemetry-proto`](https://crates.io/crates/opentelemetry-proto) crate
(maintained by the OTel Rust SIG). Its `with-serde` feature provides
OTLP-spec-compliant JSON serialization out of the box, including:

- **Hex-encoded `traceId` / `spanId` / `parentSpanId`** (OTLP spec deviation
  from proto3 canonical base64)
- **String-encoded `fixed64` timestamps** (proto3 JSON mapping)
- **Custom `AnyValue` deserializer** (handles `intValue` as string or number)
- **`#[serde(rename_all = "camelCase")]`** on all message types
- **`#[serde(default)]`** for missing fields

---

## Prerequisite: prost / tonic version upgrade

| Dependency | Vector current | Upstream requires |
|---|---|---|
| `prost` | 0.12 | ≥ 0.13 |
| `tonic` | 0.11 | ≥ 0.12 |

The upstream crate's oldest version with `with-serde` support (v0.7.0)
already requires `prost ^0.13` and `tonic ^0.12`. No compatible version
exists for Vector's current dependency set.

**The prost/tonic upgrade is a hard prerequisite.**

---

## Current dependency graph

```
vector (bin)
├── vector-lib
│   └── opentelemetry-proto          ← local crate (prost codegen + conversion logic)
│       ├── proto.rs                  ← tonic::include_proto! + DESCRIPTOR_BYTES
│       ├── common.rs                 ← PBValue → vrl::Value, kv_list_into_value, to_hex
│       ├── logs.rs                   ← ResourceLogs::into_event_iter / into_otel_event_iter
│       ├── spans.rs                  ← ResourceSpans::into_event_iter / into_otel_event_iter
│       ├── metrics.rs                ← ResourceMetrics::into_event_iter / into_otel_event_iter
│       ├── buffer_codec.rs           ← OtlpCodec: EventArray ↔ protobuf for disk buffers
│       └── build.rs                  ← tonic_build::configure().compile()
│
├── vector-core
│   └── otel-proto-types              ← local crate (prost codegen only, no gRPC stubs)
│       └── event/otel_event.rs       ← OtelLog, OtelSpan, OtelMetric wrappers
│
└── sources/opentelemetry/http.rs     ← hardcoded Content-Type: application/x-protobuf
```

Types flow: `opentelemetry-proto` types → encode→decode → `otel-proto-types` types → `OtelLog` / `OtelSpan` / `OtelMetric`.

---

## Target dependency graph

```
vector (bin)
├── vector-lib
│   └── opentelemetry-proto (upstream crate, with-serde feature)
│       └── tonic::* types with Serialize + Deserialize impls
│
├── vector-core
│   └── depends on upstream opentelemetry-proto directly
│       └── event/otel_event.rs uses upstream types (no roundtrip)
│
├── lib/vector-otel-adapter           ← renamed from lib/opentelemetry-proto
│   ├── common.rs                     ← PBValue → vrl::Value (updated import paths)
│   ├── logs.rs                       ← into_event_iter / into_otel_event_iter (no roundtrip)
│   ├── spans.rs                      ← into_event_iter / into_otel_event_iter (no roundtrip)
│   ├── metrics.rs                    ← into_event_iter / into_otel_event_iter (no roundtrip)
│   └── buffer_codec.rs              ← OtlpCodec (updated import paths)
│
└── sources/opentelemetry/http.rs     ← accepts both protobuf and JSON
```

---

## Phased execution plan

### Phase 1 — Upgrade prost / tonic

**Goal**: Bring Vector's prost/tonic versions in line with the upstream crate's
requirements.

**Steps**:

1. Update workspace `Cargo.toml`:
   - `prost`: `0.12` → `0.13`
   - `prost-build`: match prost version
   - `prost-reflect`: match prost version (currently 0.14, may need update)
   - `tonic`: `0.11` → `0.12`
   - `tonic-build`: match tonic version
2. Update all `build.rs` files for API changes between versions.
3. Fix any compile errors from prost 0.12→0.13 API changes (e.g.,
   `Message::encode` signature, `include_proto!` macro changes).
4. Fix any compile errors from tonic 0.11→0.12 API changes (e.g.,
   service trait signatures, transport layer changes).
5. Run full test suite, fix regressions.

**Risk**: High — prost and tonic are foundational; changes cascade across every
crate that uses protobuf or gRPC (internal proto types, DataDog agent proto,
gRPC sources/sinks, etc.).

**Output**: All tests pass with prost 0.13+ / tonic 0.12+.

---

### Phase 2a — Add upstream `opentelemetry-proto` dependency

**Goal**: Make upstream types available alongside the local crates (temporary
dual state).

**Steps**:

1. Add to workspace `Cargo.toml`:
   ```toml
   opentelemetry-proto-upstream = { package = "opentelemetry-proto", version = "0.31", default-features = false, features = ["gen-tonic-messages", "with-serde", "logs", "metrics", "trace"] }
   ```
   Use a rename to avoid collision with the local crate during transition.
2. Add the dependency to `lib/vector-core/Cargo.toml`.
3. Verify it compiles alongside the existing local crates.

**Risk**: Low — additive change, no existing code modified.

---

### Phase 2b — Migrate `otel_event.rs` to upstream types

**Goal**: `OtelLog`, `OtelSpan`, `OtelMetric` use upstream proto types
directly instead of `otel-proto-types`.

**Steps**:

1. In `lib/vector-core/src/event/otel_event.rs`, replace all
   `otel_proto_types::*` imports with the upstream crate's equivalents:
   - `otel_proto_types::logs::v1::LogRecord` → `opentelemetry_proto::tonic::logs::v1::LogRecord`
   - `otel_proto_types::trace::v1::Span` → `opentelemetry_proto::tonic::trace::v1::Span`
   - `otel_proto_types::metrics::v1::Metric` → `opentelemetry_proto::tonic::metrics::v1::Metric`
   - `otel_proto_types::common::v1::*` → `opentelemetry_proto::tonic::common::v1::*`
   - `otel_proto_types::resource::v1::Resource` → `opentelemetry_proto::tonic::resource::v1::Resource`
2. Update `lib/vector-core/Cargo.toml` — remove `otel-proto-types`, add
   upstream crate.
3. Fix all downstream compile errors (mostly import path changes in
   `is_log.rs`, `is_metric.rs`, `is_trace.rs`, `vrl_target.rs`, condition
   modules, codecs).

**Risk**: Medium — large surface area but purely mechanical import path changes.

---

### Phase 2c — Migrate `lib/opentelemetry-proto` to use upstream types

**Goal**: The local adapter crate re-exports upstream types instead of running
its own codegen. Eliminate all encode→decode round-trips.

**Steps**:

1. **Delete `build.rs`** — no more local protobuf compilation.
2. **Delete `proto.rs`** — no more `tonic::include_proto!` calls.
3. **Delete `src/proto/` directory** — the `.proto` source files and generated
   code are no longer needed (upstream crate provides them).
4. **Update `Cargo.toml`** — remove `prost-build`, `tonic-build`, `glob` build
   dependencies. Add upstream `opentelemetry-proto` as a regular dependency.
5. **Add a `proto.rs` re-export module**:
   ```rust
   pub use opentelemetry_proto::tonic::*;
   // Re-export DESCRIPTOR_BYTES if still needed for the protobuf codec
   ```
6. **Update `common.rs`** — change `use super::proto::common::v1::*` to use
   upstream paths.
7. **Update `logs.rs`** — change all proto type imports. **Remove
   `proto_convert_resource`, `proto_convert_scope`, `proto_convert_log_record`
   functions** — since both the conversion code and `OtelLog` now use the same
   upstream types, no round-trip is needed. The `into_otel_event_iter` method
   directly constructs `OtelLog::from_parts(log_record, resource, scope, ...)`.
8. **Update `spans.rs`** — same as logs: remove `proto_convert_*` functions,
   directly construct `OtelSpan::from_parts(span, resource, scope, ...)`.
9. **Update `metrics.rs`** — same: remove `proto_convert_*` functions,
   directly construct `OtelMetric::from_parts(metric, resource, scope, ...)`.
10. **Update `buffer_codec.rs`** — remove the `transcode` helper function and
    all its call sites. OTel-native events (`otel_logs_to_export`,
    `otel_metrics_to_export`, `otel_spans_to_export`) can directly use the
    record/span/metric without re-encoding, since the types are now identical.

**Risk**: Medium — this is the core of the migration. Each file needs careful
type path updates, but the logic simplification (removing round-trips) reduces
code rather than adding it.

---

### Phase 2d — Delete `otel-proto-types` and update all consumers

**Goal**: Remove the redundant crate entirely. Update all remaining import
paths across the codebase.

**Files to update** (non-exhaustive, based on grep):

| File | Change |
|---|---|
| `lib/otel-proto-types/` | **Delete entire crate** |
| `lib/vector-core/Cargo.toml` | Remove `otel-proto-types` dependency |
| `lib/opentelemetry-proto/Cargo.toml` | Remove `otel-proto-types` dependency |
| `src/sources/opentelemetry/grpc.rs` | Update `vector_lib::opentelemetry::proto` paths |
| `src/sinks/opentelemetry/grpc.rs` | Update proto imports, remove `SinkScope` / `SinkResource` aliases (types are now unified) |
| `lib/codecs/src/encoding/format/otlp.rs` | Update `opentelemetry_proto::proto` → upstream paths |
| `lib/codecs/src/decoding/format/otlp.rs` | Update proto paths |
| `lib/vector-lib/src/lib.rs` | Update `opentelemetry` re-export module |
| `lib/vector-lib/Cargo.toml` | Update `opentelemetry-proto` dependency |
| `src/sources/opentelemetry/tests.rs` | Update proto imports |
| `src/sources/opentelemetry/integration_tests.rs` | Update proto imports |
| `tests/e2e/opentelemetry/**` | Update proto imports |
| `src/conditions/is_log.rs` | Verify `otel_proto_types` references removed |
| `src/conditions/is_metric.rs` | Same |
| `src/conditions/is_trace.rs` | Same |
| `lib/vector-core/src/event/vrl_target.rs` | Same |
| `lib/codecs/src/encoding/format/{text,raw_message,logfmt,native}.rs` | Same |

**Risk**: Low — mechanical find-and-replace. Large diff but no logic changes.

---

### Phase 3 — OTLP HTTP JSON support

**Goal**: The `opentelemetry` source accepts `Content-Type: application/json`
in addition to `application/x-protobuf` on its HTTP endpoint.

**Prerequisite**: Phases 1–2 complete (upstream types have serde impls).

**Steps**:

1. In `src/sources/opentelemetry/http.rs`, modify `build_ingest_filter`:
   - Replace `warp::header::exact_ignore_case("content-type", "application/x-protobuf")`
     with a filter that extracts the content-type header value.
   - Pass the content-type string into the `make_events` closure.

2. In each `decode_*_body` function, branch on content type:
   ```rust
   fn decode_log_body(body: Bytes, content_type: &str, ...) -> Result<Vec<Event>, ErrorMessage> {
       let request = match content_type {
           "application/x-protobuf" => ExportLogsServiceRequest::decode(body).map_err(emit_decode_error)?,
           "application/json" => serde_json::from_slice(&body).map_err(emit_decode_error)?,
           _ => return Err(emit_decode_error("unsupported content type")),
       };
       // ... rest unchanged
   }
   ```

3. For `handle_request` responses, branch on content type to return either
   protobuf or JSON response body (the OTLP spec requires matching the
   request content type in the response).

4. Update the demo (`demo/otel-drop-in/`) — remove the `otel-to-vector`
   workaround service and test Vector directly with JSON payloads.

5. Add integration tests that send OTLP JSON payloads and verify correct
   parsing for all three signals (logs, metrics, traces).

**Risk**: Low — ~50-100 lines of code in `http.rs`.

---

## OTLP JSON compatibility notes

Analysis of the demo JSON files against the upstream crate's serde
implementation:

| Field pattern | OTLP JSON format | Upstream serde handling | Status |
|---|---|---|---|
| `traceId`, `spanId`, `parentSpanId` | hex string | `#[serde(deserialize_with = "deserialize_from_hex_string")]` | Handled |
| `fixed64` timestamps (`timeUnixNano`, etc.) | string `"154471..."` | `#[serde(deserialize_with = "deserialize_string_to_u64")]` | Handled |
| Enums (`kind`, `aggregationTemporality`, `severityNumber`) | integer | Proto3 JSON allows both string and integer | Handled |
| `AnyValue.intValue` | string `"10"` | Custom `AnyValue` deserializer accepts string or int | Handled |
| `fixed64` counts (`count`, `bucketCounts`) | bare number `2` | `deserialize_string_to_u64` — **needs verification** | To verify |
| `double` fields (`asDouble`, `min`, `max`) | bare number | Standard serde `f64` | Handled |
| camelCase field names | `resourceSpans`, `scopeLogs`, etc. | `#[serde(rename_all = "camelCase")]` | Handled |
| Missing fields | omitted from JSON | `#[serde(default)]` | Handled |

The only potential issue is `fixed64` fields sent as bare JSON numbers instead
of strings. The OTLP spec says implementations SHOULD accept both, and the
upstream crate's custom `deserialize_string_to_u64` may or may not handle bare
numbers. This must be verified during Phase 3 and a small fix applied to the
upstream serializer helper if needed.

---

## Validation gates

| Phase | Gate |
|---|---|
| 1 | `cargo build --workspace` and `cargo test --workspace` pass with new prost/tonic |
| 2a | `cargo build --workspace` passes with upstream crate added |
| 2b | `OtelLog`/`OtelSpan`/`OtelMetric` unit tests pass with upstream types |
| 2c | `opentelemetry-proto` adapter crate tests pass, no encode→decode round-trips remain |
| 2d | `cargo build --workspace` clean, `otel-proto-types` directory deleted, full test suite passes |
| 3 | Demo OTLP JSON payloads (logs, metrics, traces) accepted by Vector's HTTP source; integration tests pass |

---

## Files deleted by this migration

| Path | Reason |
|---|---|
| `lib/otel-proto-types/` | Entire crate — replaced by upstream |
| `lib/opentelemetry-proto/build.rs` | No more local codegen |
| `lib/opentelemetry-proto/src/proto.rs` | Replaced by upstream re-export |
| `lib/opentelemetry-proto/src/proto/` | `.proto` sources and generated code no longer needed |

## Estimated line count changes

| Category | Added | Removed | Net |
|---|---|---|---|
| Phase 1 (prost/tonic upgrade) | ~100 | ~100 | ~0 |
| Phase 2 (upstream migration) | ~200 | ~500 | **-300** |
| Phase 3 (HTTP JSON support) | ~100 | ~10 | +90 |
| **Total** | ~400 | ~610 | **-210** |

The migration is a net reduction in code thanks to eliminating the dual-crate
structure and encode→decode round-trips.
