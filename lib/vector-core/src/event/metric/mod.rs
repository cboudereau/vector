#[cfg(feature = "vrl")]
use std::convert::TryFrom;
use std::fmt::{self, Formatter};

use vector_config::configurable_component;
#[cfg(feature = "vrl")]
use vrl::compiler::value::VrlValueConvert;

#[cfg(any(test, feature = "test"))]
mod arbitrary;

mod data;
pub use self::data::*;

mod series;
pub use self::series::*;

mod tags;
pub use self::tags::*;

mod value;
pub use self::value::*;

#[macro_export]
macro_rules! metric_tags {
    () => { $crate::event::MetricTags::default() };

    ($($key:expr => $value:expr,)+) => { $crate::metric_tags!($($key => $value),+) };

    ($($key:expr => $value:expr),*) => {
        [
            $( ($key.into(), $crate::event::metric::TagValue::from($value)), )*
        ].into_iter().collect::<$crate::event::MetricTags>()
    };
}

/// Metric kind.
///
/// Metrics can be either absolute or incremental. Absolute metrics represent a sort of "last write wins" scenario,
/// where the latest absolute value seen is meant to be the actual metric value.  In contrast, and perhaps intuitively,
/// incremental metrics are meant to be additive, such that we don't know what total value of the metric is, but we know
/// that we'll be adding or subtracting the given value from it.
///
/// Generally speaking, most metrics storage systems deal with incremental updates. A notable exception is Prometheus,
/// which deals with, and expects, absolute values from clients.
#[configurable_component]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum MetricKind {
    /// Incremental metric.
    Incremental,

    /// Absolute metric.
    Absolute,
}

#[cfg(feature = "vrl")]
impl TryFrom<vrl::value::Value> for MetricKind {
    type Error = String;

    fn try_from(value: vrl::value::Value) -> Result<Self, Self::Error> {
        let value = value.try_bytes().map_err(|e| e.to_string())?;
        match std::str::from_utf8(&value).map_err(|e| e.to_string())? {
            "incremental" => Ok(Self::Incremental),
            "absolute" => Ok(Self::Absolute),
            value => Err(format!(
                "invalid metric kind {value}, metric kind must be `absolute` or `incremental`"
            )),
        }
    }
}

#[cfg(feature = "vrl")]
impl From<MetricKind> for vrl::value::Value {
    fn from(kind: MetricKind) -> Self {
        match kind {
            MetricKind::Incremental => "incremental".into(),
            MetricKind::Absolute => "absolute".into(),
        }
    }
}

#[macro_export]
macro_rules! samples {
    ( $( $value:expr => $rate:expr ),* ) => {
        vec![ $( $crate::event::metric::Sample { value: $value, rate: $rate }, )* ]
    }
}

#[macro_export]
macro_rules! buckets {
    ( $( $limit:expr => $count:expr ),* ) => {
        vec![ $( $crate::event::metric::Bucket { upper_limit: $limit, count: $count }, )* ]
    }
}

#[macro_export]
macro_rules! quantiles {
    ( $( $q:expr => $value:expr ),* ) => {
        vec![ $( $crate::event::metric::Quantile { quantile: $q, value: $value }, )* ]
    }
}

#[cfg(feature = "lua")]
#[inline]
pub(crate) fn zip_samples(
    values: impl IntoIterator<Item = f64>,
    rates: impl IntoIterator<Item = u32>,
) -> Vec<Sample> {
    values
        .into_iter()
        .zip(rates)
        .map(|(value, rate)| Sample { value, rate })
        .collect()
}

#[cfg(feature = "lua")]
#[inline]
pub(crate) fn zip_buckets(
    limits: impl IntoIterator<Item = f64>,
    counts: impl IntoIterator<Item = u64>,
) -> Vec<Bucket> {
    limits
        .into_iter()
        .zip(counts)
        .map(|(upper_limit, count)| Bucket { upper_limit, count })
        .collect()
}

#[cfg(feature = "lua")]
#[inline]
pub(crate) fn zip_quantiles(
    quantiles: impl IntoIterator<Item = f64>,
    values: impl IntoIterator<Item = f64>,
) -> Vec<Quantile> {
    quantiles
        .into_iter()
        .zip(values)
        .map(|(quantile, value)| Quantile { quantile, value })
        .collect()
}

