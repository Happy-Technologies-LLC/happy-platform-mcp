//! D-05 tool classification (revision 2) as data the dispatcher loads.
//!
//! The JSON is a byte copy of `/tmp/hpm-specs/p0/tool-classification.json` rev 2,
//! committed under `data/` and compiled in with `include_str!`. A committed copy is
//! reviewable in diffs, needs no build script, keeps the crate self-contained for
//! `cargo publish`, and cannot drift at build time. Changing it is a reviewed edit to
//! this crate (contract change control).
//!
//! The per-instance table rule (RFC §3.1) is applied by normalization, not here: a
//! row only states the tool's declared class and whether the table rule escalates it.

use std::collections::HashSet;

use serde::Deserialize;

use crate::tool::{AccessClass, OutputClass, RetryEligibility, ToolAnnotations};

/// Bundled classification JSON (rev 2).
pub const TOOL_CLASSIFICATION_JSON: &str = include_str!("../data/tool-classification.json");
/// Revision this crate is built against. Loading any other revision fails.
pub const TOOL_CLASSIFICATION_REVISION: u32 = 2;

/// Declared class of a tool in D-05. Classes are not ordered here (RFC §1 rule 7);
/// [`ToolClassification::descriptor_floor`] maps them explicitly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ClassifiedAccess {
    /// No state change on the instance or the server.
    Read,
    Write,
    /// Can run code on the instance.
    Execute,
    /// Mutates MCP-server configuration or process-wide shared state.
    Admin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[non_exhaustive]
pub enum SideEffect {
    None,
    LocalDisk,
    InstanceConfig,
    InstanceData,
    InstanceCode,
    SessionState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum ClassifiedRetry {
    Safe,
    /// Retry only under `retry_condition` (row text).
    Conditional,
    Never,
}

impl ClassifiedRetry {
    /// Conditional retries are only safe before the request is sent.
    pub fn eligibility(self) -> RetryEligibility {
        match self {
            ClassifiedRetry::Safe => RetryEligibility::Idempotent,
            ClassifiedRetry::Conditional => RetryEligibility::BeforeSendOnly,
            ClassifiedRetry::Never => RetryEligibility::Never,
        }
    }
}

/// One D-05 row, reduced to the fields the dispatcher uses.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ToolClassification {
    pub name: String,
    pub access_class: ClassifiedAccess,
    /// Free-text escalation note; `TABLE_RULE…` and `arg-dependent…` prefixes are
    /// meaningful (see [`Self::uses_table_rule`], [`Self::is_arg_dependent`]).
    pub table_escalation: Option<String>,
    pub side_effect: SideEffect,
    #[serde(rename = "dp_output_class")]
    pub output: OutputClass,
    pub retry: ClassifiedRetry,
    /// MCP tool hints (D4), advisory only: never read by policy or protection.
    #[serde(default)]
    pub annotation_hints: Option<ToolAnnotations>,
}

impl ToolClassification {
    /// The effective class depends on the target table (RFC §3.1).
    pub fn uses_table_rule(&self) -> bool {
        self.escalation_starts_with("TABLE_RULE")
    }

    /// The declared class applies only for some argument shapes; others are reads
    /// (e.g. `SN-Set-Instance` without `instance_name` lists instances).
    pub fn is_arg_dependent(&self) -> bool {
        self.escalation_starts_with("arg-dependent")
    }

    /// Minimum the normalized operation set must satisfy (`AccessClass::floor_satisfied`).
    /// Arg-dependent tools can legitimately produce only reads, so their floor is Read;
    /// the declared class still bounds what policy grants.
    pub fn descriptor_floor(&self) -> AccessClass {
        if self.is_arg_dependent() {
            return AccessClass::Read;
        }
        match self.access_class {
            ClassifiedAccess::Read => AccessClass::Read,
            ClassifiedAccess::Write => AccessClass::Write,
            ClassifiedAccess::Execute => AccessClass::Execute,
            ClassifiedAccess::Admin => AccessClass::Config,
        }
    }

