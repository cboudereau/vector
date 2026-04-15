# DNSTap Parser Migration to OtelLog (B1)

## Context

The dnstap parser at `lib/dnstap-parser/src/parser.rs` is the last hard
blocker in the OTLP migration's source side. It has **17 functions** taking
`&mut LogEvent` that cooperatively mutate a single event as they walk the
dnstap protobuf frame.

It has two production entry points:
1. `src/sources/dnstap/mod.rs:295` — the dnstap source, which constructs a
   `LogEvent::default()`, passes it to `DnstapParser::parse`, and wraps the
   result in an `Event` via `Event::from(log_event)`.
2. `lib/dnstap-parser/src/vrl_functions/parse_dnstap.rs:198` — the
   `parse_dnstap` VRL function, which also constructs a temporary
   `LogEvent::default()`, calls `DnstapParser::parse`, and returns
   `event.value().clone()` (a `Value` tree).

## Key observation

Both entry points ultimately operate on a VRL `Value` tree:
- The source wraps the `LogEvent` in `Event::Log(OtelLog::from_log_event(...))`
- The VRL function extracts the inner `Value` and returns it

The `&mut LogEvent` throughout the parser is therefore **incidental** —
the parser only needs a `&mut Value` tree to mutate. LogEvent is a
thin wrapper that happens to be the convenient container today.

## Design

### Target API

Change the internal helper signature:

```rust
// Before:
fn insert<'a, V>(
    event: &mut LogEvent,
    prefix: impl ValuePath<'a>,
    path: impl ValuePath<'a>,
    value: V,
) -> Option<Value> { ... }

// After:
fn insert<'a, V>(
    value_tree: &mut Value,
    prefix: impl ValuePath<'a>,
    path: impl ValuePath<'a>,
    value: V,
) -> Option<Value>
where V: Into<Value> {
    value_tree.insert(&prefix.concat(path).into(), value)
}
```

All 17 `&mut LogEvent` function signatures become `&mut Value`.

### Public entry point

```rust
// Before:
pub fn parse(
    event: &mut LogEvent,
    frame: Bytes,
    parsing_options: DnsParserOptions,
) -> Result<()>

// After:
pub fn parse(
    value_tree: &mut Value,
    frame: Bytes,
    parsing_options: DnsParserOptions,
) -> Result<()>
```

### Call-site migrations

**dnstap source** (`src/sources/dnstap/mod.rs:270-326`):

```rust
// Before:
let mut log_event = LogEvent::default();
// ...
DnstapParser::parse(&mut log_event, frame, opts)?;
// ... (insert_source_metadata calls)
Some(Event::from(log_event))

// After:
let mut log = OtelLog::new(Default::default());
log.modify_as_value(|v| {
    if let Some(obj) = v.as_object_mut() {
        // hoist from top-level Value::Object
    }
    DnstapParser::parse(v, frame, opts)
})?;
// ... insert_source_metadata via MetadataInsertable works directly on OtelLog
Some(Event::Log(log))
```

The `modify_as_value` closure amortizes the OtelLog round-trip across
**all** parser inserts, not just per-function — the entire frame parse is
one legacy-layout round-trip.

**VRL function** (`lib/dnstap-parser/src/vrl_functions/parse_dnstap.rs:196-213`):

```rust
// Before:
let mut event = LogEvent::default();
DnstapParser::parse(&mut event, ..., opts)?;
Ok(event.value().clone())

// After:
let mut value = Value::Object(Default::default());
DnstapParser::parse(&mut value, ..., opts)?;
Ok(value)
```

Simpler — no LogEvent at all.

## Field routing

The parser writes fields at paths like `.messageType`, `.queryMessage.header`,
`.responseMessage.rrs[0].rType`, etc. These are arbitrary nested paths, not
the limited set of top-level fields with OTel semantics.

**`OtelLog::from_value_map` field routing** (via `apply_value_legacy_layout`):
- `body`, `message` → `LogRecord.body`
- `timestamp` → `LogRecord.time_unix_nano`
- `severity_text`, `severity_number`, `trace_id`, `span_id` → native fields
- `source_type`, `host` → resource attributes
- **Everything else** → `LogRecord.attributes` as KeyValue entries

Dnstap output uses paths like `.messageType`, `.queryTime`, `.queryZone`,
`.query`/`.response` (nested objects), etc. None of these collide with
the OTel-native field names (except possibly the user-set `.timestamp`
at the top level via `insert_source_metadata`, which is already handled).

So **no field remapping is needed** — the parser output naturally lands
in `LogRecord.attributes` (as a nested map value) when routed through
`from_value_map`.

## Test strategy

The parser has extensive unit tests (~20 fixtures in `parser.rs` test
module). These construct a `LogEvent` via `LogEvent::default()`, call
`DnstapParser::parse`, then assert field-path reads on `event.get(...)`.

### Option A: migrate the tests to OtelLog

Change each test to construct `Value::Object(Default::default())`,
call the new `parse(&mut value, ...)`, and assert `value.get(path)`.
**Downside**: ~20 tests × 5-15 assertions each = lots of mechanical diff.

### Option B: wrap in a test helper

Keep the test signatures unchanged; add a helper:

```rust
#[cfg(test)]
fn test_parse(event: &mut LogEvent, frame: Bytes) -> Result<()> {
    let mut value = event.value().clone();
    DnstapParser::parse(&mut value, frame, Default::default())?;
    *event.value_mut() = value;
    Ok(())
}
```

Point tests at `test_parse`; **production** callers migrate to the
new `&mut Value` API. Keeps the test diff small.

### Option C: don't migrate tests

Have the tests construct `Value::Object(Default::default())` using
`value!({})` and treat it like a map. Add helper methods on `Value`
for `get` with path. VRL's `Value` already has all the necessary
APIs.

**Recommended: Option B** — minimal test diff, no behavioral surprise.

## Execution checklist

1. [ ] Add a `#[cfg(test)]` helper wrapping the new `&mut Value` API
   in the old `&mut LogEvent` signature.
2. [ ] Change 17 internal helper signatures from `&mut LogEvent` to
   `&mut Value`.
3. [ ] Change `DnstapParser::insert` body to call `Value::insert`
   directly (no `PathPrefix::Event` wrapping).
4. [ ] Change `pub fn parse` signature.
5. [ ] Update VRL function caller to drop the `LogEvent` intermediate.
6. [ ] Update the dnstap source caller to construct `OtelLog` directly
   and use `modify_as_value` around the parser call.
7. [ ] Delete `LogEvent` import from parser.rs (leave only in test helper).
8. [ ] Run `cargo test -p vector --lib sources::dnstap` and the
   parse_dnstap VRL function tests.
9. [ ] Run the full `cargo test -p vector --lib` for regression check.

## Performance expectations

**Before**: `DnstapParser::parse` is called once per frame on `&mut LogEvent`.
LogEvent's `insert` is O(path depth). With ~20-50 inserts per DNS
message, total cost is O(50 × avg depth) per message.

**After**:
- VRL function: same — operates on `&mut Value` directly.
- Source: one `to_value_legacy_layout` + 50 direct Value inserts + one
  `apply_value_legacy_layout` = amortized per-frame cost much closer to
  LogEvent performance.

**Not expected to regress.**

## Risk

Low. The parser writes via a single `insert` helper → one code path to
change. All 17 `&mut LogEvent` signatures are mechanical rewrites. Tests
continue to exercise the same inserts. The two public callers are
well-defined.

## Effort estimate

- Design doc: this document (done).
- Implementation: 2–3 hours.
- Test migration (option B): 15 min.
- Integration test verification: 30 min.

Total: **half-day**.
