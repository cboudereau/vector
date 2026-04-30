use std::{
    fmt::Display,
    io::{self, Write},
};

use bytes::{BufMut, BytesMut};
use tokio_util::codec::Encoder;
use vector_lib::event::{MetricKind, MetricTags, MetricView, OtelMetric, StatisticKind};

use crate::{
    internal_events::StatsdInvalidMetricError,
    sinks::util::encode_namespace,
};

/// Error type for errors that can never happen, but for use with `Encoder`.
///
/// For the StatsD encoder, the encoding operation is infallible. However, as `Encoder<T>` requires
/// that the associated error type can be created by `From<io::Error>`, we can't simply use
/// `Infallible`. This type exists to bridge that gap, acting as a marker type for "we emit no
/// errors" while supporting the trait bounds on `Encoder<T>::Error`.
#[derive(Debug)]
pub struct InfallibleIo;

impl From<io::Error> for InfallibleIo {
    fn from(_: io::Error) -> Self {
        Self
    }
}

#[derive(Debug, Clone)]
pub(super) struct StatsdEncoder {
    default_namespace: Option<String>,
}

impl StatsdEncoder {
    /// Creates a new `StatsdEncoder` with the given default namespace, if any.
    pub const fn new(default_namespace: Option<String>) -> Self {
        Self { default_namespace }
    }
}

impl<'a> Encoder<&'a OtelMetric> for StatsdEncoder {
    type Error = InfallibleIo;

    fn encode(&mut self, metric: &'a OtelMetric, buf: &mut BytesMut) -> Result<(), Self::Error> {
        let namespace = metric.namespace().or(self.default_namespace.as_deref());
        let name = encode_namespace(namespace, '.', metric.name());
        let tags = metric.tags().as_ref().map(encode_tags);

        match metric.view() {
            MetricView::Sum { value } => {
                encode_and_write_single_event(buf, &name, tags.as_deref(), value, "c", None);
            }
            MetricView::Gauge { value } => {
                match metric.kind() {
                    MetricKind::Incremental => encode_and_write_single_event(
                        buf,
                        &name,
                        tags.as_deref(),
                        format!("{value:+}"),
                        "g",
                        None,
                    ),
                    MetricKind::Absolute => {
                        encode_and_write_single_event(buf, &name, tags.as_deref(), value, "g", None)
                    }
                };
            }
            MetricView::Distribution { bounds, counts } => {
                let metric_type = match metric.distribution_statistic_kind() {
                    StatisticKind::Histogram => "h",
                    StatisticKind::Summary => "d",
                };

                let mut pairs: Vec<(f64, u64)> = bounds.iter().copied()
                    .zip(counts.iter().copied())
                    .collect();
                pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
                let mut compressed: Vec<(f64, u64)> = Vec::new();
                for (val, cnt) in pairs {
                    if let Some(last) = compressed.last_mut() {
                        if last.0 == val {
                            last.1 += cnt;
                            continue;
                        }
                    }
                    compressed.push((val, cnt));
                }

                for (val, cnt) in compressed {
                    encode_and_write_single_event(
                        buf,
                        &name,
                        tags.as_deref(),
                        val,
                        metric_type,
                        Some(cnt as u32),
                    );
                }
            }
            MetricView::Set { values } => {
                for val in values {
                    encode_and_write_single_event(buf, &name, tags.as_deref(), val, "s", None);
                }
            }
            _ => {
                emit!(StatsdInvalidMetricError {
                    error: "Unsupported metric type for StatsD.".into(),
                });

                return Ok(());
            }
        };

        Ok(())
    }
}

// Note that if multi-valued tags are present, this encoding may change the order from the input
// event, since the tags with multiple values may not have been grouped together.
// This is not an issue, but noting as it may be an observed behavior.
fn encode_tags(tags: &MetricTags) -> String {
    let parts: Vec<_> = tags
        .iter_all()
        .map(|(name, tag_value)| match tag_value {
            Some(value) => format!("{name}:{value}"),
            None => name.to_owned(),
        })
        .collect();

    // `parts` is already sorted by key because of BTreeMap
    parts.join(",")
}

