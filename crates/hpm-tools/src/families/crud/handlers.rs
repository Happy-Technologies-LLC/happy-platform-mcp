//! 6.2.0 handler bodies (`mcp-server-consolidated.js` handlers 1630-1794,
//! 1976-2052, 2520-2592; `servicenow-client.js` `batchCreate`, `batchUpdate`,
//! `discoverTableSchema`, `explainField`). Network access goes through [`Api`],
//! implemented by the pinned client, so every step of a multi-request flow
//! runs on the call's instance generation.

use std::collections::HashMap;
use std::time::Instant;

use hpm_core::client::{ClientError, PinnedClient, RecordParams, RecordQuery};
use hpm_types::BoxFut;
use hpm_types::tool::{MutationOutcome, OpCompletion, ToolError};
use serde_json::{Map, Value, json};

use super::json::{arg_or, length_text, or, pretty, prop, set, text, truthy};
use super::normalize::{batch_ref_name, identifier_error};
use crate::families::common::failed_outcome;
use crate::logic::bulk_limits::{
    resolve_batch_placeholders, validate_batch_create_operations, validate_batch_updates,
};
use crate::logic::js;
use crate::logic::nl::{parse_natural_language_value, supported_patterns_json};
use crate::table_metadata;

/// Table API access (the pinned client in production, a fake in tests).
pub trait Api: Send + Sync {
    fn get_records<'a>(
        &'a self,
        table: &'a str,
        query: &'a RecordQuery,
    ) -> BoxFut<'a, Result<Value, ClientError>>;
    fn get_record<'a>(
        &'a self,
        table: &'a str,
        sys_id: &'a str,
        params: &'a RecordParams,
    ) -> BoxFut<'a, Result<Value, ClientError>>;
    fn create_record<'a>(
        &'a self,
        table: &'a str,
        data: &'a Value,
    ) -> BoxFut<'a, Result<Value, ClientError>>;
    fn update_record<'a>(
        &'a self,
        table: &'a str,
        sys_id: &'a str,
        data: &'a Value,
    ) -> BoxFut<'a, Result<Value, ClientError>>;
}

impl Api for PinnedClient {
    fn get_records<'a>(
        &'a self,
        table: &'a str,
        query: &'a RecordQuery,
    ) -> BoxFut<'a, Result<Value, ClientError>> {
        Box::pin(PinnedClient::get_records(self, table, query))
    }
    fn get_record<'a>(
        &'a self,
        table: &'a str,
        sys_id: &'a str,
        params: &'a RecordParams,
    ) -> BoxFut<'a, Result<Value, ClientError>> {
        Box::pin(PinnedClient::get_record(self, table, sys_id, params))
    }
    fn create_record<'a>(
        &'a self,
        table: &'a str,
        data: &'a Value,
    ) -> BoxFut<'a, Result<Value, ClientError>> {
        Box::pin(PinnedClient::create_record(self, table, data))
    }
    fn update_record<'a>(
        &'a self,
        table: &'a str,
        sys_id: &'a str,
        data: &'a Value,
    ) -> BoxFut<'a, Result<Value, ClientError>> {
        Box::pin(PinnedClient::update_record(self, table, sys_id, data))
    }
}

/// What a handler produced, before it becomes a `HandlerCompletion`.
#[derive(Debug)]
pub struct Done {
    pub outcome: MutationOutcome,
    pub ops: Vec<OpCompletion>,
    pub result: Result<Reply, ToolError>,
}

/// Successful 6.2.0 text; `explicit_false`: 6.2.0 sent `"isError": false`.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    pub explicit_false: bool,
}

impl Done {
    fn read(result: Result<String, ToolError>) -> Self {
        Self {
            outcome: MutationOutcome::NotAMutation,
            ops: Vec::new(),
            result: result.map(|text| Reply {
                text,
                explicit_false: false,
            }),
        }
    }
}

/// Progress for one call (6.2.0 `notifyProgress` + the per-request counter in
/// `mcp-server-consolidated.js`: a missing `current` is last + 1, and progress
/// never goes backwards).
pub struct Progress<'a> {
    emit: &'a (dyn Fn(f64, Option<f64>, String) -> BoxFut<'a, ()> + Send + Sync),
    last: f64,
}

