//! Normalized operation (RFC §3, F1). Policy decides on this, never on a tool name.
//!
//! An `OperationSet` describes every explicit operation the handler may perform
//! (review edit 1), for tools and resources (edit 9), classified by one versioned,
//! per-instance classifier (§3.1, Q5).

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

/// No `Ord`: actions are not privilege levels (RFC §1 rule 7). Access requirements
/// use explicit mappings (`AccessClass::floor_satisfied`, policy rules).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Action {
    /// Schema/metadata/docs reads with no record data.
    Metadata,
    Read,
    Create,
    Update,
    Delete,
    /// Server-side code execution: background scripts, and any write to an
    /// execute-class or unknown table (table-level rule, §3.1).
    Execute,
    /// Write to a privilege/schema/credential table.
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
    /// Not in the instance's `sys_db_object` closure, or the dictionary load failed.
    /// Writes classify as Execute (fail closed).
    Unknown,
    /// The operation targets no table (resource, local config, docs).
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
    /// `/records/3` for batch element 3, `""` for the whole call.
    pub arg_location: String,
    /// Fields returned to the model. Empty means all fields.
    pub returned_fields: Vec<String>,
    /// Fields written (mutations).
    pub written_fields: Vec<String>,
    /// Fields referenced by the encoded query or filter, including dot-walks.
    pub predicate_fields: Vec<String>,
    /// Fields used for ordering (ORDERBY leaks by inference).
    pub ordering_fields: Vec<String>,
    /// Decoded, whitespace-normalized `javascript:` operands (without the prefix)
    /// found in the query or in written values (`conditions`, `reference_qual`,
    /// `default_value`, `template_value`). Checked by [`script_operand_allowed`].
    pub script_operands: Vec<String>,
}

/// A whole call: every batch element, generated convenience query and fallback
/// write. Decided as one set before the first write.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OperationSet(pub Vec<Operation>);

impl OperationSet {
    pub fn mutates(&self) -> bool {
        self.0.iter().any(|o| o.action.is_mutation())
    }
    pub fn has_unknown_class(&self) -> bool {
        self.0.iter().any(|o| o.class == TableClass::Unknown)
    }
}

/// Product-owned static carriers/containers baseline (§3.1 step 2). Unioned with the
/// per-instance dictionary-derived set; deployments may only add to it.
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

/// Table-name prefixes that are always execute-class.
pub const EXECUTE_STATIC_PREFIXES: &[&str] = &[
    "wf_",
    "sys_hub_",
    "sys_flow_",
    "sys_atf_",
    "sys_pd_",
    "sysauto",
    "sys_script",
];

/// Immutable per-instance classification, built at connect from `sys_dictionary`
/// and the `sys_db_object.super_class` closure. Versioned; pinned for a call.
#[derive(Clone, Debug, Default)]
pub struct ClassificationSnapshot {
    pub version: u64,
    /// Product baseline rule version this snapshot was derived with.
    pub baseline_version: u32,
    /// All tables visible in `sys_db_object` (ancestry known).
    pub known_tables: HashSet<String>,
    /// Closure of tables (own or inherited) carrying code, plus deployment additions.
    /// The static baseline is applied by [`ClassificationSnapshot::classify`].
    pub execute_tables: HashSet<String>,
    /// Exact privilege/schema list plus any table with a password field.
    pub admin_tables: HashSet<String>,
    /// `super_class` parent map, for diagnostics and ancestry queries.
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
            || EXECUTE_STATIC_TABLES.contains(&table)
            || EXECUTE_STATIC_PREFIXES.iter().any(|p| table.starts_with(p))
        {
            return TableClass::ExecuteBearing;
        }
        if self.admin_tables.contains(table) {
            return TableClass::Admin;
        }
        TableClass::Ordinary
    }

    /// Table-level rule (§3.1 steps 4–5): any write to an execute-class or unknown
    /// table is Execute; any write to an admin table is AdminWrite. Reads keep their
    /// requested action.
    pub fn classify_write(&self, table: &str, requested: Action) -> (Action, TableClass) {
        let class = self.classify(table);
        if !requested.is_mutation() {
            return (requested, class);
        }
        let action = match class {
            TableClass::ExecuteBearing | TableClass::Unknown => Action::Execute,
            TableClass::Admin => Action::AdminWrite,
            TableClass::Ordinary | TableClass::NotApplicable => requested,
        };
        (action, class)
    }
}

/// Supplies the pinned snapshot for the call's instance. Refresh on miss/TTL happens
/// outside a call; a call never sees two versions.
pub trait ClassificationSource: Send + Sync {
    fn snapshot(&self, instance: Option<&ResolvedInstance>) -> Arc<ClassificationSnapshot>;
}

/// Configuration of the `javascript:` operand guard (RFC §3.2).
///
/// Owner decision (2026-11-13, RFC §14): the guard rejects in every mode, single-user
/// included. There is no "off" setting; the 6.2.0 pass-through is a recorded golden
/// deviation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScriptOperandGuard {
    /// The ServiceNow account is shared (the integration user is not the caller), so
    /// `gs.getUserID()`/`gs.getUserName()` would resolve to the wrong person and are
    /// rejected.
    pub shared_account: bool,
}

