# Deferred Items & Known Issues (2026-04-17 session)

Items identified during this session that were not fixed, either by
design (trade-off) or because they're out of scope for the current
migration phase.

## 1. Code Review Findings (from earlier in session)

### CRITICAL: `Deserialize` impls always fail (T17)

**File:** `lib/vector-core/src/event/otel_event.rs:3175-3195`

All three OTel types have stub `Deserialize` that return errors:
```rust
Err(serde::de::Error::custom("OtelLog deserialization not yet implemented"))
```

**Impact:** Any serde-based deserialization path (certain source codecs,
potential future disk buffer serde paths) will hard-fail at runtime.

**Why deferred:** Needs architectural decision on what the canonical
JSON representation should be. Not blocking the Metric struct deletion
campaign, which was the session focus.

**Tracked as:** Task T17 in LEGACY_REMOVAL_PLAN.md

---

### HIGH: `apply_value_legacy_layout` destroys `scope` on every mutation

**File:** `lib/vector-core/src/event/otel_event.rs` (~line 1056)

Every `insert()`/`remove()`/`modify_as_value()` round-trip sets
`self.scope = None`. If an OtelLog has a real OTel scope (name,
version, attributes), any VRL mutation silently destroys it.

**Impact:** Silent data loss of instrumentation scope metadata after
any VRL transform modifies the event.

**Why deferred:** This is a structural limitation of the
`to_value_legacy_layout` / `apply_value_legacy_layout` round-trip.
Fixing it requires either:
1. Extending the legacy layout to preserve scope (medium)
2. Eliminating the round-trip entirely (T16, blocked on T15)

**Tracked as:** Part of Task T16 in LEGACY_REMOVAL_PLAN.md

---

### HIGH: `observed_time_unix_nano` zeroed on round-trip

**File:** `lib/vector-core/src/event/otel_event.rs` (~line 1056)

`apply_value_legacy_layout` constructs new `LogRecord` with
`..Default::default()`, zeroing `observed_time_unix_nano`. The
`to_value_legacy_layout` only uses `observed_time_unix_nano` as a
fallback timestamp when `time_unix_nano == 0`, but write-back puts
it into `time_unix_nano`, not `observed_time_unix_nano`.

**Impact:** The ingest timestamp is permanently lost after any field
mutation (insert/remove via VRL).

**Why deferred:** Same root cause as scope loss — the round-trip
doesn't preserve all proto fields symmetrically.

**Tracked as:** Part of Task T16

---

### HIGH: `VrlTarget::OtelMetric` remove is a no-op on the proto

**File:** `lib/vector-core/src/event/vrl_target.rs:914-920`

```rust
VrlTarget::OtelMetric { event: _, value } => {
    value.remove(&target_path.path, false);
    Ok(None)  // Always returns None
}
```

The `remove` modifies the VRL projection `value` but does NOT write
back to the underlying `OtelMetric` proto event. Also always returns
`None` instead of the removed value.

**Impact:** `del(.name)` on a metric in VRL appears to succeed but
has no effect on the actual event. Silent data integrity issue.

**Why deferred:** Fixing requires designing a write-back mechanism
for OtelMetric similar to `apply_value_legacy_layout` for OtelLog.
This is a known limitation of the VRL target system for metrics.

**Tracked as:** Not yet in task list — should be added as T19.

---

### MEDIUM: `OtelLog::get()` returns owned `Value` not `&Value`

**File:** `lib/vector-core/src/event/otel_event.rs:724`

Every `get()` call builds the full legacy layout, then clones the
target field. O(event_size) per call vs O(depth) for the old LogEvent.

**Impact:** Performance regression for hot paths doing multiple gets
(route conditions, filters).

**Why deferred:** Structural to the legacy layout round-trip. Will be
fixed by T16 (eliminate round-trip).

---

### MEDIUM: Resource/scope round-trip asymmetry

`to_value_legacy_layout` emits `"resource"` and `"scope"` as nested
objects. `apply_value_legacy_layout` does NOT extract them back — they
become record attributes. After two mutations, nested objects get
double-nested.

**Why deferred:** Same T16 root cause.

---

## 2. Dead Code Not Cleaned Up

### Dead span helper functions in buffer_codec.rs

**File:** `lib/opentelemetry-proto/src/buffer_codec.rs:227-290`

Three functions produce compiler warnings:
- `value_to_span_event` (line 227) — NEVER CALLED
- `value_to_span_link` (line 250) — NEVER CALLED
- `value_to_span_status` (line 276) — NEVER CALLED

These were used by the deleted `trace_event_to_span` function. They
should be deleted but were missed in the G.1 cleanup.

**Fix:** Delete the 3 functions (~60 lines). Trivial.

---

## 3. Stale Naming Not Addressed

### `try_into_log_coerce` / `into_log_coerce` (17 callers)

**File:** `lib/vector-core/src/event/mod.rs:157-164`

These are exact aliases for `try_into_log` / `into_log`. They exist
for backward compatibility but add unnecessary API surface. 17 callers
across codecs, sinks, transforms.

**Why deferred:** Renaming 17 callers risks introducing issues during
autopilot mode. Better done as a dedicated cleanup pass.

**Tracked in:** T18 (partial — noted but not executed)

---

### `log_event!` macro (392 usages)

**File:** `src/test_util/mod.rs:84`

Works correctly (creates `OtelLog` internally) but the name suggests
LogEvent. Used in 3 test files (vrl.rs, datadog_search.rs, http/tests).

