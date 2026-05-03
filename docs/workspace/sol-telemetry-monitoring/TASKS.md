# sol-telemetry-monitoring — Tasks

Design: [DESIGN.md](./DESIGN.md)

## Analysis

Build: `cargo build -p vector` — verified green
Test: `cargo test -p vector-core --lib metrics && cargo test -p vector --lib sources::internal_metrics` — verified green (30 tests)
Lint: `cargo clippy -p vector -p vector-core`

### Known-failing tests
| Test | Reason | Action |
|---|---|---|
| (none) | | |

### Files to modify

**Task 1 — Namespace renaming (code):**

| File | Change | Occurrences |
|---|---|---|
| `lib/vector-core/src/metrics/recorder.rs:87,100,113` | `"vector"` → `"sol"` | 3 |
| `lib/vector-core/src/metrics/mod.rs:180,185` | `"vector"` → `"sol"` | 2 |
| `src/sources/internal_metrics.rs:92` | `default_namespace()` returns `"sol"` | 1 |
| `src/sources/internal_metrics.rs:108` | update comment | 1 |
| `src/sources/internal_metrics.rs:186` | `!= "vector"` → `!= "sol"` | 1 |
| `src/sources/internal_metrics.rs:324` | test assertion `Some("vector")` → `Some("sol")` | 1 |
| `src/transforms/tail_sampling/transform.rs:146,155` | `"vector_tail_sampling_trace_dropped_too_early"` → `"tail_sampling_trace_dropped_too_early"` | 2 |
| `src/transforms/tail_sampling/transform.rs:197,209,216` | `"vector_tail_sampling_traces_sampled"` → `"tail_sampling_traces_sampled"` | 3 |
| `src/sinks/opentelemetry/grpc.rs:459,470` | `"vector_lb_num_backends"` → `"lb_num_backends"` | 2 |
| `src/sinks/opentelemetry/grpc.rs:471` | `"vector_lb_num_resolutions"` → `"lb_num_resolutions"` | 1 |
| `src/sinks/opentelemetry/grpc.rs:564,567` | `"vector_lb_backend_outcome"` → `"lb_backend_outcome"` | 2 |
| `src/sinks/opentelemetry/load_balancing.rs:153` | `"vector_lb_num_resolutions"` → `"lb_num_resolutions"` | 1 |
| `lib/k8s-e2e-tests/src/metrics.rs:109-112,168,176` | `"vector_component_*"` → `"sol_component_*"` | 6 |

**Task 2 — Self-monitoring pipeline (config):**

| File | Change |
|---|---|
| `demo/otel-vector-grafana-dotnet/sol/sol-gateway.yaml` | Add `internal_metrics` source + OTLP sink to Mimir |
| `demo/otel-vector-grafana-dotnet/sol/sol-loadbalancer.yaml` | Add `internal_metrics` source + OTLP sink to Mimir |
| `demo/otel-vector-grafana-dotnet/sol/sol-collector.yaml` | Add `internal_metrics` source + OTLP sink to Mimir |

**Task 3 — SOL Pipeline dashboard (Grafana JSON):**

| File | Change |
|---|---|
| `demo/otel-vector-grafana-dotnet/grafana/provisioning/dashboards/Sol/` | New directory |
| `demo/otel-vector-grafana-dotnet/grafana/provisioning/dashboards/Sol/SOL Pipeline.json` | New dashboard |

