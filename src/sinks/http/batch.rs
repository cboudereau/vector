//! Batch settings for the `http` sink.

use sol_lib::{
    ByteSizeOf, EstimatedJsonEncodedSizeOf, codecs::encoding::Framer, event::Event,
    stream::batcher::limiter::ItemBatchSize,
};

use sol_lib::codecs::Encoder;

/// Uses the configured encoder to determine batch sizing.
#[derive(Default, Clone)]
pub(super) struct HttpBatchSizer {
    pub(super) encoder: Encoder<Framer>,
}

impl ItemBatchSize<Event> for HttpBatchSizer {
    fn size(&self, item: &Event) -> usize {
        match self.encoder.serializer() {
            sol_lib::codecs::encoding::Serializer::Json(_) => {
                item.estimated_json_encoded_size_of().get()
            }
            _ => item.size_of(),
        }
    }
}
