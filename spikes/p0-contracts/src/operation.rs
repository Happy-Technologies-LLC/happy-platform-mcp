//! Normalized operation (F1). Policy decides on THIS, never on tool name alone.
//! Describes every explicit operation the handler may perform (review edit 1),
//! for tools AND resources (edit 9), classified by one versioned, per-instance
//! classifier shared with D05 (Q5; exec-table-verification.md §3).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::context::ResolvedInstance;

/// What policy and protection evaluate (edit 9). Visibility is a listing filter only.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Subject {
    Tool(&'static str),
    /// Resource URI, e.g. `servicenow://instance`.
    Resource(String),
}

/// No `Ord`: actions are not privilege levels (non-blocking suggestion 2). Access
/// requirements use explicit mappings (`AccessClass::floor_satisfied`, policy rules).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Action {
    /// Schema/metadata/docs reads with no record data.
    Metadata,
    Read,
    Create,
    Update,
    Delete,
    /// Server-side code execution: background scripts, AND any write to an
    /// execute-class table (table-level rule; unknown tables land here too).
    Execute,
    /// Write to a privilege/schema/credential table (exec-table-verification.md §4).
    AdminWrite,
    /// Registry / instance / update-set selection changes (F9).
    ConfigWrite,
}

impl Action {
    pub fn is_mutation(self) -> bool {
        !matches!(self, Action::Metadata | Action::Read)
    }
}

/// Table class from the classifier. `Unknown` never silently becomes ordinary Write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum TableClass {
    Ordinary,
    ExecuteBearing,
    Admin,
    /// Not in the instance's `sys_db_object` closure, or dictionary load failed.
    /// Writes classify as Execute (fail closed).
    Unknown,
    /// Operation targets no table (resource, local config, docs).
    NotApplicable,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub subject: Subject,
    pub instance_id: Option<String>,
    pub table: Option<String>,
    pub action: Action,
    pub class: TableClass,
    /// JSON pointer into the validated arguments this operation came from, e.g.
    /// `/records/3` for batch element 3, `""` for the whole call (edit 1).
    pub arg_location: String,
    /// Fields returned to the model. Empty = all fields.
    pub returned_fields: Vec<String>,
    /// Fields written (mutations).
    pub written_fields: Vec<String>,
    /// Fields referenced by the encoded query / filter, incl. dot-walks.
    pub predicate_fields: Vec<String>,
    /// Decoded + normalized `javascript:` operands found in the query or in written
    /// values (`conditions`, `reference_qual`, `default_value`, `template_value`).
    /// The input guard rejects any not on the product allowlist
    /// (javascript-query-probe.md): Table API evaluates them server-side and a
    /// blocked script returns HTTP 200 + empty result (silent).
    pub script_operands: Vec<String>,
    /// Fields used for ordering (ORDERBY leaks by inference).
    pub ordering_fields: Vec<String>,
}

/// A whole call: every batch element, generated convenience query and fallback
/// write. Decided as one set before the first write.
#[derive(Clone, Debug, Default)]
pub struct OperationSet(pub Vec<Operation>);

impl OperationSet {
    pub fn mutates(&self) -> bool {
        self.0.iter().any(|o| o.action.is_mutation())
    }
    pub fn has_unknown_class(&self) -> bool {
        self.0.iter().any(|o| o.class == TableClass::Unknown)
    }
}

/// Product-owned static carriers/containers baseline (exec-table-verification.md §3
/// step 2). Unioned with the per-instance dictionary-derived set; deployments may
/// only ADD to it.
pub const EXECUTE_STATIC_TABLES: &[&str] = &[
    "sys_variable_value",
    "sys_update_xml",
    "sys_remote_update_set",
    "ecc_queue",
    "wf_workflow",
    "wf_transition",
    "sys_ws_definition",
    "sys_atf_test",
    "sys_atf_test_suite",
];
pub const EXECUTE_STATIC_PREFIXES: &[&str] =
    &["wf_", "sys_hub_", "sys_flow_", "sys_atf_", "sys_pd_", "sysauto", "sys_script"];

