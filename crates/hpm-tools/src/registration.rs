//! Tool and resource registration. Each tool lane exposes
//! `pub fn register(registry: &mut ToolRegistry) -> Result<(), RegistrationError>`;
//! the composition root calls every lane's `register` in 6.2.0 `tools/list` order (C5)
//! and hands the registry to [`crate::dispatcher::Dispatcher::new`]. Listing order
//! is registration order.
//!
//! Every tool must match its row in the bundled D-05 classification
//! ([`ToolClassificationSet::bundled`]): declared access floor, output class, retry
//! eligibility and side-effect class (review M3).

use hpm_types::classification::{SideEffect, ToolClassification, ToolClassificationSet};
use hpm_types::tool::{ResourceDescriptor, SideEffectClass, ToolDescriptor};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum RegistrationError {
    #[error("duplicate tool name: {0}")]
    DuplicateTool(&'static str),
    #[error("duplicate resource uri: {0}")]
    DuplicateResource(&'static str),
    /// `input_schema_json` must parse as a JSON object (it is served verbatim as
    /// `inputSchema`).
    #[error("tool {0}: input schema is not a JSON object")]
    InvalidSchema(&'static str),
    /// The bundled D-05 classification did not load.
    #[error("tool classification unavailable: {0}")]
    Classification(String),
    /// The tool has no row in the bundled D-05 classification.
    #[error("tool {0}: not in the D-05 classification")]
    UnclassifiedTool(&'static str),
    /// A declared class disagrees with the tool's D-05 row (`field` names which).
    #[error("tool {tool}: {field} does not match the D-05 classification")]
    ClassificationMismatch {
        tool: &'static str,
        field: &'static str,
    },
}

pub struct ToolRegistry {
    classification: ToolClassificationSet,
    tools: Vec<ToolDescriptor>,
    resources: Vec<ResourceDescriptor>,
}

/// D-05 side effect → descriptor side-effect class. Session state is process-local.
fn side_effect_class(e: SideEffect) -> Option<SideEffectClass> {
    match e {
        SideEffect::None => Some(SideEffectClass::None),
        SideEffect::LocalDisk | SideEffect::SessionState => Some(SideEffectClass::Local),
        SideEffect::InstanceConfig | SideEffect::InstanceData | SideEffect::InstanceCode => {
            Some(SideEffectClass::Remote)
        }
        _ => None,
    }
}

/// First field of `tool` that disagrees with `row`.
fn mismatch(tool: &ToolDescriptor, row: &ToolClassification) -> Option<&'static str> {
    if tool.access != row.descriptor_floor() {
        Some("access")
    } else if tool.output != row.output {
        Some("output")
    } else if tool.retry != row.retry.eligibility() {
        Some("retry")
    } else if side_effect_class(row.side_effect) != Some(tool.side_effect) {
        Some("side_effect")
    } else {
        None
    }
}

impl ToolRegistry {
    /// Loads the bundled D-05 classification every registration is checked against.
    pub fn new() -> Result<Self, RegistrationError> {
        let classification = ToolClassificationSet::bundled()
            .map_err(|e| RegistrationError::Classification(e.to_string()))?;
        Ok(Self {
            classification,
            tools: Vec::new(),
            resources: Vec::new(),
        })
    }

    pub fn register_tool(&mut self, tool: ToolDescriptor) -> Result<(), RegistrationError> {
        if self.tools.iter().any(|t| t.name == tool.name) {
            return Err(RegistrationError::DuplicateTool(tool.name));
        }
        let schema: Result<serde_json::Value, _> = serde_json::from_str(tool.input_schema_json);
        if !matches!(schema, Ok(serde_json::Value::Object(_))) {
            return Err(RegistrationError::InvalidSchema(tool.name));
        }
        let row = self
            .classification
            .get(tool.name)
            .ok_or(RegistrationError::UnclassifiedTool(tool.name))?;
        if let Some(field) = mismatch(&tool, row) {
            return Err(RegistrationError::ClassificationMismatch {
                tool: tool.name,
                field,
            });
        }
        self.tools.push(tool);
        Ok(())
    }

    pub fn register_resource(
        &mut self,
        resource: ResourceDescriptor,
    ) -> Result<(), RegistrationError> {
        if self.resources.iter().any(|r| r.uri == resource.uri) {
            return Err(RegistrationError::DuplicateResource(resource.uri));
        }
        self.resources.push(resource);
        Ok(())
    }

    /// Registered tools in registration (`tools/list`) order; used to compile input schemas.
    pub fn tools(&self) -> &[ToolDescriptor] {
        &self.tools
    }

    pub(crate) fn into_parts(self) -> (Vec<ToolDescriptor>, Vec<ResourceDescriptor>) {
        (self.tools, self.resources)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, LazyLock};

    use hpm_types::BoxFut;
    use hpm_types::context::RequestContext;
    use hpm_types::operation::OperationSet;
    use hpm_types::tool::{
        AccessClass, HandlerCompletion, OutputClass, RetryEligibility, ToolHandler,
    };

    use super::*;

    struct NoHandler;
    impl ToolHandler for NoHandler {
        fn call<'a>(
            &'a self,
            _: &'a RequestContext,
            _: serde_json::Value,
        ) -> BoxFut<'a, HandlerCompletion> {
            unreachable!("registration tests never call handlers")
        }
    }

    /// `'static` rows so descriptor names borrow from them.
    static SET: LazyLock<ToolClassificationSet> =
        LazyLock::new(|| ToolClassificationSet::bundled().expect("bundled"));

    fn row(n: &str) -> &'static ToolClassification {
        SET.get(n).expect(n)
    }

    fn from_row(row: &'static ToolClassification) -> ToolDescriptor {
        ToolDescriptor::new(
            row.name.as_str(),
            "",
            r#"{"type":"object"}"#,
            |_, _| Ok(OperationSet::default()),
            Arc::new(NoHandler),
            |_| String::new(),
        )
        .access(row.descriptor_floor())
        .output(row.output)
        .retry(row.retry.eligibility())
        .side_effect(side_effect_class(row.side_effect).expect("mapped side effect"))
    }

    #[test]
    fn every_d05_row_registers_when_declared_to_match() {
        let mut registry = ToolRegistry::new().expect("registry");
        for row in SET.iter() {
            registry.register_tool(from_row(row)).expect(&row.name);
        }
        assert_eq!(registry.tools().len(), 60);
    }

    #[test]
    fn declared_class_mismatch_is_rejected() {
        let mut registry = ToolRegistry::new().expect("registry");
        let cases: Vec<(ToolDescriptor, &'static str)> = vec![
            (
                from_row(row("SN-Execute-Background-Script")).access(AccessClass::Read),
                "access",
            ),
            (
                from_row(row("SN-Query-Table")).output(OutputClass::None),
                "output",
            ),
            (
                from_row(row("SN-Create-Record")).retry(RetryEligibility::Idempotent),
                "retry",
            ),
            (
                from_row(row("SN-Create-Record")).side_effect(SideEffectClass::None),
                "side_effect",
            ),
        ];
        for (tool, field) in cases {
            let name = tool.name;
            assert_eq!(
                registry.register_tool(tool),
                Err(RegistrationError::ClassificationMismatch { tool: name, field })
            );
        }
        let unknown = ToolDescriptor::new(
            "Not-A-Tool",
            "",
            r#"{"type":"object"}"#,
            |_, _| Ok(OperationSet::default()),
            Arc::new(NoHandler),
            |_| String::new(),
        );
        assert_eq!(
            registry.register_tool(unknown),
            Err(RegistrationError::UnclassifiedTool("Not-A-Tool"))
        );
        assert!(registry.tools().is_empty());
    }
}