impl<'a> Progress<'a> {
    pub fn new(
        emit: &'a (dyn Fn(f64, Option<f64>, String) -> BoxFut<'a, ()> + Send + Sync),
    ) -> Self {
        Self { emit, last: 0.0 }
    }

    #[allow(clippy::cast_precision_loss)]
    async fn notify(&mut self, message: String, current: Option<usize>, total: Option<usize>) {
        let mut progress = current.map_or(self.last + 1.0, |c| c as f64);
        if progress <= self.last {
            progress = self.last + 1.0;
        }
        self.last = progress;
        (self.emit)(progress, total.map(|t| t as f64), message).await;
    }
}

/// 6.2.0 thrown-error text of a client failure (`error.message`).
fn message(e: &ClientError) -> String {
    e.message.clone()
}

/// Overall outcome of per-op facts.
fn aggregate(ops: &[OpCompletion]) -> MutationOutcome {
    let any = |o: MutationOutcome| ops.iter().any(|f| f.outcome == o);
    if ops.is_empty() || ops.iter().all(|f| f.outcome == MutationOutcome::NotSent) {
        MutationOutcome::NotSent
    } else if any(MutationOutcome::Committed)
        && ops.iter().all(|f| f.outcome == MutationOutcome::Committed)
    {
        MutationOutcome::Committed
    } else if any(MutationOutcome::Committed) {
        MutationOutcome::PartiallyCommitted
    } else if any(MutationOutcome::Unknown) {
        MutationOutcome::Unknown
    } else {
        MutationOutcome::Failed
    }
}

/// `String(x)` for a Table API query parameter, sent only when truthy
/// (`if (query.sysparm_x) params.append(...)`).
fn param(value: Option<&Value>) -> Option<String> {
    truthy(value).then(|| text(value))
}

fn str_arg<'a>(args: &'a Value, key: &str) -> &'a str {
    args.get(key).and_then(Value::as_str).unwrap_or_default()
}

// ---------------------------------------------------------------- reads

pub async fn query_table(api: &dyn Api, args: &Value) -> Done {
    Done::read(query_table_text(api, args).await)
}

async fn query_table_text(api: &dyn Api, args: &Value) -> Result<String, ToolError> {
    let table = str_arg(args, "table_name");
    let limit = arg_or(args, "limit", json!(25));
    let query = RecordQuery {
        query: param(args.get("query")),
        limit: param(Some(&limit)),
        fields: param(args.get("fields")),
        offset: param(args.get("offset")),
        order_by: param(args.get("order_by")),
    };
    let results = api.get_records(table, &query).await?;
    let count = length_text(&results).map_err(ToolError::Message)?;
    Ok(format!(
        "Found {count} records in {}:\n{}",
        text(args.get("table_name")),
        pretty(&results)
    ))
}

pub async fn get_record(api: &dyn Api, args: &Value) -> Done {
    Done::read(
        async {
            let params = RecordParams {
                fields: param(args.get("fields")),
                ..RecordParams::default()
            };
            let result = api
                .get_record(
                    str_arg(args, "table_name"),
                    str_arg(args, "sys_id"),
                    &params,
                )
                .await?;
            Ok(pretty(&result))
        }
        .await,
    )
}

const CATEGORIES: [(&str, &[&str]); 4] = [
    (
        "core_itsm",
        &[
            "incident",
            "change_request",
            "problem",
            "change_task",
            "problem_task",
        ],
    ),
    (
        "platform",
        &[
            "sys_user",
            "sys_user_group",
            "sys_db_object",
            "sys_dictionary",
            "sys_properties",
        ],
    ),
    (
        "service_catalog",
        &["sc_request", "sc_req_item", "sc_cat_item", "sc_category"],
    ),
    (
        "cmdb",
        &[
            "cmdb_ci",
            "cmdb_ci_computer",
            "cmdb_ci_server",
            "cmdb_rel_ci",
        ],
    ),
];

pub fn list_available_tables(args: &Value) -> Done {
    let category = args.get("category");
    let filter = category
        .filter(|c| truthy(Some(c)))
        .and_then(Value::as_str)
        .filter(|c| *c != "all")
        .and_then(|c| CATEGORIES.iter().find(|(k, _)| *k == c))
        .map(|(_, tables)| *tables);
    let list: Vec<Value> = table_metadata::tables()
        .iter()
        .filter(|(name, _)| filter.is_none_or(|f| f.contains(&name.as_str())))
        .map(|(name, meta)| {
            let mut m = Map::new();
            m.insert("table_name".into(), Value::String(name.clone()));
            for key in ["label", "description", "key_field", "priority", "package"] {
                set(&mut m, key, meta.get(key));
            }
            Value::Object(m)
        })
        .collect();
    Done::read(Ok(format!(
        "Available ServiceNow tables ({} total):\n{}",
        list.len(),
        pretty(&Value::Array(list))
    )))
}

