# Step 4c — Pipeline Telemetry (span_metrics transform)

## Context

Steps 4a (LB sink) and 4b (tail sampling) are complete. Pipeline telemetry completes
the 3-tier deployment story by providing RED metrics from traces. Mirrors the OTel
Collector Contrib `spanmetricsconnector`.

## span_metrics transform

Consumes `Event::Trace(OtelSpan)`, aggregates by dimensions, emits `Event::Metric(OtelMetric)`.

**Output metrics:**
- `{namespace}.calls` — Sum counter, one per unique dimension set
- `{namespace}.duration` — Histogram, one per unique dimension set

**Default dimensions:** `service.name`, `span.name`, `span.kind`, `status.code`

## Phases

1. Config + transform shell + aggregation engine (~300 lines) — PENDING
2. Dimension extraction + OtelMetric emission (~300 lines) — PENDING
3. Role awareness + docs (~100 lines) — PENDING
