use vector_lib::lookup::{lookup_v2::{OptionalTargetPath, OptionalValuePath}, OwnedTargetPath, owned_value_path};

pub mod logs;
pub mod metrics;

pub fn config_host_key_target_path() -> OptionalTargetPath {
    OptionalTargetPath {
        path: Some(OwnedTargetPath::event(owned_value_path!("host"))),
    }
}

pub fn config_host_key() -> OptionalValuePath {
    OptionalValuePath {
        path: Some(owned_value_path!("host")),
    }
}