pub async fn get_table_schema(api: &dyn Api, args: &Value) -> Done {
    if let Some(e) = identifier_error(args, "table_name") {
        return Done::read(Err(ToolError::Message(e)));
    }
    let table_name = str_arg(args, "table_name");
    let name_text = text(args.get("table_name"));
    let Some(schema) = table_metadata::table(table_name) else {
        let reply = match discover(api, table_name, &DiscoverOptions::default()).await {
            Ok(api_schema) => {
                let mut m = Map::new();
                set(&mut m, "table_name", args.get("table_name"));
                set(&mut m, "label", api_schema.get("label"));
                set(&mut m, "fields", api_schema.get("fields"));
                m.insert("source".into(), json!("live_api"));
                m.insert("note".into(), json!("This table is not in local metadata. Consider adding it to comprehensive-table-definitions.json for faster lookups."));
                Reply {
                    text: format!(
                        "Schema for {name_text} (fetched from ServiceNow API):\n{}",
                        pretty(&Value::Object(m))
                    ),
                    explicit_false: false,
                }
            }
            Err(e) => Reply {
                text: format!(
                    "No schema metadata found for table \"{name_text}\" in local cache, and API lookup failed: {e}. The table may not exist or you may not have permissions. Use SN-Query-Table to attempt to query it."
                ),
                explicit_false: true,
            },
        };
        return Done {
            outcome: MutationOutcome::NotAMutation,
            ops: Vec::new(),
            result: Ok(reply),
        };
    };
    let mut m = Map::new();
    set(&mut m, "table_name", args.get("table_name"));
    for key in ["label", "key_field", "display_field"] {
        set(&mut m, key, schema.get(key));
    }
    let defaulted = |key: &str, default: Value| -> Value {
        or(schema.get(key), None).cloned().unwrap_or(default)
    };
    m.insert(
        "required_fields".into(),
        defaulted("required_fields", json!([])),
    );
    m.insert(
        "common_fields".into(),
        defaulted("common_fields", json!([])),
    );
    m.insert(
        "operations".into(),
        defaulted("operations", json!(["create", "read", "update", "list"])),
    );
    set(&mut m, "description", schema.get("description"));
    set(&mut m, "package", schema.get("package"));
    m.insert("source".into(), json!("local_cache"));
    Done::read(Ok(pretty(&Value::Object(m))))
}

#[derive(Default)]
pub struct DiscoverOptions {
    pub type_codes: bool,
    pub choice_tables: bool,
    pub relationships: bool,
    pub ui_policies: bool,
    pub business_rules: bool,
    pub field_constraints: bool,
}

fn rows(v: &Value) -> &[Value] {
    v.as_array().map_or(&[], Vec::as_slice)
}

fn bool_flag(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Bool(true))) || v.and_then(Value::as_str) == Some("true")
}

fn records_query(query: String, limit: &str, fields: Option<&str>) -> RecordQuery {
    RecordQuery {
        query: Some(query),
        limit: Some(limit.to_owned()),
        fields: fields.map(str::to_owned),
        ..RecordQuery::default()
    }
}

/// `discoverTableSchema`; errors carry the wrapped 6.2.0 text.
pub async fn discover(api: &dyn Api, table: &str, o: &DiscoverOptions) -> Result<Value, String> {
    discover_inner(api, table, o)
        .await
        .map_err(|e| format!("Failed to discover schema for {table}: {e}"))
}

