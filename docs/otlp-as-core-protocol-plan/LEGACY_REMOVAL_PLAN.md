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
| `host_key` | `"host"` | `resource.attributes."host.name"` | ~4 (was ~35) |
| `source_type_key` | `"source_type"` | `resource.attributes."source_type"` | **0** (all migrated) |
| `timestamp_key` | `"time_unix_nano"` | `time_unix_nano` (proto field) | **0** (all migrated) |

`message_key` already defaults to `"body"` (the canonical name).
`source_type_key` has zero remaining callers — all migrated to `set_source_type()`.
`host_key` has 4 remaining callers: `internal_metrics` (metrics domain),
`dedupe` (uses virtual field compat), and 2 test files.
Schema definitions all use hardcoded resource paths (`149f4c3`).

### Remaining legacy code

| Item | Location | Role | Status |
|------|----------|------|--------|
| `apply_value_map` "message" fallback | — | — | **DELETED** (Phase B) |
| `normalize_for_eq` | `otel_event.rs:3952` | Only strips `observed_time_unix_nano` | **SIMPLIFIED** (Phase B) |
| `get_source_type()` | `otel_event.rs:1691` | Reads from `resource_attribute("source_type")` | **FIXED** (reads correct location) |
| `set_source_type()` | `otel_event.rs:1697` | Writes to `resource_attribute("source_type")` | **ADDED** (Phase B) |
| `get_host()` | `otel_event.rs:1712` | Reads from `resource_attribute("host.name")` | Correct |
| `set_host()` | `otel_event.rs:1718` | Writes to `resource_attribute("host.name")` | **ADDED** (Phase B) |
| `log_schema()` mechanism | `lib/vector-core/src/config/log_schema.rs` | 5-field user-configurable indirection | **2 callers remain** (host_key: internal_metrics, dedupe) |

---

## Phase B — Remove remaining VRL aliases

### Goal

Eliminate the `log_schema()` indirection for `message_key`, `host_key`,
and `source_type_key`. All production code should use typed proto methods
(like `set_timestamp()` / `remove_timestamp()` did for timestamp). Then
deprecate and eventually delete the `LogSchema` mechanism.

### Dependency graph

```
B.1 (message cleanup)           ✅ DONE
B.2 (host migration)            ✅ DONE
B.3 (source_type migration)     ✅ DONE
B.4 (normalize_for_eq cleanup)  ✅ DONE (already simplified)
B.5 (deprecate log_schema())    ✅ DONE — 2 host_key callers remain (internal_metrics, dedupe)
```

---

### B.1 — Message cleanup ✅

`message_key` defaults to `"body"`. The `"message"` fallback in
`apply_value_map` was deleted in Phase B. No further action needed.

---

### B.2 — Host migration ✅

`set_host()` / `try_set_host()` / `get_host()` added. All source callers
migrated to typed methods. Schema definitions updated to `resource.host.name`
path. Sink `config_host_key()` defaults changed:
- humio logs: `path: None` (uses semantic `get_host()` fallback)
- splunk_hec logs: already `None` by default
- splunk_hec/humio metrics: `"host"` (correct for metric tag_value)

4 remaining `log_schema().host_key()` calls:
- `internal_metrics.rs` — metrics domain, not OtelLog
- `dedupe/common.rs` — default match fields, works via record attribute fallback
- 2 test files — non-critical

---

### B.3 — Source_type migration ✅

`set_source_type()` / `try_set_source_type()` / `get_source_type()` added.
All read from/write to `resource_attribute("source_type")` (OTLP-aligned).
Zero remaining `log_schema().source_type_key()` callers.
Schema definitions use `resource.source_type` path.

---

### B.4 — Clean up `normalize_for_eq` ✅

Already simplified — only strips `observed_time_unix_nano`. No more
resource.source_type hoisting needed since `to_value_canonical()` now
includes resource attributes at `resource.*` paths natively.

---

### B.5 — Eliminate `log_schema()` callers ✅

All `message_key`, `timestamp_key`, `source_type_key` callers eliminated
across 66 files (`8cfd0ae`). Only 2 `host_key` callers intentionally remain:
- `internal_metrics.rs` — metrics domain, not OtelLog
- `dedupe/common.rs` — default match fields, works via record attribute fallback

**Next steps (future release):**
1. Add startup deprecation warning if user config sets `host_key`,
   `source_type_key`, or `message_key` explicitly
2. Delete `LogSchema` fields (keep only `metadata_key` if still needed)
3. Document the migration in release notes

---

## Completed work (summary)

All tasks below are **DONE**. See git history for details.

| Phase | Description | Key commits |
|-------|-------------|-------------|
| A | VRL migration tool (22 rules, 3 passes) | `src/vrl_migrate/` |
| B (timestamp) | Virtual field removal, all 6 phases | `302d3a1` |
| B (host/source_type) | Runtime migration to typed methods | `31f6668`, `b3564d6`, `2361edb` |
| B (schema defs) | Schema definitions → resource paths | `149f4c3` |
| B (sink host_key fix) | Humio semantic fallback | `4bbf03d` |
| B.5 (log_schema elimination) | Replace all message_key/timestamp_key callers | `8cfd0ae` |
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
