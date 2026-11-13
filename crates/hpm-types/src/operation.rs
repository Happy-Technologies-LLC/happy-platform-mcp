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

/// One normalized operation. `#[non_exhaustive]`: build with [`Operation::new`] and
/// the builder setters; fields stay readable.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
    /// found in the encoded query or filter only. Checked by
    /// [`script_operand_allowed`]; a rejected operand is `QUERY_SCRIPT_NOT_ALLOWED`
    /// (signed deviation, RFC §14).
    pub query_script_operands: Vec<String>,
    /// Names of written fields whose value is script ([`is_script_value`]:
    /// `reference_qual`, `default_value`, `conditions`, …). Never rejected: the
    /// operation escalates to Execute (D-05 value rule 5, 6.2.0 parity), see
    /// [`OperationSet::escalate_written_scripts`].
    pub written_script_fields: Vec<String>,
}

impl Operation {
    /// No table (`TableClass::NotApplicable`), whole-call location, no fields.
    pub fn new(subject: Subject, action: Action) -> Self {
        Self {
            subject,
            instance_id: None,
            table: None,
            action,
            class: TableClass::NotApplicable,
            arg_location: String::new(),
            returned_fields: Vec::new(),
            written_fields: Vec::new(),
            predicate_fields: Vec::new(),
            ordering_fields: Vec::new(),
            query_script_operands: Vec::new(),
            written_script_fields: Vec::new(),
        }
    }

    /// Sets the table and applies the table rule ([`ClassificationSnapshot::classify_write`])
    /// to the current action.
    pub fn on_table(mut self, table: impl Into<String>, snapshot: &ClassificationSnapshot) -> Self {
        let table = table.into();
        (self.action, self.class) = snapshot.classify_write(&table, self.action);
        self.table = Some(table);
        self
    }
    /// Sets table and class verbatim (no table rule). Prefer [`Self::on_table`].
    pub fn table(mut self, table: impl Into<String>, class: TableClass) -> Self {
        self.table = Some(table.into());
        self.class = class;
        self
    }
    pub fn instance_id(mut self, id: Option<String>) -> Self {
        self.instance_id = id;
        self
    }
    pub fn at(mut self, arg_location: impl Into<String>) -> Self {
        self.arg_location = arg_location.into();
        self
    }
    pub fn returned_fields(mut self, f: Vec<String>) -> Self {
        self.returned_fields = f;
        self
    }
    pub fn written_fields(mut self, f: Vec<String>) -> Self {
        self.written_fields = f;
        self
    }
    pub fn predicate_fields(mut self, f: Vec<String>) -> Self {
        self.predicate_fields = f;
        self
    }
    pub fn ordering_fields(mut self, f: Vec<String>) -> Self {
        self.ordering_fields = f;
        self
    }
    pub fn query_script_operands(mut self, o: Vec<String>) -> Self {
        self.query_script_operands = o;
        self
    }
    pub fn written_script_fields(mut self, f: Vec<String>) -> Self {
        self.written_script_fields = f;
        self
    }
}

