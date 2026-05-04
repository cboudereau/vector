# rename-vector-to-sol — Tasks

Design: [DESIGN.md](./DESIGN.md)

## Analysis

Build: `cargo build` — verifying (background)
Test: `cargo test -p vector` — run after Session 1 as `cargo test -p sol`
Lint: `cargo clippy -p vector` — run after Session 1 as `cargo clippy -p sol`

### Known-failing tests
| Test | Reason | Action |
|---|---|---|
| (none known) | | |

### Crate rename mapping (21 crates)

| Current name | New name | Path (current → new) |
|---|---|---|
| `vector` | `sol` | `.` (root, no move) |
| `vector-api-client` | `sol-api-client` | `lib/vector-api-client` → `lib/sol-api-client` |
| `vector-buffers` | `sol-buffers` | `lib/vector-buffers` → `lib/sol-buffers` |
| `vector-common` | `sol-common` | `lib/vector-common` → `lib/sol-common` |
| `vector-common-macros` | `sol-common-macros` | `lib/vector-common-macros` → `lib/sol-common-macros` |
| `vector-config` | `sol-config` | `lib/vector-config` → `lib/sol-config` |
| `vector-config-common` | `sol-config-common` | `lib/vector-config-common` → `lib/sol-config-common` |
| `vector-config-macros` | `sol-config-macros` | `lib/vector-config-macros` → `lib/sol-config-macros` |
| `vector-core` | `sol-core` | `lib/vector-core` → `lib/sol-core` |
| `vector-lib` | `sol-lib` | `lib/vector-lib` → `lib/sol-lib` |
| `vector-lookup` | `sol-lookup` | `lib/vector-lookup` → `lib/sol-lookup` |
| `vector-opentelemetry-proto` | `sol-opentelemetry-proto` | `lib/opentelemetry-proto` (no move — path has no "vector") |
| `vector-stream` | `sol-stream` | `lib/vector-stream` → `lib/sol-stream` |
| `vector-tap` | `sol-tap` | `lib/vector-tap` → `lib/sol-tap` |
| `vector-top` | `sol-top` | `lib/vector-top` → `lib/sol-top` |
| `vector-vrl-category` | `sol-vrl-category` | `lib/vector-vrl/category` → `lib/sol-vrl/category` |
| `vector-vrl-cli` | `sol-vrl-cli` | `lib/vector-vrl/cli` → `lib/sol-vrl/cli` |
| `vector-vrl-functions` | `sol-vrl-functions` | `lib/vector-vrl/functions` → `lib/sol-vrl/functions` |
| `vector-vrl-metrics` | `sol-vrl-metrics` | `lib/vector-vrl-metrics` → `lib/sol-vrl-metrics` |
| `vector-vrl-tests` | `sol-vrl-tests` | `lib/vector-vrl/tests` → `lib/sol-vrl/tests` |
| `vector-vrl-web-playground` | `sol-vrl-web-playground` | `lib/vector-vrl/web-playground` → `lib/sol-vrl/web-playground` |

### Package alias warning

Two crates use `package = "vector-lookup"` aliased to `lookup`:
- `lib/vector-core/Cargo.toml`: `lookup = { package = "vector-lookup", ... }`
- `lib/opentelemetry-proto/Cargo.toml`: `lookup = { package = "vector-lookup", ... }`
- `lib/codecs/Cargo.toml`: `lookup = { package = "vector-lookup", ... }`

These must become `package = "sol-lookup"` — the alias `lookup` stays unchanged so source code using `use lookup::*` needs no update.

### Non-vector-prefixed crates that depend on vector-* crates

These Cargo.toml files reference vector-* crates and need dependency name updates:
- `lib/codecs/Cargo.toml` — depends on `vector-common`, `vector-common-macros`, `vector-config`, `vector-config-macros`, `vector-core`, `vector-opentelemetry-proto`, `vector-vrl-functions`
- `lib/enrichment/Cargo.toml` — depends on `vector-vrl-category`
- `lib/dnstap-parser/Cargo.toml` — depends on `vector-config`, `vector-common`, `vector-lookup`, `vector-core`
- `lib/file-source/Cargo.toml` — depends on `vector-common`
- `lib/file-source-common/Cargo.toml` — depends on `vector-common`, `vector-config`
- `lib/prometheus-parser/Cargo.toml` — depends on `vector-common`
- `lib/docs-renderer/Cargo.toml` — depends on `vector-config`, `vector-config-common`
- `lib/k8s-e2e-tests/Cargo.toml` — depends on `vector` (root crate)