fn encode_and_write_single_event<V: Display>(
    buf: &mut BytesMut,
    metric_name: &str,
    metric_tags: Option<&str>,
    val: V,
    metric_type: &str,
    sample_rate: Option<u32>,
) {
    let mut writer = buf.writer();

    write!(&mut writer, "{metric_name}:{val}|{metric_type}").unwrap();

    if let Some(sample_rate) = sample_rate
        && sample_rate != 1
    {
        write!(&mut writer, "|@{}", 1.0 / f64::from(sample_rate)).unwrap();
    };

    if let Some(t) = metric_tags {
        write!(&mut writer, "|#{t}").unwrap();
    };

    writeln!(&mut writer).unwrap();
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "sources-statsd")]
    use vector_lib::event::{
        MetricKind, OtelMetric, StatisticKind,
    };
    use vector_lib::{
        event::{MetricTags, metric::TagValue},
        metric_tags,
    };

    use super::encode_tags;

    #[cfg(feature = "sources-statsd")]
    fn encode_metric(metric: &OtelMetric) -> bytes::BytesMut {
        use tokio_util::codec::Encoder;

        let mut encoder = super::StatsdEncoder {
            default_namespace: None,
        };
        let mut frame = bytes::BytesMut::new();
        encoder.encode(metric, &mut frame).unwrap();
        frame
    }

    #[cfg(feature = "sources-statsd")]
    fn parse_encoded_metrics(metric: &[u8]) -> Vec<OtelMetric> {
        use crate::sources::statsd::{ConversionUnit, parser::Parser};
        let statsd_parser = Parser::new(true, ConversionUnit::Seconds);

        let s = std::str::from_utf8(metric).unwrap().trim();
        s.split('\n')
            .map(|packet| {
                statsd_parser
                    .parse(packet)
                    .expect("should not fail to parse statsd packet")
            })
            .collect()
    }

    fn tags() -> MetricTags {
        metric_tags!(
            "normal_tag" => "value",
            "multi_value" => "true",
            "multi_value" => "false",
            "multi_value" => TagValue::Bare,
            "bare_tag" => TagValue::Bare,
        )
    }

    #[test]
    fn test_encode_tags() {
        let actual = encode_tags(&tags());
        let mut actual = actual.split(',').collect::<Vec<_>>();
        actual.sort();

        let mut expected =
            "bare_tag,normal_tag:value,multi_value:true,multi_value:false,multi_value"
                .split(',')
                .collect::<Vec<_>>();
        expected.sort();

        assert_eq!(actual, expected);
    }

    #[test]
    fn tags_order() {
        assert_eq!(
            &encode_tags(
                &vec![
                    ("a", "value"),
                    ("b", "value"),
                    ("c", "value"),
                    ("d", "value"),
                    ("e", "value"),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect()
            ),
            "a:value,b:value,c:value,d:value,e:value"
        );
    }

    #[cfg(feature = "sources-statsd")]
    #[test]
    fn test_encode_counter() {
        let input = {
            let otel = OtelMetric::new_counter("counter", MetricKind::Incremental, 1.5)
                .with_tags(Some(tags()));
            otel
        };

        let frame = encode_metric(&input);
        let mut output = parse_encoded_metrics(&frame);
        vector_lib::assert_event_data_eq!(input, output.remove(0));
    }

    #[cfg(feature = "sources-statsd")]
    #[test]
    fn test_encode_absolute_counter() {
        let input = {
            let otel = OtelMetric::new_counter("counter", MetricKind::Absolute, 1.5);
            otel
        };

        let frame = encode_metric(&input);
        // The statsd parser will parse the counter as Incremental,
        // so we can't compare it with the parsed value.
        assert_eq!("counter:1.5|c\n", std::str::from_utf8(&frame).unwrap());
    }

    #[cfg(feature = "sources-statsd")]
    #[test]
    fn test_encode_gauge() {
        let input = {
            let otel = OtelMetric::new_gauge_delta("gauge", -1.5)
                .with_tags(Some(tags()));
            otel
        };

        let frame = encode_metric(&input);
        let mut output = parse_encoded_metrics(&frame);
        vector_lib::assert_event_data_eq!(input, output.remove(0));
    }

    #[cfg(feature = "sources-statsd")]
    #[test]
    fn test_encode_absolute_gauge() {
        let input = {
            let otel = OtelMetric::new_gauge("gauge", 1.5)
                .with_tags(Some(tags()));
            otel
        };

        let frame = encode_metric(&input);
        let mut output = parse_encoded_metrics(&frame);
        vector_lib::assert_event_data_eq!(input, output.remove(0));
    }

    #[cfg(feature = "sources-statsd")]
    #[test]
    fn test_encode_distribution() {
        let input = {
            let otel = OtelMetric::new_distribution_from_samples(
                "distribution",
                MetricKind::Incremental,
                &vector_lib::samples![1.5 => 1, 1.5 => 1],
                StatisticKind::Histogram,
            )
            .with_tags(Some(tags()));
            otel
        };

        let expected = {
            let otel = OtelMetric::new_distribution_from_samples(
                "distribution",
                MetricKind::Incremental,
                &vector_lib::samples![1.5 => 2],
                StatisticKind::Histogram,
            )
            .with_tags(Some(tags()));
            otel
        };

        let frame = encode_metric(&input);
        let mut output = parse_encoded_metrics(&frame);
        vector_lib::assert_event_data_eq!(expected, output.remove(0));
    }

    #[cfg(feature = "sources-statsd")]
    #[test]
    fn test_encode_distribution_aggregated() {
        let input = {
            let otel = OtelMetric::new_distribution_from_samples(
                "distribution",
                MetricKind::Incremental,
                &vector_lib::samples![2.5 => 1, 1.5 => 1, 1.5 => 1],
                StatisticKind::Histogram,
            )
            .with_tags(Some(tags()));
            otel
        };

        let expected1 = {
            let otel = OtelMetric::new_distribution_from_samples(
                "distribution",
                MetricKind::Incremental,
                &vector_lib::samples![1.5 => 2],
                StatisticKind::Histogram,
            )
            .with_tags(Some(tags()));
            otel
        };
        let expected2 = {
            let otel = OtelMetric::new_distribution_from_samples(
                "distribution",
                MetricKind::Incremental,
                &vector_lib::samples![2.5 => 1],
                StatisticKind::Histogram,
            )
            .with_tags(Some(tags()));
            otel
        };

        let frame = encode_metric(&input);
        let mut output = parse_encoded_metrics(&frame);
        vector_lib::assert_event_data_eq!(expected1, output.remove(0));
        vector_lib::assert_event_data_eq!(expected2, output.remove(0));
    }

    #[cfg(feature = "sources-statsd")]
    #[test]
    fn test_encode_set() {
        let input = {
            let otel = OtelMetric::new_set_from_values(
                "set",
                MetricKind::Incremental,
                vec!["abc".to_owned()],
            )
            .with_tags(Some(tags()));
            otel
        };

        let frame = encode_metric(&input);
        let mut output = parse_encoded_metrics(&frame);
        vector_lib::assert_event_data_eq!(input, output.remove(0));
    }
}
