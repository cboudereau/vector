use vector_lib::config::LogNamespace;
use vrl::{owned_value_path, path::OwnedTargetPath};

#[allow(dead_code)]
pub(crate) fn get_message_path(log_namespace: LogNamespace) -> OwnedTargetPath {
    match log_namespace {
        LogNamespace::Vector => OwnedTargetPath::event(owned_value_path!()),
        LogNamespace::Legacy => OwnedTargetPath::event(owned_value_path!("body")),
    }
}
