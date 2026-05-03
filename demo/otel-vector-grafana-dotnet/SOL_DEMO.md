# Full Sol demo

A previous demo has been made to test Sol as an OTLP forwarder to be sure that all signals from receivers to sinks work correctly.

The goal of this demo is to test loadbalancer and tail sampling combined.

The goal is also to replace the otel contrib col gateway and use vector as a gateway instead.

The existing where the existing tail sampling + loadbalancing is located `C:\Users\cboudereau\gh\o11y-weekly\2024-02-28_OpenTelemetry_Looks_Good_To_Me_dotnet\otelcontribcol\gateway\pipeline.traces.yml` where some demo parameters are commented.

For each step, a manual test should be done before committing.
Migrate step by step in the following order:

1. ~~Migrate otel contrib col gateway to vector gateway~~ DONE
   - Removed `otelcontribcol-gateway` from compose.yml
   - Renamed `vector-forwarder` → `vector-gateway`
   - Apps now send OTLP directly to `vector-gateway:4317`
   - Vector routes: traces → traces-loadbalancer, logs → Loki, metrics → Mimir
2. Load balancing
3. Tail sampling