use std::{
    error, fmt,
    num::{ParseFloatError, ParseIntError},
    str::Utf8Error,
    sync::LazyLock,
};

use regex::Regex;

use crate::{
    event::{AnyValue, OtelAttributes, metric::MetricKind, string_value},
    event::OtelMetric,
    sources::{
        statsd::ConversionUnit,
        statsd::aggregator::{MetricKey, ParsedMetric},
        util::extract_tag_key_and_value,
    },
};

static WHITESPACE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").unwrap());
static NONALPHANUM: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-zA-Z_\-0-9\.]").unwrap());

#[derive(Clone)]
pub struct Parser {
    sanitize: bool,
    convert_to: ConversionUnit,
}

impl Parser {
    pub const fn new(sanitize_keys: bool, convert_to: ConversionUnit) -> Self {
        Self {
            sanitize: sanitize_keys,
            convert_to,
        }
    }

    pub fn parse(&self, packet: &str) -> Result<OtelMetric, ParseError> {
        // https://docs.datadoghq.com/developers/dogstatsd/datagram_shell/#datagram-format
        let key_and_body = packet.splitn(2, ':').collect::<Vec<_>>();
        if key_and_body.len() != 2 {
            return Err(ParseError::Malformed(
                "should be key and body with ':' separator",
            ));
        }
        let (key, body) = (key_and_body[0], key_and_body[1]);

        let parts = body.split('|').collect::<Vec<_>>();
        if parts.len() < 2 {
            return Err(ParseError::Malformed(
                "body should have at least two pipe separated components",
            ));
        }

        let name = sanitize_key(key, self.sanitize);
        let metric_type = parts[1];

        // sampling part is optional and comes after metric type part
        let sampling = parts.get(2).filter(|s| s.starts_with('@'));
        let sample_rate = if let Some(s) = sampling {
            1.0 / sanitize_sampling(parse_sampling(s)?)
        } else {
            1.0
        };

        // tags are optional and could be found either after sampling of after metric type part
        let tags = if sampling.is_none() {
            parts.get(2)
        } else {
            parts.get(3)
        };
        let tags = tags.filter(|s| s.starts_with('#'));
        let tags = tags.map(parse_tags).transpose()?;

        let metric = match metric_type {
            "c" => {
                let val: f64 = parts[0].parse()?;
                OtelMetric::new_counter(name, MetricKind::Incremental, val * sample_rate)
                    .with_tags(tags)
            }
            unit @ "h" | unit @ "ms" | unit @ "d" => {
                let val: f64 = parts[0].parse()?;
                let converted_val = match unit {
                    "ms" => match self.convert_to {
                        ConversionUnit::Seconds => val / 1000.0,
                        ConversionUnit::Milliseconds => val,
                    },
                    _ => val,
                };
                let samples = vector_lib::samples![converted_val => sample_rate as u32];
                OtelMetric::new_distribution_from_samples(
                    name,
                    MetricKind::Incremental,
                    &samples,
                    convert_to_statistic(unit),
                )
                .with_tags(tags)
            }
            "g" => {
                let value = if parts[0]
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .ok_or(ParseError::Malformed("empty first body component"))?
                {
                    parts[0].parse()?
                } else {
                    parts[0][1..].parse()?
                };

                match parse_direction(parts[0])? {
                    None => OtelMetric::new_gauge(name, value).with_tags(tags),
                    Some(sign) => OtelMetric::new_gauge_delta(name, value * sign)
                        .with_tags(tags),
                }
            }
            "s" => OtelMetric::new_set_from_values(
                name,
                MetricKind::Incremental,
                vec![parts[0].to_string()],
            )
            .with_tags(tags),
            other => return Err(ParseError::UnknownMetricType(other.into())),
        };
        Ok(metric)
    }

