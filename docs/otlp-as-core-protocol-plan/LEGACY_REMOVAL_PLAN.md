# Plan: Remove Legacy Backward Compat + Update VRL Migration Tool

> **Part of** [`CONSOLIDATED_MIGRATION_PLAN.md`](CONSOLIDATED_MIGRATION_PLAN.md).
> Cross-links: `VRL_MIGRATION_TOOL.md`, `VRL_OTEL_NATIVE_TARGETS.md`.

## Current state (2026-04-22)

### All legacy types and layout eliminated

| Item | Status |
|------|--------|
| `LogEvent` type | **DELETED** (`80ff2fb`, -1217 lines) |
| `TraceEvent` type | **DELETED** (`1236e8e`, -191 lines) |
| `Metric` struct | **DELETED** (`9bd0d06`, -1040 lines) |
| Proto backward buffer compat | **DELETED** (`9363568`) |
| All `Event::from(Legacy)` bridges | **DELETED** |
| All `from_legacy_*` / `from_log_event` / `from_trace_event` | **DELETED** |
| `to_value_legacy_layout` | **DELETED** (all callers → `to_value_canonical`) |
| `hoist_resource_fields` / `hoist_scope_fields` | **DELETED** |
| `message_path()` / `host_path()` | **DELETED** |
| `log_event!` macro | Renamed → `otel_event!` |
| Virtual `"timestamp"` / `"@timestamp"` fast-path | **DELETED** (2026-04-22) |
| `log_schema().timestamp_key()` default | Changed: `"timestamp"` → `"time_unix_nano"` |

### Fast-path aliases still live in `otel_event.rs`

The `get_single_segment` / `insert_single_segment` / `remove_single_segment`
functions have **no remaining hardcoded aliases** for message, host, or
source_type. They were removed in T15 Phase 1. Accessing `"host"` or
`"source_type"` goes through the generic record-attribute path.

### `log_schema()` indirection still live

| Key | Default | Canonical OTLP location | Production callers |
|-----|---------|-------------------------|-------------------|
| `message_key` | `"body"` | `body` (proto field) | ~65 |
| `host_key` | `"host"` | `resource.attributes."host.name"` | ~35 |
| `source_type_key` | `"source_type"` | `resource.attributes."source_type"` | ~35 |
| `timestamp_key` | `"time_unix_nano"` | `time_unix_nano` (proto field) | **0** (all migrated) |

`message_key` already defaults to `"body"` (the canonical name). The other
two default to legacy names that store values as **record attributes** instead
of at their canonical OTLP resource-attribute locations.

### Remaining legacy code

| Item | Location | Role |
|------|----------|------|
| `apply_value_map` "message" fallback | `otel_event.rs:1557` | `map.remove("body").or_else(\|\| map.remove("message"))` |
| `normalize_for_eq` | `otel_event.rs:3916-3937` | Hoists `resource.source_type` → top-level for test equality |
| `get_source_type()` | `otel_event.rs:1682` | Reads from `attribute("source_type")` — wrong location |
| `get_host()` | `otel_event.rs:1688` | Reads from `resource_attribute("host.name")` — correct |
| `log_schema()` mechanism | `lib/vector-core/src/config/log_schema.rs` | 5-field user-configurable indirection |

---

## Phase B — Remove remaining VRL aliases (PLAN)

### Goal

Eliminate the `log_schema()` indirection for `message_key`, `host_key`,
and `source_type_key`. All production code should use typed proto methods
(like `set_timestamp()` / `remove_timestamp()` did for timestamp). Then
deprecate and eventually delete the `LogSchema` mechanism.

### Dependency graph

```
B.1 (message cleanup)           — independent, easy
B.2 (host migration)            — independent, medium
B.3 (source_type migration)     — independent, medium
B.4 (normalize_for_eq cleanup)  — after B.2 + B.3
B.5 (deprecate log_schema())    — after B.1 + B.2 + B.3
```

B.1, B.2, B.3 can proceed in parallel.

---

### B.1 — Message cleanup

**Effort:** Low (~1h)

`message_key` already defaults to `"body"`, which IS the proto field name.
No caller migration needed — `log.insert(event_path!("body"), v)` already
hits the proto fast-path.

**Actions:**
1. Delete the `"message"` fallback in `apply_value_map` (line 1557):
   `map.remove("body").or_else(|| map.remove("message"))` → `map.remove("body")`
2. Update VRL migration tool docs to note that `.message` no longer works
   even via `from_value_map`
3. Fix any test that constructs events with `"message"` key expecting it
   to become `body`

**Risk:** LOW. Only affects events constructed via `from_value_map` with
a `"message"` key and no `"body"` key. Generic decoders (Avro, Protobuf)
pass user-defined field names — if user schema has a field literally named
"message" that they want as the body, they need VRL to rename it.

---

### B.2 — Host migration

**Effort:** Medium (~4h, 2 sub-phases)

`host_key` defaults to `"host"`, which stores as a record attribute.
The canonical OTLP location is `resource.attributes."host.name"`.

**Current typed API:**
- `get_host()` — reads `resource_attribute("host.name")` (correct location)
- `set_host()` — **does not exist**, needs to be added

**B.2a — Add `set_host()` + migrate callers (~35 production sites):**

Add to OtelLog:
```rust
pub fn set_host(&mut self, value: impl Into<Value>) -> Option<Value> {
    self.set_resource_attribute("host.name", value)
}
pub fn try_set_host(&mut self, value: impl Into<Value>) {
    if self.get_host().is_none() { self.set_host(value); }
}
```

