# T15: VRL Alias Removal Campaign

## Scope

Remove legacy VRL field aliases from `to_value_legacy_layout` so that
OtelLog exposes OpenTelemetry-native field names in VRL programs.

## Aliases to remove

| Old (legacy) | New (OTel-native) | Scope |
|-------------|-------------------|-------|
| `.message` | `.body` | Body field — already uses `"body"`, but KvList bodies expand to top-level |
| `.timestamp` | `.time_unix_nano` | Timestamp — currently coerced to `Value::Timestamp` |
| `.host` | `.resource.host.name` | Hoisted from resource attributes |
| `.source_type` | `.resource.source_type` | Hoisted from resource attributes |
| `.tags."key"` | `.attributes."key"` | Already top-level in current layout |

## Impact estimate

- ~339 `.message` references (many are field names in sinks, not VRL aliases)
- ~255 `.timestamp` references (most are structural, not the alias)
- ~1 `.host` hoisting reference
- `vector vrl-migrate` tool exists to rewrite user configs

## Execution plan

1. Modify `to_value_legacy_layout` to emit OTel-native field names
2. Modify `apply_value_legacy_layout` to read OTel-native field names
3. Fix compilation errors (structural changes)
4. Fix test assertions (field name changes)
5. Update `vector vrl-migrate` rules if needed

## Breaking change

This is a user-facing breaking change. Users must run
`vector vrl-migrate` on their configs before upgrading.