    pub fn parse_for_aggregation(&self, packet: &str) -> Result<ParsedMetric, ParseError> {
        let key_and_body = packet.splitn(2, ':').collect::<Vec<_>>();
        if key_and_body.len() != 2 {
            return Err(ParseError::Malformed(
                "should be key and body with ':' separator",
            ));
        }
        let (key, body) = (key_and_body[0], key_and_body[1]);

        let parts = body.split('|').collect::<Vec<_>>();
        if parts.len() < 2 {
            return Err(ParseError::Malformed(
                "body should have at least two pipe separated components",
            ));
        }

        let name = sanitize_key(key, self.sanitize);
        let metric_type = parts[1];

        let sampling = parts.get(2).filter(|s| s.starts_with('@'));
        let sample_rate = if let Some(s) = sampling {
            sanitize_sampling(parse_sampling(s)?)
        } else {
            1.0
        };

        // Remaining parts after metric type: @sampling, #tags, c:containerID, T<timestamp>
        let mut tags = None;
        let mut container_id = None;
        let mut _timestamp: Option<u64> = None;
        let extra_start = if sampling.is_some() { 3 } else { 2 };
        for i in extra_start..parts.len() {
            let part = parts[i];
            if part.starts_with('#') {
                tags = Some(parse_tags_d39(&part)?);
            } else if part.starts_with("c:") && part.len() > 2 {
                container_id = Some(part[2..].to_string());
            } else if part.starts_with('T') && part.len() > 1 {
                if let Ok(ts) = part[1..].parse::<u64>() {
                    _timestamp = Some(ts * 1_000_000_000);
                }
            }
        }

        // Also check original tag position for backward compat
        if tags.is_none() {
            let tag_part = if sampling.is_none() {
                parts.get(2)
            } else {
                parts.get(3)
            };
            if let Some(t) = tag_part.filter(|s| s.starts_with('#')) {
                tags = Some(parse_tags_d39(t)?);
            }
        }

        // Add container.id as a data point attribute
        if let Some(ref cid) = container_id {
            let attrs = tags.get_or_insert_with(OtelAttributes::default);
            attrs.insert("container.id".to_string(), string_value(cid));
        }

        let key = MetricKey { name, tags };

        match metric_type {
            "c" => {
                let val: f64 = parts[0].parse()?;
                Ok(ParsedMetric::Counter {
                    key,
                    value: val / sample_rate.max(f64::MIN_POSITIVE),
                })
            }
            "h" | "ms" | "d" => {
                let val: f64 = parts[0].parse()?;
                let converted_val = match metric_type {
                    "ms" => match self.convert_to {
                        ConversionUnit::Seconds => val / 1000.0,
                        ConversionUnit::Milliseconds => val,
                    },
                    _ => val,
                };
                Ok(ParsedMetric::Timer {
                    key,
                    value: converted_val,
                    sample_rate,
                })
            }
            "g" => {
                let value = if parts[0]
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .ok_or(ParseError::Malformed("empty first body component"))?
                {
                    parts[0].parse()?
                } else {
                    parts[0][1..].parse()?
                };
                let direction = parse_direction(parts[0])?;
                Ok(ParsedMetric::Gauge {
                    key,
                    value,
                    direction,
                })
            }
            "s" => Ok(ParsedMetric::Set {
                key,
                value: parts[0].to_string(),
            }),
            other => Err(ParseError::UnknownMetricType(other.into())),
        }
    }
}

/// Parse tags with D39 semantics: bare tags become empty string, not None.
fn parse_tags_d39(input: &str) -> Result<OtelAttributes, ParseError> {
    if !input.starts_with('#') || input.len() < 2 {
        return Err(ParseError::Malformed(
            "expected non empty '#'-prefixed tags component",
        ));
    }

    let mut attrs = OtelAttributes::default();
    for (key, tag_value) in input[1..].split(',').map(extract_tag_key_and_value) {
        let av = match tag_value {
            Some(s) => string_value(s),
            None => string_value(""),
        };
        attrs.insert(key, av);
    }
    Ok(attrs)
}

fn parse_sampling(input: &str) -> Result<f64, ParseError> {
    if !input.starts_with('@') || input.len() < 2 {
        return Err(ParseError::Malformed(
            "expected non empty '@'-prefixed sampling component",
        ));
    }

    let num: f64 = input[1..].parse()?;
    if num.is_sign_positive() {
        Ok(num)
    } else {
        Err(ParseError::Malformed("sample rate can't be negative"))
    }
}

/// Statsd (and dogstatsd) support bare, single and multi-value tags.
fn parse_tags(input: &&str) -> Result<OtelAttributes, ParseError> {
    if !input.starts_with('#') || input.len() < 2 {
        return Err(ParseError::Malformed(
            "expected non empty '#'-prefixed tags component",
        ));
    }

    let mut attrs = OtelAttributes::default();
    for (key, tag_value) in input[1..].split(',').map(extract_tag_key_and_value) {
        let av = match tag_value {
            Some(s) => string_value(s),
            None => AnyValue { value: None },
        };
        attrs.insert(key, av);
    }
    Ok(attrs)
}