### Environment variables (47 total in .rs files)

**Clap attribute `env = "VECTOR_*"` (34 occurrences):**
- `src/cli.rs`: `VECTOR_CONFIG`, `VECTOR_CONFIG_DIR`, `VECTOR_CONFIG_TOML`, `VECTOR_CONFIG_JSON`, `VECTOR_CONFIG_YAML`, `VECTOR_REQUIRE_HEALTHY`, `VECTOR_THREADS`, `VECTOR_DISABLE_ENV_VAR_INTERPOLATION`, `VECTOR_LOG_FORMAT`, `VECTOR_COLOR`, `VECTOR_WATCH_CONFIG`, `VECTOR_WATCH_CONFIG_METHOD`, `VECTOR_WATCH_CONFIG_POLL_INTERVAL_SECONDS`, `VECTOR_INTERNAL_LOG_RATE_LIMIT`, `VECTOR_GRACEFUL_SHUTDOWN_LIMIT_SECS`, `VECTOR_NO_GRACEFUL_SHUTDOWN_LIMIT`, `VECTOR_OPENSSL_NO_PROBE`, `VECTOR_ALLOW_EMPTY_CONFIG`
- `src/validate.rs`: `VECTOR_CONFIG_TOML`, `VECTOR_CONFIG_JSON`, `VECTOR_CONFIG_YAML`, `VECTOR_CONFIG`, `VECTOR_CONFIG_DIR`, `VECTOR_DISABLE_ENV_VAR_INTERPOLATION`
- `src/unit_test.rs`: `VECTOR_CONFIG_DIR`
- `src/service.rs`: `VECTOR_CONFIG_DIR`, `VECTOR_DISABLE_ENV_VAR_INTERPOLATION`
- `src/graph.rs`: `VECTOR_CONFIG`, `VECTOR_CONFIG_DIR`, `VECTOR_DISABLE_ENV_VAR_INTERPOLATION`
- `src/config/cmd.rs`: `VECTOR_CONFIG`, `VECTOR_CONFIG_DIR`, `VECTOR_DISABLE_ENV_VAR_INTERPOLATION`
- `vdev/src/commands/release/homebrew.rs`: `VECTOR_VERSION`

**Runtime `env::var()` (10 occurrences):**
- `src/app.rs`: `VECTOR_LOG`
- `src/lib.rs`: `VECTOR_HOSTNAME`, `VECTOR_APP_NAME` (option_env!)
- `src/test_util/mod.rs`: `VECTOR_LOG`
- `src/sources/host_metrics/mod.rs`: `VECTOR_GENERATE_SCHEMA` (×2)
- `src/sinks/util/request_builder.rs`: `VECTOR_EXPERIMENTAL_REQUEST_BUILDER_CONCURRENCY`
- `lib/vector-config/src/schema/helpers.rs`: `VECTOR_GENERATE_SCHEMA` (set + remove)
- `lib/k8s-test-framework/src/interface.rs`: `VECTOR_TEST_KUBECTL`
- `vdev/src/utils/cargo.rs`: `VECTOR_VERSION`
- `tests/e2e/datadog/mod.rs`: `FAKE_INTAKE_VECTOR_ENDPOINT`

### Product name strings (6 literals + 6 event structs)

**String literals:**
- `src/lib.rs:144`: `"Vector"` (app name default)
- `src/top/cmd.rs:51`: `"Vector"` (top command app name)
- `lib/codecs/src/encoding/format/cef.rs:18`: `"Vector"` (CEF device product)
- `src/sinks/elasticsearch/config.rs:231`: `"Vector"` (X-Powered-By header)
- `src/service.rs:70,174`: `"Vector Service"` (Windows service name)

**Event structs in `src/internal_events/process.rs`:**
- `VectorStarted` → `SolStarted` + message `"Sol has started."`
- `VectorReloaded` → `SolReloaded` + message `"Sol has reloaded."`
- `VectorStopped` → `SolStopped` + message `"Sol has stopped."`
- `VectorQuit` → `SolQuit` + message `"Sol has quit."`
- `VectorConfigLoadError` → `SolConfigLoadError`
- `VectorRecoveryError` → `SolRecoveryError` + message `"Sol has failed to recover from a failed reload."`

### Config path defaults

