# Disk Buffer Migration: Vector Proto → OTel Proto

This document specifies a zero-downtime disk buffer migration strategy using Vector's existing
`Encodable` metadata bitflag mechanism.

---

## 1. How the Disk Buffer Encodes Events Today

Every record written to disk has the following on-disk layout (from
`lib/vector-buffers/src/variants/disk_v2/record.rs`):

```
Record {
    checksum: u32,    // CRC32C(BE(id) + BE(metadata) + payload)
    id:       u64,    // monotonically increasing record ID
    metadata: u32,    // Encodable::Metadata bitfield — per-record encoding version
    payload:  [u8],   // protobuf-encoded EventArray
}
```

The `metadata` u32 is a **per-record bitfield** defined by `EventEncodableMetadataFlags` in
`lib/vector-core/src/event/ser.rs`. The reader calls `T::can_decode(metadata)` before
attempting to decode each record, and the writer stamps `T::get_metadata()` onto every new record.

This is not a file-level header — it is stored **individually per record** and covered by the
CRC32C checksum, so it cannot be patched after the fact without rewriting the record.

### Existing precedent: DiskBufferV1CompatibilityMode

Vector has already done this migration once, for the v1 → v2 buffer format change. The flag:

```rust
// lib/vector-core/src/event/ser.rs
pub enum EventEncodableMetadataFlags {
    DiskBufferV1CompatibilityMode = 0b1,
}
```

During that migration, the decoder tried `EventArray` first, then fell back to the old
`EventWrapper` format:

```rust
fn decode(metadata, buffer) {
    proto::EventArray::decode(buffer.clone())
        .or_else(|_| proto::EventWrapper::decode(buffer).map(EventArray::from))
}
```

The writer always used the new format. This produced a **natural drain**: old records were read
with the fallback path until exhausted, then all new records used the new format. Zero downtime,
zero manual drain step. The OTel migration uses the exact same mechanism.

---

## 2. The Migration Toggle

### 2.1 New metadata flag

Add a new flag to `EventEncodableMetadataFlags`:

```rust
// lib/vector-core/src/event/ser.rs
#[bitflags]
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EventEncodableMetadataFlags {
    /// Existing flag — v1 single-event compatibility.
    DiskBufferV1CompatibilityMode = 0b01,

    /// New flag — record payload is encoded as OTel protobuf (ExportLogsServiceRequest /
    /// ExportMetricsServiceRequest / ExportTraceServiceRequest) rather than the Vector-native
    /// proto::EventArray.
    OtlpEncoding = 0b10,
}
```

Rules:
- `DiskBufferV1CompatibilityMode` present, `OtlpEncoding` absent → legacy Vector proto (current)
- `DiskBufferV1CompatibilityMode` present, `OtlpEncoding` present → OTel proto (new)
- Both flags are independent; the CRC covers the metadata, so a record's encoding cannot be
  misidentified.

### 2.2 New topology config option

Add a `buffer_format` option to the global config (or per-component buffer config):

```toml
[global_options]
# Controls the on-disk encoding for new records written to disk buffers.
# - "vector"  → legacy proto::EventArray (current default, reads old buffers)
# - "otlp"    → OTel protobuf (write new records in OTel format)
# - "migrate" → read both formats; write in OTel format (migration mode)
buffer_format = "migrate"  # set this during the upgrade window
```

Or equivalently per-component:

```toml
[sinks.my_sink.buffer]
type = "disk"
max_size = 268435488
buffer_format = "migrate"
```

### 2.3 Encoding implementation

