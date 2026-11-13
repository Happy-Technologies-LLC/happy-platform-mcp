//! Lane B: ITSM, catalog, core listings (23 tools).
//! Empty until the lane ports its 6.2.0 tools.

use crate::families::FamilyDeps;
use crate::registration::{RegistrationError, ToolRegistry};

pub fn register(_registry: &mut ToolRegistry, _deps: &FamilyDeps) -> Result<(), RegistrationError> {
    Ok(())
}
