# Vector as OTel Collector drop-in replacement

Side-by-side comparison: same OTLP/JSON payloads (logs, metrics, traces) sent to
both **OTel Collector Contrib** and **Vector**, each printing received data to stdout.

Based on [o11y-weekly OTEL-example](https://github.com/o11y-weekly/o11y-weekly.github.io/tree/main/_DRAFT/OTEL-example).

## Architecture

```
curl (OTLP/JSON) ──→ otelcontribcol:4318 ──→ debug exporter (stdout)
curl (OTLP/JSON) ──→ vector:4318          ──→ console sink   (stdout)
```

Both receive the exact same payloads on the same OTLP HTTP endpoints (`/v1/logs`,
`/v1/metrics`, `/v1/traces`).

## Run

```bash
# Build Vector from local source and start everything
docker compose up --build

# Compare outputs side by side
docker compose logs otelcontribcol
docker compose logs vector
```

First build takes ~15-20 min (Rust compilation). Subsequent runs use Docker cache.

## Ports

| Service         | HTTP (OTLP) | gRPC (OTLP) |
|-----------------|-------------|-------------|
| otelcontribcol  | 4318        | 4317        |
| vector          | 4328        | 4327        |

## Send data manually

```bash
# To OTel Collector
curl -X POST -H 'Content-Type: application/json' \
  -d @otlpjson/logs.json http://localhost:4318/v1/logs

# To Vector (same payload, same endpoint path)
curl -X POST -H 'Content-Type: application/json' \
  -d @otlpjson/logs.json http://localhost:4328/v1/logs
```

## What this validates

- Vector's `opentelemetry` source accepts OTLP/JSON HTTP on the standard `/v1/{signal}` endpoints
- All 3 signals (logs, metrics, traces) are received and printed
- Zero config translation needed — Vector is a protocol-compatible drop-in