/// `operand` is already decoded and whitespace-normalized by the normalizer, without
/// the `javascript:` prefix. Allowed: `gs.getUserID()`, `gs.getUserName()` (unless the
/// account is shared), and `gs.beginningOf*` / `gs.endOf*` / `gs.*Ago*` with zero or
/// one integer argument: the shapes `src/natural-language.js` emits. Everything else
/// is rejected with `QUERY_SCRIPT_NOT_ALLOWED` before policy and before any network
/// call.
pub fn script_operand_allowed(operand: &str, guard: ScriptOperandGuard) -> bool {
    let Some(call) = operand.trim().strip_prefix("gs.") else {
        return false;
    };
    let Some((name, rest)) = call.split_once('(') else {
        return false;
    };
    let Some(arg) = rest.strip_suffix(')') else {
        return false;
    };
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()) {
        return false;
    }
    let int_arg = arg.trim();
    let arg_ok = int_arg.is_empty()
        || (int_arg.parse::<i64>().is_ok()
            && int_arg.chars().all(|c| c.is_ascii_digit() || c == '-'));
    match name {
        "getUserID" | "getUserName" => arg.is_empty() && !guard.shared_account,
        n if n.starts_with("beginningOf") || n.starts_with("endOf") || n.contains("Ago") => arg_ok,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOLO: ScriptOperandGuard = ScriptOperandGuard {
        shared_account: false,
    };
    const SHARED: ScriptOperandGuard = ScriptOperandGuard {
        shared_account: true,
    };

    /// RFC §3.2 accept/reject table. Each row: operand, allowed (solo), allowed (shared).
    #[test]
    fn script_operand_allowlist_table() {
        let rows: &[(&str, bool, bool)] = &[
            // Identity helpers: only when the account is the caller's own.
            ("gs.getUserID()", true, false),
            ("gs.getUserName()", true, false),
            (" gs.getUserID() ", true, false),
            ("gs.getUserID(1)", false, false),
            // Date helpers emitted by natural-language.js, zero or one integer arg.
            ("gs.beginningOfToday()", true, true),
            ("gs.endOfThisMonth()", true, true),
            ("gs.daysAgo(7)", true, true),
            ("gs.daysAgoStart(0)", true, true),
            ("gs.hoursAgo(-3)", true, true),
            ("gs.beginningOfLast7Days()", true, true),
            // Non-integer or multiple arguments.
            ("gs.daysAgo('7')", false, false),
            ("gs.daysAgo(7, 1)", false, false),
            ("gs.daysAgo(1.5)", false, false),
            ("gs.daysAgo(+7)", false, false),
            ("gs.daysAgo(0x10)", false, false),
            // Other gs calls, including state-changing ones on the evaluator allowlist.
            ("gs.getProperty('glide.war')", false, false),
            ("gs.setProperty('a','b')", false, false),
            ("gs.eventQueue('x')", false, false),
            ("gs.nowDateTime()", false, false),
            // Not a single gs call.
            ("new GlideRecord('sys_user').deleteMultiple()", false, false),
            ("gs.daysAgo(7);gs.setProperty('a','b')", false, false),
            ("gs.daysAgo(7)+gs.setProperty('a','b')", false, false),
            ("gs.daysAgo(gs.getUserID())", false, false),
            ("gs.daysAgo(7))", false, false),
            ("gs.daysAgo", false, false),
            ("gs.()", false, false),
            ("gs.days_Ago(7)", false, false),
            ("getUserID()", false, false),
            ("javascript:gs.getUserID()", false, false),
            ("", false, false),
        ];
        for &(operand, solo, shared) in rows {
            assert_eq!(
                script_operand_allowed(operand, SOLO),
                solo,
                "solo: {operand:?}"
            );
            assert_eq!(
                script_operand_allowed(operand, SHARED),
                shared,
                "shared: {operand:?}"
            );
        }
    }

    fn snapshot() -> ClassificationSnapshot {
        let set = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<HashSet<_>>();
        ClassificationSnapshot {
            version: 1,
            baseline_version: 1,
            known_tables: set(&[
                "incident",
                "sys_script_include",
                "sys_user_has_role",
                "u_custom_code",
                "sys_update_xml",
                "wf_activity",
            ]),
            execute_tables: set(&["u_custom_code"]),
            admin_tables: set(&["sys_user_has_role"]),
            parent: HashMap::new(),
            complete: true,
        }
    }

    #[test]
    fn table_rule_escalates_writes_and_fails_closed() {
        let s = snapshot();
        let cases: &[(&str, Action, Action, TableClass)] = &[
            (
                "incident",
                Action::Create,
                Action::Create,
                TableClass::Ordinary,
            ),
            ("incident", Action::Read, Action::Read, TableClass::Ordinary),
            // Derived execute set, static list and static prefix.
            (
                "u_custom_code",
                Action::Update,
                Action::Execute,
                TableClass::ExecuteBearing,
            ),
            (
                "sys_update_xml",
                Action::Delete,
                Action::Execute,
                TableClass::ExecuteBearing,
            ),
            (
                "sys_script_include",
                Action::Create,
                Action::Execute,
                TableClass::ExecuteBearing,
            ),
            (
                "wf_activity",
                Action::Create,
                Action::Execute,
                TableClass::ExecuteBearing,
            ),
            (
                "sys_user_has_role",
                Action::Create,
                Action::AdminWrite,
                TableClass::Admin,
            ),
            // Not in sys_db_object: write is Execute, read stays Read but is Unknown.
            (
                "var__m_x",
                Action::Create,
                Action::Execute,
                TableClass::Unknown,
            ),
            ("var__m_x", Action::Read, Action::Read, TableClass::Unknown),
        ];
        for &(table, requested, action, class) in cases {
            assert_eq!(
                s.classify_write(table, requested),
                (action, class),
                "{table} {requested:?}"
            );
        }
        let failed = ClassificationSnapshot {
            complete: false,
            ..snapshot()
        };
        assert_eq!(
            failed.classify_write("incident", Action::Update),
            (Action::Execute, TableClass::Unknown)
        );
    }
}
