use std::collections::VecDeque;

use vector_lib::event::{MetricValue, OtelMetric, metric::MetricData};

#[allow(clippy::large_enum_variant)]
enum SplitState {
    Single(Option<OtelMetric>),
    Multiple(VecDeque<OtelMetric>),
}

/// An iterator that returns the result of a metric split operation.
pub struct SplitIterator {
    state: SplitState,
}

impl SplitIterator {
    /// Creates an iterator for a single metric.
    pub const fn single(metric: OtelMetric) -> Self {
        Self {
            state: SplitState::Single(Some(metric)),
        }
    }

    /// Creates an iterator for multiple metrics.
    pub fn multiple<I>(metrics: I) -> Self
    where
        I: Into<VecDeque<OtelMetric>>,
    {
        Self {
            state: SplitState::Multiple(metrics.into()),
        }
    }
}

impl Iterator for SplitIterator {
    type Item = OtelMetric;

    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.state {
            SplitState::Single(metric) => metric.take(),
            SplitState::Multiple(metrics) => metrics.pop_front(),
        }
    }
}

/// Splits a metric into potentially multiple metrics.
pub trait MetricSplit {
    fn split(&mut self, input: OtelMetric) -> SplitIterator;
}

/// A self-contained metric splitter.
pub struct MetricSplitter<S> {
    splitter: S,
}

impl<S: MetricSplit> MetricSplitter<S> {
    pub fn split(&mut self, input: OtelMetric) -> SplitIterator {
        self.splitter.split(input)
    }
}

impl<S: Default> Default for MetricSplitter<S> {
    fn default() -> Self {
        Self {
            splitter: S::default(),
        }
    }
}

impl<S> From<S> for MetricSplitter<S> {
    fn from(splitter: S) -> Self {
        Self { splitter }
    }
}

/// A splitter that separates an aggregated summary into its various parts.
#[derive(Clone, Copy, Debug, Default)]
pub struct AggregatedSummarySplitter;

impl MetricSplit for AggregatedSummarySplitter {
    fn split(&mut self, input: OtelMetric) -> SplitIterator {
        let (series, data, metadata) = input.into_metric_parts();
        match &data.value {
            // If it's not an aggregated summary, just send it on.
            MetricValue::Counter { .. }
            | MetricValue::Gauge { .. }
            | MetricValue::Set { .. }
            | MetricValue::Distribution { .. }
            | MetricValue::AggregatedHistogram { .. } => {
                SplitIterator::single(OtelMetric::from_metric_parts(series, data, metadata))
            }
            MetricValue::AggregatedSummary { .. } => {
                let (time, kind, value) = data.into_parts();
                let (quantiles, count, sum) = match value {
                    MetricValue::AggregatedSummary {
                        quantiles,
                        count,
                        sum,
                    } => (quantiles, count, sum),
                    _ => unreachable!("metric value must be aggregated summary to be here"),
                };

                let mut metrics = VecDeque::new();

                let mut count_series = series.clone();
                count_series.name_mut().name_mut().push_str("_count");
                let count_data = MetricData::from_parts(
                    time,
                    kind,
                    MetricValue::Counter {
                        value: count as f64,
                    },
                );
                let count_metadata = metadata.clone();
                metrics.push_back(OtelMetric::from_metric_parts(count_series, count_data, count_metadata));

                for quantile in quantiles {
                    let mut quantile_series = series.clone();
                    quantile_series
                        .replace_tag(String::from("quantile"), quantile.to_quantile_string());
                    let quantile_data = MetricData::from_parts(
                        time,
                        kind,
                        MetricValue::Gauge {
                            value: quantile.value,
                        },
                    );
                    let quantile_metadata = metadata.clone();
                    metrics.push_back(OtelMetric::from_metric_parts(
                        quantile_series,
                        quantile_data,
                        quantile_metadata,
                    ));
                }

                let mut sum_series = series;
                sum_series.name_mut().name_mut().push_str("_sum");
                let sum_data =
                    MetricData::from_parts(time, kind, MetricValue::Counter { value: sum });
                let sum_metadata = metadata;
                metrics.push_back(OtelMetric::from_metric_parts(sum_series, sum_data, sum_metadata));

                SplitIterator::multiple(metrics)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use vector_lib::event::{Metric, MetricKind, MetricValue, OtelMetric, metric::Quantile};

    use super::*;

    fn otel(m: Metric) -> OtelMetric {
        let (s, d, md) = m.into_parts();
        OtelMetric::from_metric_parts(s, d, md)
    }

    #[test]
    fn split_non_summary_passes_through() {
        let counter = otel(Metric::new(
            "counter",
            MetricKind::Incremental,
            MetricValue::Counter { value: 42.0 },
        ));

        let mut splitter = MetricSplitter::<AggregatedSummarySplitter>::default();
        let results: Vec<_> = splitter.split(counter).collect();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn split_aggregated_summary() {
        let summary = otel(Metric::new(
            "requests",
            MetricKind::Absolute,
            MetricValue::AggregatedSummary {
                quantiles: vec![
                    Quantile {
                        quantile: 0.5,
                        value: 100.0,
                    },
                    Quantile {
                        quantile: 0.99,
                        value: 200.0,
                    },
                ],
                count: 10,
                sum: 500.0,
            },
        ));

        let mut splitter = MetricSplitter::<AggregatedSummarySplitter>::default();
        let results: Vec<_> = splitter.split(summary).collect();
        // count + 2 quantiles + sum = 4
        assert_eq!(results.len(), 4);
    }
}
