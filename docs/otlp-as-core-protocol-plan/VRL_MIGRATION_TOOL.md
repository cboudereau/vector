# VRL Migration Tool: Vector Field Semantics → OTel Field Semantics

This document specifies the `vector vrl-migrate` tool that automatically rewrites user VRL
programs from Vector internal field semantics to OTel field semantics.

---

## 1. Why a Migration Tool Is Needed

Step 5 of the migration replaces the Vector internal event model with OTel native types.
VRL programs written against the Vector model use field paths like `.message`, `.timestamp`,
`.host`, and `.tags` that no longer exist in the OTel model.

Without a migration tool, every user would need to manually audit and rewrite their VRL
programs. The tool automates ~91% of rewrites, flagging the remaining ~9% for human review.

---

## 2. Invocation

```bash
# Rewrite a single file in-place
vector vrl-migrate --in-place my_transform.vrl

# Rewrite and write to a new file
vector vrl-migrate my_transform.vrl > my_transform_otlp.vrl

# Dry-run: show diff without modifying
vector vrl-migrate --diff my_transform.vrl

# Rewrite all VRL in a Vector config file
vector vrl-migrate --config vector.toml
```

Every rewritten expression is annotated with a `# MIGRATED:` comment explaining the change.
Lines that could not be automatically rewritten are annotated with `# REVIEW:`.

---

## 3. Rewrite Rules

### Pass 1: Structural rewrites (mechanical, always safe)

| Pattern | Rewrite | Rule ID |
|---|---|---|
| `.message` (standalone, root assignment target) | `.` | LOG-01 |
| `.timestamp` | `.time_unix_nano` | LOG-02 |
| `.source_type` | `.attributes."pipeline.source_type"` | LOG-03 |
| `.host` | `.resource.attributes."host.name"` | LOG-04 |
| `.tags` (map access) | `.attributes` | LOG-05 |
| `.tags.<key>` | `.attributes."<key>"` | LOG-06 |
| `.level` / `.severity` | `.severity_text` | LOG-07 |
| `.span_id` | `.span_id` (unchanged — OTel native) | TRC-01 |
| `.trace_id` | `.trace_id` (unchanged — OTel native) | TRC-02 |
| `.parent_span_id` | `.parent_span_id` (unchanged) | TRC-03 |
| `%vector.source_type` | `%pipeline.source_type` | META-01 |
| `%vector.source_id` | `%pipeline.source_id` | META-02 |

### Pass 2: Semantic rewrites (context-sensitive)

| Pattern | Rewrite | Rule ID | Comment |
|---|---|---|---|
| `.message` on RHS of non-root assignment (`.x = .message`) | `.x = string!(.)` | SEM-08 | `.message` is event root; coerced to string to avoid assigning whole event object |
| `.message` in string concatenation (`.message + suffix`) | `string!(.) + suffix` | SEM-09 | log body is event root; coerced to string for concatenation |
| `get_field!(., "message")` | `string!(.)` | SEM-01 | dynamic field access to log body |
| `exists(.message)` | `true` | SEM-02 | root always exists |
| `del(.message)` | `# REVIEW: del(.) would delete the entire event` | SEM-03 | cannot be automatically migrated |
| `encode_json(.message)` | `encode_json(.)` | SEM-04 | encode whole event as JSON |
| `parse_json(.message)` | `parse_json(string!(.))` | SEM-05 | parse log body as JSON |
| `is_string(.message)` | `is_string(.)` | SEM-06 | type check on root |
| `assert_eq!(.message, ...)` | `assert_eq!(string!(.), ...)` | SEM-07 | assert with coercion |

**Note on SEM-08/SEM-09:** `LOG-01` rewrites `.message` to `.` unconditionally. SEM-08 and
SEM-09 run in a second sub-pass over the result of `LOG-01` and tighten any case where `.`
appears as a value expression in a non-root context. The VRL `TypeState` is used to confirm
that `.` is `Bytes`-typed in these positions; if type information is absent, the tool emits
the rewrite with a `# MIGRATED:` comment for operator review.