async fn discover_inner(api: &dyn Api, table: &str, o: &DiscoverOptions) -> Result<Value, String> {
    let get = |t: &'static str, q: RecordQuery| async move {
        api.get_records(t, &q).await.map_err(|e| message(&e))
    };
    let mut schema = Map::new();
    schema.insert("table".into(), json!(table));
    schema.insert("label".into(), Value::Null);
    schema.insert("fields".into(), json!([]));

    let tables = get(
        "sys_db_object",
        records_query(format!("name={table}"), "1", None),
    )
    .await?;
    if let Some(first) = rows(&tables).first() {
        set(&mut schema, "label", first.get("label"));
        set(
            &mut schema,
            "super_class",
            prop(first.get("super_class"), "value"),
        );
    }

    let fields = get(
        "sys_dictionary",
        records_query(format!("name={table}"), "1000", None),
    )
    .await?;
    let mut infos: Vec<Map<String, Value>> = Vec::new();
    for field in rows(&fields) {
        let internal = or(
            prop(Some(field), "internal_type").and_then(|t| prop(Some(t), "value")),
            field.get("internal_type"),
        );
        let mut info = Map::new();
        set(&mut info, "name", field.get("element"));
        set(&mut info, "label", field.get("column_label"));
        set(&mut info, "internal_type", internal);
        set(&mut info, "max_length", field.get("max_length"));
        info.insert("mandatory".into(), json!(bool_flag(field.get("mandatory"))));
        info.insert("read_only".into(), json!(bool_flag(field.get("read_only"))));
        let internal_is = |s: &str| internal.and_then(Value::as_str) == Some(s);

        if o.type_codes
            && internal_is("integer")
            && field.get("element").and_then(Value::as_str) == Some("type")
        {
            let choices = get(
                "sys_choice",
                records_query(format!("name={table}^element=type"), "100", None),
            )
            .await?;
            if !rows(&choices).is_empty() {
                let mut codes = Map::new();
                for choice in rows(&choices) {
                    set(&mut codes, &text(choice.get("value")), choice.get("label"));
                }
                info.insert("type_codes".into(), Value::Object(codes));
            }
        }
        if internal_is("reference") && truthy(field.get("reference")) {
            let reference = field.get("reference");
            set(
                &mut info,
                "reference_table",
                or(prop(reference, "value"), reference),
            );
        }
        if o.field_constraints && truthy(field.get("default_value")) {
            set(&mut info, "default_value", field.get("default_value"));
        }
        infos.push(info);
    }

    if o.relationships {
        let mut rel = Map::new();
        for f in &infos {
            if f.get("internal_type").and_then(Value::as_str) == Some("reference")
                && truthy(f.get("reference_table"))
            {
                let mut r = Map::new();
                r.insert("type".into(), json!("reference"));
                set(&mut r, "table", f.get("reference_table"));
                set(&mut r, "description", f.get("label"));
                rel.insert(text(f.get("name")), Value::Object(r));
            }
        }
        // Inserted after `fields` below, in 6.2.0 order.
        schema.insert(
            "fields".into(),
            Value::Array(infos.into_iter().map(Value::Object).collect()),
        );
        if o.choice_tables {
            schema.insert("choice_tables".into(), choice_tables());
        }
        schema.insert("relationships".into(), Value::Object(rel));
    } else {
        schema.insert(
            "fields".into(),
            Value::Array(infos.into_iter().map(Value::Object).collect()),
        );
        if o.choice_tables {
            schema.insert("choice_tables".into(), choice_tables());
        }
    }

    if o.ui_policies {
        let policies = get(
            "sys_ui_policy",
            records_query(
                format!("table={table}^active=true"),
                "100",
                Some("sys_id,short_description"),
            ),
        )
        .await?;
        let list = rows(&policies)
            .iter()
            .map(|p| {
                let mut m = Map::new();
                set(&mut m, "sys_id", p.get("sys_id"));
                set(&mut m, "description", p.get("short_description"));
                Value::Object(m)
            })
            .collect();
        schema.insert("ui_policies".into(), Value::Array(list));
    }
    if o.business_rules {
        let rules = get(
            "sys_script",
            records_query(
                format!("collection={table}^active=true"),
                "100",
                Some("sys_id,name,when"),
            ),
        )
        .await?;
        let list = rows(&rules)
            .iter()
            .map(|r| {
                let mut m = Map::new();
                for k in ["sys_id", "name", "when"] {
                    set(&mut m, k, r.get(k));
                }
                Value::Object(m)
            })
            .collect();
        schema.insert("business_rules".into(), Value::Array(list));
    }
    Ok(Value::Object(schema))
}

fn choice_tables() -> Value {
    json!({
        "sys_choice": "For table field choices",
        "question_choice": "For catalog variable choices"
    })
}

pub async fn discover_table_schema(api: &dyn Api, args: &Value) -> Done {
    if let Some(e) = identifier_error(args, "table_name") {
        return Done::read(Err(ToolError::Message(e)));
    }
    let flag = |k: &str| truthy(args.get(k));
    let o = DiscoverOptions {
        type_codes: flag("include_type_codes"),
        choice_tables: flag("include_choice_tables"),
        relationships: flag("include_relationships"),
        ui_policies: flag("include_ui_policies"),
        business_rules: flag("include_business_rules"),
        field_constraints: flag("include_field_constraints"),
    };
    let result = discover(api, str_arg(args, "table_name"), &o)
        .await
        .map(|schema| {
            format!(
                "Enhanced schema for {}:\n{}",
                text(args.get("table_name")),
                pretty(&schema)
            )
        })
        .map_err(ToolError::Message);
    Done::read(result)
}