fn parse_direction(input: &str) -> Result<Option<f64>, ParseError> {
    match input
        .chars()
        .next()
        .ok_or(ParseError::Malformed("empty body component"))?
    {
        '+' => Ok(Some(1.0)),
        '-' => Ok(Some(-1.0)),
        c if c.is_ascii_digit() => Ok(None),
        _other => Err(ParseError::Malformed("invalid gauge value prefix")),
    }
}

fn sanitize_key(key: &str, sanitize: bool) -> String {
    if !sanitize {
        key.to_owned()
    } else {
        let s = key.replace('/', "'-");
        let s = WHITESPACE.replace_all(&s, "_");
        let s = NONALPHANUM.replace_all(&s, "");
        s.into()
    }
}

fn sanitize_sampling(sampling: f64) -> f64 {
    if sampling == 0.0 { 1.0 } else { sampling }
}

fn convert_to_statistic(unit: &str) -> &'static str {
    match unit {
        "d" => "summary",
        _ => "histogram",
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    InvalidUtf8(Utf8Error),
    Malformed(&'static str),
    UnknownMetricType(String),
    InvalidInteger(ParseIntError),
    InvalidFloat(ParseFloatError),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Statsd parse error: {self:?}")
    }
}

vector_lib::impl_event_data_eq!(ParseError);

impl error::Error for ParseError {}

impl From<ParseIntError> for ParseError {
    fn from(e: ParseIntError) -> ParseError {
        ParseError::InvalidInteger(e)
    }
}

impl From<ParseFloatError> for ParseError {
    fn from(e: ParseFloatError) -> ParseError {
        ParseError::InvalidFloat(e)
    }
}

#[cfg(test)]
mod test {
    use vector_lib::assert_event_data_eq;

    use super::{ParseError, Parser, sanitize_key, sanitize_sampling};
    use crate::{
        event::{AnyValue, OtelAttributes, metric::MetricKind, string_value},
        event::OtelMetric,
        sources::statsd::ConversionUnit,
    };

    /// Build OtelAttributes with support for bare tags (None value).
    fn build_tags(entries: Vec<(&str, Option<&str>)>) -> OtelAttributes {
        let mut attrs = OtelAttributes::default();
        for (key, value) in entries {
            let av = match value {
                Some(s) => string_value(s),
                None => AnyValue { value: None },
            };
            attrs.insert(key.to_string(), av);
        }
        attrs
    }

    const SANITIZING_PARSER: Parser = Parser::new(true, ConversionUnit::Seconds);
    fn parse(packet: &str) -> Result<OtelMetric, ParseError> {
        SANITIZING_PARSER.parse(packet)
    }

    const NON_CONVERTING_PARSER: Parser = Parser::new(true, ConversionUnit::Milliseconds);
    fn parse_non_converting(packet: &str) -> Result<OtelMetric, ParseError> {
        NON_CONVERTING_PARSER.parse(packet)
    }

    const NON_SANITIZING_PARSER: Parser = Parser::new(false, ConversionUnit::Seconds);
    fn unsanitized_parse(packet: &str) -> Result<OtelMetric, ParseError> {
        NON_SANITIZING_PARSER.parse(packet)
    }

    #[test]
    fn basic_counter() {
        assert_event_data_eq!(
            parse("foo:1|c"),
            Ok(OtelMetric::new_counter("foo", MetricKind::Incremental, 1.0)),
        );
    }

    #[test]
    fn tagged_counter() {
        assert_event_data_eq!(
            parse("foo/how@ever baz:1|c|#tag1,tag2:value"),
            Ok(OtelMetric::new_counter("foo-however_baz", MetricKind::Incremental, 1.0)
                .with_tags(Some(build_tags(vec![
                    ("tag1", None),
                    ("tag2", Some("value")),
                ])))),
        );
    }

    #[test]
    fn tagged_not_sanitized_counter() {
        assert_event_data_eq!(
            unsanitized_parse("foo/bar@baz baz:1|c|#tag1,tag2:value"),
            Ok(OtelMetric::new_counter("foo/bar@baz baz", MetricKind::Incremental, 1.0)
                .with_tags(Some(build_tags(vec![
                    ("tag1", None),
                    ("tag2", Some("value")),
                ])))),
        );
    }

