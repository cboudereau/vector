# Plan: Remove Legacy Backward Compat + Update VRL Migration Tool

## Context

OTel VRL targets expose OTel-native paths alongside backward-compat aliases.
Remove aliases, rely on `vector vrl-migrate` to rewrite user programs.
Removes ~750 lines of bridge code + simplifies 233 call sites across 62 files.

## Phases

- **A**: Update VRL migration tool rules — FIRST, prerequisite for everything
- **B**: Remove aliases from VRL projections, fix ~15-25 test failures
- **C**: Remove bridge functions from OtelLog (111 call sites)
- **D**: Remove bridge functions from OtelMetric (78 call sites)
- **E**: Remove legacy types (LogEvent, Metric, TraceEvent) — ~3,000+ lines

## Bridge functions (~750 lines in otel_event.rs)

| Function | Lines | Callers |
|----------|-------|---------|
| `from_log_event()` | 71 | 35 |
| `to_log_event()` (OtelLog) | 85 | 111 |
| `to_log_event()` (OtelSpan) | 63 | (included above) |
| `from_legacy_metric()` | 253 | 36 |
| `to_legacy_metric()` | 263 | 42 |

## VRL Migration Rules (Phase A)

| Rule | Old | New |
|------|-----|-----|
| LOG-01 | `.message` | `.body` |
| LOG-02 | `.timestamp` | `.time_unix_nano` |
| LOG-05 | `.tags` | `.attributes` |
| LOG-06 | `.tags."key"` | `.attributes."key"` |
| MET-06 | `.value.counter.value` | `.data.sum.data_points[0].value` |
| MET-07 | `.value.gauge.value` | `.data.gauge.data_points[0].value` |
