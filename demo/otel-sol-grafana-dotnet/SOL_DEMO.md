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

4. ~~Service graph~~ DONE
   - Added `servicegraph` transform to `sol-collector`
   - Pairs CLIENT/SERVER spans via parent span ID matching (same trace routed to same collector by load balancer)
   - Emits OTLP metrics: `traces_service_graph_request`, `traces_service_graph_request_failed`, `traces_service_graph_request_server`, `traces_service_graph_request_client`
   - Mimir converts to Prometheus names (`_total`, `_seconds` suffixes) via `-distributor.otel-metric-suffixes-enabled`
   - Compatible with otelcontribcol `servicegraphconnector` metric names and dimensions
   - Edge metrics exported to Mimir alongside span_metrics

## Known limitations

### Virtual nodes
The `servicegraph` transform does not yet synthesize edges for uninstrumented services (virtual nodes via `peer.service`, `db.name`). This is planned for a follow-up.
