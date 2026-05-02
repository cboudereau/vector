use vector_lib::{
    event::{MetricView, OtelMetric},
    stream::batcher::limiter::ItemBatchSize,
};

use super::request_builder::SUMMARY_STAT_FIELD_COUNT;

const F64_BYTE_SIZE: usize = 8;
const I64_BYTE_SIZE: usize = 8;

/// GreptimeDBBatchSizer is a batch sizer for metrics.
#[derive(Default)]
pub struct GreptimeDBBatchSizer;

impl GreptimeDBBatchSizer {
    pub fn estimated_size_of(&self, item: &OtelMetric) -> usize {
        // Metric name.
        item.name().len()
        // Metric namespace, with an additional 1 to account for the namespace separator.
        + item.namespace().map(|s| s.len() + 1).unwrap_or(0)
        // Metric tags, with an additional 1 per tag to account for the tag key/value separator.
        + item.tags().map(|t| {
            t.iter_single().map(|(k, v)| {
                k.len() + 1 + v.map(|v| v.len()).unwrap_or(0)
            })
            .sum::<usize>()
        })
            .unwrap_or(0)
            // timestamp
            + I64_BYTE_SIZE
            +
        // value size
            match item.view() {
                MetricView::Sum { .. } | MetricView::Gauge { .. } | MetricView::Set { ..} => F64_BYTE_SIZE,
                MetricView::Histogram { bounds, .. }  => F64_BYTE_SIZE * (bounds.len() + SUMMARY_STAT_FIELD_COUNT),
                MetricView::Summary { quantiles, .. } => F64_BYTE_SIZE * (quantiles.len() + SUMMARY_STAT_FIELD_COUNT),
                MetricView::ExponentialHistogram { .. } => F64_BYTE_SIZE,
            }
    }
}

impl ItemBatchSize<OtelMetric> for GreptimeDBBatchSizer {
    fn size(&self, item: &OtelMetric) -> usize {
        self.estimated_size_of(item)
    }
}
