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

/// Validators for a fixed set of tools, compiled up front (with the parsed schema,
/// needed to judge the lenient-type exceptions).
pub struct CompiledSchemaValidator {
    by_tool: HashMap<&'static str, (Validator, serde_json::Value)>,
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
            if by_tool.insert(tool.name, (validator, schema)).is_some() {
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
        let Some((validator, schema)) = self.by_tool.get(tool.name) else {
            // Fail closed: a tool whose schema was never compiled is not callable.
            return Err("tool input schema is not registered".into());
        };
        let mut messages: Vec<String> = validator
            .iter_errors(args)
            .filter(|e| !lenient(schema, e))
            .map(|e| describe(&e))
            .collect();
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

/// Owner decision (W3): type checks are as lenient as 6.2.0 effectively was, which
/// passed raw arguments to handlers that coerce or test truthiness. Exactly two
/// exceptions, nothing else:
/// - `number`/`integer` accepts a string that parses as one (`"2"`; JS uses the
///   value in query strings and arithmetic, e.g. SN-List-Incidents `priority`).
/// - `object` accepts a boolean when every declared property of that object is a
///   boolean flag: JS tests the object for truthiness and then reads flags off it
///   (`validateCatalogConfiguration`: `if (checks.ui_policies)` then
///   `checks.ui_policies.check_actions_linked`, undefined on `true`).
///
/// Arguments reach the handler unchanged; no value is coerced here.
fn lenient(schema: &serde_json::Value, err: &ValidationError<'_>) -> bool {
    let ValidationErrorKind::Type { kind } = err.kind() else {
        return false;
    };
    let expected: Vec<String> = match kind {
        TypeKind::Single(t) => vec![t.to_string()],
        TypeKind::Multiple(set) => set.iter().map(|t| t.to_string()).collect(),
    };
    let wants = |t: &str| expected.iter().any(|e| e == t);
    match err.instance().as_ref() {
        serde_json::Value::String(s) => {
            let parsed = if wants("integer") && is_json_integer(s) {
                s.parse::<i64>().ok().map(|v| v as f64)
            } else if wants("number") && is_json_number(s) {
                s.parse::<f64>().ok().filter(|v| v.is_finite())
            } else {
                None
            };
            // The range keywords skip strings, so apply them to the value here.
            parsed.is_some_and(|v| {
                err.schema_path()
                    .as_str()
                    .strip_suffix("/type")
                    .and_then(|parent| schema.pointer(parent))
                    .is_some_and(|sub| in_range(sub, v))
            })
        }
        serde_json::Value::Bool(_) if wants("object") => {
            // `schema_path` ends in `/type`; its parent is the object's subschema.
            let Some(parent) = err.schema_path().as_str().strip_suffix("/type") else {
                return false;
            };
            schema
                .pointer(parent)
                .and_then(|s| s.get("properties"))
                .and_then(|p| p.as_object())
                .is_some_and(|props| {
                    !props.is_empty()
                        && props
                            .values()
                            .all(|p| p.get("type").and_then(|t| t.as_str()) == Some("boolean"))
                })
        }
        _ => false,
    }
}

/// `-?\d+`: no sign but `-`, no whitespace or control characters.
fn is_json_integer(s: &str) -> bool {
    let digits = s.strip_prefix('-').unwrap_or(s);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

/// `-?(\d+\.?\d*|\.\d+)([eE][+-]?\d+)?`, the JS numeric-literal subset 6.2.0
/// handlers coerce; anything else (whitespace, CR/LF, `+`, `NaN`) is refused.
fn is_json_number(s: &str) -> bool {
    let s = s.strip_prefix('-').unwrap_or(s);
    let (mantissa, exponent) = match s.find(['e', 'E']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    let (int, frac) = match mantissa.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (mantissa, None),
    };
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    let mantissa_ok = digits(int)
        && frac.is_none_or(digits)
        && (!int.is_empty() || frac.is_some_and(|f| !f.is_empty()));
    let exponent_ok = exponent.is_none_or(|e| {
        let e = e.strip_prefix(['+', '-']).unwrap_or(e);
        !e.is_empty() && digits(e)
    });
    mantissa_ok && exponent_ok
}

/// `minimum` / `maximum` / `exclusive*` / `multipleOf` of `sub` on `v`.
fn in_range(sub: &serde_json::Value, v: f64) -> bool {
    let num = |k: &str| sub.get(k).and_then(serde_json::Value::as_f64);
    num("minimum").is_none_or(|m| v >= m)
        && num("maximum").is_none_or(|m| v <= m)
        && num("exclusiveMinimum").is_none_or(|m| v > m)
        && num("exclusiveMaximum").is_none_or(|m| v < m)
        && num("multipleOf").is_none_or(|m| m > 0.0 && (v / m).fract() == 0.0)
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

    const LENIENT: &str = r#"{
        "type":"object",
        "properties":{
            "priority":{"type":"number"},
            "count":{"type":"integer"},
            "limit":{"type":"integer","minimum":1,"maximum":1000},
            "ratio":{"type":"number","exclusiveMinimum":0,"exclusiveMaximum":1},
            "checks":{"type":"object","properties":{
                "ui_policies":{"type":"object","properties":{"check_a":{"type":"boolean"}}},
                "mixed":{"type":"object","properties":{"n":{"type":"string"}}}
            }}
        }
    }"#;

    #[test]
    fn numeric_strings_and_flag_object_booleans_pass_like_6_2_0() {
        let t = tool("Lenient", LENIENT);
        let v = CompiledSchemaValidator::new(std::slice::from_ref(&t)).expect("compiles");
        for ok in [
            json!({"priority": "2"}),
            json!({"priority": "2.5", "count": "3"}),
            json!({"checks": {"ui_policies": true, "business_rules": true}}),
            json!({"priority": "-1", "count": "-7", "limit": "1000"}),
            json!({"priority": ".5"}),
            json!({"priority": "1e3"}),
            json!({"ratio": "0.5"}),
            json!({"checks": {"ui_policies": false}}),
        ] {
            assert_eq!(v.validate(&t, &ok), Ok(()), "{ok}");
        }
        for bad in [
            json!({"priority": "two"}),
            json!({"priority": ""}),
            json!({"count": "2.5"}),
            json!({"priority": true}),
            json!({"checks": true}),
            json!({"checks": {"mixed": true}}),
            json!({"checks": {"ui_policies": "yes"}}),
            // M1: strict JSON-number text only, no whitespace or line breaks.
            json!({"priority": "2\r\n"}),
            json!({"priority": "2\n"}),
            json!({"priority": " 2"}),
            json!({"priority": "\t2"}),
            json!({"priority": "+2"}),
            json!({"priority": "NaN"}),
            json!({"priority": "1e"}),
            json!({"priority": "."}),
            json!({"priority": "-"}),
            json!({"count": "1e3"}),
            json!({"count": "3\r\n"}),
            // Range keywords apply to the parsed value.
            json!({"limit": "5000"}),
            json!({"limit": "0"}),
            json!({"ratio": "1"}),
            json!({"ratio": "0"}),
        ] {
            assert!(v.validate(&t, &bad).is_err(), "{bad}");
        }
    }
}
