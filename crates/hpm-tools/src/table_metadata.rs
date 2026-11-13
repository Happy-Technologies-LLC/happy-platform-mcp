//! Shared table metadata (W3 baseline G8): 6.2.0
//! `src/config/comprehensive-table-definitions.json`, byte copy committed under
//! `data/` and compiled in. Read by `SN-List-Available-Tables`, the schema tools
//! (lane A) and the `servicenow://tables/all` resource (lane E).

use std::sync::LazyLock;

use serde_json::{Map, Value};

/// Bundled JSON, byte-equal to 6.2.0.
pub const TABLE_DEFINITIONS_JSON: &str =
    include_str!("../data/comprehensive-table-definitions.json");

static TABLES: LazyLock<Map<String, Value>> = LazyLock::new(|| {
    let full: Map<String, Value> = serde_json::from_str(TABLE_DEFINITIONS_JSON).unwrap_or_default();
    // 6.2.0 loader: drop `_`-prefixed keys and entries without a truthy `table`.
    full.into_iter()
        .filter(|(key, value)| !key.starts_with('_') && value.get("table").is_some_and(truthy))
        .collect()
});

fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

/// 6.2.0 `tableMetadata`: table name → definition, in file order.
pub fn tables() -> &'static Map<String, Value> {
    &TABLES
}

/// One table's definition.
pub fn table(name: &str) -> Option<&'static Value> {
    TABLES.get(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_loads_like_6_2_0() {
        let full: Map<String, Value> =
            serde_json::from_str(TABLE_DEFINITIONS_JSON).expect("bundled JSON parses");
        assert!(full.contains_key("_metadata"));
        assert!(!tables().contains_key("_metadata"));
        assert!(!tables().is_empty());
        assert!(table("incident").is_some());
        assert!(tables().keys().all(|k| !k.starts_with('_')));
    }
}