    #[test]
    fn enhanced_tags() {
        // With OtelAttributes, duplicate keys are overwritten (last one wins).
        // tag2:valueA is overwritten by tag2:valueB, tag3:value is overwritten by tag3 (bare).
        assert_event_data_eq!(
            parse("foo:1|c|#tag1,tag2:valueA,tag2:valueB,tag3:value,tag3,tag4:"),
            Ok(OtelMetric::new_counter("foo", MetricKind::Incremental, 1.0)
                .with_tags(Some(build_tags(vec![
                    ("tag1", None),
                    ("tag2", Some("valueB")),
                    ("tag3", None),
                    ("tag4", Some("")),
                ])))),
        );
    }

    #[test]
    fn sampled_counter() {
        assert_event_data_eq!(
            parse("bar:2|c|@0.1"),
            Ok(OtelMetric::new_counter("bar", MetricKind::Incremental, 20.0)),
        );
    }

    #[test]
    fn zero_sampled_counter() {
        assert_event_data_eq!(
            parse("bar:2|c|@0"),
            Ok(OtelMetric::new_counter("bar", MetricKind::Incremental, 2.0)),
        );
    }

    #[test]
    fn sampled_timer() {
        let samples = vector_lib::samples![0.320 => 10];
        assert_event_data_eq!(
            parse("glork:320|ms|@0.1"),
            Ok(OtelMetric::new_distribution_from_samples(
                "glork",
                MetricKind::Incremental,
                &samples,
                "histogram",
            )),
        );
    }

    #[test]
    fn sampled_timer_non_converting() {
        let samples = vector_lib::samples![320.0 => 10];
        assert_event_data_eq!(
            parse_non_converting("glork:320|ms|@0.1"),
            Ok(OtelMetric::new_distribution_from_samples(
                "glork",
                MetricKind::Incremental,
                &samples,
                "histogram",
            )),
        );
    }

    #[test]
    fn sampled_tagged_histogram() {
        let samples = vector_lib::samples![320.0 => 10];
        assert_event_data_eq!(
            parse("glork:320|h|@0.1|#region:us-west1,production,e:"),
            Ok(OtelMetric::new_distribution_from_samples(
                "glork",
                MetricKind::Incremental,
                &samples,
                "histogram",
            )
            .with_tags(Some(build_tags(vec![
                ("e", Some("")),
                ("production", None),
                ("region", Some("us-west1")),
            ])))),
        );
    }

    #[test]
    fn sampled_distribution() {
        let samples = vector_lib::samples![320.0 => 10];
        assert_event_data_eq!(
            parse("glork:320|d|@0.1|#region:us-west1,production,e:"),
            Ok(OtelMetric::new_distribution_from_samples(
                "glork",
                MetricKind::Incremental,
                &samples,
                "summary",
            )
            .with_tags(Some(build_tags(vec![
                ("e", Some("")),
                ("production", None),
                ("region", Some("us-west1")),
            ])))),
        );
    }

    #[test]
    fn simple_gauge() {
        assert_event_data_eq!(
            parse("gaugor:333|g"),
            Ok(OtelMetric::new_gauge("gaugor", 333.0)),
        );
    }

    #[test]
    fn signed_gauge() {
        assert_event_data_eq!(
            parse("gaugor:-4|g"),
            Ok(OtelMetric::new_gauge_delta("gaugor", -4.0)),
        );
        assert_event_data_eq!(
            parse("gaugor:+10|g"),
            Ok(OtelMetric::new_gauge_delta("gaugor", 10.0)),
        );
    }

    #[test]
    fn sets() {
        assert_event_data_eq!(
            parse("uniques:765|s"),
            Ok(OtelMetric::new_set_from_values(
                "uniques",
                MetricKind::Incremental,
                vec!["765".to_string()],
            )),
        );
    }

    #[test]
    fn sanitizing_keys() {
        assert_eq!("foo-bar-baz", sanitize_key("foo/bar/baz", true));
        assert_eq!("foo/bar/baz", sanitize_key("foo/bar/baz", false));
        assert_eq!("foo_bar_baz", sanitize_key("foo bar  baz", true));
        assert_eq!("foo.__bar_.baz", sanitize_key("foo. @& bar_$!#.baz", true));
        assert_eq!(
            "foo. @& bar_$!#.baz",
            sanitize_key("foo. @& bar_$!#.baz", false)
        );
    }

