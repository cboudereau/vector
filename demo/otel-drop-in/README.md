# Vector as OTel Collector drop-in replacement

Side-by-side comparison: same OTLP/JSON payloads (logs, metrics, traces) sent to
both **OTel Collector Contrib** and **Vector**, each printing received data to stdout.

Based on [o11y-weekly OTEL-example](https://github.com/o11y-weekly/o11y-weekly.github.io/tree/main/_DRAFT/OTEL-example).

## Architecture

```
                         ┌─ otelcontribcol ──→ debug exporter (stdout)
curl (OTLP/JSON) ──────►│
                         └─ vector ──→ console sink (stdout)
```

Both paths receive the exact same OTLP/JSON payloads on `/v1/logs`, `/v1/metrics`,
`/v1/traces`. Vector accepts `application/json` and `application/x-protobuf`
natively — no intermediate forwarder needed.

## Run

```bash
cd demo/otel-drop-in
docker compose up --build
```

First build takes ~15-20 min (Rust compilation). Subsequent runs use Docker cache.

## Compare outputs

```bash
docker compose logs otelcontribcol    # OTel Collector received data
docker compose logs vector            # Vector received data

# Save full output for review
docker compose logs > logs.log 2>&1
```

## Ports

| Service         | HTTP (OTLP) | gRPC (OTLP) |
|-----------------|-------------|-------------|
| otelcontribcol  | 4318        | 4317        |
| vector          | 4328        | 4327        |

## Send data manually

```bash
# To OTel Collector (JSON)
curl -X POST -H 'Content-Type: application/json' \
  -d @otlpjson/logs.json http://localhost:4318/v1/logs

# To Vector (JSON, direct)
curl -X POST -H 'Content-Type: application/json' \
  -d @otlpjson/logs.json http://localhost:4328/v1/logs

# To Vector (protobuf — also supported)
# Use any OTLP client that sends application/x-protobuf
```

## Findings

- **Works:** Vector's `opentelemetry` source handles both OTLP/HTTP (JSON and
  protobuf) and OTLP/gRPC for all 3 signals — logs, metrics, traces.
- **JSON responses:** When a request arrives with `Content-Type: application/json`,
  Vector responds with `application/json`. Protobuf requests get protobuf responses.
- **Drop-in parity:** Vector can receive the same OTLP/JSON payloads as the
  OTel Collector Contrib without any intermediate forwarder or conversion layer.
