# Vector as OTel Collector drop-in replacement

Side-by-side comparison: same OTLP/JSON payloads (logs, metrics, traces) sent to
both **OTel Collector Contrib** and **Vector**, each printing received data to stdout.

Based on [o11y-weekly OTEL-example](https://github.com/o11y-weekly/o11y-weekly.github.io/tree/main/_DRAFT/OTEL-example).

## Architecture

```
                         ┌─ otelcontribcol ──→ debug exporter (stdout)
curl (OTLP/JSON) ──────►│
                         └─ otel-to-vector ──(gRPC)──→ vector ──→ console sink (stdout)
```

Both paths receive the exact same OTLP/JSON payloads on `/v1/logs`, `/v1/metrics`,
`/v1/traces`.

**Note:** Vector's OTLP HTTP source currently only accepts `application/x-protobuf`,
not `application/json`. The demo uses an otelcontribcol instance as a thin forwarder
that receives JSON over HTTP and re-exports as protobuf over gRPC to Vector.
Adding OTLP/JSON HTTP support to Vector is a follow-up task.

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
```

## Ports

| Service         | HTTP (OTLP/JSON) | gRPC (OTLP/protobuf) |
|-----------------|-------------------|-----------------------|
| otelcontribcol  | 4318              | 4317                  |
| vector          | 4328 (protobuf only) | 4327               |
| otel-to-vector  | 4338 (JSON→gRPC bridge) | —              |

## Send data manually

```bash
# To OTel Collector (JSON)
curl -X POST -H 'Content-Type: application/json' \
  -d @otlpjson/logs.json http://localhost:4318/v1/logs

# To Vector via forwarder (JSON→gRPC)
curl -X POST -H 'Content-Type: application/json' \
  -d @otlpjson/logs.json http://localhost:4338/v1/logs
```

## Findings

- **Works:** Vector's `opentelemetry` source handles OTLP/gRPC (protobuf) for all 3
  signals — logs, metrics, traces are received and printed correctly.
- **Gap:** Vector's OTLP HTTP endpoint rejects `Content-Type: application/json` with
  HTTP 500. It only accepts `application/x-protobuf`. The OTLP spec requires both.
  This is tracked as a follow-up to achieve true drop-in parity.
