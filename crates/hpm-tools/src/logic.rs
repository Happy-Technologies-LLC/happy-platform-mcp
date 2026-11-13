//! Pure-logic ports (lane C): deterministic and free of network or ServiceNow
//! access. Each module mirrors one JavaScript 6.2.0 module output for output,
//! including error messages and limits. The one filesystem helper is
//! [`generated_scripts::write_fix_script_file`], ported with its containment
//! rules from `src/generated-scripts.js`.

pub mod bulk_limits;
pub mod encoded_query;
pub mod generated_scripts;
pub mod js;
pub mod nl;
pub mod url_limits;
