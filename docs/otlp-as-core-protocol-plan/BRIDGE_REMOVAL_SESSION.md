# Bridge Removal Session Task

## Goal

Remove the legacy bridge functions from OtelLog, OtelSpan, and OtelMetric.
Replace all 465 callers of `get()`/`insert()`/`remove()` with direct proto accessors.

## Context

Read these docs first:
- `docs/otlp-as-core-protocol-plan/LEGACY_BRIDGE_REMOVAL.md` — design discovery
- `docs/otlp-as-core-protocol-plan/VRL_OTEL_NATIVE_TARGETS.md` — VRL path model

## What to remove

Bridge functions in `lib/vector-core/src/event/otel_event.rs`:
- `OtelLog::to_log_event()` (~85 lines)
- `OtelLog::from_log_event()` (~71 lines)
- `OtelSpan::to_log_event()` (~63 lines)
- `OtelMetric::to_legacy_metric()` (~263 lines)
- `OtelMetric::from_legacy_metric()` (~253 lines)

## How to replace callers

For each call site, replace with direct proto accessor:

```rust
// BEFORE (bridge)
log.get("body")                    → log.body().map(any_value_to_vrl)
log.get("timestamp")               → log.get_timestamp()
log.get("source_type")             → log.get_source_type()
log.get("some_attr")               → log.attribute("some_attr").map(any_value_to_vrl)
log.insert("key", value)           → log.set_attribute("key", vrl_value_to_any_value(&value))
log.insert("body", value)          → log.set_body(vrl_value_to_any_value(&value))

metric.to_legacy_metric().name()   → metric.name()
metric.to_legacy_metric().tags()   → metric.first_data_point_attributes()
metric.to_legacy_metric().value()  → metric.metric().data (match on variant)
```

## Approach

Work file-by-file. After each file, run `cargo check -p vector --lib`.
After each group of ~5 files, run `cargo test -p vector --lib -- --skip throttle`.
Monitor for hangs (tests running > 60s).

## Order

1. Start with `lib/vector-core/src/event/otel_event.rs` — remove bridge method
   bodies but keep the method signatures (make them call the proto accessors)
2. Fix `lib/codecs/` callers (~45)
3. Fix `src/sinks/` callers (~30)
4. Fix `src/transforms/` callers (~20)
5. Fix `src/sources/` callers (~40)
6. Fix remaining callers (API, test_util, etc.)
7. Remove the `LogEvent`, `Metric`, `TraceEvent` types if no consumers remain

## Key constraint

Do NOT change `get()`'s TargetPath signature. Instead, have `get()` build a
Value internally (same layout as `to_log_event()`) and traverse it. The
difference from the previous attempt: keep the SAME Value layout as
`to_log_event()` to avoid breaking callers. Then migrate callers one by one
to not use `get()` at all.

## Verification

- `cargo test -p vector --lib -- --skip throttle` — all tests pass
- No hanging tests (monitor for 60s warnings)
- `cargo check -p vector-core` — no legacy types in core (final step)
