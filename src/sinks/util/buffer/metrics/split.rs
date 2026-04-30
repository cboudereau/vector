use std::collections::VecDeque;

use vector_lib::event::{MetricKind, MetricValue, OtelMetric};

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
        let value = input.value();
        let MetricValue::AggregatedSummary {
            quantiles,
            count,
            sum,
        } = value
        else {
            return SplitIterator::single(input);
        };

        let name = input.name().to_string();
        let namespace = input.namespace().map(|s| s.to_string());
        let tags = input.tags();
        let timestamp = input.timestamp();
        let metadata = input.metadata().clone();
        let kind = input.kind();

        let mut metrics = VecDeque::new();

        let count_metric =
            OtelMetric::new_counter(format!("{name}_count"), kind, count as f64)
                .with_namespace(namespace.clone())
                .with_tags(tags.clone())
                .with_timestamp(timestamp)
                .with_metadata(metadata.clone());
        metrics.push_back(count_metric);

        for quantile in quantiles {
            let mut qtags = tags.clone().unwrap_or_default();
            qtags.replace(String::from("quantile"), quantile.to_quantile_string());
            let q_metric = if kind == MetricKind::Incremental {
                OtelMetric::new_gauge_delta(&name, quantile.value)
            } else {
                OtelMetric::new_gauge(&name, quantile.value)
            }
            .with_namespace(namespace.clone())
            .with_tags(Some(qtags))
            .with_timestamp(timestamp)
            .with_metadata(metadata.clone());
            metrics.push_back(q_metric);
        }

        let sum_metric =
            OtelMetric::new_counter(format!("{name}_sum"), kind, sum)
                .with_namespace(namespace)
                .with_tags(tags)
                .with_timestamp(timestamp)
                .with_metadata(metadata);
        metrics.push_back(sum_metric);

        SplitIterator::multiple(metrics)
    }
}

#[cfg(test)]
mod tests {
    use vector_lib::event::{MetricKind, OtelMetric, metric::Quantile};

    use super::*;

    #[test]
    fn split_non_summary_passes_through() {
        let counter = OtelMetric::new_counter("counter", MetricKind::Incremental, 42.0);

        let mut splitter = MetricSplitter::<AggregatedSummarySplitter>::default();
        let results: Vec<_> = splitter.split(counter).collect();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn split_aggregated_summary() {
        let quantiles = vec![
            Quantile {
                quantile: 0.5,
                value: 100.0,
            },
            Quantile {
                quantile: 0.99,
                value: 200.0,
            },
        ];
        let summary = OtelMetric::new_summary("requests", &quantiles, 10, 500.0);

        let mut splitter = MetricSplitter::<AggregatedSummarySplitter>::default();
        let results: Vec<_> = splitter.split(summary).collect();
        // count + 2 quantiles + sum = 4
        assert_eq!(results.len(), 4);
    }
}