Migrate callers that insert via `log_schema().host_key()`:

| Location | Pattern | New |
|----------|---------|-----|
| Sources (journald, kafka, socket, etc.) | `log.insert(host_key, hostname)` | `log.set_host(hostname)` |
| Sinks (splunk_hec, humio, etc.) | `log.get(host_key)` | `log.get_host()` |
| Schema defs | `Kind::bytes()` at `"host"` | `Kind::bytes()` at resource path |

**B.2b — Change default + deprecation:**

Change `const HOST: &str = "host"` → not needed if all callers migrated.
Instead, mark `log_schema().host_key()` as deprecated. Callers that still
use it (user-configurable overrides) continue to work via the generic
attribute path.

**Risk:** MEDIUM. Sources that currently write `"host"` as a record
attribute will now write `resource.attributes."host.name"`. Sinks that
read `"host"` (via `log_schema().host_key()`) need to be migrated to
`get_host()` simultaneously. Must be done atomically per source/sink pair.

---

### B.3 — Source_type migration

**Effort:** Medium (~4h, 2 sub-phases)

`source_type_key` defaults to `"source_type"`, which stores as a record
attribute. Currently `get_source_type()` reads from `attribute("source_type")`
(record attribute) — this matches where callers write, but is NOT the
canonical resource-attribute location.

**Decision needed:** Should `source_type` live at:
- (a) `resource.attributes."source_type"` (OTLP-aligned), or
- (b) `record.attributes."source_type"` (current behavior)?

Recommendation: **(a)** — source_type describes the origin/source, which is
conceptually a resource attribute. It's analogous to `service.name` in OTLP.

**B.3a — Add `set_source_type()` + migrate callers (~35 production sites):**

Add to OtelLog:
```rust
pub fn set_source_type(&mut self, value: impl Into<Value>) -> Option<Value> {
    self.set_resource_attribute("source_type", value)
}
pub fn get_source_type(&self) -> Option<Value> {
    // Fix: read from resource, not record
    self.resource_attribute("source_type").map(any_value_to_vrl)
}
```

Migrate callers that insert via `log_schema().source_type_key()`.

**B.3b — Change default + deprecation:**

Same pattern as B.2b.

**Risk:** MEDIUM. Same atomicity requirement as host. Additionally,
`get_source_type()` currently reads from record attributes; changing to
resource attributes means events written before this change will have
source_type at the old location. Need a migration path or dual-read
during transition.

---

### B.4 — Delete `normalize_for_eq`

**Effort:** Low (~30min). After B.2 + B.3.

Once host lives at `resource.attributes."host.name"` and source_type at
`resource.attributes."source_type"` canonically, the hoisting in
`normalize_for_eq` is unnecessary — `to_value_canonical()` already
includes resource attributes at `resource.*` paths.

**Actions:**
1. Delete `normalize_for_eq()` function
2. Update `EventDataEq for OtelLog/OtelSpan/OtelMetric` to compare
   `to_value_canonical()` directly (still strip `observed_time_unix_nano`)
3. Fix any test equality assertions that relied on hoisted field positions

---

### B.5 — Deprecate `log_schema()` mechanism

**Effort:** Medium (~2h). After B.1 + B.2 + B.3.

At this point, zero production callers use `log_schema().{message,host,source_type}_key()`
for runtime access. The mechanism only matters for:
- User VRL programs referencing `.host` or `.source_type`
- User configs with explicit `host_key = "..."` overrides

**Actions:**
1. Add startup deprecation warning if user config sets `host_key`,
   `source_type_key`, or `message_key` explicitly
2. Document the migration in release notes
3. Update `vector vrl-migrate` rules for host/source_type paths
4. In a future release: delete `LogSchema` fields (keep only
   `timestamp_key` and `metadata_key` if still needed)

**Risk:** HIGH (user-facing breaking change). Mitigated by:
- `vector vrl-migrate` tool handles path rewrites
- Deprecation warnings one release before removal
- Config validation explains what to change

---

## Completed work (summary)

All tasks below are **DONE**. See git history for details.

| Phase | Description | Key commits |
|-------|-------------|-------------|
| A | VRL migration tool (22 rules, 3 passes) | `src/vrl_migrate/` |
| B (timestamp) | Virtual field removal, all 6 phases | `302d3a1` |
| C | OTel → Legacy bridge removal (~250 lines) | — |
| E | Remove legacy types from production | — |
| F | Delete LogEvent + TraceEvent types | `80ff2fb`, `1236e8e` |
| G | Delete Metric struct + buffer compat | `9bd0d06`, `9363568` |
| T15 | Remove VRL aliases + Serialize → proto-canonical | `940da7e` |
| T16 | Fast-path get/insert/remove (bypass legacy round-trip) | `5198ea7`..`a86b435` |
| T23 | Delete legacy layout functions, rename to canonical | — |

### Types that stay (OtelMetric public API)

`MetricKind`, `MetricValue`, `MetricTags`, `TagValue`, `TagValueSet`,
`Bucket`, `Quantile`, `Sample`, `StatisticKind`, `MetricSeries`,
`MetricName`, `MetricData`, `MetricTime` — used by `into_metric_parts()`
/ `with_tags()` / `with_timestamp()`.

## Related docs

- `VRL_MIGRATION_TOOL.md` — Phase A spec + rules reference
- `VRL_OTEL_NATIVE_TARGETS.md` — path model for OTel-native VRL targets
- `CONSOLIDATED_MIGRATION_PLAN.md` — master plan (Steps 0-7)