fn write_list<I, T, W>(
    fmt: &mut Formatter<'_>,
    sep: &str,
    items: I,
    writer: W,
) -> Result<(), fmt::Error>
where
    I: IntoIterator<Item = T>,
    W: Fn(&mut Formatter<'_>, T) -> Result<(), fmt::Error>,
{
    let mut this_sep = "";
    for item in items {
        write!(fmt, "{this_sep}")?;
        writer(fmt, item)?;
        this_sep = sep;
    }
    Ok(())
}

fn write_word(fmt: &mut Formatter<'_>, word: &str) -> Result<(), fmt::Error> {
    if word.contains(|c: char| !c.is_ascii_alphanumeric() && c != '_') {
        write!(fmt, "{word:?}")
    } else {
        write!(fmt, "{word}")
    }
}

pub fn samples_to_buckets(samples: &[Sample], buckets: &[f64]) -> (Vec<Bucket>, u64, f64) {
    let mut counts = vec![0; buckets.len()];
    let mut sum = 0.0;
    let mut count = 0;
    for sample in samples {
        let rate = u64::from(sample.rate);

        if let Some((i, _)) = buckets
            .iter()
            .enumerate()
            .find(|&(_, b)| *b >= sample.value)
        {
            counts[i] += rate;
        }

        sum += sample.value * f64::from(sample.rate);
        count += rate;
    }

    let buckets = buckets
        .iter()
        .zip(counts.iter())
        .map(|(b, c)| Bucket {
            upper_limit: *b,
            count: *c,
        })
        .collect();

    (buckets, count, sum)
}

#[cfg(test)]
mod test {
    use chrono::{DateTime, Timelike, Utc, offset::TimeZone};
    use similar_asserts::assert_eq;

    use super::*;

    fn ts() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2018, 11, 14, 8, 9, 10)
            .single()
            .and_then(|t| t.with_nanosecond(11))
            .expect("invalid timestamp")
    }

    fn make_data(kind: MetricKind, value: MetricValue) -> MetricData {
        MetricData {
            time: MetricTime { timestamp: None, interval_ms: None },
            kind,
            value,
        }
    }

    #[test]
    fn merge_counters() {
        let mut data = make_data(MetricKind::Incremental, MetricValue::Counter { value: 1.0 });
        let delta = MetricData {
            time: MetricTime { timestamp: Some(ts()), interval_ms: None },
            kind: MetricKind::Incremental,
            value: MetricValue::Counter { value: 2.0 },
        };

        assert!(data.add(&delta));
        assert_eq!(data.value, MetricValue::Counter { value: 3.0 });
        assert_eq!(data.time.timestamp, Some(ts()));
    }

    #[test]
    fn merge_gauges() {
        let mut data = make_data(MetricKind::Incremental, MetricValue::Gauge { value: 1.0 });
        let delta = MetricData {
            time: MetricTime { timestamp: Some(ts()), interval_ms: None },
            kind: MetricKind::Incremental,
            value: MetricValue::Gauge { value: -2.0 },
        };

        assert!(data.add(&delta));
        assert_eq!(data.value, MetricValue::Gauge { value: -1.0 });
        assert_eq!(data.time.timestamp, Some(ts()));
    }

    #[test]
    fn merge_sets() {
        let mut data = make_data(MetricKind::Incremental, MetricValue::Set {
            values: vec!["old".into()].into_iter().collect(),
        });
        let delta = MetricData {
            time: MetricTime { timestamp: Some(ts()), interval_ms: None },
            kind: MetricKind::Incremental,
            value: MetricValue::Set {
                values: vec!["new".into()].into_iter().collect(),
            },
        };

        assert!(data.add(&delta));
        let MetricValue::Set { values } = &data.value else { panic!("expected set") };
        assert!(values.contains("old"));
        assert!(values.contains("new"));
        assert_eq!(data.time.timestamp, Some(ts()));
    }

    #[test]
    fn merge_histograms() {
        let mut data = make_data(MetricKind::Incremental, MetricValue::Distribution {
            samples: samples![1.0 => 10],
            statistic: StatisticKind::Histogram,
        });
        let delta = MetricData {
            time: MetricTime { timestamp: Some(ts()), interval_ms: None },
            kind: MetricKind::Incremental,
            value: MetricValue::Distribution {
                samples: samples![1.0 => 20],
                statistic: StatisticKind::Histogram,
            },
        };

        assert!(data.add(&delta));
        assert_eq!(data.value, MetricValue::Distribution {
            samples: samples![1.0 => 10, 1.0 => 20],
            statistic: StatisticKind::Histogram,
        });
    }

    #[test]
    fn subtract_counters() {
        let old = make_data(MetricKind::Absolute, MetricValue::Counter { value: 4.0 });
        let mut new = make_data(MetricKind::Absolute, MetricValue::Counter { value: 6.0 });

        assert!(new.subtract(&old));
        assert_eq!(new.value, MetricValue::Counter { value: 2.0 });

        let old = make_data(MetricKind::Absolute, MetricValue::Counter { value: 6.0 });
        let mut new_reset = make_data(MetricKind::Absolute, MetricValue::Counter { value: 1.0 });
        assert!(!new_reset.subtract(&old));
    }

    #[test]
    fn subtract_aggregated_histograms() {
        let old = make_data(MetricKind::Absolute, MetricValue::AggregatedHistogram {
            count: 1, sum: 1.0, buckets: buckets!(2.0 => 1),
        });
        let mut new = make_data(MetricKind::Absolute, MetricValue::AggregatedHistogram {
            count: 3, sum: 3.0, buckets: buckets!(2.0 => 3),
        });

        assert!(new.subtract(&old));
        assert_eq!(new.value, MetricValue::AggregatedHistogram {
            count: 2, sum: 2.0, buckets: buckets!(2.0 => 2),
        });

        let old = make_data(MetricKind::Absolute, MetricValue::AggregatedHistogram {
            count: 3, sum: 3.0, buckets: buckets!(2.0 => 3),
        });
        let mut new_reset = make_data(MetricKind::Absolute, MetricValue::AggregatedHistogram {
            count: 1, sum: 1.0, buckets: buckets!(2.0 => 1),
        });
        assert!(!new_reset.subtract(&old));
    }

    #[test]
    fn subtract_aggregated_histograms_bucket_redistribution() {
        let old = make_data(MetricKind::Absolute, MetricValue::AggregatedHistogram {
            count: 15, sum: 15.0, buckets: buckets!(1.0 => 10, 2.0 => 5),
        });
        let mut new = make_data(MetricKind::Absolute, MetricValue::AggregatedHistogram {
            count: 20, sum: 20.0, buckets: buckets!(1.0 => 8, 2.0 => 12),
        });
        assert!(!new.subtract(&old));
    }

    #[test]
    fn quantile_to_percentile_string() {
        let quantiles = [
            (-1.0, "0"),
            (0.0, "0"),
            (0.25, "25"),
            (0.50, "50"),
            (0.999, "999"),
            (0.9999, "9999"),
            (0.99999, "9999"),
            (1.0, "100"),
            (3.0, "100"),
        ];

        for (quantile, expected) in quantiles {
            let quantile = Quantile { quantile, value: 1.0 };
            let result = quantile.to_percentile_string();
            assert_eq!(result, expected);
        }
    }

    #[test]
    fn quantile_to_string() {
        let quantiles = [
            (-1.0, "0"),
            (0.0, "0"),
            (0.25, "0.25"),
            (0.50, "0.5"),
            (0.999, "0.999"),
            (0.9999, "0.9999"),
            (0.99999, "0.9999"),
            (1.0, "1"),
            (3.0, "1"),
        ];

        for (quantile, expected) in quantiles {
            let quantile = Quantile { quantile, value: 1.0 };
            let result = quantile.to_quantile_string();
            assert_eq!(result, expected);
        }
    }

    #[test]
    fn value_conversions() {
        let counter_value = MetricValue::Counter { value: 3.13 };
        assert_eq!(counter_value.distribution_to_agg_histogram(&[1.0]), None);

        let distrib_value = MetricValue::Distribution {
            samples: samples!(1.0 => 10, 2.0 => 5, 5.0 => 2),
            statistic: StatisticKind::Summary,
        };
        let converted = distrib_value.distribution_to_agg_histogram(&[1.0, 5.0, 10.0]);
        assert_eq!(
            converted,
            Some(MetricValue::AggregatedHistogram {
                buckets: vec![
                    Bucket { upper_limit: 1.0, count: 10 },
                    Bucket { upper_limit: 5.0, count: 7 },
                    Bucket { upper_limit: 10.0, count: 0 },
                ],
                sum: 30.0,
                count: 17,
            })
        );
    }

    #[test]
    fn merge_non_contiguous_interval() {
        let mut data = MetricData {
            time: MetricTime { timestamp: Some(ts()), interval_ms: std::num::NonZeroU32::new(10) },
            kind: MetricKind::Incremental,
            value: MetricValue::Gauge { value: 12.0 },
        };
        let delta = MetricData {
            time: MetricTime {
                timestamp: Some(ts() + chrono::Duration::milliseconds(20)),
                interval_ms: std::num::NonZeroU32::new(15),
            },
            kind: MetricKind::Incremental,
            value: MetricValue::Gauge { value: -5.0 },
        };

        assert!(data.add(&delta));
        assert_eq!(data.value, MetricValue::Gauge { value: 7.0 });
        assert_eq!(data.time.timestamp, Some(ts()));
        assert_eq!(data.time.interval_ms, std::num::NonZeroU32::new(35));
    }

    #[test]
    fn merge_contiguous_interval() {
        let mut data = MetricData {
            time: MetricTime { timestamp: Some(ts()), interval_ms: std::num::NonZeroU32::new(10) },
            kind: MetricKind::Incremental,
            value: MetricValue::Gauge { value: 12.0 },
        };
        let delta = MetricData {
            time: MetricTime {
                timestamp: Some(ts() + chrono::Duration::milliseconds(5)),
                interval_ms: std::num::NonZeroU32::new(15),
            },
            kind: MetricKind::Incremental,
            value: MetricValue::Gauge { value: -5.0 },
        };

        assert!(data.add(&delta));
        assert_eq!(data.value, MetricValue::Gauge { value: 7.0 });
        assert_eq!(data.time.timestamp, Some(ts()));
        assert_eq!(data.time.interval_ms, std::num::NonZeroU32::new(20));
    }
}
