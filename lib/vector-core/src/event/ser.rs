use bytes::{Buf, BufMut};
use crossbeam_utils::atomic::AtomicCell;
use enumflags2::{BitFlags, FromBitsError, bitflags};
use prost::Message;
use snafu::Snafu;
use vector_buffers::encoding::{AsMetadata, Encodable};

use super::{Event, EventArray, proto};

#[derive(Debug, Snafu)]
pub enum EncodeError {
    #[snafu(display("the provided buffer was too small to fully encode this item"))]
    BufferTooSmall,
}

#[derive(Debug, Snafu)]
pub enum DecodeError {
    #[snafu(display(
        "the provided buffer could not be decoded as a valid Protocol Buffers payload"
    ))]
    InvalidProtobufPayload,
    #[snafu(display("unsupported encoding metadata for this context"))]
    UnsupportedEncodingMetadata,
}
/// Controls the on-disk encoding format for new records written to disk buffers.
///
/// Set once at process startup from the global config before any buffer is opened.
/// Defaults to [`BufferFormat::Vector`] so existing deployments are unaffected.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum BufferFormat {
    /// Legacy Vector protobuf encoding (`proto::EventArray`). Default.
    #[default]
    Vector,
    /// OTel protobuf encoding (`OtlpBufferBatch`). Use after migration is complete.
    Otlp,
    /// Transition mode: writes OTel records, reads both Vector and OTel records.
    /// Enables zero-downtime drain of existing Vector-encoded buffers.
    Migrate,
}

/// Process-wide buffer format toggle. Read by `get_metadata`, `can_decode`, `encode`, and
/// `decode`. Must be set before any disk buffer is opened.
pub static BUFFER_FORMAT: AtomicCell<BufferFormat> = AtomicCell::new(BufferFormat::Vector);

/// Flags for describing the encoding scheme used by our primary event types that flow through buffers.
///
/// # Stability
///
/// This enumeration should never have any flags removed, only added.  This ensures that previously
/// used flags cannot have their meaning changed/repurposed after-the-fact.
#[bitflags]
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum EventEncodableMetadataFlags {
    /// Chained encoding scheme that first tries to decode as `EventArray` and then as `Event`, as a
    /// way to support gracefully migrating existing v1-based disk buffers to the new
    /// `EventArray`-based architecture.
    ///
    /// All encoding uses the `EventArray` variant, however.
    DiskBufferV1CompatibilityMode = 0b01,

    /// Record payload is encoded as OTel protobuf (`OtlpBufferBatch`) rather than the
    /// Vector-native `proto::EventArray`. Set by [`BufferFormat::Otlp`] and
    /// [`BufferFormat::Migrate`].
    OtlpEncoding = 0b10,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventEncodableMetadata(BitFlags<EventEncodableMetadataFlags>);

impl EventEncodableMetadata {
    fn contains(self, flag: EventEncodableMetadataFlags) -> bool {
        self.0.contains(flag)
    }
}

impl From<EventEncodableMetadataFlags> for EventEncodableMetadata {
    fn from(flag: EventEncodableMetadataFlags) -> Self {
        Self(BitFlags::from(flag))
    }
}

impl From<BitFlags<EventEncodableMetadataFlags>> for EventEncodableMetadata {
    fn from(flags: BitFlags<EventEncodableMetadataFlags>) -> Self {
        Self(flags)
    }
}

impl TryFrom<u32> for EventEncodableMetadata {
    type Error = FromBitsError<EventEncodableMetadataFlags>;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        BitFlags::try_from(value).map(Self)
    }
}

impl AsMetadata for EventEncodableMetadata {
    fn into_u32(self) -> u32 {
        self.0.bits()
    }

    fn from_u32(value: u32) -> Option<Self> {
        EventEncodableMetadata::try_from(value).ok()
    }
}

impl Encodable for EventArray {
    type Metadata = EventEncodableMetadata;
    type EncodeError = EncodeError;
    type DecodeError = DecodeError;

    fn get_metadata() -> Self::Metadata {
        use EventEncodableMetadataFlags::{DiskBufferV1CompatibilityMode, OtlpEncoding};
        match BUFFER_FORMAT.load() {
            BufferFormat::Vector => DiskBufferV1CompatibilityMode.into(),
            BufferFormat::Otlp | BufferFormat::Migrate => {
                (DiskBufferV1CompatibilityMode | OtlpEncoding).into()
            }
        }
    }

