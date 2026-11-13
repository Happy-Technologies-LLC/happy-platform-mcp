//! Input validation against each tool's exact `inputSchema` (conformance C6, RFC §10.1
//! step 2).
//!
//! [`CompiledSchemaValidator`] implements [`SchemaValidator`] with the `jsonschema`
//! crate (MIT; default features off, so no HTTP client and no remote `$ref`
//! resolution: a validator must never make network calls). Every schema is compiled
//! once, when the validator is built from the registered tools, never per call.
//!
//! Parity: 6.2.0 does not validate arguments before the handler (its tools check
//! their own inputs and answer `Error: ...` text), so there is no 6.2.0 text to copy;
//! C6 is "Partial" in plan §4 and this is the Rust-side completion. A failure maps to
//! `ToolError::InvalidParams` (JSON-RPC -32602) in the dispatcher. Messages name
//! the failing location and keyword but NEVER echo an argument value (the RFC's "no
//! echoed values" rule), so the text is safe to audit-log and to return to a client.

use std::collections::HashMap;

use hpm_types::dispatch::SchemaValidator;
use hpm_types::tool::ToolDescriptor;
use jsonschema::error::{TypeKind, ValidationErrorKind};
use jsonschema::{ValidationError, Validator};

/// At most this many violations are reported per call.
const MAX_REPORTED: usize = 5;

