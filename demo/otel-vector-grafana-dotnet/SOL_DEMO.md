# Full Sol demo

A previous demo has been made to test Sol as an OTLP forwarder to be sure that all signals from receivers to sinks work correctly.

The goal of this demo is to test loadbalancer and tail sampling combined.

The goal is also to replace the otel contrib col gateway and use Sol as a gateway instead.

The existing where the existing tail sampling + loadbalancing is located `C:\Users\cboudereau\gh\o11y-weekly\2024-02-28_OpenTelemetry_Looks_Good_To_Me_dotnet\otelcontribcol\gateway\pipeline.traces.yml` where some demo parameters are commented.

For each step, a manual test should be done before committing.
Migrate step by step in the following order:

1. ~~Migrate otel contrib col gateway to sol gateway~~ DONE
   - Removed `otelcontribcol-gateway` from compose.yml
   - Renamed `sol-forwarder` → `sol-gateway`
   - Apps now send OTLP directly to `sol-gateway:4317`
   - Sol routes: traces → traces-loadbalancer, logs → Loki, metrics → Mimir
2. ~~Load balancing~~ DONE
   - Removed `otelcontribcol-traces-loadbalancer` from compose.yml
   - Added dedicated `sol-loadbalancer` with `load_balancing` (consistent hash on traceID, DNS resolver)
   - `sol-gateway` forwards traces to `sol-loadbalancer:4317`
   - `sol-loadbalancer` routes to `sol-collector` replicas via gRPC
3. ~~Tail sampling~~ DONE
   - Removed `otelcontribcol-traces-collector` from compose.yml
   - Added `sol-collector` (2 replicas) with `tail_sampling` transform
   - Policies: latency >= 100ms, ERROR status (excluding 4xx via AND + string_attribute), 10% probabilistic
   - Added `span_metrics` transform for RED metrics → Mimir
   - Sampled traces exported to Tempo via gRPC

## Known limitations

### Service graph vs span_metrics
OTel `servicegraph` connector generates inter-service edge metrics (client→server pairs).
Sol `span_metrics` generates per-service RED metrics (rate, errors, duration histograms).
They are complementary, not equivalent — the Grafana service graph panel requires the edge metrics.