    fn can_decode(metadata: Self::Metadata) -> bool {
        use EventEncodableMetadataFlags::{DiskBufferV1CompatibilityMode, OtlpEncoding};
        match BUFFER_FORMAT.load() {
            BufferFormat::Vector => {
                metadata.contains(DiskBufferV1CompatibilityMode)
                    && !metadata.contains(OtlpEncoding)
            }
            BufferFormat::Otlp => metadata.contains(OtlpEncoding),
            BufferFormat::Migrate => {
                metadata.contains(DiskBufferV1CompatibilityMode)
                    || metadata.contains(OtlpEncoding)
            }
        }
    }

    fn encode<B>(self, buffer: &mut B) -> Result<(), Self::EncodeError>
    where
        B: BufMut,
    {
        proto::EventArray::from(self)
            .encode(buffer)
            .map_err(|_| EncodeError::BufferTooSmall)
    }

    fn decode<B>(metadata: Self::Metadata, buffer: B) -> Result<Self, Self::DecodeError>
    where
        B: Buf + Clone,
    {
        if metadata.contains(EventEncodableMetadataFlags::DiskBufferV1CompatibilityMode) {
            proto::EventArray::decode(buffer.clone())
                .map(Into::into)
                .or_else(|_| {
                    proto::EventWrapper::decode(buffer)
                        .map(|pe| EventArray::from(Event::from(pe)))
                        .map_err(|_| DecodeError::InvalidProtobufPayload)
                })
        } else {
            Err(DecodeError::UnsupportedEncodingMetadata)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset_buffer_format(format: BufferFormat) {
        BUFFER_FORMAT.store(format);
    }

    #[test]
    fn buffer_format_vector_get_metadata_has_only_v1_flag() {
        reset_buffer_format(BufferFormat::Vector);
        let metadata = EventArray::get_metadata();
        assert!(
            metadata.contains(EventEncodableMetadataFlags::DiskBufferV1CompatibilityMode),
            "Vector mode must set DiskBufferV1CompatibilityMode"
        );
        assert!(
            !metadata.contains(EventEncodableMetadataFlags::OtlpEncoding),
            "Vector mode must not set OtlpEncoding"
        );
    }

    #[test]
    fn buffer_format_otlp_get_metadata_has_otlp_flag() {
        reset_buffer_format(BufferFormat::Otlp);
        let metadata = EventArray::get_metadata();
        assert!(
            metadata.contains(EventEncodableMetadataFlags::OtlpEncoding),
            "Otlp mode must set OtlpEncoding"
        );
    }

    #[test]
    fn buffer_format_migrate_get_metadata_has_otlp_flag() {
        reset_buffer_format(BufferFormat::Migrate);
        let metadata = EventArray::get_metadata();
        assert!(
            metadata.contains(EventEncodableMetadataFlags::OtlpEncoding),
            "Migrate mode writes OTel records"
        );
    }

    #[test]
    fn buffer_format_vector_can_decode_only_v1_records() {
        reset_buffer_format(BufferFormat::Vector);
        let v1_meta: EventEncodableMetadata =
            EventEncodableMetadataFlags::DiskBufferV1CompatibilityMode.into();
        let otlp_meta: EventEncodableMetadata =
            EventEncodableMetadataFlags::OtlpEncoding.into();

        assert!(EventArray::can_decode(v1_meta), "Vector mode must accept V1 records");
        assert!(!EventArray::can_decode(otlp_meta), "Vector mode must reject OTel records");
    }

    #[test]
    fn buffer_format_otlp_can_decode_only_otlp_records() {
        reset_buffer_format(BufferFormat::Otlp);
        let v1_meta: EventEncodableMetadata =
            EventEncodableMetadataFlags::DiskBufferV1CompatibilityMode.into();
        let otlp_meta: EventEncodableMetadata =
            EventEncodableMetadataFlags::OtlpEncoding.into();

        assert!(!EventArray::can_decode(v1_meta), "Otlp mode must reject V1 records");
        assert!(EventArray::can_decode(otlp_meta), "Otlp mode must accept OTel records");
    }

    #[test]
    fn buffer_format_migrate_can_decode_both_formats() {
        reset_buffer_format(BufferFormat::Migrate);
        let v1_meta: EventEncodableMetadata =
            EventEncodableMetadataFlags::DiskBufferV1CompatibilityMode.into();
        let otlp_meta: EventEncodableMetadata =
            EventEncodableMetadataFlags::OtlpEncoding.into();

        assert!(EventArray::can_decode(v1_meta), "Migrate mode must accept V1 records");
        assert!(EventArray::can_decode(otlp_meta), "Migrate mode must accept OTel records");
    }
}
