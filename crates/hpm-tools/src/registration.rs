//! Tool and resource registration. Each tool lane exposes
//! `pub fn register(registry: &mut ToolRegistry) -> Result<(), RegistrationError>`;
//! the composition root calls every lane's `register` in 6.2.0 `tools/list` order (C5)
//! and hands the registry to [`crate::dispatcher::Dispatcher::new`]. Listing order
//! is registration order.

use hpm_types::tool::{ResourceDescriptor, ToolDescriptor};

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
}

#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<ToolDescriptor>,
    resources: Vec<ResourceDescriptor>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_tool(&mut self, tool: ToolDescriptor) -> Result<(), RegistrationError> {
        if self.tools.iter().any(|t| t.name == tool.name) {
            return Err(RegistrationError::DuplicateTool(tool.name));
        }
        let schema: Result<serde_json::Value, _> = serde_json::from_str(tool.input_schema_json);
        if !matches!(schema, Ok(serde_json::Value::Object(_))) {
            return Err(RegistrationError::InvalidSchema(tool.name));
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

    pub(crate) fn into_parts(self) -> (Vec<ToolDescriptor>, Vec<ResourceDescriptor>) {
        (self.tools, self.resources)
    }
}