/// D-05 value rule 5: a written string whose trimmed text starts with `javascript:`
/// (case-insensitive) or contains a `<g:evaluate` / `${` Jelly/script marker.
pub fn is_script_value(value: &str) -> bool {
    let t = value.trim_start();
    t.get(..11)
        .is_some_and(|p| p.eq_ignore_ascii_case("javascript:"))
        || value.contains("<g:evaluate")
        || value.contains("${")
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
    pub fn has_execute(&self) -> bool {
        self.0.iter().any(|o| o.action == Action::Execute)
    }
    /// D-05 value rule 5: a mutation writing a script value becomes Execute. Admin
    /// outranks execute (D-05 `execute < admin`), so `AdminWrite` stays. The
    /// dispatcher applies this to every normalized set before the access floor.
    pub fn escalate_written_scripts(&mut self) {
        for o in &mut self.0 {
            if !o.written_script_fields.is_empty()
                && o.action.is_mutation()
                && o.action != Action::AdminWrite
            {
                o.action = Action::Execute;
            }
        }
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

/// D-05 admin tables (exact list, rule 7). Match the table or any ancestor.
/// `user_criteria` and `sys_security_acl` are also script-bearing; admin wins.
pub const ADMIN_STATIC_TABLES: &[&str] = &[
    "sys_user",
    "sys_user_role",
    "sys_user_has_role",
    "sys_user_grmember",
    "sys_user_group",
    "sys_group_has_role",
    "sys_user_role_contains",
    "sys_user_has_role_time_limited",
    "sys_user_delegate",
    "sys_security_acl",
    "sys_security_acl_role",
    "sys_security_attribute",
    "sys_security_data_filter",
    "sys_security_restricted_list",
    "sys_security_type",
    "sys_security_operation",
    "sys_security_obj_oper",
    "sys_properties",
    "sys_auth_profile",
    "sys_auth_profile_basic",
    "sys_auth_profile_oauth2",
    "oauth_entity",
    "oauth_credential",
    "oauth_entity_profile",
    "sys_certificate",
    "discovery_credentials",
    "sys_alias",
    "sys_connection",
    "sys_rest_message",
    "sys_rest_message_fn",
    "sys_soap_message",
    "sys_scope_privilege",
    "sys_restricted_caller_access",
    "sys_api_access_policy",
    "sys_soap_api_access_policy",
    "ip_access",
    "sys_cors_rule",
    "sys_dictionary_override",
    "user_criteria",
    "sys_glide_object",
    "sys_scope",
    "sys_app",
    "sys_store_app",
];

/// D-05 admin tables that match exact-only: no propagation to descendants (otherwise
/// the `var_dictionary`-derived variable tables would become admin).
pub const ADMIN_EXACT_ONLY: &[&str] = &["sys_dictionary", "sys_db_object"];

/// D-05 admin prefixes. Match the table or any ancestor.
pub const ADMIN_STATIC_PREFIXES: &[&str] = &["sys_auth_", "oauth_"];

/// Bound on `super_class` walks (defends against a cyclic parent map).
const MAX_ANCESTRY: usize = 64;

/// Immutable per-instance classification, built at connect from `sys_dictionary`
/// and the `sys_db_object.super_class` closure. Versioned; pinned for a call.
/// `#[non_exhaustive]`: start from `Default` and assign fields.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct ClassificationSnapshot {
    pub version: u64,
    /// Product baseline rule version this snapshot was derived with.
    pub baseline_version: u32,
    /// All tables visible in `sys_db_object` (ancestry known).
    pub known_tables: HashSet<String>,
    /// Closure of tables (own or inherited) carrying code, plus deployment additions.
    /// The static baseline is applied by [`ClassificationSnapshot::classify`].
    pub execute_tables: HashSet<String>,
    /// Tables with an own or inherited password field, plus deployment additions.
    /// The static admin baseline is applied by [`ClassificationSnapshot::classify`].
    pub admin_tables: HashSet<String>,
    /// `super_class` parent map, for ancestry and diagnostics.
    pub parent: HashMap<String, String>,
    /// False if the dictionary load failed: every table outside the static
    /// baselines is then Unknown.
    pub complete: bool,
}

impl ClassificationSnapshot {
    /// The table followed by its `super_class` ancestors (bounded).
    fn lineage<'a>(&'a self, table: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        std::iter::successors(Some(table), |t| self.parent.get(*t).map(String::as_str))
            .take(MAX_ANCESTRY)
    }

    /// D-05 order: static baselines first (they hold even when the table is not in
    /// `sys_db_object` or the load failed), admin before execute (admin outranks
    /// execute), then the known-table check, then Ordinary.
    pub fn classify(&self, table: &str) -> TableClass {
        let admin = ADMIN_EXACT_ONLY.contains(&table)
            || self.lineage(table).any(|t| {
                ADMIN_STATIC_TABLES.contains(&t)
                    || ADMIN_STATIC_PREFIXES.iter().any(|p| t.starts_with(p))
                    || self.admin_tables.contains(t)
            });
        if admin {
            return TableClass::Admin;
        }
        let execute = self.lineage(table).any(|t| {
            EXECUTE_STATIC_TABLES.contains(&t)
                || EXECUTE_STATIC_PREFIXES.iter().any(|p| t.starts_with(p))
                || self.execute_tables.contains(t)
        });
        if execute {
            return TableClass::ExecuteBearing;
        }
        if !self.complete || !self.known_tables.contains(table) {
            return TableClass::Unknown;
        }
        TableClass::Ordinary
    }

    /// Table-level rule (§3.1 steps 4–5): any write to an execute-class or unknown
    /// table is Execute; any write to an admin table is AdminWrite (admin outranks
    /// execute; policy must treat AdminWrite as at least as privileged as Execute).
    /// Reads keep their requested action.
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

    /// D-05 rule 7 static admin baseline, per table class. The snapshot knows none
    /// of these tables and has no derived admin set: the static list still applies.
    #[test]
    fn static_admin_baseline_per_table_class() {
        let mut s = ClassificationSnapshot {
            complete: true,
            ..Default::default()
        };
        s.known_tables.insert("incident".into());
        s.parent.insert(
            "sys_generated_security_acl".into(),
            "sys_security_acl".into(),
        );
        s.parent
            .insert("var_dictionary".into(), "sys_dictionary".into());
        s.parent.insert("u_my_users".into(), "sys_user".into());
        // Script-bearing per the instance dictionary, but admin wins.
        s.execute_tables.insert("sys_security_acl".into());
        s.execute_tables.insert("user_criteria".into());
        let rows: &[(&str, TableClass)] = &[
            // exact list
            ("sys_user_has_role", TableClass::Admin),
            ("sys_properties", TableClass::Admin),
            ("sys_store_app", TableClass::Admin),
            // admin ∧ execute ⇒ admin
            ("sys_security_acl", TableClass::Admin),
            ("user_criteria", TableClass::Admin),
            // propagation to descendants
            ("sys_generated_security_acl", TableClass::Admin),
            ("u_my_users", TableClass::Admin),
            // prefixes
            ("sys_auth_policy_x", TableClass::Admin),
            ("oauth_jwt", TableClass::Admin),
            // exact-only: no propagation
            ("sys_dictionary", TableClass::Admin),
            ("sys_db_object", TableClass::Admin),
            ("var_dictionary", TableClass::Unknown),
            // static execute applies before the known-table check
            ("sys_script_include", TableClass::ExecuteBearing),
            ("ecc_queue", TableClass::ExecuteBearing),
            // non-privilege sys_user_* tables are not admin by prefix
            ("sys_user_preference", TableClass::Unknown),
            ("incident", TableClass::Ordinary),
        ];
        for &(table, class) in rows {
            assert_eq!(s.classify(table), class, "{table}");
        }
        assert_eq!(
            s.classify_write("sys_security_acl", Action::Update),
            (Action::AdminWrite, TableClass::Admin)
        );
        // A failed load keeps the static baselines.
        s.complete = false;
        assert_eq!(s.classify("oauth_credential"), TableClass::Admin);
        assert_eq!(s.classify("sys_trigger"), TableClass::Unknown);
        assert_eq!(s.classify("sysauto_script"), TableClass::ExecuteBearing);
        assert_eq!(s.classify("incident"), TableClass::Unknown);
    }

    #[test]
    fn written_script_values_escalate_to_execute() {
        for v in [
            "javascript:new MyQual().get()",
            "  JavaScript:x",
            "<j><g:evaluate>x</g:evaluate></j>",
            "a ${x} b",
        ] {
            assert!(is_script_value(v), "{v}");
        }
        for v in ["java", "active=true", "javascript", "$ {x}"] {
            assert!(!is_script_value(v), "{v}");
        }
        let s = snapshot();
        let mut set = OperationSet(vec![
            Operation::new(Subject::Tool("T"), Action::Update)
                .on_table("incident", &s)
                .written_script_fields(vec!["reference_qual".into()]),
            Operation::new(Subject::Tool("T"), Action::Create)
                .on_table("sys_user_has_role", &s)
                .written_script_fields(vec!["x".into()]),
            Operation::new(Subject::Tool("T"), Action::Read)
                .on_table("incident", &s)
                .written_script_fields(vec!["x".into()]),
            Operation::new(Subject::Tool("T"), Action::Create).on_table("incident", &s),
        ]);
        set.escalate_written_scripts();
        let actions: Vec<_> = set.0.iter().map(|o| o.action).collect();
        assert_eq!(
            actions,
            [
                Action::Execute,
                Action::AdminWrite,
                Action::Read,
                Action::Create
            ]
        );
        assert!(set.0[0].query_script_operands.is_empty());
    }
}
