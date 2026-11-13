//! 6.2.0 `tools/list` order (C5): source registration order,
//! `fixtures/compat-manifest.json` `tools.items` (`listSource`
//! `src/mcp-server-consolidated.js:205-1537`). The tools-list fixture is
//! name-sorted and is compared by name, never for order.

/// Full server, source order.
pub const TOOLS_LIST_ORDER: [&str; 60] = [
    "SN-Set-Instance",
    "SN-Get-Current-Instance",
    "SN-Query-Table",
    "SN-Create-Record",
    "SN-Get-Record",
    "SN-Update-Record",
    "SN-Get-Table-Schema",
    "SN-List-Available-Tables",
    "SN-List-Incidents",
    "SN-Create-Incident",
    "SN-Get-Incident",
    "SN-List-SysUsers",
    "SN-List-CmdbCis",
    "SN-List-SysUserGroups",
    "SN-List-ChangeRequests",
    "SN-Set-Update-Set",
    "SN-Get-Current-Update-Set",
    "SN-List-Update-Sets",
    "SN-Set-Current-Application",
    "SN-List-Problems",
    "SN-Natural-Language-Search",
    "SN-Execute-Background-Script",
    "SN-Create-Fix-Script",
    "SN-Run-ATF-Test",
    "SN-Run-ATF-Suite",
    "SN-Get-CICD-Progress",
    "SN-Get-ATF-Results",
    "SN-Get-ATF-Test-Logs",
    "SN-Discover-Table-Schema",
    "SN-Batch-Create",
    "SN-Batch-Update",
    "SN-Explain-Field",
    "SN-Validate-Configuration",
    "SN-Inspect-Update-Set",
    "SN-Create-Workflow",
    "SN-Create-Activity",
    "SN-Create-Transition",
    "SN-Publish-Workflow",
    "SN-Move-Records-To-Update-Set",
    "SN-Clone-Update-Set",
    "SN-Add-Comment",
    "SN-Add-Work-Notes",
    "SN-Assign-Incident",
    "SN-Resolve-Incident",
    "SN-Close-Incident",
    "SN-Add-Change-Comment",
    "SN-Assign-Change",
    "SN-Approve-Change",
    "SN-Add-Problem-Comment",
    "SN-Close-Problem",
    "SN-Catalog-Get-Item",
    "SN-Catalog-Search-Items",
    "SN-Catalog-Submit",
    "SN-Catalog-Get-Categories",
    "SN-Register-Instance",
    "SN-Docs-Families",
    "SN-Docs-Status",
    "SN-Docs-Sync",
    "SN-Docs-Search",
    "SN-Docs-Get",
];

/// `--docs-only` server (`docsOnlyList`, `mcp-server-consolidated.js:208-211`).
/// The same relative order as [`TOOLS_LIST_ORDER`].
pub const DOCS_ONLY_ORDER: [&str; 6] = [
    "SN-Register-Instance",
    "SN-Docs-Families",
    "SN-Docs-Status",
    "SN-Docs-Sync",
    "SN-Docs-Search",
    "SN-Docs-Get",
];

/// Position in [`TOOLS_LIST_ORDER`]; `None` for names 6.2.0 does not list.
pub fn rank(name: &str) -> Option<usize> {
    TOOLS_LIST_ORDER.iter().position(|n| *n == name)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn manifest() -> serde_json::Value {
        let path: PathBuf = [
            env!("CARGO_MANIFEST_DIR"),
            "..",
            "..",
            "fixtures",
            "compat-manifest.json",
        ]
        .iter()
        .collect();
        serde_json::from_str(&std::fs::read_to_string(path).expect("manifest")).expect("json")
    }

    #[test]
    fn order_is_the_compat_manifest_source_order() {
        let m = manifest();
        let items = m["tools"]["items"].as_array().expect("items");
        let names: Vec<&str> = items.iter().filter_map(|i| i["name"].as_str()).collect();
        assert_eq!(names, TOOLS_LIST_ORDER);
        let docs: Vec<&str> = items
            .iter()
            .filter(|i| i["docsOnly"] == serde_json::Value::Bool(true))
            .filter_map(|i| i["name"].as_str())
            .collect();
        assert_eq!(docs, DOCS_ONLY_ORDER);
        let listed = m["tools"]["docsOnlyList"].as_str().expect("docsOnlyList");
        assert!(listed.contains(&DOCS_ONLY_ORDER.join(", ")), "{listed}");
    }

    #[test]
    fn docs_only_keeps_full_relative_order() {
        let ranks: Vec<_> = DOCS_ONLY_ORDER.iter().map(|n| rank(n)).collect();
        assert!(ranks.iter().all(Option::is_some));
        assert!(ranks.windows(2).all(|w| w[0] < w[1]));
    }
}