/// `explainField`; errors carry the wrapped 6.2.0 text.
pub async fn explain(
    api: &dyn Api,
    table: &str,
    field_name: &str,
    include_examples: bool,
) -> Result<Value, String> {
    explain_inner(api, table, field_name, include_examples)
        .await
        .map_err(|e| format!("Failed to explain field: {e}"))
}

async fn explain_inner(
    api: &dyn Api,
    table: &str,
    field_name: &str,
    include_examples: bool,
) -> Result<Value, String> {
    let fields = api
        .get_records(
            "sys_dictionary",
            &records_query(format!("name={table}^element={field_name}"), "1", None),
        )
        .await
        .map_err(|e| message(&e))?;
    let Some(field) = rows(&fields).first() else {
        return Err(format!("Field {field_name} not found in table {table}"));
    };
    let internal = field.get("internal_type");
    let mut m = Map::new();
    m.insert("field".into(), json!(field_name));
    m.insert("table".into(), json!(table));
    set(&mut m, "label", field.get("column_label"));
    set(&mut m, "type", or(prop(internal, "value"), internal));
    set(&mut m, "max_length", field.get("max_length"));
    m.insert("mandatory".into(), json!(bool_flag(field.get("mandatory"))));
    m.insert("read_only".into(), json!(bool_flag(field.get("read_only"))));
    set(&mut m, "comments", field.get("comments"));
    set(&mut m, "help", field.get("help"));
    if truthy(field.get("reference")) {
        let reference = field.get("reference");
        set(
            &mut m,
            "reference_table",
            or(prop(reference, "value"), reference),
        );
    }
    if matches!(internal.and_then(Value::as_str), Some("choice" | "integer")) {
        let choices = api
            .get_records(
                "sys_choice",
                &records_query(format!("name={table}^element={field_name}"), "100", None),
            )
            .await
            .map_err(|e| message(&e))?;
        if !rows(&choices).is_empty() {
            let list = rows(&choices)
                .iter()
                .map(|c| {
                    let mut x = Map::new();
                    set(&mut x, "value", c.get("value"));
                    set(&mut x, "label", c.get("label"));
                    Value::Object(x)
                })
                .collect();
            m.insert("choices".into(), Value::Array(list));
        }
    }
    if table == "catalog_ui_policy_action"
        && (field_name == "ui_policy" || field_name == "catalog_variable")
    {
        let mut issues = vec![json!(
            "Cannot be set via REST API - use background script with setValue()"
        )];
        if field_name == "catalog_variable" {
            issues.push(json!("Must include IO: prefix or linkage will fail"));
        }
        m.insert("known_issues".into(), Value::Array(issues));
        if field_name == "catalog_variable" {
            m.insert("special_format".into(), json!("IO:<variable_sys_id>"));
            if include_examples {
                m.insert(
                    "example".into(),
                    json!("IO:94ababd1c35432101fcbbd43e40131bf"),
                );
            }
        }
    }
    Ok(Value::Object(m))
}

pub async fn explain_field(api: &dyn Api, args: &Value) -> Done {
    if let Some(e) = ["table", "field"]
        .iter()
        .find_map(|k| identifier_error(args, k))
    {
        return Done::read(Err(ToolError::Message(e)));
    }
    let include_examples = truthy(Some(&arg_or(args, "include_examples", json!(true))));
    let result = explain(
        api,
        str_arg(args, "table"),
        str_arg(args, "field"),
        include_examples,
    )
    .await
    .map(|e| {
        format!(
            "Field explanation for {}.{}:\n{}",
            text(args.get("table")),
            text(args.get("field")),
            pretty(&e)
        )
    })
    .map_err(ToolError::Message);
    Done::read(result)
}

pub async fn natural_language_search(api: &dyn Api, args: &Value) -> Done {
    Done::read(nl_text(api, args).await)
}

