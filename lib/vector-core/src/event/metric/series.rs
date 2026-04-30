use core::fmt;

use vector_common::byte_size_of::ByteSizeOf;
use vector_config::configurable_component;

use super::{MetricTags, TagValue};

/// Metric identity — the grouping key for metric aggregation.
#[configurable_component]
#[derive(Clone, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct MetricIdentity {
    /// The name of the metric.
    pub name: String,

    /// The namespace of the metric.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,

    #[configurable(derived)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tags: Option<MetricTags>,
}

impl MetricIdentity {
    /// Gets the metric name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Gets a mutable reference to the name.
    pub fn name_mut(&mut self) -> &mut String {
        &mut self.name
    }

    /// Gets the namespace.
    pub fn namespace(&self) -> Option<&String> {
        self.namespace.as_ref()
    }

    /// Gets a mutable reference to the namespace.
    pub fn namespace_mut(&mut self) -> &mut Option<String> {
        &mut self.namespace
    }

    /// Gets an optional reference to the tags.
    pub fn tags(&self) -> Option<&MetricTags> {
        self.tags.as_ref()
    }

    /// Gets an optional mutable reference to the tags.
    pub fn tags_mut(&mut self) -> &mut Option<MetricTags> {
        &mut self.tags
    }

    /// Sets or updates the string value of a tag.
    pub fn replace_tag(&mut self, key: String, value: impl Into<TagValue>) -> Option<String> {
        (self.tags.get_or_insert_with(Default::default)).replace(key, value)
    }

    pub fn set_multi_value_tag(&mut self, key: String, values: impl IntoIterator<Item = TagValue>) {
        (self.tags.get_or_insert_with(Default::default)).set_multi_value(key, values);
    }

    /// Removes all the tags.
    pub fn remove_tags(&mut self) {
        self.tags = None;
    }

    /// Removes the tag entry for the named key, if it exists, and returns the old value.
    pub fn remove_tag(&mut self, key: &str) -> Option<String> {
        match &mut self.tags {
            None => None,
            Some(tags) => {
                let result = tags.remove(key);
                if tags.is_empty() {
                    self.tags = None;
                }
                result
            }
        }
    }
}

impl ByteSizeOf for MetricIdentity {
    fn allocated_bytes(&self) -> usize {
        self.name.allocated_bytes() + self.namespace.allocated_bytes() + self.tags.allocated_bytes()
    }
}

impl fmt::Display for MetricIdentity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(namespace) = &self.namespace {
            write!(f, "{namespace}_")?;
        }
        write!(f, "{}", self.name)?;
        write!(f, "{{")?;
        if let Some(tags) = &self.tags {
            let mut first = true;
            for (tag, value) in tags.iter_all() {
                if !first {
                    write!(f, ",")?;
                }
                first = false;
                write!(f, "{tag}")?;
                if let Some(value) = value {
                    write!(f, "={value:?}")?;
                }
            }
        }
        write!(f, "}}")
    }
}

pub type MetricSeries = MetricIdentity;
