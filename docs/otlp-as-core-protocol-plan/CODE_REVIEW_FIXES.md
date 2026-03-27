# Code Review Fixes Plan

Addresses issues found in the code review of Steps 7 + 4.

## Issues by Priority

### High — Correctness

| # | Issue | File | Fix |
|---|-------|------|-----|
| 9 | `byte_size` uses `size_of_val` (stack size, not heap) — `max_trace_size_bytes` doesn't work | `tail_sampling/transform.rs:118` | Use `event.size_of()` (ByteSizeOf trait) which includes heap |
| 7 | `crc32_hash` imported from `sinks::opentelemetry` into `transforms::tail_sampling` — inverted dependency | `tail_sampling/policies.rs:233` | Inline `crc32fast::hash()` directly |
| 12 | URI parse panic in LB sink on malformed endpoints | `grpc.rs:436,466` | Replace `expect()` with error log + skip |

### Medium — Performance

| # | Issue | File | Fix |
|---|-------|------|-----|
| 4 | `insertion_order.retain()` is O(n) on every oversized trace | `tail_sampling/transform.rs:136` | Use HashMap for O(1) removal from insertion_order |
| 5 | `on_tick()` scans all traces every second | `tail_sampling/transform.rs:160` | Use BinaryHeap/BTreeMap ordered by deadline for O(log n) expiry |
| 3 | K8s resolver creates `Client::try_default()` on every resolve() | `load_balancing.rs:219` | Create client once in constructor |
| 6 | `to_emit.extend(trace.spans.iter().cloned())` clones all spans | `tail_sampling/transform.rs:182` | Collect trace_ids first, then drain spans |

### Medium — Reliability

| # | Issue | File | Fix |
|---|-------|------|-----|
| 10 | Resolver JoinHandle dropped — panics go unnoticed | `grpc.rs:154` | Store handle in sink, abort on drop |
| 11 | No batch size limit in LB sink inner loop | `grpc.rs:488-527` | Add event count check against batch_settings.size |

### Low — Minor

| # | Issue | File | Fix |
|---|-------|------|-----|
| 8 | Unnecessary Mutex in RateLimiting (single-threaded context) | `tail_sampling/policies.rs:244` | Use Cell<f64> or RefCell |
| 13 | `dd_ts_to_nanos` casts negative i64 to u64 | `metrics.rs` | Clamp to 0 for negative timestamps |
| 14 | DD span `service` not on Resource | `traces.rs` | Extract first span's service → Resource attribute |

---

## Phases

### Phase 1: High-priority correctness fixes

1. Fix `byte_size` in tail_sampling to use `ByteSizeOf::size_of()`
2. Inline `crc32fast::hash()` in probabilistic policy (remove sink dependency)
3. Replace URI `expect()` with error handling in LB sink

### Phase 2: Performance fixes

4. Replace `insertion_order` VecDeque with indexed structure for O(1) removal
5. Add deadline-ordered expiry to `on_tick()` (BTreeMap<Instant, Vec<TraceId>>)
6. Create K8s client once in K8sResolver constructor
7. Drain spans instead of cloning on emit

### Phase 3: Reliability fixes

8. Store resolver JoinHandle, abort on sink drop
9. Add batch size limit to LB sink inner loop

### Phase 4: Minor fixes

10. Replace Mutex with Cell in RateLimiting
11. Clamp negative timestamps in DD source
12. Set service.name on Resource from first DD span
