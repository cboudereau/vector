use vector_lib::configurable::configurable_component;

mod config;

pub use config::VectorConfig;

/// Marker type for the version two of the configuration for the `vector` sink.
#[configurable_component]
#[derive(Clone, Debug)]
enum VectorConfigVersion {
    /// Marker value for version two.
    #[serde(rename = "2")]
    V2,
}

#[cfg(test)]
mod test {
    #[test]
    fn generate_config() {
        crate::test_util::test_generate_config::<super::VectorConfig>();
    }
}

#[cfg(test)]
mod tests {
    use super::config::with_default_scheme;

    #[test]
    fn test_with_default_scheme() {
        assert_eq!(
            with_default_scheme("0.0.0.0", false).unwrap().to_string(),
            "http://0.0.0.0/"
        );
        assert_eq!(
            with_default_scheme("0.0.0.0", true).unwrap().to_string(),
            "https://0.0.0.0/"
        );
    }
}
