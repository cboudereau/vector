# Plan: Remove Legacy Backward Compat + Update VRL Migration Tool

> **Part of** [`CONSOLIDATED_MIGRATION_PLAN.md`](CONSOLIDATED_MIGRATION_PLAN.md).
> Cross-links: `VRL_MIGRATION_TOOL.md`, `VRL_OTEL_NATIVE_TARGETS.md`.

## Current state (2026-04-22)

### All legacy types and layout eliminated

| Item | Status |
|------|--------|
| `LogEvent` type | **DELETED** (`80ff2fb`) |
| `TraceEvent` type | **DELETED** (`1236e8e`) |
| `Metric` struct | **DELETED** (`9bd0d06`) |
| Proto backward buffer compat | **DELETED** (`9363568`) |
| All legacy bridges / `from_legacy_*` | **DELETED** |
| `to_value_legacy_layout` / `hoist_*` | **DELETED** |
| `message_path()` / `host_path()` | **DELETED** |
| `log_event!` macro | Renamed → `otel_event!` |
| Virtual fast-path aliases | **DELETED** (T15) |

### `log_schema()` indirection — **DELETED**

| Key | Default | Canonical OTLP location | Status |
|-----|---------|-------------------------|--------|
| `message_key` | `"body"` | `body` (proto field) | ✅ **DELETED** — hardcoded |
| `timestamp_key` | `"time_unix_nano"` | `time_unix_nano` (proto field) | ✅ **DELETED** — hardcoded |
| `source_type_key` | `"source_type"` | `resource.attributes."source_type"` | ✅ **DELETED** — hardcoded |
| `host_key` | `"host"` | `resource.attributes."host.name"` | ✅ **DELETED** — hardcoded |
| `metadata_key` | `"metadata"` | *(no OTLP mapping)* | ✅ **DELETED** — hardcoded |

The `LogSchema` struct, `init_log_schema()`, `log_schema()` global,
`[log_schema]` config section, and all accessors/setters/`merge()` logic
have been **completely removed**. Public constants (`BODY`, `TIME_UNIX_NANO`,
`HOST`, `SOURCE_TYPE`, `METADATA`) are exported from `log_schema.rs`.

### Remaining legacy code

| Item | Location | Status |
|------|----------|--------|
| `normalize_for_eq` | `otel_event.rs` | **SIMPLIFIED** — only strips `observed_time_unix_nano` |
| `get/set_source_type()` | `otel_event.rs` | ✅ OTLP-aligned (resource attribute) |
| `get/set_host()` | `otel_event.rs` | ✅ OTLP-aligned (resource attribute) |

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
B.5 (eliminate log_schema callers) ✅ DONE — 2 host_key callers remain (internal_metrics, dedupe)
B.6 (deprecate + VRL migration)  ✅ DONE — startup warnings + LS-01..05 rules
B.7 (remove LogSchema struct)     ✅ DONE
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

2 remaining `log_schema().host_key()` calls:
- `internal_metrics.rs` — metrics domain, not OtelLog
- `dedupe/common.rs` — default match fields, works via record attribute fallback

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

### B.6 — Deprecate `log_schema` config + schema-aware VRL migration ✅

Startup deprecation warnings emitted for non-default `log_schema` fields (`6e6d47b`).
`GlobalOptions.log_schema` marked `#[configurable(deprecated)]`.

VRL migrate tool enhanced with Pass 0 (LS-01..LS-05):
- `--config` mode parses `[log_schema]`, rewrites custom field names in
  VRL blocks, and strips the `[log_schema]` section from output
- `--log-schema <config>` flag for standalone VRL file migration
- 10 new tests covering all rule IDs and edge cases

---

### B.7 — Remove `LogSchema` struct ✅

All items completed:
1. ✅ Deleted `[log_schema]` field from `GlobalOptions`
2. ✅ Deleted `LogSchema` struct, all accessors, setters, `merge()`, `non_default_fields()`
3. ✅ Deleted `init_log_schema()` / `log_schema()` global
4. ✅ Hardcoded 2 remaining callers (internal_metrics → `owned_value_path!("host")`, dedupe → hardcoded vec)
5. ✅ Replaced `log_schema.rs` with public constants (`BODY`, `TIME_UNIX_NANO`, `HOST`, `SOURCE_TYPE`, `METADATA`)
6. ✅ VRL migrate tool uses local `MigrateLogSchema` struct (no vector-lib dependency)
7. ✅ Removed init calls from `app.rs`, `validate.rs`, `unit_test/mod.rs`
8. ✅ Deleted config tests, integration tests, TOML fixtures for `[log_schema]`
9. ✅ Updated doc comments in ~19 source/sink/transform files

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
| B.6 (deprecate log_schema) | Startup warnings + schema-aware VRL migration | `6e6d47b` |
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
