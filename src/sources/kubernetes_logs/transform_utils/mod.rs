use vector_lib::config::LogNamespace;
use vrl::{owned_value_path, path::OwnedTargetPath};

#[allow(dead_code)]
pub(crate) fn get_message_path(_log_namespace: LogNamespace) -> OwnedTargetPath {
    OwnedTargetPath::event(owned_value_path!())
}