**Why deferred:** Cosmetic only. 392 usages make rename noisy for
zero functional benefit. The macro implementation is correct.

---

### `event.proto` deprecated field declarations

**File:** `lib/vector-core/proto/event.proto`

The proto definition still declares deprecated fields:
- `Log.fields` (deprecated, no longer decoded)
- `Log.metadata` (deprecated, no longer decoded)
- `Trace.metadata` (deprecated, no longer decoded)
- `Metric.metadata` (deprecated, no longer decoded)
- `Metric.tags_v1` (deprecated, no longer encoded)

These fields occupy proto field numbers but serve no purpose after
G.2 removed the decoders.

**Why deferred:** Removing fields from a proto definition can break
wire compatibility if any external system reads these protos.
Keeping them deprecated is safe; removing is a proto-level breaking
change that needs separate consideration.

---

## 4. Bulk Agent Quality Issues

### Paren-balance errors from `from_legacy_metric` bulk migration

The bulk migration agent (for remap.rs) introduced **3 extra `)`**
in the `Event::Metric({...})` pattern. All 3 were caught and fixed
by hand during the session.

**Pattern:** The agent wrapped `OtelMetric::from_legacy_metric(expr)`
with a block `{ let m = expr; ... OtelMetric::from_metric_parts(...) }`,
but when the original was inside `Event::Metric(...)`, the block's
closing `}` + Event::Metric's `)` produced `})`, and the agent added
an extra `)`.

**Remaining risk:** Other files touched by the bulk agent may have
similar issues. The compilation check passed for the full workspace
(lib + tests), so no undetected paren issues exist — but future bulk
agents should be more careful with nested delimiter balancing.

---

## 5. Incomplete Task Execution

### T3 (MetricSet refactor) — only external API migrated

The `normalize_otel`, `make_absolute_otel`, `make_incremental_otel`
methods now use `from_metric_parts` instead of `from_legacy_metric`.
But the internal `normalize()`, `make_absolute()`, `make_incremental()`
still accept and return `Metric`. Full T3 requires changing these
signatures to accept `(MetricSeries, MetricData, EventMetadata)` tuples,
which ripples to all sink normalizer implementations.

### T7 (test migration) — 68 sites remaining

| File | Sites | Why deferred |
|------|-------|--------------|
| `otel_event.rs` | 19 | Parity tests — need T1 done first (done), then rewrite |
| `prometheus/exporter.rs` | 9 | Needs T5 (prom exporter refactor) |
| `statsd/parser.rs` | 14 | Test assertions comparing parser output |
| Others | 26 | Various test files, lower priority |

### T9 (Lua bindings) — not started

`LuaMetric` holds `Metric`. `IntoLua`/`FromLua` impls are ~80 lines
of pattern matching. ~200 lines of test code. Needs dedicated effort.

### T15 (VRL aliases) — product decision gate

Cannot remove `.message`, `.timestamp`, `.tags`, `.host`,
`.source_type` aliases without a product decision. `vector vrl-migrate`
tool exists to rewrite user configs.

### T16 (eliminate legacy layout) — blocked on T15

The `to_value_legacy_layout` / `apply_value_legacy_layout` round-trip
is the root cause of the scope loss, observed_time loss, O(n) get,
and resource/scope asymmetry issues. Cannot eliminate until VRL
aliases (T15) are removed.

---

## 7. Trade-offs Made During Migration (2026-04-17)

### T9 (Lua bindings) deferred — high effort, low impact

`LuaMetric` holds a `Metric` struct and directly accesses its fields
(`metric.data.value`, `metric.series.tags`, etc.) across ~200 lines of
IntoLua/FromLua pattern-matching. Migrating would require restructuring
LuaMetric to hold `(MetricSeries, MetricData, EventMetadata)` and
updating all field accesses.

**Trade-off:** Deferred because:
1. Lua bindings are behind `#[cfg(feature = "lua")]` — not compiled
   in most builds
2. The Metric struct hasn't been deleted yet — T9 can be done right
   before T13 (struct deletion) with no wasted work
3. Higher-impact tasks (T7, T11) were completed instead, bringing
   from_legacy_metric callers to ZERO

### T3 internal methods deferred — ripple to 12+ sink normalizers

MetricSet's internal methods (`normalize()`, `make_absolute()`,
`make_incremental()`, `insert_update()`) still accept/return `Metric`.
The external OTel API (`normalize_otel`, `make_*_otel`) was migrated.

**Trade-off:** Full internal refactor requires changing the
`MetricNormalize` trait signature, which ripples to:
- `statsd/normalizer.rs`, `appsignal/normalizer.rs`
- `prometheus/remote_write/sink.rs`
- All sinks that implement the `Normalize` trait

This is a wide-blast-radius change better done as a dedicated task
with all sinks tested in sequence, rather than during a bulk migration.

### Bulk agent paren-balance risk accepted

The `from_legacy_metric` → `from_metric_parts` bulk agents occasionally
introduced extra closing parens in nested `Event::Metric({...})`
blocks. 3 were found and fixed in remap.rs earlier. The full test suite
passes (1782 tests), so no undetected issues remain, but this is a
known quality issue with bulk pattern replacement via agents.

## 6. Pre-existing Issues (not from this session)

- **6 TLS test failures** in vector-core — pre-existing cert/infrastructure issue
- **Flaky `file_start_position_server_restart_unfinalized`** test — timing-dependent
- **5 buffer_codec warnings** — 3 now dead functions (above), 2 pre-existing suggestions
