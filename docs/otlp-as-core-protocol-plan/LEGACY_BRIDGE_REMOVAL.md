# Legacy Bridge Removal: Design Discovery

## Why bridges exist

`OtelLog::get(path)` takes a `TargetPath` (VRL's abstract path type) and returns
a `Value`. To do this, it currently:

1. Converts OtelLog → LogEvent via `to_log_event()` (builds a `Value` tree)
2. Calls `LogEvent::get(path)` (traverses the `Value` tree using `TargetPath`)
3. Returns the result

This exists because **proto fields and VRL `Value` are different type systems**.
`TargetPath` knows how to traverse a `Value` tree but not proto structs.

## The discovery: TargetPath is not needed outside VRL

`TargetPath` is only needed inside VRL transforms (remap, filter, route, etc.)
where the VRL runtime resolves paths against a `VrlTarget`. The `VrlTarget`
already builds its own `Value` projection — it does NOT use `OtelLog::get()`.

Outside VRL, callers use `OtelLog::get("field_name")` as a convenience to read
fields by string name. But they could use proto accessors directly:

| Current (via bridge) | Replacement (direct proto) |
|---------------------|---------------------------|
| `log.get("body")` | `log.body().map(any_value_to_vrl)` |
| `log.get("timestamp")` | `log.get_timestamp()` |
| `log.get("source_type")` | `log.get_source_type()` |
| `log.get("some_attr")` | `log.attribute("some_attr").map(any_value_to_vrl)` |
| `log.insert("key", val)` | `log.set_attribute("key", vrl_value_to_any_value(&val))` |
| `log.remove("key")` | `log.remove_attribute("key")` |

## log_schema() is not OTel-compliant

The `log_schema()` config defines Vector's legacy field naming:

| log_schema key | TargetPath string | OTel proto field | Match? |
|----------------|-------------------|------------------|--------|
| `message_key` | `"body"` (was `"message"`) | `LogRecord.body` | ✅ (after rename) |
| `timestamp_key` | `"timestamp"` | `LogRecord.time_unix_nano` | ❌ |
| `host_key` | `"host"` | `Resource.attributes["host.name"]` | ❌ |
| `source_type_key` | `"source_type"` | `Resource.attributes["source_type"]` | ❌ |

These names were designed for Vector's flat `LogEvent` model. In the OTel model:
- Timestamps are `time_unix_nano` (nanoseconds, not a formatted string)
- Host is a resource attribute `host.name`, not a top-level field
- Source type is a resource attribute, not a top-level field

**`log_schema()` should be deprecated.** Callers should use proto field names directly.
The VRL migration tool must be updated to handle these remaining mismatches.

### VRL migration tool gaps

The VRL migration tool currently handles:
- ✅ `.message` → `.body`
- ✅ `.host` → `.resource.attributes."host.name"`
- ✅ `.source_type` → `.attributes."pipeline.source_type"`
- ✅ `.timestamp` → `.time_unix_nano`

But production code still uses `log_schema()` to get field names:
```rust
// Current (legacy)
log.get(log_schema().timestamp_key().unwrap())  // gets "timestamp"

// Should be (OTel-native)
log.time_unix_nano()  // gets u64 nanoseconds directly
```

The `log_schema()` indirection must be removed from production code alongside
the bridges. The VRL migration tool handles user VRL programs; the production
code migration is manual (replace `log_schema()` calls with proto accessors).

### log_schema() removal plan

1. **Audit all `log_schema()` callers** — ~50 call sites across sources, sinks, codecs
2. **Replace with proto accessors** — `timestamp_key()` → `time_unix_nano()`, etc.
3. **Deprecate `log_schema()` config** — keep for one release with warning
4. **Remove `log_schema()`** — field names are the proto field names, no mapping

## Two separate path systems

After cleanup, there will be two distinct path systems:

### 1. VRL paths (inside VRL transforms only)

Used by `VrlTarget` to project OTel events into VRL `Value` trees.
The VRL runtime resolves paths against these projections.

```
VrlTarget::OtelLog(Value, EventMetadata)
  → otel_log_event_to_value() builds Value with .body, .attributes, .resource, etc.
  → VRL program accesses .body, .attributes."key", etc. on this Value
  → value_to_otel_log_event() writes back to proto
```

`TargetPath` is used here — inside the VRL runtime only.

### 2. Proto accessors (everywhere else)

Used by sources, sinks, transforms, codecs, API — any code that reads/writes
OTel event fields directly.

```rust
// Read
let body = otel_log.body();                    // Option<&AnyValue>
let attr = otel_log.attribute("service.name"); // Option<&AnyValue>
let ts = otel_log.time_unix_nano();            // u64

// Write
otel_log.set_body(string_value("hello"));
otel_log.set_attribute("key".into(), string_value("value"));
```

No `TargetPath`, no `Value` conversion, no bridge.

## What gets removed

| Component | Lines | Status |
|-----------|-------|--------|
| `OtelLog::get(TargetPath)` | ~5 | Replace callers with proto accessors |
| `OtelLog::insert(TargetPath, Value)` | ~10 | Replace callers with proto accessors |
| `OtelLog::remove(TargetPath)` | ~10 | Replace callers with proto accessors |
| `OtelLog::to_log_event()` | ~85 | Remove (only used by get/insert/remove + Serialize) |
| `OtelLog::from_log_event()` | ~71 | Remove (only used by insert write-back + Event::from) |
| `OtelSpan::to_log_event()` | ~63 | Remove (used by trace_to_log transform + serializers) |
| `OtelMetric::to_legacy_metric()` | ~263 | Remove (used by metric sinks/transforms) |
| `OtelMetric::from_legacy_metric()` | ~253 | Remove (used by Event::from(Metric)) |
| `LogEvent`, `Metric`, `TraceEvent` types | ~3,000+ | Remove when no consumers remain |

## Implementation order

1. **Replace `get()`/`insert()`/`remove()` callers** with proto accessors (~40 files)
2. **Remove `to_log_event()` from OtelLog** — fix Serialize impls to use proto directly
3. **Remove `from_log_event()`** — fix Event::from(LogEvent) to build OtelLog directly
4. **Remove `to_log_event()` from OtelSpan** — fix trace_to_log + serializers
5. **Remove metric bridges** — fix metric sinks/transforms to use OtelMetric API
6. **Remove legacy types** — delete LogEvent, Metric, TraceEvent

Each step compiles and tests pass independently.

## Attempted approach: replace get()/insert() with proto-native Value

Tried replacing `to_log_event()` inside `get()` with a direct `to_value()` that
builds the Value from proto fields. Result: **109 failures** because the Value
layout differs from what `to_log_event()` produces:

- `to_log_event()` expands KvList body → top-level fields
- `to_log_event()` formats timestamps as `DateTime<Utc>`
- `to_log_event()` hoists `host.name` and `source_type` from resource to top-level

The 465 callers depend on this exact layout. A proto-native `to_value()` produces
a different shape (raw proto field names, nanos as integers, no hoisting).

**Conclusion:** Can't replace `get()` implementation until all 465 callers are
migrated to use proto accessors directly. This is a large effort (multiple sessions)
that should be done caller-by-caller, not by changing the `get()` implementation.