async fn nl_text(api: &dyn Api, args: &Value) -> Result<String, ToolError> {
    let query = args.get("query");
    let table_v = arg_or(args, "table", json!("incident"));
    let table = table_v.as_str().unwrap_or("incident");
    let limit = arg_or(args, "limit", json!(25));
    let show_patterns = truthy(Some(&arg_or(args, "show_patterns", json!(true))));
    let parsed = parse_natural_language_value(query, Some(table))
        .map_err(|e| ToolError::Message(e.to_string()))?;
    let query_text = text(query);

    if parsed.encoded_query.is_empty() {
        let patterns = if show_patterns {
            format!(
                "\n## Supported Patterns:\n{}",
                pretty(&supported_patterns_json())
            )
        } else {
            String::new()
        };
        return Ok(format!(
            "❌ Unable to parse query: \"{query_text}\"\n\n{}\n\nUnmatched text: \"{}\"\n\n{patterns}",
            parsed.suggestions.join("\n"),
            parsed.unmatched_text
        ));
    }

    let request = RecordQuery {
        query: Some(parsed.encoded_query.clone()),
        limit: param(Some(&limit)),
        fields: param(args.get("fields")),
        offset: None,
        order_by: param(args.get("order_by")),
    };
    let results = api.get_records(table, &request).await?;
    let count = length_text(&results).map_err(ToolError::Message)?;
    let mut out = format!(
        "✅ Natural Language Search Results\n\n**Original Query:** \"{query_text}\"\n**Target Table:** {}\n**Parsed Encoded Query:** `{}`\n**Records Found:** {count}/{}\n\n",
        js::to_js_string(&table_v),
        parsed.encoded_query,
        js::to_js_string(&limit)
    );
    if show_patterns && !parsed.matched_patterns.is_empty() {
        out.push_str("## Matched Patterns:\n");
        for (i, p) in parsed.matched_patterns.iter().enumerate() {
            out.push_str(&format!(
                "{}. **\"{}\"** → `{}`\n",
                i + 1,
                p.matched,
                p.condition
            ));
        }
        out.push('\n');
    }
    if js::utf16_len(&parsed.unmatched_text) > 3 {
        out.push_str(&format!(
            "⚠️ **Unrecognized:** \"{}\"\n\n",
            parsed.unmatched_text
        ));
    }
    let non_empty = results.as_array().is_some_and(|a| !a.is_empty())
        || results.as_str().is_some_and(|s| !s.is_empty());
    if non_empty {
        out.push_str(&format!("## Results:\n```json\n{}\n```", pretty(&results)));
    } else {
        out.push_str("## No records found matching the query.\n\nTry adjusting your search criteria or use SN-Query-Table for more control.");
    }
    Ok(out)
}

// ---------------------------------------------------------------- writes

fn single_write(r: Result<Value, ClientError>, render: impl FnOnce(&Value) -> String) -> Done {
    match r {
        Ok(result) => Done {
            outcome: MutationOutcome::Committed,
            ops: vec![OpCompletion {
                op_index: 0,
                outcome: MutationOutcome::Committed,
            }],
            result: Ok(Reply {
                text: render(&result),
                explicit_false: false,
            }),
        },
        Err(e) => {
            let outcome = failed_outcome(&e);
            Done {
                outcome,
                ops: vec![OpCompletion {
                    op_index: 0,
                    outcome,
                }],
                result: Err(e.into()),
            }
        }
    }
}

fn label_of(table: &str) -> (String, Option<&'static Value>) {
    let meta = table_metadata::table(table);
    let label = or(meta.and_then(|m| m.get("label")), None)
        .map_or_else(|| table.to_owned(), js::to_js_string);
    (label, meta)
}

fn key_field(meta: Option<&Value>) -> String {
    or(meta.and_then(|m| m.get("key_field")), None)
        .map_or_else(|| "sys_id".to_owned(), js::to_js_string)
}

pub async fn create_record(api: &dyn Api, args: &Value) -> Done {
    let table = str_arg(args, "table_name");
    let data = args.get("data").cloned().unwrap_or(Value::Null);
    let r = api.create_record(table, &data).await;
    single_write(r, |result| {
        let (label, meta) = label_of(table);
        let id = or(
            prop(Some(result), &key_field(meta)),
            prop(Some(result), "sys_id"),
        );
        format!(
            "Created {label} successfully: {}\n{}",
            text(id),
            pretty(result)
        )
    })
}