- `lib/vector-core/src/lib.rs:63`: `/var/lib/vector/` → `/var/lib/sol/`
- `src/config/loading/mod.rs:349`: `/etc/vector/vector.yaml` → `/etc/sol/sol.yaml`
- `src/config/loading/mod.rs:356`: `{ProgramFiles}\Vector\config\vector.yaml` → `{ProgramFiles}\Sol\config\sol.yaml`
- `src/generate.rs`: 7 occurrences of `/var/lib/vector/` → `/var/lib/sol/`
- `src/config/watcher.rs:328`: `"vector.toml"` → `"sol.toml"`
- Docstrings in `src/cli.rs`, `src/validate.rs`, `src/unit_test.rs`, `src/graph.rs`, `src/config/cmd.rs`: `/etc/vector/vector.yaml` → `/etc/sol/sol.yaml`
- `lib/codecs/src/encoding/format/protobuf.rs:61`: `/etc/vector/` → `/etc/sol/`
- `src/transforms/lua/v2/mod.rs:70`: `/etc/vector/lua` → `/etc/sol/lua`

### Rust import paths to update

- `src/main.rs`: `extern crate vector` → `extern crate sol`
- `src/lib.rs`: `extern crate vector_lib` → `extern crate sol_lib`
- ~27 files with `use vector::*` → `use sol::*` (benches, tests, schema gen)
- All `use vector_lib::*` → `use sol_lib::*` across the codebase
- All `use vector_core::*` → `use sol_core::*`
- All `use vector_common::*` → `use sol_common::*`
- All `use vector_config::*` → `use sol_config::*`
- etc. for every renamed crate

### Systemd / distribution files

- `distribution/systemd/vector.service` → `sol.service`
- `distribution/systemd/hardened-vector.service` → `hardened-sol.service`
- `distribution/systemd/vector.default` → `sol.default`
- `config/vector.yaml` → `config/sol.yaml`
- Root `Cargo.toml` `[package.metadata.deb]` section: all paths and names

### Transformations
| Function | Input → Output | Invariant / Rule |
|---|---|---|
| `git mv` (directory renames) | `lib/vector-*` → `lib/sol-*` | 14 directories; `lib/opentelemetry-proto` stays (no "vector" in path) |
| Cargo.toml package name update | `name = "vector-*"` → `name = "sol-*"` | Must update all 21 crates + all cross-references atomically |
| Rust import rewrite | `use vector_*::` → `use sol_*::` | Only rewrite crate-name imports, not variable names or comments about Vec type |
| Env var rename | `VECTOR_*` → `SOL_*` | All 47 occurrences in .rs, preserving the suffix |
| Product string replace | `"Vector"` → `"Sol"` | Only the 6 identified product name strings |
| Config path replace | `/etc/vector/` → `/etc/sol/` | All hardcoded paths + docstrings + generated examples |

## Tasks

