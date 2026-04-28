use vrl::{owned_value_path, path::OwnedTargetPath};

#[allow(dead_code)]
pub(crate) fn get_message_path() -> OwnedTargetPath {
    OwnedTargetPath::event(owned_value_path!())
}