```rust
impl Encodable for EventArray {
    type Metadata = EventEncodableMetadata;
    type EncodeError = EncodeError;
    type DecodeError = DecodeError;

    fn get_metadata() -> Self::Metadata {
        match BUFFER_FORMAT.load() {
            BufferFormat::Vector  => EventEncodableMetadataFlags::DiskBufferV1CompatibilityMode.into(),
            BufferFormat::Otlp    => (DiskBufferV1CompatibilityMode | OtlpEncoding).into(),
            BufferFormat::Migrate => (DiskBufferV1CompatibilityMode | OtlpEncoding).into(),
            // "migrate" write path is identical to "otlp" — the difference is only in can_decode
        }
    }

    fn can_decode(metadata: Self::Metadata) -> bool {
        match BUFFER_FORMAT.load() {
            // Legacy mode: only reads Vector proto records
            BufferFormat::Vector  => metadata.contains(DiskBufferV1CompatibilityMode)
                                     && !metadata.contains(OtlpEncoding),

            // OTel mode: only reads OTel proto records
            BufferFormat::Otlp    => metadata.contains(OtlpEncoding),

            // Migrate mode: reads BOTH formats — this is the drain window
            BufferFormat::Migrate => metadata.contains(DiskBufferV1CompatibilityMode)
                                     || metadata.contains(OtlpEncoding),
        }
    }

    fn encode<B: BufMut>(self, buffer: &mut B) -> Result<(), Self::EncodeError> {
        match BUFFER_FORMAT.load() {
            BufferFormat::Vector => {
                // Current behaviour: encode as proto::EventArray
                proto::EventArray::from(self).encode(buffer)
                    .map_err(|_| EncodeError::BufferTooSmall)
            }
            BufferFormat::Otlp | BufferFormat::Migrate => {
                // New behaviour: encode as OTel signal batches
                encode_as_otlp(self, buffer)
            }
        }
    }

    fn decode<B: Buf + Clone>(metadata: Self::Metadata, buffer: B)
        -> Result<Self, Self::DecodeError>
    {
        if metadata.contains(OtlpEncoding) {
            decode_from_otlp(buffer)
        } else if metadata.contains(DiskBufferV1CompatibilityMode) {
            // Existing fallback chain: EventArray → EventWrapper
            proto::EventArray::decode(buffer.clone())
                .map(Into::into)
                .or_else(|_| proto::EventWrapper::decode(buffer)
                    .map(|pe| EventArray::from(Event::from(pe)))
                    .map_err(|_| DecodeError::InvalidProtobufPayload))
        } else {
            Err(DecodeError::UnsupportedEncodingMetadata)
        }
    }
}
```

The `BUFFER_FORMAT` is a process-wide `AtomicCell<BufferFormat>` set at startup from the
config, before any buffer is opened.

---

## 3. OTel Encoding for the Buffer

### 3.1 Signal-segregated batches within a single record

The current Vector buffer stores a `proto::EventArray` which is a heterogeneous bag of
`EventWrapper` oneofs. The OTel wire format has three separate request types
(`ExportLogsServiceRequest`, `ExportMetricsServiceRequest`, `ExportTraceServiceRequest`).

For the disk buffer, we do not need to match the wire format exactly — the buffer is internal
storage. The simplest approach is a **length-prefixed triple** in one record payload:

```
OtlpBufferRecord {
    logs_len:    u32,                        // byte length of the logs payload (0 if absent)
    logs:        ExportLogsServiceRequest,   // protobuf, logs_len bytes
    metrics_len: u32,                        // byte length of the metrics payload
    metrics:     ExportMetricsServiceRequest,
    traces_len:  u32,
    traces:      ExportTraceServiceRequest,
}
```

All three sections are always present (length = 0 means empty). This preserves the single-record
atomic unit that the disk buffer's CRC and acknowledgement model relies on.

Alternatively, since `prost` already handles the three types, a thin wrapper proto can be defined:

```protobuf
// lib/vector-core/proto/otlp_buffer.proto
syntax = "proto3";

import "opentelemetry/proto/collector/logs/v1/logs_service.proto";
import "opentelemetry/proto/collector/metrics/v1/metrics_service.proto";
import "opentelemetry/proto/collector/trace/v1/trace_service.proto";

message OtlpBufferBatch {
  opentelemetry.proto.collector.logs.v1.ExportLogsServiceRequest logs = 1;
  opentelemetry.proto.collector.metrics.v1.ExportMetricsServiceRequest metrics = 2;
  opentelemetry.proto.collector.trace.v1.ExportTraceServiceRequest traces = 3;
}
```

This is valid protobuf: absent fields encode as zero bytes. A batch with only logs occupies the
same bytes as just the logs payload. No padding or length prefix needed.

### 3.2 encode_as_otlp / decode_from_otlp

```rust
fn encode_as_otlp<B: BufMut>(array: EventArray, buffer: &mut B) -> Result<(), EncodeError> {
    let batch = OtlpBufferBatch::from(array); // splits EventArray into signal batches
    batch.encode(buffer).map_err(|_| EncodeError::BufferTooSmall)
}

fn decode_from_otlp<B: Buf>(buffer: B) -> Result<EventArray, DecodeError> {
    let batch = OtlpBufferBatch::decode(buffer)
        .map_err(|_| DecodeError::InvalidProtobufPayload)?;
    Ok(EventArray::from(batch))
}
```

`From<EventArray> for OtlpBufferBatch` splits by signal type using the existing
`EventArray::Logs` / `EventArray::Metrics` / `EventArray::Traces` variants.
`From<OtlpBufferBatch> for EventArray` reconstructs using the existing OTel → Vector conversion
in `lib/opentelemetry-proto/src/`.

---

## 4. Migration Runbook

### Step 1 — Before upgrade: enable migrate mode

Update `vector.toml`:

```toml
[global_options]
buffer_format = "migrate"
```