### Pass 3: Metric field rewrites

| Pattern | Rewrite | Rule ID |
|---|---|---|
| `.name` (metric name) | `.name` (unchanged — OTel native) | MET-01 |
| `.namespace` | `.attributes."metric.namespace"` | MET-02 |
| `.tags` (metric tags map) | `.attributes` | MET-03 |
| `.tags.<key>` | `.attributes."<key>"` | MET-04 |
| `.kind` (`"absolute"` / `"incremental"`) | Depends on metric type — `# REVIEW:` | MET-05 |
| `.value.counter.value` | `.data_points[0].as_double` | MET-06 |
| `.value.gauge.value` | `.data_points[0].as_double` | MET-07 |

---

## 4. Coverage Analysis

Estimated coverage on real-world VRL programs after all rewrite passes:

| Category | Auto-rewrite rate | Notes |
|---|---|---|
| Log field path rewrites (LOG-*) | ~98% | Structural, mechanical |
| Semantic `.message` patterns (SEM-*) | ~85% | Context-sensitive; SEM-08/09 cover most cases |
| Metric field rewrites (MET-*) | ~80% | `.kind` requires human review |
| Dynamic field paths (`get_field!(., var)`) | ~70% | Literal variable heuristic (see §5) |
| **Overall** | **~91%** | Remaining 9% flagged with `# REVIEW:` |

---

## 5. Limitations and Manual Review Cases

### Dynamic field paths

When a field path is computed at runtime via a variable, the tool cannot always determine
whether the variable holds `"message"` or another field name:

```vrl
field = get_env_var!("LOG_FIELD")
value = get_field!(., field)  # REVIEW: cannot determine if field == "message"
```

**Heuristic:** If the variable is assigned a string literal earlier in the same scope
(e.g., `field = "message"`), the tool inlines the literal and applies LOG-01:

```vrl
field = "message"
value = get_field!(., field)
# → becomes:
field = "."  # MIGRATED: "message" is now root
value = .    # MIGRATED: get_field!(., ".") → .
```

If the variable is not a known literal, the tool emits `# REVIEW:`.

### `del(.message)`

Deleting the log body would delete the entire event root. The tool flags this:

```vrl
del(.message)
# → becomes:
# REVIEW: del(.) would delete the entire event — did you mean to clear the body?
# del(.)
```

### Metric `.kind` field

The Vector `kind` field (`"absolute"` / `"incremental"`) maps to OTel `AggregationTemporality`
but the mapping depends on the metric type and is not mechanical. The tool flags all `.kind`
references for human review.

### User-defined type aliases

If the user has defined a VRL function that wraps `.message` access, the tool cannot rewrite
inside the function body without understanding the call sites. All such cases are flagged.

---

## 6. Implementation Plan

| Task | Location | Step |
|---|---|---|
| AST walker infrastructure | `lib/vrl/src/migrate/` | 5 |
| Pass 1 structural rewrites (LOG-*, META-*) | `lib/vrl/src/migrate/structural.rs` | 5 |
| Pass 2 semantic rewrites (SEM-*) with TypeState | `lib/vrl/src/migrate/semantic.rs` | 5 |
| Pass 3 metric rewrites (MET-*) | `lib/vrl/src/migrate/metric.rs` | 5 |
| Dynamic path literal heuristic | `lib/vrl/src/migrate/dynamic.rs` | 5 |
| `# MIGRATED:` / `# REVIEW:` comment injection | `lib/vrl/src/migrate/annotate.rs` | 5 |
| CLI integration (`vector vrl-migrate`) | `src/cli/vrl_migrate.rs` | 5 |
| Test corpus: Vector project's own VRL programs | `lib/vrl/tests/migrate/` | 5 |
| Coverage metric: assert ≥91% auto-rewrite on corpus | `lib/vrl/tests/migrate/coverage_test.rs` | 5 |
