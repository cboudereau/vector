use std::collections::BTreeMap;

use opentelemetry_proto::tonic::common::v1::InstrumentationScope;
use opentelemetry_proto::tonic::resource::v1::Resource;
use vector_lib::event::string_value;

pub fn build_source_resource(
    source_type: &str,
    config_overrides: &BTreeMap<String, String>,
) -> Resource {
    let mut attrs = Vec::new();

    let service_name = config_overrides
        .get("service.name")
        .cloned()
        .unwrap_or_else(|| format!("sol/{source_type}"));
    attrs.push(opentelemetry_proto::tonic::common::v1::KeyValue {
        key: "service.name".into(),
        value: Some(string_value(service_name)),
    });

    let host_name = config_overrides
        .get("host.name")
        .cloned()
        .unwrap_or_else(|| crate::get_hostname().unwrap_or_default());
    if !host_name.is_empty() {
        attrs.push(opentelemetry_proto::tonic::common::v1::KeyValue {
            key: "host.name".into(),
            value: Some(string_value(host_name)),
        });
    }

    for (k, v) in config_overrides {
        if k == "service.name" || k == "host.name" {
            continue;
        }
        attrs.push(opentelemetry_proto::tonic::common::v1::KeyValue {
            key: k.clone(),
            value: Some(string_value(v.clone())),
        });
    }

    Resource {
        attributes: attrs,
        dropped_attributes_count: 0,
    }
}

pub fn build_source_scope(source_type: &str) -> InstrumentationScope {
    InstrumentationScope {
        name: format!("sol/{source_type}"),
        version: crate::built_info::PKG_VERSION.to_string(),
        attributes: vec![],
        dropped_attributes_count: 0,
    }
}
