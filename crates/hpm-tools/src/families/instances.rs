//! Lane D: instance tools (instance-tools.js).
//! Empty until the lane ports its 6.2.0 tools.

use crate::families::FamilyDeps;
use crate::registration::{RegistrationError, ToolRegistry};

pub fn register(_registry: &mut ToolRegistry, _deps: &FamilyDeps) -> Result<(), RegistrationError> {
    Ok(())
}