Restart Vector. From this point:
- The reader can decode **both** Vector proto and OTel proto records.
- The writer writes **OTel proto** records for all new events.
- Old Vector proto records in the existing data files are drained naturally as the reader
  advances through them.

No data is lost. No manual drain or shutdown is required. The disk buffer directory and file
IDs are unchanged.

### Step 2 — Wait for full drain

Monitor the buffer depth metric `vector_buffer_events` or `vector_buffer_byte_size`. When both
reach zero (or when `vector_buffer_events_count` stops decreasing while in-flight), all legacy
records have been consumed.

Alternatively, inspect the ledger: when the reader file ID catches up to the writer file ID
established before the restart, all pre-migration files have been fully consumed.

### Step 3 — After drain: switch to otlp mode

Update `vector.toml`:

```toml
[global_options]
buffer_format = "otlp"
```

Restart Vector. From this point:
- The reader only accepts OTel proto records. Any lingering legacy record would be rejected with
  `UnsupportedEncodingMetadata` — but since the drain completed in Step 2, none should exist.
- `migrate` mode can be left on permanently if a mixed fleet is acceptable; the overhead is a
  single flag check per record decode.

### Step 4 — Remove the option (future major version)

In a future major version, `buffer_format = "vector"` and `"migrate"` can be removed. The
`DiskBufferV1CompatibilityMode` flag remains in the enum (never remove flags) but
`can_decode` stops accepting it, matching the precedent set when v1 compatibility was dropped.

---

## 5. Per-File Mixing Is Safe

Because the metadata is stored **per record** (not per file or per ledger), a single data file
can contain a mix of Vector proto and OTel proto records without ambiguity. The reader processes
records sequentially and dispatches to the correct decoder based on each record's own metadata
bits. The CRC covers the metadata, so a corrupted or misidentified metadata field is detected
before decoding is attempted.

This means the migration window does not require a clean file boundary. Vector can be restarted
mid-file and the reader will continue from exactly where it left off, correctly decoding whichever
format each record happens to use.

---

## 6. Implementation Checklist

| Status | Task | Location | Notes |
|---|---|---|---|
| ✅ Done | Add `OtlpEncoding = 0b10` flag | `lib/vector-core/src/event/ser.rs` | Never remove old flags |
| ✅ Done | Add `BufferFormat` enum | `lib/vector-core/src/event/ser.rs` | `Vector` / `Otlp` / `Migrate` |
| ✅ Done | Add process-wide `BUFFER_FORMAT` atomic | `lib/vector-core/src/event/ser.rs` | Default `Vector` |
| ✅ Done | Wire `get_metadata()` to `BUFFER_FORMAT` | `lib/vector-core/src/event/ser.rs` | Stamps correct flags |
| ✅ Done | Wire `can_decode()` to `BUFFER_FORMAT` | `lib/vector-core/src/event/ser.rs` | Accepts/rejects by flag |
| ✅ Done | Unit tests for all three modes | `lib/vector-core/src/event/ser.rs` | 6 tests |
| ⬜ Todo | Define `OtlpBufferBatch` proto | `lib/vector-core/proto/otlp_buffer.proto` | Thin wrapper proto |
| ⬜ Todo | Add proto to `build.rs` compilation | `lib/vector-core/build.rs` | |
| ⬜ Todo | Implement `encode_as_otlp` | `lib/vector-core/src/event/ser.rs` | Via `OtlpBufferBatch` |
| ⬜ Todo | Implement `decode_from_otlp` | `lib/vector-core/src/event/ser.rs` | Via existing OTel conversions |
| ⬜ Todo | Wire `encode()` to `BUFFER_FORMAT` | `lib/vector-core/src/event/ser.rs` | Currently always `proto::EventArray` |
| ⬜ Todo | Wire `decode()` to `OtlpEncoding` flag | `lib/vector-core/src/event/ser.rs` | Currently ignores the flag |
| ⬜ Todo | Add `buffer_format` to `GlobalOptions` | `lib/vector-core/src/config/global_options.rs` | Default `"vector"` |
| ⬜ Todo | Startup wiring: `BUFFER_FORMAT.store(...)` | startup / `src/app.rs` | Before any buffer opens |
| ⬜ Todo | Startup guard: force `Migrate` if existing buffer detected with `buffer_format = "otlp"` | startup | Prevents crash on unreadable records |
| ⬜ Todo | Add golden tests for mixed-format decode | `lib/vector-core/src/event/tests/` | Write Vector record, then OTel, read both |
| ⬜ Todo | Add integration test for migrate runbook | `tests/` | Simulate restart mid-buffer |
| ⬜ Todo | Update `vector validate` to warn on `buffer_format = "vector"` post-migration | `src/validate.rs` | Nudge users to migrate |
