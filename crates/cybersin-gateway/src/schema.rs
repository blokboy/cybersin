//! Tool-call argument schema validation (spec §8.2: "schema validation,
//! then the idempotency ledger" — validation is the first gate, before a
//! call is ever admitted to the ledger).
//!
//! Deliberately minimal: required-field + type checks against a hand-rolled
//! schema type, not a JSON-Schema crate. Nothing in `Cargo.lock` already
//! pulls one in, and a real JSON-Schema implementation (`$ref`, `oneOf`,
//! formats, ...) is far more than tool-call argument validation needs —
//! per spec §13's dependency discipline and this issue's scope guidance.
//! Tools with no registered schema (the default) skip validation entirely,
//! so this never blocks a call the caller didn't opt into checking.

use std::collections::BTreeMap;

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    String,
    Number,
    Bool,
    Object,
    Array,
}

impl FieldType {
    fn matches(&self, value: &Value) -> bool {
        match self {
            FieldType::String => value.is_string(),
            FieldType::Number => value.is_number(),
            FieldType::Bool => value.is_boolean(),
            FieldType::Object => value.is_object(),
            FieldType::Array => value.is_array(),
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::Number => "number",
            FieldType::Bool => "bool",
            FieldType::Object => "object",
            FieldType::Array => "array",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct FieldSchema {
    type_: FieldType,
    required: bool,
}

/// A declared shape for one tool's `args` object: which fields must be
/// present, and what type each named field must be when present.
#[derive(Debug, Clone, Default)]
pub struct ToolSchema {
    fields: BTreeMap<String, FieldSchema>,
}

impl ToolSchema {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn required(mut self, name: impl Into<String>, type_: FieldType) -> Self {
        self.fields.insert(
            name.into(),
            FieldSchema {
                type_,
                required: true,
            },
        );
        self
    }

    pub fn optional(mut self, name: impl Into<String>, type_: FieldType) -> Self {
        self.fields.insert(
            name.into(),
            FieldSchema {
                type_,
                required: false,
            },
        );
        self
    }

    /// Validate `args` (expected to be a JSON object) against this schema.
    pub fn validate(&self, args: &Value) -> Result<(), SchemaError> {
        let Some(obj) = args.as_object() else {
            return Err(SchemaError::NotAnObject);
        };
        for (name, field) in &self.fields {
            match obj.get(name) {
                Some(value) if !field.type_.matches(value) => {
                    return Err(SchemaError::WrongType {
                        field: name.clone(),
                        expected: field.type_.as_str(),
                    });
                }
                None if field.required => {
                    return Err(SchemaError::MissingField(name.clone()));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SchemaError {
    #[error("args must be a JSON object")]
    NotAnObject,
    #[error("missing required field {0:?}")]
    MissingField(String),
    #[error("field {field:?} must be a {expected}")]
    WrongType {
        field: String,
        expected: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> ToolSchema {
        ToolSchema::new()
            .required("query", FieldType::String)
            .optional("limit", FieldType::Number)
    }

    #[test]
    fn accepts_valid_args() {
        assert!(schema().validate(&json!({"query": "cybernetics"})).is_ok());
        assert!(schema()
            .validate(&json!({"query": "cybernetics", "limit": 5}))
            .is_ok());
    }

    #[test]
    fn rejects_missing_required_field() {
        assert_eq!(
            schema().validate(&json!({"limit": 5})),
            Err(SchemaError::MissingField("query".to_string()))
        );
    }

    #[test]
    fn rejects_wrong_type() {
        assert_eq!(
            schema().validate(&json!({"query": 5})),
            Err(SchemaError::WrongType {
                field: "query".to_string(),
                expected: "string"
            })
        );
    }

    #[test]
    fn rejects_non_object_args() {
        assert_eq!(
            schema().validate(&json!("not an object")),
            Err(SchemaError::NotAnObject)
        );
    }

    #[test]
    fn unregistered_optional_field_is_ignored_when_absent() {
        assert!(schema().validate(&json!({"query": "x"})).is_ok());
    }
}