    #[test]
    fn sanitizing_sampling() {
        assert_eq!(1.0, sanitize_sampling(0.0));
        assert_eq!(2.5, sanitize_sampling(2.5));
        assert_eq!(-5.0, sanitize_sampling(-5.0));
    }

    // --- parse_for_aggregation tests ---

    use crate::sources::statsd::aggregator::ParsedMetric;

    fn parse_agg(packet: &str) -> Result<ParsedMetric, ParseError> {
        SANITIZING_PARSER.parse_for_aggregation(packet)
    }

    #[test]
    fn agg_basic_counter() {
        let m = parse_agg("foo:5|c").unwrap();
        match m {
            ParsedMetric::Counter { key, value } => {
                assert_eq!(key.name, "foo");
                assert!(key.tags.is_none());
                assert_eq!(value, 5.0);
            }
            _ => panic!("expected Counter"),
        }
    }

    #[test]
    fn agg_sampled_counter() {
        let m = parse_agg("foo:2|c|@0.5").unwrap();
        match m {
            ParsedMetric::Counter { key, value } => {
                assert_eq!(key.name, "foo");
                assert_eq!(value, 4.0); // 2 / 0.5
            }
            _ => panic!("expected Counter"),
        }
    }

    #[test]
    fn agg_timer_with_sample_rate() {
        let m = parse_agg("latency:320|ms|@0.1").unwrap();
        match m {
            ParsedMetric::Timer {
                key,
                value,
                sample_rate,
            } => {
                assert_eq!(key.name, "latency");
                assert_eq!(value, 0.320); // ms → seconds
                assert_eq!(sample_rate, 0.1);
            }
            _ => panic!("expected Timer"),
        }
    }

    #[test]
    fn agg_gauge_absolute() {
        let m = parse_agg("mem:42|g").unwrap();
        match m {
            ParsedMetric::Gauge {
                key,
                value,
                direction,
            } => {
                assert_eq!(key.name, "mem");
                assert_eq!(value, 42.0);
                assert!(direction.is_none());
            }
            _ => panic!("expected Gauge"),
        }
    }

    #[test]
    fn agg_gauge_delta() {
        let m = parse_agg("mem:-5|g").unwrap();
        match m {
            ParsedMetric::Gauge {
                value, direction, ..
            } => {
                assert_eq!(value, 5.0);
                assert_eq!(direction, Some(-1.0));
            }
            _ => panic!("expected Gauge"),
        }
    }

    #[test]
    fn agg_set() {
        let m = parse_agg("users:alice|s").unwrap();
        match m {
            ParsedMetric::Set { key, value } => {
                assert_eq!(key.name, "users");
                assert_eq!(value, "alice");
            }
            _ => panic!("expected Set"),
        }
    }

    #[test]
    fn agg_d39_bare_tags() {
        let m = parse_agg("foo:1|c|#env:prod,bare_tag").unwrap();
        match m {
            ParsedMetric::Counter { key, .. } => {
                let tags = key.tags.unwrap();
                assert_eq!(
                    tags.get("env").unwrap(),
                    &string_value("prod")
                );
                assert_eq!(
                    tags.get("bare_tag").unwrap(),
                    &string_value("")
                );
            }
            _ => panic!("expected Counter"),
        }
    }

    #[test]
    fn agg_dogstatsd_container_id() {
        let m = parse_agg("foo:1|c|#env:prod|c:abc123").unwrap();
        match m {
            ParsedMetric::Counter { key, .. } => {
                let tags = key.tags.unwrap();
                assert_eq!(
                    tags.get("container.id").unwrap(),
                    &string_value("abc123")
                );
                assert_eq!(
                    tags.get("env").unwrap(),
                    &string_value("prod")
                );
            }
            _ => panic!("expected Counter"),
        }
    }

    #[test]
    fn agg_dogstatsd_timestamp() {
        // T<unix_seconds> parsed but stored in _timestamp (not yet wired to output)
        let m = parse_agg("foo:1|c|#env:prod|T1234567890").unwrap();
        assert!(matches!(m, ParsedMetric::Counter { .. }));
    }
}