### Requirement traceability
| File / Config | Addresses | Notes |
|---|---|---|
| `recorder.rs`, `mod.rs` namespace | [NFR3](./DESIGN.md#nfr3) | Registry-level namespace change |
| `internal_metrics.rs` namespace | [NFR3](./DESIGN.md#nfr3) | Source-level default |
| `tail_sampling/transform.rs` metric names | [NFR3](./DESIGN.md#nfr3) | Strip `vector_` prefix |
| `grpc.rs`, `load_balancing.rs` metric names | [NFR3](./DESIGN.md#nfr3) | Strip `vector_` prefix |
| `k8s-e2e-tests/metrics.rs` | [NFR3](./DESIGN.md#nfr3) | Update test expectations |
| `sol-gateway.yaml` self-monitoring | [FR1](./DESIGN.md#fr1) | Gateway self-monitoring pipeline |
| `sol-loadbalancer.yaml` self-monitoring | [FR1](./DESIGN.md#fr1) | Loadbalancer self-monitoring pipeline |
| `sol-collector.yaml` self-monitoring | [FR1](./DESIGN.md#fr1) | Collector self-monitoring pipeline |
| `SOL Pipeline.json` dashboard | [FR2](./DESIGN.md#fr2) | Grafana dashboard |

### Transformations
| Change | Input → Output | Invariant |
|---|---|---|
| Registry namespace | metric registered as `component_*` → captured as `sol_component_*` | All metrics from `visit_metrics()` / `capture_metrics()` get `namespace = "sol"` |
| Custom metric rename | `counter!("tail_sampling_*")` → captured as `sol_tail_sampling_*` | Custom metrics must NOT include namespace prefix |
| `internal_metrics` namespace override | `namespace: "custom"` in config → metrics emitted as `custom_component_*` | When config namespace differs from default, it replaces the registry namespace |

## Tasks

### 1. Rename metrics namespace from `vector` to `sol` ([NFR3](./DESIGN.md#nfr3))
**Goal**: Give SOL its own metrics identity, distinct from upstream Vector, to avoid collisions in mixed fleets.
**Files**: see "Task 1" table above
**Constraints**:
- [ADR: metrics namespace renaming](./adrs/metrics-namespace-renaming.md) — change at registry level, strip `vector_` from custom metric names
- Custom metrics registered via `counter!()` / `gauge!()` / `histogram!()` must NOT include a namespace prefix
- The `internal_metrics` source config `namespace` override must still work
**Tests**: existing tests updated to expect `"sol"` namespace
- `default_namespace` — assert `namespace() == Some("sol")`
- `namespace` (custom override) — assert custom namespace still works
- `captures_internal_metrics` — verify metrics are captured with `sol` prefix
**Verify**: `cargo test -p vector-core --lib metrics && cargo test -p vector --lib sources::internal_metrics`
**Acceptance criteria**:
- [x] `recorder.rs` uses `"sol"` namespace (3 occurrences)
- [x] `mod.rs` uses `"sol"` namespace (2 occurrences)
- [x] `internal_metrics.rs` default is `"sol"`, check updated, test updated
- [x] `tail_sampling/transform.rs` metrics use bare names without `vector_` prefix (5 occurrences)
- [x] `grpc.rs` and `load_balancing.rs` metrics use bare names without `vector_` prefix (6 occurrences)
- [x] `k8s-e2e-tests/metrics.rs` references updated to `sol_*` (6 occurrences)
- [x] All tests pass
**Depends on**: (none)
**Time-box**: ~30 min

### 2. Add self-monitoring pipeline to each SOL instance ([FR1](./DESIGN.md#fr1), [NFR1](./DESIGN.md#nfr1), [NFR2](./DESIGN.md#nfr2))
**Goal**: Each SOL instance exports its own operational metrics to Mimir for pipeline health monitoring.
**Files**: `sol-gateway.yaml`, `sol-loadbalancer.yaml`, `sol-collector.yaml`
**Constraints**:
- Resource attributes follow OTel conventions: `service.name`, `service.namespace`
- `internal_metrics` source default `service.name` is already `"sol"` — override with instance-specific `resource_attributes`
- Scrape interval: 15s (matching OTel Collector pattern)
- Self-monitoring sink must not interfere with data pipeline sinks (separate `opentelemetry` sink with dedicated input)
**Tests**: manual — run the demo with `docker compose up`, verify metrics appear in Mimir
- Query Mimir for `sol_component_received_events_total` grouped by `service_name`
- Verify 3 distinct service names appear: `sol/gateway`, `sol/loadbalancer`, `sol/collector`
**Verify**: `docker compose up -d && sleep 30 && curl -s 'http://localhost:9009/prometheus/api/v1/query?query=sol_component_received_events_total' | jq '.data.result | length'` (expect > 0)
**Acceptance criteria**:
- [x] `sol-gateway.yaml` has `internal_metrics` source with `scrape_interval_secs: 15`, `resource_attributes.service.name: sol/gateway`, `resource_attributes.service.namespace: gateway`
- [x] `sol-gateway.yaml` has `opentelemetry` sink for self-monitoring metrics → `http://mimir:9009/otlp`
- [x] `sol-loadbalancer.yaml` has equivalent self-monitoring pipeline with `service.name: sol/loadbalancer`, `service.namespace: loadbalancer`
- [x] `sol-collector.yaml` has equivalent self-monitoring pipeline with `service.name: sol/collector`, `service.namespace: collector`
- [x] Self-monitoring sinks use `internal_metrics` as input, not data pipeline sources
- [x] Existing data pipeline is unchanged (same sources, transforms, sinks as before)
**Depends on**: task 1 (metrics need `sol_*` namespace)
**Time-box**: ~30 min

### 3. Create SOL Pipeline Grafana dashboard ([FR2](./DESIGN.md#fr2), [FR3](./DESIGN.md#fr3))
**Goal**: Provide Grafana dashboard for SOL pipeline health monitoring, equivalent to the OTel Collector dashboard.
**Files**: new `demo/otel-vector-grafana-dotnet/grafana/provisioning/dashboards/Sol/SOL Pipeline.json`
**Constraints**:
- Dashboard auto-provisioned via Grafana's `foldersFromFilesStructure` (existing `default.yaml`)
- Must use `mimir` datasource UID (matching existing dashboards)
- Variables: `$datasource`, `$job` (service.name filter)
- All queries use `sol_*` metric names
- Structure follows [DESIGN.md dashboard structure](./DESIGN.md#dashboard-structure):
  - Row 1: Signal flows (stat panels + node graph per signal: Traces, Metrics, Logs)
  - Row 2: Sources (received events rate, errors)
  - Row 3: Transforms (incoming/outgoing rate, drop ratio)
  - Row 4: Sinks (sent rate, errors, bytes)
  - Row 5: Tail sampling (sampled/dropped by policy, evictions, sampling ratio)
**Tests**: manual — open Grafana at `http://localhost:3000`, verify the "SOL Pipeline" dashboard appears under the "Sol" folder, panels render data
**Verify**: dashboard JSON is valid (`python3 -c "import json; json.load(open('demo/otel-vector-grafana-dotnet/grafana/provisioning/dashboards/Sol/SOL Pipeline.json'))"`)
**Acceptance criteria**:
- [x] Dashboard JSON is valid and auto-provisioned
- [x] Row 1: Signal flow stat panels (Traces, Metrics, Logs) showing received/sent rate and ratio
- [x] Row 2: Sources panel with per-component received events rate
- [x] Row 3: Transforms panel with per-component incoming/outgoing rate
- [x] Row 4: Sinks panel with per-component sent rate and error rate
- [x] Row 5: Tail sampling panel with sampled/dropped by policy
- [x] Variables `$datasource` and `$instance` (service_name) are defined
- [x] All queries use `sol_*` metric names (zero `vector_*` or `otelcol_*` references)
**Depends on**: task 1 (metric names), task 2 (self-monitoring pipeline must be configured for data to appear)
**Time-box**: ~90 min

### 4. Verify application RED metrics unaffected ([FR4](./DESIGN.md#fr4))
**Goal**: Confirm that the self-monitoring pipeline does not interfere with the existing `span_metrics` application metrics.
**Files**: no changes — verification only
**Constraints**:
- `span_metrics` transform outputs to `otlp_span_metrics` sink (application metrics path)
- Self-monitoring `internal_metrics` outputs to a separate `otlp_self_monitoring` sink
- The two paths must be independent (no shared inputs)
**Tests**: manual — after task 2, verify the "OpenTelemetry dotnet webapi" dashboard in `Apps/` still renders correctly
**Verify**: inspect each SOL config to confirm `internal_metrics` source is only connected to the self-monitoring sink
**Acceptance criteria**:
- [x] In each SOL config, `internal_metrics` source feeds only the self-monitoring sink
- [x] `span_metrics` transform feeds only `otlp_span_metrics` sink (unchanged)
- [x] No cross-wiring between self-monitoring and data pipelines
**Depends on**: task 2
**Time-box**: ~10 min

## Sessions

### Session 1 — Namespace rename + self-monitoring configs (~1.5H)
Tasks: 1, 2, 4
**Skills**: `rust-software-engineer`
**Checkpoint**: `cargo test -p vector-core --lib metrics && cargo test -p vector --lib sources::internal_metrics && python3 -c "import yaml; [yaml.safe_load(open(f)) for f in ['demo/otel-vector-grafana-dotnet/sol/sol-gateway.yaml','demo/otel-vector-grafana-dotnet/sol/sol-loadbalancer.yaml','demo/otel-vector-grafana-dotnet/sol/sol-collector.yaml']]"`
**Commit point**: yes — commit after checkpoint passes

### Session 2 — SOL Pipeline dashboard (~1.5H)
Tasks: 3
**Skills**: `software-engineer`
**Checkpoint**: `python3 -c "import json; json.load(open('demo/otel-vector-grafana-dotnet/grafana/provisioning/dashboards/Sol/SOL Pipeline.json'))"`
**Commit point**: yes — commit after checkpoint passes

## Quality gates (post-session review)
- [ ] Acceptance criteria: all green above
- [ ] Code review: implementation matches [DESIGN.md](./DESIGN.md) intent
- [ ] Code organization: config files consistent, dashboard JSON well-structured
- [ ] No `vector_*` metric names remain in SOL code (only in k8s-e2e-tests for legacy Vector compat, and in the `vector` source adapter scope)
- [ ] No cross-wiring between self-monitoring and data pipelines
- [ ] Dashboard queries use only `sol_*` metrics
