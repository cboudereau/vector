use vector_core::schema;
use vrl::value::Kind;

/// Inspect the global log schema and create a schema requirement.
pub fn get_serializer_schema_requirement() -> schema::Requirement {
    schema::Requirement::empty().required_meaning("body".to_string(), Kind::any())
}
