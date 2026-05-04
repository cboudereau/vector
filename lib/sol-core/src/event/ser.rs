use bytes::{Buf, BufMut};
use snafu::Snafu;
use sol_buffers::encoding::{AsMetadata, Encodable};

use super::{EventArray, otlp};

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

/// Flags for describing the encoding scheme used by our primary event types that flow through buffers.
///
/// # Stability
///
/// This enumeration should never have any flags removed, only added.  This ensures that previously
/// used flags cannot have their meaning changed/repurposed after-the-fact.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct EventEncodableMetadata(u32);

const OTLP_ENCODING_FLAG: u32 = 0b10;

impl EventEncodableMetadata {
    fn is_otlp(self) -> bool {
        self.0 & OTLP_ENCODING_FLAG != 0
    }
}

impl AsMetadata for EventEncodableMetadata {
    fn into_u32(self) -> u32 {
        self.0
    }

    fn from_u32(value: u32) -> Option<Self> {
        Some(Self(value))
    }
}

impl Encodable for EventArray {
    type Metadata = EventEncodableMetadata;
    type EncodeError = EncodeError;
    type DecodeError = DecodeError;

    fn get_metadata() -> Self::Metadata {
        EventEncodableMetadata(OTLP_ENCODING_FLAG)
    }

    fn can_decode(metadata: Self::Metadata) -> bool {
        metadata.is_otlp()
    }

    fn encode<B>(self, buffer: &mut B) -> Result<(), Self::EncodeError>
    where
        B: BufMut,
    {
        otlp::encode_as_otlp(&self, buffer).map_err(|_| EncodeError::BufferTooSmall)
    }

    fn decode<B>(metadata: Self::Metadata, buffer: B) -> Result<Self, Self::DecodeError>
    where
        B: Buf + Clone,
    {
        if metadata.is_otlp() {
            let bytes = {
                let remaining = buffer.remaining();
                let mut b = bytes::BytesMut::with_capacity(remaining);
                b.put(buffer);
                b.freeze()
            };
            otlp::decode_from_otlp(bytes).map_err(|_| DecodeError::InvalidProtobufPayload)
        } else {
            Err(DecodeError::UnsupportedEncodingMetadata)
        }
    }
}