pub async fn update_record(api: &dyn Api, args: &Value) -> Done {
    let table = str_arg(args, "table_name");
    let data = args.get("data").cloned().unwrap_or(Value::Null);
    let r = api
        .update_record(table, str_arg(args, "sys_id"), &data)
        .await;
    single_write(r, |result| {
        let (label, meta) = label_of(table);
        let id = or(prop(Some(result), &key_field(meta)), args.get("sys_id"));
        format!(
            "Updated {label} {} successfully\n{}",
            text(id),
            pretty(result)
        )
    })
}

/// `shouldReport` of `batchCreate` / `batchUpdate`.
fn should_report(report: bool, index: usize, total: usize) -> bool {
    if !report {
        return false;
    }
    if total <= 10 {
        return true;
    }
    if total <= 50 {
        return (index + 1).is_multiple_of(5) || index == total - 1;
    }
    (index + 1).is_multiple_of(total.div_ceil(10)) || index == total - 1
}

/// `Math.round(((i + 1) / total) * 100)`.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn percentage(done: usize, total: usize) -> u64 {
    ((done as f64 / total as f64) * 100.0 + 0.5).floor() as u64
}

fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Facts for a batch whose element `i` ran with `outcome` and later ones never ran.
fn facts(done: &[MutationOutcome], total: usize) -> Vec<OpCompletion> {
    (0..total)
        .map(|op_index| OpCompletion {
            op_index,
            outcome: done
                .get(op_index)
                .copied()
                .unwrap_or(MutationOutcome::NotSent),
        })
        .collect()
}

fn batch_done(done: &[MutationOutcome], total: usize, result: Result<Reply, ToolError>) -> Done {
    let ops = facts(done, total);
    Done {
        outcome: aggregate(&ops),
        ops,
        result,
    }
}

