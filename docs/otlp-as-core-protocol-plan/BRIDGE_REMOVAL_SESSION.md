# Bridge Removal Session Task

## Progress from previous session

Most OtelLog methods no longer call `to_log_event()`. They use
`to_value_legacy_layout()` instead — builds the same Value tree directly
from proto fields without constructing a LogEvent.

### Done (OtelLog methods no longer using to_log_event)

- `get()`, `insert()`, `remove()` — use `to_value_legacy_layout()`
- `get_source_type()`, `get_host()` — direct resource attribute lookup
- `get_timestamp()` — direct proto + Vector namespace meaning fallback
- `parse_path_and_get_value()`, `convert_to_fields()`, `rename_key()`
- `try_insert()`, `value()`, `keys()`, `is_empty_object()`
- `as_map()`, `value_mut()`, `convert_to_fields_unquoted()`
- `timestamp_path()`, `host_path()`, `source_type_path()`, `body_path()`

### Still using to_log_event() — FIX THESE

In `lib/vector-core/src/event/otel_event.rs`:
- `namespace()` (line ~885) — reads LogNamespace from schema definition
- `get_timestamp()` Vector namespace fallback (line ~832) — resolves meaning
- `Serialize` impl (line ~2275) — serializes via to_log_event()
- `EventDataEq` impl (line ~2255) — compares via to_log_event()

In OtelSpan:
- `get()` (line ~1266) — delegates to to_log_event()
- `insert()` (line ~1275) — round-trips through to_log_event()
- `as_map()` (line ~1290) — delegates to to_log_event()
- All other bridge methods

In OtelMetric:
- `to_legacy_metric()` (line ~1727) — 263 lines, used by metric sinks/transforms
- `from_legacy_metric()` (line ~1295) — 253 lines, used by Event::from(Metric)

### 3 pre-existing test failures (NOT caused by bridge removal)

- `template::tests::render_log_timestamp_strftime_style_namespace`
- `sinks::influxdb::logs::tests::test_encode_nested_fields`
- `sinks::new_relic::tests::generates_event_api_model_with_dotted_fields`

These existed before bridge removal. Root cause: Phase B+C `.message` → `.body`
migration changed how `from_str_legacy` sets fields, affecting timestamp
resolution and nested/dotted field encoding.

## Goal for this session

1. Fix the 3 pre-existing test failures
2. Remove remaining `to_log_event()` calls from OtelLog (namespace, Serialize, EventDataEq)
3. Remove `to_log_event()` from OtelSpan
4. Remove `to_legacy_metric()` / `from_legacy_metric()` from OtelMetric
5. Remove `to_log_event()` / `from_log_event()` function bodies entirely
6. Remove `LogEvent`, `Metric`, `TraceEvent` types if no consumers remain

## Context docs

- `docs/otlp-as-core-protocol-plan/LEGACY_BRIDGE_REMOVAL.md` — design discovery
- `docs/otlp-as-core-protocol-plan/VRL_OTEL_NATIVE_TARGETS.md` — VRL path model

## Key insight

`TargetPath` (VRL's abstract path type) is only needed inside VRL transforms.
Outside VRL, callers should use proto accessors directly. The `get()`/`insert()`
methods now build a Value tree inline via `to_value_legacy_layout()` — same
layout as `to_log_event()` but without constructing a LogEvent.

Next step: replace `to_value_legacy_layout()` callers with direct proto accessors
(caller by caller, ~40 files). Then remove `to_value_legacy_layout()` itself.

## Verification

- `cargo test -p vector --lib -- --skip throttle` — all tests pass
- No hanging tests (monitor for 60s warnings)
- `cargo check -p vector-core` — no legacy types in core (final step)