    fn escalation_starts_with(&self, prefix: &str) -> bool {
        self.table_escalation
            .as_deref()
            .is_some_and(|t| t.starts_with(prefix))
    }
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ClassificationLoadError {
    #[error("tool classification does not parse")]
    Parse(#[from] serde_json::Error),
    #[error("tool classification revision {found}, expected {expected}")]
    Revision { found: u32, expected: u32 },
    #[error("tool classification lists {0} twice")]
    DuplicateTool(String),
}

/// All rows of one classification revision.
#[derive(Clone, Debug)]
pub struct ToolClassificationSet {
    revision: u32,
    tools: Vec<ToolClassification>,
}

#[derive(Deserialize)]
struct Document {
    meta: Meta,
    rows: Vec<ToolClassification>,
}

#[derive(Deserialize)]
struct Meta {
    revision: u32,
}

impl ToolClassificationSet {
    /// Parses the bundled revision.
    pub fn bundled() -> Result<Self, ClassificationLoadError> {
        Self::parse(TOOL_CLASSIFICATION_JSON)
    }

    pub fn parse(json: &str) -> Result<Self, ClassificationLoadError> {
        let doc: Document = serde_json::from_str(json)?;
        if doc.meta.revision != TOOL_CLASSIFICATION_REVISION {
            return Err(ClassificationLoadError::Revision {
                found: doc.meta.revision,
                expected: TOOL_CLASSIFICATION_REVISION,
            });
        }
        let mut seen = HashSet::new();
        for row in &doc.rows {
            if !seen.insert(row.name.as_str()) {
                return Err(ClassificationLoadError::DuplicateTool(row.name.clone()));
            }
        }
        Ok(Self {
            revision: doc.meta.revision,
            tools: doc.rows,
        })
    }

    pub fn revision(&self) -> u32 {
        self.revision
    }

    pub fn get(&self, name: &str) -> Option<&ToolClassification> {
        self.tools.iter().find(|t| t.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &ToolClassification> {
        self.tools.iter()
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 6.2.0 public tool surface. Name coverage only; golden comparisons go
    /// through `cargo xtask` (D-07 §2.4).
    const TOOLS_LIST: &str = include_str!("../../../fixtures/tools-list-6.2.0.json");

    #[test]
    fn bundled_classification_covers_every_6_2_0_tool_exactly() {
        let set = ToolClassificationSet::bundled().expect("bundled classification loads");
        assert_eq!(set.revision(), 2);
        let list: serde_json::Value = serde_json::from_str(TOOLS_LIST).expect("tools list parses");
        let public: HashSet<&str> = list["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|t| t["name"].as_str().expect("tool name"))
            .collect();
        let classified: HashSet<&str> = set.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(public.len(), 60);
        assert_eq!(set.len(), 60);
        let missing: Vec<_> = public.difference(&classified).collect();
        let extra: Vec<_> = classified.difference(&public).collect();
        assert!(
            missing.is_empty() && extra.is_empty(),
            "missing {missing:?}, extra {extra:?}"
        );
    }

    #[test]
    fn floors_follow_declared_class_except_arg_dependent_tools() {
        let set = ToolClassificationSet::bundled().expect("bundled classification loads");
        let floor = |n: &str| set.get(n).expect(n).descriptor_floor();
        assert_eq!(floor("SN-Query-Table"), AccessClass::Read);
        assert_eq!(floor("SN-Create-Record"), AccessClass::Write);
        assert_eq!(floor("SN-Execute-Background-Script"), AccessClass::Execute);
        assert_eq!(floor("SN-Register-Instance"), AccessClass::Config);
        // Lists instances without `instance_name`: a read-only call must not trip the floor.
        assert_eq!(floor("SN-Set-Instance"), AccessClass::Read);
        let arg_dependent: Vec<_> = set
            .iter()
            .filter(|t| t.is_arg_dependent())
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(arg_dependent, ["SN-Set-Instance"]);
        let table_rule: Vec<_> = set
            .iter()
            .filter(|t| t.uses_table_rule())
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(
            table_rule,
            [
                "SN-Batch-Create",
                "SN-Batch-Update",
                "SN-Create-Record",
                "SN-Move-Records-To-Update-Set",
                "SN-Update-Record"
            ]
        );
    }

    #[test]
    fn rejects_other_revisions_and_duplicates() {
        let wrong = r#"{"meta":{"revision":1},"rows":[]}"#;
        assert!(matches!(
            ToolClassificationSet::parse(wrong),
            Err(ClassificationLoadError::Revision { found: 1, .. })
        ));
        let row = r#"{"name":"A","access_class":"read","table_escalation":null,
            "side_effect":"none","dp_output_class":"none","retry":"safe"}"#;
        let dup = format!(r#"{{"meta":{{"revision":2}},"rows":[{row},{row}]}}"#);
        assert!(matches!(
            ToolClassificationSet::parse(&dup),
            Err(ClassificationLoadError::DuplicateTool(n)) if n == "A"
        ));
    }
}