/// A registered tool's schema is unusable; registration must fail.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum SchemaCompileError {
    #[error("tool {0}: input schema is not valid JSON")]
    NotJson(&'static str),
    #[error("tool {0}: input schema is not a JSON object")]
    NotObject(&'static str),
    #[error("tool {tool}: input schema does not compile: {reason}")]
    Invalid { tool: &'static str, reason: String },
    #[error("duplicate tool name: {0}")]
    Duplicate(&'static str),
}

/// Validators for a fixed set of tools, compiled up front.
pub struct CompiledSchemaValidator {
    by_tool: HashMap<&'static str, Validator>,
}

impl CompiledSchemaValidator {
    /// Compile every tool's `input_schema_json`. Fails on the first unusable schema.
    pub fn new(tools: &[ToolDescriptor]) -> Result<Self, SchemaCompileError> {
        let mut by_tool = HashMap::with_capacity(tools.len());
        for tool in tools {
            let schema: serde_json::Value = serde_json::from_str(tool.input_schema_json)
                .map_err(|_| SchemaCompileError::NotJson(tool.name))?;
            if !schema.is_object() {
                return Err(SchemaCompileError::NotObject(tool.name));
            }
            let validator =
                jsonschema::validator_for(&schema).map_err(|e| SchemaCompileError::Invalid {
                    tool: tool.name,
                    reason: schema_error_text(&e),
                })?;
            if by_tool.insert(tool.name, validator).is_some() {
                return Err(SchemaCompileError::Duplicate(tool.name));
            }
        }
        Ok(Self { by_tool })
    }

    /// Number of compiled tool schemas.
    pub fn len(&self) -> usize {
        self.by_tool.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_tool.is_empty()
    }
}

impl SchemaValidator for CompiledSchemaValidator {
    fn validate(&self, tool: &ToolDescriptor, args: &serde_json::Value) -> Result<(), String> {
        let Some(validator) = self.by_tool.get(tool.name) else {
            // Fail closed: a tool whose schema was never compiled is not callable.
            return Err("tool input schema is not registered".into());
        };
        let mut messages: Vec<String> = validator.iter_errors(args).map(|e| describe(&e)).collect();
        if messages.is_empty() {
            return Ok(());
        }
        // Validation order is not specified; sort for a deterministic message.
        messages.sort();
        messages.dedup();
        let total = messages.len();
        messages.truncate(MAX_REPORTED);
        let mut text = messages.join("; ");
        if total > MAX_REPORTED {
            text.push_str(&format!("; and {} more", total - MAX_REPORTED));
        }
        Err(text)
    }
}

/// One violation as text, built from the schema keyword and the instance location only.
fn describe(err: &ValidationError<'_>) -> String {
    let path = err.instance_path().as_str();
    let at = if path.is_empty() {
        "arguments".to_owned()
    } else {
        format!("argument {path}")
    };
    match err.kind() {
        // Same wording as the baseline `RequiredPropertiesValidator` (a property name
        // from the schema, not a value).
        ValidationErrorKind::Required { property } => {
            let name = property.as_str().unwrap_or("?");
            if path.is_empty() {
                format!("missing required property: {name}")
            } else {
                format!("missing required property: {name} in {path}")
            }
        }
        ValidationErrorKind::Type { kind } => {
            let expected = match kind {
                TypeKind::Single(t) => t.to_string(),
                TypeKind::Multiple(set) => set
                    .iter()
                    .map(|t| t.to_string())
                    .collect::<Vec<_>>()
                    .join(" or "),
            };
            format!("{at} must be of type {expected}")
        }
        // Unexpected names are client input; report only that some exist.
        ValidationErrorKind::AdditionalProperties { unexpected } => {
            format!(
                "{at} has {} property(ies) not allowed by the schema",
                unexpected.len()
            )
        }
        other => format!("{at} violates schema keyword '{}'", other.keyword()),
    }
}

/// Schema-compile errors describe the schema, which is trusted source, not input.
fn schema_error_text(err: &ValidationError<'_>) -> String {
    format!("{} at {}", err.kind().keyword(), err.schema_path().as_str())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use hpm_types::BoxFut;
    use hpm_types::context::RequestContext;
    use hpm_types::operation::OperationSet;
    use hpm_types::tool::{HandlerCompletion, ToolHandler};
    use serde_json::json;

    use super::*;

    // Representative shapes lifted from the 6.2.0 tools list: required, enum,
    // numeric bounds, pattern, maxLength, items/maxItems, const, oneOf and the
    // `additionalProperties: false` of SN-Register-Instance.
    const QUERY: &str = r#"{
        "type":"object",
        "properties":{
            "table_name":{"type":"string","description":"t"},
            "limit":{"type":"number","minimum":1,"maximum":1000,"default":10},
            "mode":{"type":"string","enum":["a","b"]},
            "fields":{"type":"array","items":{"type":"string"},"maxItems":2}
        },
        "required":["table_name"]
    }"#;
    const CLOSED: &str = r#"{
        "type":"object",
        "properties":{"name":{"type":"string","pattern":"^[a-z]+$","maxLength":8}},
        "required":["name"],
        "additionalProperties":false
    }"#;

    struct NoHandler;
    impl ToolHandler for NoHandler {
        fn call<'a>(
            &'a self,
            _: &'a RequestContext,
            _: serde_json::Value,
        ) -> BoxFut<'a, HandlerCompletion> {
            unreachable!("validation tests never call handlers")
        }
    }

    fn tool(name: &'static str, schema: &'static str) -> ToolDescriptor {
        ToolDescriptor::new(
            name,
            "",
            schema,
            |_, _| Ok(OperationSet::default()),
            Arc::new(NoHandler),
            |_| String::new(),
        )
    }

    fn validator() -> (CompiledSchemaValidator, ToolDescriptor, ToolDescriptor) {
        let q = tool("Q", QUERY);
        let c = tool("C", CLOSED);
        let v = CompiledSchemaValidator::new(&[q.clone(), c.clone()]).unwrap();
        (v, q, c)
    }

    #[test]
    fn valid_input_passes() {
        let (v, q, c) = validator();
        assert_eq!(v.len(), 2);
        assert!(
            v.validate(&q, &json!({"table_name":"incident","limit":5,"mode":"a"}))
                .is_ok()
        );
        assert!(v.validate(&c, &json!({"name":"abc"})).is_ok());
        // Unknown extra properties are fine where the schema does not close the object.
        assert!(v.validate(&q, &json!({"table_name":"t","extra":1})).is_ok());
    }

    #[test]
    fn missing_required_names_property() {
        let (v, q, _) = validator();
        assert_eq!(
            v.validate(&q, &json!({})).unwrap_err(),
            "missing required property: table_name"
        );
    }

    #[test]
    fn wrong_type_and_non_object_arguments() {
        let (v, q, _) = validator();
        let err = v.validate(&q, &json!({"table_name": 5})).unwrap_err();
        assert_eq!(err, "argument /table_name must be of type string");
        let err = v.validate(&q, &json!(null)).unwrap_err();
        assert_eq!(err, "arguments must be of type object");
        let err = v.validate(&q, &json!(["x"])).unwrap_err();
        assert_eq!(err, "arguments must be of type object");
    }

    #[test]
    fn keyword_violations_use_instance_path() {
        let (v, q, c) = validator();
        assert_eq!(
            v.validate(&q, &json!({"table_name":"t","limit":0}))
                .unwrap_err(),
            "argument /limit violates schema keyword 'minimum'"
        );
        assert_eq!(
            v.validate(&q, &json!({"table_name":"t","mode":"z"}))
                .unwrap_err(),
            "argument /mode violates schema keyword 'enum'"
        );
        assert_eq!(
            v.validate(&q, &json!({"table_name":"t","fields":["a","b","c"]}))
                .unwrap_err(),
            "argument /fields violates schema keyword 'maxItems'"
        );
        assert_eq!(
            v.validate(&q, &json!({"table_name":"t","fields":["a",1]}))
                .unwrap_err(),
            "argument /fields/1 must be of type string"
        );
        assert_eq!(
            v.validate(&c, &json!({"name":"ABC"})).unwrap_err(),
            "argument /name violates schema keyword 'pattern'"
        );
        assert_eq!(
            v.validate(&c, &json!({"name":"abcdefghi"})).unwrap_err(),
            "argument /name violates schema keyword 'maxLength'"
        );
    }

    #[test]
    fn additional_properties_false_is_enforced_without_echoing_names() {
        let (v, _, c) = validator();
        let err = v
            .validate(&c, &json!({"name":"abc","secret_key_name":1}))
            .unwrap_err();
        assert_eq!(
            err,
            "arguments has 1 property(ies) not allowed by the schema"
        );
        assert!(!err.contains("secret_key_name"));
    }

    #[test]
    fn messages_never_echo_values() {
        let (v, q, c) = validator();
        for (t, args) in [
            (
                &q,
                json!({"table_name": "TOPSECRET-VALUE-1", "limit": "TOPSECRET-VALUE-2"}),
            ),
            (&q, json!({"table_name": {"k": "TOPSECRET-VALUE-3"}})),
            (&c, json!({"name": "TOPSECRET-VALUE-4"})),
        ] {
            let err = v.validate(t, &args).unwrap_err();
            assert!(!err.contains("TOPSECRET"), "{err}");
        }
    }

    #[test]
    fn multiple_violations_are_sorted_bounded_and_deterministic() {
        let (v, q, _) = validator();
        let err = v.validate(&q, &json!({"limit":"x","mode":1})).unwrap_err();
        assert_eq!(
            err,
            "argument /limit must be of type number; argument /mode must be of type string; \
             argument /mode violates schema keyword 'enum'; missing required property: table_name"
        );
    }

    #[test]
    fn unregistered_tool_fails_closed() {
        let (v, _, _) = validator();
        let other = tool("Other", QUERY);
        assert_eq!(
            v.validate(&other, &json!({"table_name":"t"})).unwrap_err(),
            "tool input schema is not registered"
        );
    }

    #[test]
    fn invalid_schemas_are_reported_at_build_time() {
        let bad_json = tool("BadJson", "{not json");
        assert_eq!(
            CompiledSchemaValidator::new(&[bad_json]).err(),
            Some(SchemaCompileError::NotJson("BadJson"))
        );
        let not_object = tool("Arr", "[]");
        assert_eq!(
            CompiledSchemaValidator::new(&[not_object]).err(),
            Some(SchemaCompileError::NotObject("Arr"))
        );
        let bad_type = tool("BadType", r#"{"type":"nope"}"#);
        assert!(matches!(
            CompiledSchemaValidator::new(&[bad_type]).err(),
            Some(SchemaCompileError::Invalid {
                tool: "BadType",
                ..
            })
        ));
        let a = tool("Dup", QUERY);
        assert_eq!(
            CompiledSchemaValidator::new(&[a.clone(), a]).err(),
            Some(SchemaCompileError::Duplicate("Dup"))
        );
    }

    #[test]
    fn remote_refs_are_never_fetched() {
        let t = tool(
            "Remote",
            r#"{"type":"object","properties":{"x":{"$ref":"http://127.0.0.1:1/s.json"}}}"#,
        );
        // Either compilation rejects the unresolvable reference or validation does; it
        // must never block on, or succeed through, a network fetch.
        if let Ok(v) = CompiledSchemaValidator::new(std::slice::from_ref(&t)) {
            assert!(v.validate(&t, &json!({"x": 1})).is_err());
        }
    }
}
