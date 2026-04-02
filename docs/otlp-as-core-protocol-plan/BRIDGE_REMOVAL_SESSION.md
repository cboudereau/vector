# Bridge Removal Session Task

## Progress (updated 2026-04-01)

No OtelLog or OtelSpan methods delegate to `to_log_event()` anymore.
OtelMetric's `value()`, `kind()`, `tag_value()` extract directly from proto.
Only `Serialize` for OtelMetric still uses `to_legacy_metric()` (wire format risk).

### Done — OtelLog (all bridge delegation removed)

- `get()`, `insert()`, `remove()` — use `to_value_legacy_layout()`
- `get_source_type()`, `get_host()` — direct resource attribute lookup
- `get_timestamp()` — direct proto + overflow attr + Vector namespace meaning
- `get_by_meaning()` — direct schema definition lookup
- `namespace()` — direct metadata check (no bridge)
- `parse_path_and_get_value()`, `convert_to_fields()`, `rename_key()`
- `try_insert()`, `value()`, `keys()`, `is_empty_object()`
- `as_map()`, `value_mut()`, `convert_to_fields_unquoted()`
- `timestamp_path()`, `host_path()`, `source_type_path()`, `body_path()`
- `Serialize` impl — serializes `to_value_legacy_layout()` directly
- `EventDataEq` impl — compares `to_value_legacy_layout()` directly
- `convert_to_fields()` / `convert_to_fields_unquoted()` — use `all_fields` / `all_fields_unquoted` for proper nested flattening

### Done — OtelSpan (all bridge delegation removed)

- `get()`, `insert()`, `contains()` — use `to_value_legacy_layout()` / `apply_value_legacy_layout()`
- `as_map()` — direct from `to_value_legacy_layout()`
- `parse_path_and_get_value()` — parses path and delegates to `get()`
- `Serialize` impl — serializes `to_value_legacy_layout()` directly
- `EventDataEq` impl — compares `to_value_legacy_layout()` directly
- `to_log_event()` / `to_trace_event()` kept for external callers

### Done — OtelMetric (value/kind/tag_value debridged)

- `value()` — `extract_kind_and_value()` from proto (no Metric construction)
- `kind()` — `extract_kind_and_value()` from proto (no Metric construction)
- `tag_value()` — searches resource, scope, and data point attributes directly
- `Serialize` impl — still uses `to_legacy_metric()` (changing wire format is risky; deferred)

### Fixed test failures

- `template::tests::render_log_timestamp_strftime_style_namespace` — added `get_by_meaning` + `coerce_to_timestamp` for Vector namespace
- `sinks::influxdb::logs::tests::test_encode_nested_fields` — `convert_to_fields` now uses `all_fields` for nested flattening
- `sinks::new_relic::tests::generates_event_api_model_with_dotted_fields` — same fix via `all_fields_unquoted`
- `event::otel_event::tests::otel_log_serializes_as_structured_json` — updated assertions for legacy layout format
- `event::merge_state::test::log_event_merge_state_example` — updated merge field from "message" to "body"
- `event::test::serialization::serialization` — updated expected field from "message" to "body"

## Still blocked — cannot remove yet

### Bridge function bodies (46+ external callers)

`to_log_event()` and `from_log_event()` are called from 46 files (codecs, sinks,
transforms, sources). Each caller needs migration to direct proto access.

`to_legacy_metric()` and `from_legacy_metric()` are called from 24 files.

### Legacy types (LogEvent, Metric, TraceEvent)

Still have many consumers. Blocked on bridge function body removal.

## Next steps

1. Migrate external `to_log_event()` callers (46 files) to use OtelLog proto directly
2. Migrate external `to_legacy_metric()` callers (24 files) to use OtelMetric proto directly
3. Replace `to_value_legacy_layout()` callers with direct proto accessors (~40 files)
4. Remove `to_value_legacy_layout()` itself
5. Remove bridge function bodies
6. Remove `LogEvent`, `Metric`, `TraceEvent` types

## Context docs

- `docs/otlp-as-core-protocol-plan/LEGACY_BRIDGE_REMOVAL.md` — design discovery
- `docs/otlp-as-core-protocol-plan/VRL_OTEL_NATIVE_TARGETS.md` — VRL path model

## Key insight

`TargetPath` (VRL's abstract path type) is only needed inside VRL transforms.
Outside VRL, callers should use proto accessors directly. The `get()`/`insert()`
methods now build a Value tree inline via `to_value_legacy_layout()` — same
layout as `to_log_event()` but without constructing a LogEvent.

## Verification

- `cargo test -p vector --lib -- --skip throttle` — all tests pass
- `cargo test -p vector-core --lib` — all event tests pass
- `cargo check -p vector-core` — compiles clean