### 1. Rename library directories ([FR3](./DESIGN.md#fr3), [NFR4](./DESIGN.md#nfr4))
**Goal**: Move all `lib/vector-*` directories to `lib/sol-*` using `git mv` so git tracks the renames.
**Constraints**:
- Use `git mv` (not `mv`) for all moves
- `lib/opentelemetry-proto/` stays — its path has no "vector"
- `lib/vector-vrl/` → `lib/sol-vrl/` (parent dir, moves all children)
- `lib/vector-vrl-metrics/` → `lib/sol-vrl-metrics/` (separate from vector-vrl tree)
- 14 directories total
**Tests**: none (directory moves don't compile until Cargo.toml is updated)
**Verify**: `ls lib/sol-*` shows all expected directories; `ls lib/vector-*` shows nothing
**Acceptance criteria**:
- [ ] All 14 `lib/vector-*` directories renamed to `lib/sol-*`
- [ ] `lib/opentelemetry-proto/` unchanged
- [ ] `git status` shows renames, not deletes+adds
**Depends on**: (none)
**Time-box**: ~10 min

### 2. Update all Cargo.toml files ([FR1](./DESIGN.md#fr1), [FR2](./DESIGN.md#fr2))
**Goal**: Rename all package names, binary target, workspace member paths, dependency references, and feature flags from `vector-*` to `sol-*` across every Cargo.toml in the workspace.
**Constraints**:
- Root Cargo.toml: update `name`, `default-run`, `[[bin]]` name, `[workspace] members` paths, `[workspace.dependencies]` names and paths, `[dependencies]` names, `[dev-dependencies]` names, all feature flag references (`vector-lib/*` → `sol-lib/*`, `vector-vrl-functions/*` → `sol-vrl-functions/*`)
- Root Cargo.toml `[package.metadata.deb]`: update all paths (`/etc/vector/` → `/etc/sol/`, binary name references)
- Each lib crate Cargo.toml: update `name`, all dependency names and paths
- Package aliases: `lookup = { package = "vector-lookup" }` → `lookup = { package = "sol-lookup" }` in `sol-core`, `opentelemetry-proto`, `codecs`
- Non-vector-prefixed crates (`codecs`, `enrichment`, `dnstap-parser`, `file-source`, `file-source-common`, `prometheus-parser`, `docs-renderer`, `k8s-e2e-tests`): update their dependency references
- `vdev/Cargo.toml`: check for vector references
**Tests**: `cargo metadata` must succeed (validates dependency graph)
**Verify**: `cargo metadata --format-version=1 | grep '"name":"vector'` returns nothing
**Acceptance criteria**:
- [ ] All 21 crate `name` fields renamed to `sol-*`
- [ ] Binary target is `sol`
- [ ] All workspace member paths point to `lib/sol-*`
- [ ] All cross-crate dependency references updated
- [ ] `cargo metadata` succeeds
**Depends on**: task 1
**Time-box**: ~60 min

### 3. Update Rust import paths ([FR4](./DESIGN.md#fr4), [NFR2](./DESIGN.md#nfr2))
**Goal**: Update all `use vector*::`, `extern crate vector*`, and crate-name references in Rust source code.
**Constraints**:
- [NFR2](./DESIGN.md#nfr2): Do NOT rename variable names, comments about Vec/vector data structures, or string literals (those are separate tasks)
- Targeted replacements only:
  - `extern crate vector;` → `extern crate sol;` (src/main.rs)
  - `extern crate vector_lib;` → `extern crate sol_lib;` (src/lib.rs)
  - `use vector::` → `use sol::` (benches, tests, schema gen)
  - `use vector_lib::` → `use sol_lib::` (throughout)
  - `use vector_core::` → `use sol_core::` (throughout)
  - `use vector_common::` → `use sol_common::` (throughout)
  - `use vector_config::` → `use sol_config::` (throughout)
  - `use vector_config_common::` → `use sol_config_common::` (throughout)
  - `use vector_config_macros::` → `use sol_config_macros::` (throughout)
  - `use vector_common_macros::` → `use sol_common_macros::` (throughout)
  - `use vector_buffers::` → `use sol_buffers::` (throughout)
  - `use vector_stream::` → `use sol_stream::` (throughout)
  - `use vector_tap::` → `use sol_tap::` (throughout)
  - `use vector_top::` → `use sol_top::` (throughout)
  - `use vector_vrl_functions::` → `use sol_vrl_functions::` (throughout)
  - `use vector_vrl_metrics::` → `use sol_vrl_metrics::` (throughout)
  - `use vector_vrl_category::` → `use sol_vrl_category::` (throughout)
  - `use vector_vrl_cli::` → `use sol_vrl_cli::` (throughout)
  - `vector_opentelemetry_proto::` → `sol_opentelemetry_proto::` (throughout)
  - `vector_lib::` in path expressions (not just `use` — also `vector_lib::event::*`, etc.)
  - `vector_core::` in path expressions
  - `vector_common::` in path expressions
  - `vector_config::` in path expressions
  - `vector_buffers::` in path expressions
- Also update crate-level attributes: `#[cfg(feature = "vector-*")]` if any exist
- Also update `doc(cfg(...))` attributes if they reference vector crate features
- K8s e2e test functions: `extract_vector_started` → `extract_sol_started`, `assert_vector_started` → `assert_sol_started`, `wait_for_vector_started` → `wait_for_sol_started`, and their string references to `vector_started` metric → `sol_started`
**Tests**: `cargo build` must succeed
**Verify**: `cargo build 2>&1 | tail -1` shows success; `grep -r 'use vector_lib::' src/ lib/ | grep -v target/` returns nothing
**Acceptance criteria**:
- [ ] `cargo build` succeeds
- [ ] No remaining `use vector_*::` or `extern crate vector` in source (excluding comments and string literals)
- [ ] No remaining `vector_lib::`, `vector_core::`, `vector_common::` path expressions in source
**Depends on**: task 2
**Time-box**: ~90 min

### 4. Rename environment variables ([FR5](./DESIGN.md#fr5))
**Goal**: Replace all `VECTOR_*` environment variable names with `SOL_*` in Rust source.
**Constraints**:
- 34 clap `env = "VECTOR_*"` attributes → `env = "SOL_*"`
- 10 runtime `env::var("VECTOR_*")` calls → `env::var("SOL_*")`
- 1 compile-time `option_env!("VECTOR_APP_NAME")` → `option_env!("SOL_APP_NAME")`
- 2 unsafe env manipulation in `lib/sol-config/src/schema/helpers.rs` (`VECTOR_GENERATE_SCHEMA` → `SOL_GENERATE_SCHEMA`)
- Also update env var references in test files and vdev
- `VECTOR_LOG` in `src/app.rs` also sets the tracing filter target — `VECTOR_LOG=vector=debug` becomes `SOL_LOG=sol=debug`. The filter target `vector` must match the binary crate name (which is now `sol`).
**Tests**: `cargo build` must succeed
**Verify**: `grep -rn 'VECTOR_' src/ lib/ vdev/ --include='*.rs' | grep -v target/ | grep -v '// ' | wc -l` returns 0
**Acceptance criteria**:
- [ ] All `VECTOR_*` env var references in .rs files renamed to `SOL_*`
- [ ] `cargo build` succeeds
**Depends on**: task 3
**Time-box**: ~30 min

### 5. Rename product name strings and event structs ([FR7](./DESIGN.md#fr7))
**Goal**: Replace all user-visible "Vector" product name strings and internal event struct names.
**Constraints**:
- 6 string literals (see analysis: src/lib.rs, src/top/cmd.rs, codecs/cef.rs, elasticsearch config, service.rs)
- 6 event structs in `src/internal_events/process.rs`: rename structs and update message strings
- Update all import sites for renamed structs (src/app.rs, src/topology/controller.rs, etc.)
- `src/unit_test.rs:84`: `"Vector Unit Tests"` → `"Sol Unit Tests"`
- K8s e2e tests: `"vector_started"` metric string → `"sol_started"`
**Tests**: `cargo build` must succeed
**Verify**: `grep -rn '"Vector"' src/ lib/ --include='*.rs' | grep -v target/ | grep -v comment` returns only non-product-name uses (if any)
**Acceptance criteria**:
- [ ] All "Vector" product name strings changed to "Sol"
- [ ] All `Vector*` event structs renamed to `Sol*`
- [ ] `cargo build` succeeds
**Depends on**: task 4
**Time-box**: ~30 min

### 6. Rename config paths and defaults ([FR8](./DESIGN.md#fr8))
**Goal**: Update all hardcoded config file paths and generated config examples.
**Constraints**:
- `lib/sol-core/src/lib.rs`: `/var/lib/vector/` → `/var/lib/sol/`
- `src/config/loading/mod.rs`: `/etc/vector/vector.yaml` → `/etc/sol/sol.yaml`, Windows path too
- `src/generate.rs`: 7 occurrences of `/var/lib/vector/` → `/var/lib/sol/`
- `src/config/watcher.rs`: `"vector.toml"` → `"sol.toml"`
- Docstrings in cli.rs, validate.rs, unit_test.rs, graph.rs, config/cmd.rs: `/etc/vector/vector.yaml` → `/etc/sol/sol.yaml`
- `lib/codecs/src/encoding/format/protobuf.rs`: `/etc/vector/` → `/etc/sol/`
- `src/transforms/lua/v2/mod.rs`: `/etc/vector/lua` → `/etc/sol/lua`
- Rename file: `config/vector.yaml` → `config/sol.yaml`
- Regression test configs: rename `vector.yaml` files under `regression/`
**Tests**: `cargo test -p sol` must pass
**Verify**: `grep -rn '/etc/vector\|/var/lib/vector' src/ lib/ --include='*.rs' | grep -v target/` returns nothing
**Acceptance criteria**:
- [ ] All hardcoded paths updated
- [ ] `config/vector.yaml` renamed to `config/sol.yaml`
- [ ] `cargo build` succeeds
**Depends on**: task 5
**Time-box**: ~30 min

### 7. Rename systemd, Docker, and distribution files ([FR9](./DESIGN.md#fr9), [FR10](./DESIGN.md#fr10))
**Goal**: Update all distribution packaging files.
**Constraints**:
- `git mv` systemd files: `vector.service` → `sol.service`, `hardened-vector.service` → `hardened-sol.service`, `vector.default` → `sol.default`
- Update content: `ExecStart=/usr/bin/vector` → `ExecStart=/usr/bin/sol`, descriptions, documentation URLs, environment file refs
- Root Cargo.toml `[package.metadata.deb]`: update `name`, `assets` paths, `conf-files`, `maintainer-scripts`
- Docker files in `distribution/docker/`: update binary name references
- `distribution/install.sh`: update binary and path references
- `demo/Dockerfile.vector`: rename to `demo/Dockerfile.sol`, update content
**Tests**: none (no runtime validation for packaging files)
**Verify**: `grep -rn 'vector' distribution/ --include='*.service' --include='*.default'` returns nothing
**Acceptance criteria**:
- [ ] Systemd files renamed and content updated
- [ ] Debian package metadata updated
- [ ] Docker files updated
- [ ] Install script updated
**Depends on**: task 6
**Time-box**: ~30 min

### 8. Update CI/CD workflow files ([FR11](./DESIGN.md#fr11))
**Goal**: Update GitHub Actions workflows for the new binary and crate names.
**Constraints**:
- `VECTOR_LOG=vector=debug` → `SOL_LOG=sol=debug` in workflow env sections
- Docker image/container name references
- Cache key references containing "vector"
- Binary name references in build/test steps
- Volume names: `vector-target`, `vector-cargo-cache`, `vector-rustup-cache`
- Workflow step names referencing "vector" (cosmetic but worth updating)
**Tests**: none (CI validation requires push)
**Verify**: `grep -rn 'VECTOR_LOG' .github/` returns nothing
**Acceptance criteria**:
- [ ] All `VECTOR_*` env vars in workflows renamed to `SOL_*`
- [ ] Binary name references updated
- [ ] Docker image/volume names updated
**Depends on**: task 7
**Time-box**: ~30 min

### 9. Add attribution notice and final verification ([FR12](./DESIGN.md#fr12), [NFR1](./DESIGN.md#nfr1), [NFR3](./DESIGN.md#nfr3))
**Goal**: Add MPL-2.0 attribution and run final build + test.
**Constraints**:
- Add attribution to README.md or a NOTICE file: "Built on technology originally from the Vector project (MPL-2.0), Copyright Datadog, Inc."
- Do NOT modify the LICENSE file
- Do NOT remove any existing copyright headers
- Run full `cargo build` and `cargo test -p sol`
**Tests**: `cargo test -p sol`
**Verify**: `cargo build && cargo test -p sol`
**Acceptance criteria**:
- [ ] Attribution notice present
- [ ] `cargo build` succeeds
- [ ] `cargo test -p sol` passes (same pass rate as before rename)
- [ ] LICENSE file unchanged
- [ ] Existing copyright headers intact
**Depends on**: task 8
**Time-box**: ~30 min

## Sessions

### Session 1 — Cargo + imports (~2.5H)
Tasks: 1, 2, 3
**Skills**: `rust-software-engineer`, `rust-build`
**Checkpoint**: `cargo build 2>&1 | tail -1` shows `Finished`
**Commit point**: yes — commit after checkpoint passes

### Session 2 — Branding + config (~2H)
Tasks: 4, 5, 6
**Skills**: `rust-software-engineer`
**Checkpoint**: `cargo build && cargo test -p sol -- --test-threads=4 2>&1 | tail -5`
**Commit point**: yes — commit after checkpoint passes

### Session 3 — Distribution + CI + attribution (~1.5H)
Tasks: 7, 8, 9
**Skills**: `rust-software-engineer`, `rust-build`
**Checkpoint**: `cargo build && cargo test -p sol -- --test-threads=4 2>&1 | tail -5`
**Commit point**: yes — commit after checkpoint passes

## Quality gates (post-session review)
- [ ] Acceptance criteria: all green above
- [ ] Code review: implementation matches [DESIGN.md](./DESIGN.md) intent
- [ ] Code organization: file placement, module structure, naming conventions (refactoring pass)
- [ ] Code quality: no new complexity, clean types, no duplication
- [ ] Security review: OWASP check, dependency audit, no secrets exposed
- [ ] Observability: `sol_*` metrics confirmed via `internal_metrics` source
- [ ] Performance: no regressions (binary size check before/after)