/// Immutable per-instance classification, built at connect from `sys_dictionary`
/// (script-typed fields + named string code fields) and `sys_db_object.super_class`
/// closure (~62 KB, one request). Versioned; pinned for a call.
#[derive(Debug)]
pub struct ClassificationSnapshot {
    pub version: u64,
    /// Product baseline rule version this was derived with.
    pub baseline_version: u32,
    /// All tables visible in `sys_db_object` (ancestry known).
    pub known_tables: HashSet<String>,
    /// Closure: tables (own or inherited) carrying code, plus static baseline,
    /// plus deployment additions.
    pub execute_tables: HashSet<String>,
    /// Exact privilege/schema list + any table with a password field.
    pub admin_tables: HashSet<String>,
    /// `parent` map, for diagnostics/ancestry queries.
    pub parent: HashMap<String, String>,
    /// False if the dictionary load failed: every table is then Unknown.
    pub complete: bool,
}

impl ClassificationSnapshot {
    pub fn classify(&self, table: &str) -> TableClass {
        if !self.complete || !self.known_tables.contains(table) {
            return TableClass::Unknown;
        }
        if self.execute_tables.contains(table)
            || EXECUTE_STATIC_PREFIXES.iter().any(|p| table.starts_with(p))
        {
            return TableClass::ExecuteBearing;
        }
        if self.admin_tables.contains(table) {
            return TableClass::Admin;
        }
        TableClass::Ordinary
    }

    /// Table-level rule: any write to an execute-class (or unknown) table is Execute.
    pub fn classify_write(&self, table: &str, requested: Action) -> (Action, TableClass) {
        let class = self.classify(table);
        if !requested.is_mutation() {
            return (requested, class);
        }
        let action = match class {
            TableClass::ExecuteBearing | TableClass::Unknown => Action::Execute,
            TableClass::Admin => Action::AdminWrite,
            _ => requested,
        };
        (action, class)
    }
}

/// Supplies the pinned snapshot for the call's instance. Refresh on miss/TTL
/// happens outside a call; a call never sees two versions.
pub trait ClassificationSource: Send + Sync {
    fn snapshot(&self, instance: Option<&ResolvedInstance>) -> Arc<ClassificationSnapshot>;
}

/// Input-guard stage for `javascript:` operands (javascript-query-probe.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScriptOperandGuard {
    /// 6.2.0 behaviour (byte parity). Only if the owner keeps parity for SU.
    Off,
    /// Reject everything except the allowlist below with an explicit error.
    /// `shared_account`: the integration user is not the caller, so
    /// `gs.getUserID()`/`gs.getUserName()` would resolve to the wrong person ⇒ rejected.
    Allowlist { shared_account: bool },
}

/// `operand` is already decoded and whitespace-normalized by the normalizer,
/// without the `javascript:` prefix. Allowed: `gs.getUserID()`, `gs.getUserName()`,
/// and `gs.beginningOf*` / `gs.endOf*` / `gs.*Ago*` with zero or one integer
/// argument (the shapes `src/natural-language.js` emits).
pub fn script_operand_allowed(operand: &str, guard: ScriptOperandGuard) -> bool {
    let shared = match guard {
        ScriptOperandGuard::Off => return true,
        ScriptOperandGuard::Allowlist { shared_account } => shared_account,
    };
    let Some(call) = operand.trim().strip_prefix("gs.") else { return false };
    let Some((name, rest)) = call.split_once('(') else { return false };
    let Some(arg) = rest.strip_suffix(')') else { return false };
    if !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let arg_ok = arg.is_empty() || arg.parse::<i64>().is_ok();
    match name {
        "getUserID" | "getUserName" => arg.is_empty() && !shared,
        n if n.starts_with("beginningOf") || n.starts_with("endOf") || n.contains("Ago") => arg_ok,
        _ => false,
    }
}