/// `batchCreate(operations, transaction, progress)` and its tool wrapper. 6.2.0
/// does not roll back: with `transaction` the batch stops at the first failure and
/// earlier records stay created (reported as partially committed).
pub async fn batch_create(
    api: &dyn Api,
    args: &Value,
    progress: &mut Progress<'_>,
    cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> Done {
    let operations = args.get("operations");
    let serialized = match validate_batch_create_operations(operations) {
        Ok(s) => s,
        Err(e) => return batch_done(&[], 1, Err(ToolError::Message(e.message))),
    };
    let ops = operations
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let transaction = truthy(Some(&arg_or(args, "transaction", json!(true))));
    let report = truthy(Some(&arg_or(args, "progress", json!(true))));
    let total = ops.len();
    let start = Instant::now();
    let mut saved: HashMap<String, String> = HashMap::new();
    let mut sys_ids = Map::new();
    let mut errors: Vec<Value> = Vec::new();
    let mut created = 0u64;
    let mut done: Vec<MutationOutcome> = Vec::with_capacity(total);
    // Names of earlier operations a `${name}` ref may point at.
    let mut earlier: Vec<String> = Vec::with_capacity(total);

    for (i, op) in ops.iter().enumerate() {
        if cancelled() {
            return batch_done(&done, total, Err(ToolError::Cancelled));
        }
        let table = op.get("table").and_then(Value::as_str).unwrap_or_default();
        let resolved = resolve_batch_placeholders(&serialized[i], &saved);
        let unresolved = earlier
            .iter()
            .find(|name| resolved.contains(&format!("${{{name}}}")));
        let attempt = if let Some(name) = unresolved {
            Err((
                MutationOutcome::NotSent,
                format!("Reference ${{{name}}} did not resolve to a sys_id; operation not sent"),
            ))
        } else {
            match serde_json::from_str::<Value>(&resolved) {
                Ok(data) => api
                    .create_record(table, &data)
                    .await
                    .map_err(|e| (failed_outcome(&e), message(&e))),
                Err(e) => Err((MutationOutcome::NotSent, e.to_string())),
            }
        };
        let key = batch_ref_name(op, i);
        match attempt {
            Ok(result) => {
                done.push(MutationOutcome::Committed);
                let sys_id = prop(Some(&result), "sys_id");
                set(&mut sys_ids, &key, sys_id);
                if let Some(Value::String(id)) = sys_id {
                    saved.insert(key.clone(), id.clone());
                }
                created += 1;
                if should_report(report, i, total) {
                    progress
                        .notify(
                            format!(
                                "Creating record {}/{total} ({}%): {table}",
                                i + 1,
                                percentage(i + 1, total)
                            ),
                            Some(i + 1),
                            Some(total),
                        )
                        .await;
                }
            }
            Err((outcome, msg)) => {
                done.push(outcome);
                errors.push(json!({"operation_index": i, "table": table, "error": msg}));
                if report {
                    progress
                        .notify(
                            format!("Failed {}/{total}: {table} - {msg}", i + 1),
                            Some(i + 1),
                            Some(total),
                        )
                        .await;
                }
                if transaction {
                    // 6.2.0 sentence, plus (owner decision M3) the records that
                    // stay created: 6.2.0 never rolls back. Verbatim Text so it
                    // wins over the generic partial-commit text.
                    let mut text = format!("Error: Batch create failed at operation {i}: {msg}");
                    if created > 0 {
                        text.push_str(&format!(
                            "\nAlready created and not rolled back: {}. Do not re-run the whole batch.",
                            Value::Object(sys_ids)
                        ));
                    }
                    return batch_done(&done, total, Err(ToolError::Text(text)));
                }
            }
        }
        earlier.push(key);
    }
    if report {
        let summary = if errors.is_empty() {
            format!("Complete: All {total} records created successfully")
        } else {
            format!(
                "Complete: {created}/{total} records created ({} failed)",
                errors.len()
            )
        };
        progress.notify(summary, None, None).await;
    }
    let success = errors.is_empty();
    let body = json!({
        "success": success,
        "created_count": created,
        "sys_ids": sys_ids,
        "errors": errors,
        "execution_time_ms": elapsed_ms(start),
    });
    let text = format!(
        "Batch create {}:\n{}",
        if success { "completed" } else { "failed" },
        pretty(&body)
    );
    let result = if success {
        Ok(Reply {
            text,
            explicit_false: true,
        })
    } else {
        Err(ToolError::Text(text))
    };
    batch_done(&done, total, result)
}

/// `batchUpdate(updates, stop_on_error, progress)` and its tool wrapper.
pub async fn batch_update(
    api: &dyn Api,
    args: &Value,
    progress: &mut Progress<'_>,
    cancelled: &(dyn Fn() -> bool + Send + Sync),
) -> Done {
    let updates = args.get("updates");
    if let Err(e) = validate_batch_updates(updates) {
        return batch_done(&[], 1, Err(ToolError::Message(e.message)));
    }
    let updates = updates
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let stop_on_error = truthy(args.get("stop_on_error"));
    let report = truthy(Some(&arg_or(args, "progress", json!(true))));
    let total = updates.len();
    let start = Instant::now();
    let mut errors: Vec<Value> = Vec::new();
    let mut updated = 0u64;
    let mut done: Vec<MutationOutcome> = Vec::with_capacity(total);

    for (i, update) in updates.iter().enumerate() {
        if cancelled() {
            return batch_done(&done, total, Err(ToolError::Cancelled));
        }
        let table = update
            .get("table")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let sys_id = update
            .get("sys_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let data = update.get("data").cloned().unwrap_or(Value::Null);
        match api.update_record(table, sys_id, &data).await {
            Ok(_) => {
                done.push(MutationOutcome::Committed);
                updated += 1;
                if should_report(report, i, total) {
                    progress
                        .notify(
                            format!(
                                "Updating record {}/{total} ({}%): {table}",
                                i + 1,
                                percentage(i + 1, total)
                            ),
                            Some(i + 1),
                            Some(total),
                        )
                        .await;
                }
            }
            Err(e) => {
                done.push(failed_outcome(&e));
                let msg = message(&e);
                errors.push(json!({
                    "update_index": i,
                    "table": table,
                    "sys_id": sys_id,
                    "error": msg,
                }));
                if report {
                    progress
                        .notify(
                            format!("Failed {}/{total}: {table} - {msg}", i + 1),
                            Some(i + 1),
                            Some(total),
                        )
                        .await;
                }
                if stop_on_error {
                    break;
                }
            }
        }
    }
    if report {
        let summary = if errors.is_empty() {
            format!("Complete: All {total} records updated successfully")
        } else {
            format!(
                "Complete: {updated}/{total} records updated ({} failed)",
                errors.len()
            )
        };
        progress.notify(summary, None, None).await;
    }
    let success = errors.is_empty();
    let body = json!({
        "success": success,
        "updated_count": updated,
        "errors": errors,
        "execution_time_ms": elapsed_ms(start),
    });
    let text = format!(
        "Batch update {}:\n{}",
        if success {
            "completed"
        } else {
            "completed with errors"
        },
        pretty(&body)
    );
    let result = if success {
        Ok(Reply {
            text,
            explicit_false: true,
        })
    } else {
        Err(ToolError::Text(text))
    };
    batch_done(&done, total, result)
}
