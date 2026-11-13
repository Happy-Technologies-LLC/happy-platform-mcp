//! Service Catalog tools and `SN-Validate-Configuration` (6.2.0
//! `servicenow-client.js` `getCatalogItemDetail`, `searchCatalogItems`,
//! `submitCatalogForm`, `getCatalogCategories`, `validateCatalogConfiguration`).
//! The 6.2.0 detail fetch runs four requests with `Promise.all`; they start in
//! source order, and run here sequentially in that same order.

use std::collections::HashMap;

use hpm_core::client::{RecordParams, RecordQuery};
use hpm_types::operation::{Action, Operation};
use hpm_types::tool::NormalizeInput;
use serde_json::{Map, Value, json};

use super::js_out::{or, param, pretty, prop, put, tpl, truthy};
use super::{Call, Fail, Kind, Sn, js_length, object, order_now_path, read, write};

const ITEM_FIELDS: &str = "sys_id,name,short_description,description,category,sc_catalogs,active,order,price,recurring_price,delivery_time,sys_class_name";
const VARIABLE_FIELDS: &str = "sys_id,name,question_text,type,mandatory,order,default_value,help_text,reference,reference_qual,cat_item,variable_set";
const CHOICE_FIELDS: &str = "sys_id,question,text,value,order";
const SEARCH_FIELDS: &str = "sys_id,name,short_description,category,active,sys_class_name";
const CATEGORY_FIELDS: &str = "sys_id,title,description,parent,active";

fn variable_type(code: &str) -> Option<&'static str> {
    Some(match code {
        "1" => "single_line_text",
        "2" => "multi_line_text",
        "3" => "multiple_choice",
        "4" => "checkbox",
        "5" => "select_box",
        "6" => "reference",
        "7" => "date",
        "8" => "date_time",
        "9" => "label",
        "10" => "break",
        "11" => "macro",
        "12" => "ui_page",
        "14" => "wide_single_line_text",
        "15" => "masked",
        "16" => "container_start",
        "17" => "container_end",
        "18" => "list_collector",
        "19" => "lookup_select_box",
        "20" => "lookup_multiple_choice",
        "21" => "html",
        "22" => "split_attachments",
        "23" => "macro_with_label",
        "24" => "email",
        "25" => "url",
        "26" => "ip_address",
        "27" => "duration",
        "28" => "number_scale",
        "29" => "template",
        "30" => "custom_table",
        _ => return None,
    })
}

fn rq(query: String, fields: &str, limit: &str) -> RecordQuery {
    RecordQuery {
        query: Some(query),
        fields: Some(fields.to_owned()),
        limit: Some(limit.to_owned()),
        ..RecordQuery::default()
    }
}

/// `typeof x === 'object' ? x.value : x`.
fn ref_value(v: Option<&Value>) -> Option<&Value> {
    match v {
        Some(Value::Object(_)) => prop(v, "value"),
        other => other,
    }
}

/// `parseInt(String(v), 10) || 0` as JSON: an i64 when it fits, else the
/// correctly rounded double JS computes (TF-4); a digit run past the double
/// range is `Infinity`, which `JSON.stringify` writes as `null`.
fn parse_int(v: Option<&Value>) -> Value {
    let s = tpl(v);
    let s = crate::logic::js::trim(&s);
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let end = digits
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(digits.len());
    let digits = &digits[..end];
    if digits.is_empty() {
        return json!(0);
    }
    if let Ok(n) = digits.parse::<i64>() {
        return json!(if neg { -n } else { n });
    }
    let f: f64 = digits.parse().unwrap_or(f64::INFINITY);
    serde_json::Number::from_f64(if neg { -f } else { f }).map_or(Value::Null, Value::Number)
}

/// `for (const x of v)`: arrays iterate their items, strings their UTF-16
/// code points (as one-character strings); anything else throws V8's
/// `<name> is not iterable`.
fn for_of<'a>(v: &'a Value, name: &str) -> Result<std::borrow::Cow<'a, [Value]>, Fail> {
    match v {
        Value::Array(a) => Ok(std::borrow::Cow::Borrowed(a)),
        Value::String(s) => Ok(std::borrow::Cow::Owned(
            s.chars().map(|c| Value::String(c.to_string())).collect(),
        )),
        _ => Err(Fail::Js(format!("{name} is not iterable"))),
    }
}

/// `v.<method>(...)` for an array method (`filter`, `map`, `forEach`): V8's
/// TypeError text when `v` is not an array (TF-3). The client maps a missing
/// `result` (JS `undefined`, the common malformed shape) and an explicit
/// `"result": null` both to `Value::Null`; the text follows the common case,
/// so only an explicit `null` result reads `undefined` instead of `null`.
fn array_method<'a>(v: &'a Value, name: &str, method: &str) -> Result<&'a [Value], Fail> {
    match v {
        Value::Array(a) => Ok(a),
        Value::Null => Err(Fail::Js(format!(
            "Cannot read properties of undefined (reading '{method}')"
        ))),
        _ => Err(Fail::Js(format!("{name}.{method} is not a function"))),
    }
}

/// `v.length > 0` then `v.map(...)`: `undefined` (see [`array_method`])
/// throws on `.length`; a value
/// without a positive `length` skips the branch; a non-empty string reaches
/// `.map` and throws there.
fn non_empty_then_map<'a>(v: &'a Value, name: &str) -> Result<&'a [Value], Fail> {
    match v {
        Value::Array(a) => Ok(a),
        Value::Null => Err(Fail::Js(
            "Cannot read properties of undefined (reading 'length')".to_owned(),
        )),
        Value::String(s) if !s.is_empty() => Err(Fail::Js(format!("{name}.map is not a function"))),
        _ => Ok(&[]),
    }
}

/// `list.map(v => v.sys_id).join(',')`: null and undefined join as empty.
fn ids(list: &[Value]) -> String {
    list.iter()
        .map(|v| match prop(Some(v), "sys_id") {
            None | Some(Value::Null) => String::new(),
            id => tpl(id),
        })
        .collect::<Vec<_>>()
        .join(",")
}

const CHOICE_TYPES: [&str; 4] = ["3", "5", "19", "20"];

fn choice_var_ids(vars: &[Value]) -> Vec<Value> {
    vars.iter()
        .filter(|v| CHOICE_TYPES.contains(&tpl(prop(Some(v), "type")).as_str()))
        .cloned()
        .collect()
}

pub(super) async fn get_item<S: Sn>(call: &Call<'_, S>, args: &Value) -> Result<String, Fail> {
    let sys_id = tpl(args.get("sys_id"));
    let item = call
        .record(
            "sc_cat_item",
            &sys_id,
            RecordParams {
                fields: Some(ITEM_FIELDS.to_owned()),
                display_value: Some("false".to_owned()),
                ..RecordParams::default()
            },
        )
        .await?;
    let variables = call
        .records(
            "item_option_new",
            rq(
                format!("cat_item={sys_id}^active=true^ORDERBYorder"),
                VARIABLE_FIELDS,
                "500",
            ),
        )
        .await?;
    let vars = array_method(&variables, "variables", "filter")?;
    let choice_vars = choice_var_ids(vars);

    // TF-5: 6.2.0 starts these four reads together (`Promise.all`). They run
    // here one after another in source order, and every one is sent even when
    // an earlier one failed, so the outbound requests match 6.2.0. When more
    // than one fails, the error shown is the first in source order; 6.2.0
    // shows the first to reject, which depends on response timing.
    let choices = if choice_vars.is_empty() {
        Ok(Value::Array(Vec::new()))
    } else {
        call.records(
            "question_choice",
            rq(
                format!("question=IN{}^ORDERBYorder", ids(&choice_vars)),
                CHOICE_FIELDS,
                "2000",
            ),
        )
        .await
    };
    let set_links = call
        .records(
            "io_set_item",
            rq(format!("sc_cat_item={sys_id}"), "sys_id,variable_set", "50"),
        )
        .await;
    let policies = call
        .records(
            "catalog_ui_policy",
            rq(
                format!("catalog_item={sys_id}^active=true"),
                "sys_id,short_description,conditions,run_scripts,reverse_if_false",
                "100",
            ),
        )
        .await;
    let scripts = call
        .records(
            "catalog_script_client",
            rq(
                format!("cat_item={sys_id}^active=true"),
                "sys_id,name,script_name,type,applies_to",
                "100",
            ),
        )
        .await;
    let (choices, set_links, policies, scripts) = (choices?, set_links?, policies?, scripts?);

    let mut by_var: HashMap<String, Vec<Value>> = HashMap::new();
    for c in array_method(&choices, "allChoicesRaw", "forEach")? {
        let key = tpl(ref_value(prop(Some(c), "question")));
        by_var.entry(key).or_default().push(object(vec![
            ("text", prop(Some(c), "text").cloned()),
            ("value", prop(Some(c), "value").cloned()),
        ]));
    }

    let policies = non_empty_then_map(&policies, "uiPolicies")?;
    let actions = if policies.is_empty() {
        Value::Array(Vec::new())
    } else {
        call.records(
            "catalog_ui_policy_action",
            rq(
                format!("ui_policy=IN{}", ids(policies)),
                "sys_id,ui_policy,catalog_variable,mandatory,visible,read_only",
                "500",
            ),
        )
        .await?
    };

    let enriched: Vec<Value> = vars
        .iter()
        .map(|v| {
            let f = |k: &str| prop(Some(v), k);
            let code = tpl(f("type"));
            let name =
                variable_type(&code).map_or_else(|| format!("unknown_{code}"), str::to_owned);
            let mandatory =
                f("mandatory") == Some(&json!("true")) || f("mandatory") == Some(&json!(true));
            let or_null = |k: &str| {
                Some(if truthy(f(k)) {
                    f(k).cloned().unwrap_or(Value::Null)
                } else {
                    Value::Null
                })
            };
            let mut m = Map::new();
            put(&mut m, "sys_id", f("sys_id"));
            put(&mut m, "name", f("name"));
            put(&mut m, "label", f("question_text"));
            m.insert("type".into(), json!(name));
            m.insert("type_code".into(), json!(code));
            m.insert("mandatory".into(), json!(mandatory));
            m.insert("order".into(), parse_int(f("order")));
            put(&mut m, "default_value", or_null("default_value").as_ref());
            put(&mut m, "help_text", or_null("help_text").as_ref());
            put(&mut m, "reference_table", or_null("reference").as_ref());
            if let Some(c) = by_var.get(&tpl(f("sys_id"))) {
                m.insert("choices".into(), Value::Array(c.clone()));
            }
            Value::Object(m)
        })
        .collect();

    // The return literal reads `item.sys_id` first.
    if item.is_null() {
        return Err(Fail::Js(
            "Cannot read properties of undefined (reading 'sys_id')".to_owned(),
        ));
    }
    let ip = |k: &str| prop(Some(&item), k).cloned();
    let item_type = if prop(Some(&item), "sys_class_name") == Some(&json!("sc_cat_item_producer")) {
        "record_producer"
    } else {
        "catalog_item"
    };
    let sets: Vec<Value> = array_method(&set_links, "varSetLinks", "map")?
        .iter()
        .map(|l| {
            let vs = prop(Some(l), "variable_set");
            let (id, display) = match vs {
                Some(Value::Object(_)) => (prop(vs, "value"), prop(vs, "display_value")),
                other => (other, other),
            };
            object(vec![
                ("sys_id", id.cloned()),
                ("display_value", display.cloned()),
            ])
        })
        .collect();
    // `policyActions.filter` runs once per policy, so it only throws when
    // there is a policy.
    let actions: &[Value] = if policies.is_empty() {
        &[]
    } else {
        array_method(&actions, "policyActions", "filter")?
    };
    let ui_policies: Vec<Value> = policies
        .iter()
        .map(|p| {
            let pid = prop(Some(p), "sys_id");
            let acts: Vec<Value> = actions
                .iter()
                .filter(|a| ref_value(prop(Some(a), "ui_policy")) == pid)
                .map(|a| {
                    let cv = prop(Some(a), "catalog_variable");
                    let variable = match cv {
                        Some(Value::Object(_)) => or(prop(cv, "display_value"), prop(cv, "value")),
                        other => other,
                    };
                    object(vec![
                        ("variable", variable.cloned()),
                        ("mandatory", prop(Some(a), "mandatory").cloned()),
                        ("visible", prop(Some(a), "visible").cloned()),
                        ("read_only", prop(Some(a), "read_only").cloned()),
                    ])
                })
                .collect();
            object(vec![
                ("sys_id", pid.cloned()),
                ("description", prop(Some(p), "short_description").cloned()),
                ("conditions", prop(Some(p), "conditions").cloned()),
                ("actions", Some(Value::Array(acts))),
            ])
        })
        .collect();
    let client_scripts: Vec<Value> = array_method(&scripts, "clientScripts", "map")?
        .iter()
        .map(|s| {
            object(vec![
                (
                    "name",
                    or(prop(Some(s), "name"), prop(Some(s), "script_name")).cloned(),
                ),
                ("type", prop(Some(s), "type").cloned()),
                ("applies_to", prop(Some(s), "applies_to").cloned()),
            ])
        })
        .collect();
    let detail = object(vec![
        ("sys_id", ip("sys_id")),
        ("name", ip("name")),
        ("short_description", ip("short_description")),
        ("description", ip("description")),
        ("category", ip("category")),
        ("active", ip("active")),
        ("item_type", Some(json!(item_type))),
        ("variables", Some(Value::Array(enriched))),
        ("variable_sets", Some(Value::Array(sets))),
        ("ui_policies", Some(Value::Array(ui_policies))),
        ("client_scripts", Some(Value::Array(client_scripts))),
    ]);
    Ok(format!(
        "Catalog item details for {}:\n{}",
        tpl(prop(Some(&detail), "name")),
        pretty(Some(&detail))
    ))
}

/// `searchCatalogItems` query.
fn search_query(args: &Value) -> String {
    let mut conditions = Vec::new();
    match args.get("active") {
        None => conditions.push("active=true".to_owned()),
        Some(Value::Null) => {}
        v => conditions.push(format!("active={}", tpl(v))),
    }
    if truthy(args.get("category")) {
        conditions.push(format!("category={}", tpl(args.get("category"))));
    }
    if truthy(args.get("keyword")) {
        let k = tpl(args.get("keyword"));
        conditions.push(format!("nameLIKE{k}^ORshort_descriptionLIKE{k}"));
    }
    if conditions.is_empty() {
        "active=true".to_owned()
    } else {
        conditions.join("^")
    }
}

fn with_default(args: &Value, key: &str, default: Value) -> Value {
    args.get(key).cloned().unwrap_or(default)
}

fn includes_producers(args: &Value) -> bool {
    args.get("include_producers")
        .is_none_or(|v| truthy(Some(v)))
}

pub(super) async fn search<S: Sn>(call: &Call<'_, S>, args: &Value) -> Result<String, Fail> {
    let q = search_query(args);
    let limit = param(Some(&with_default(args, "limit", json!(25))));
    let fetch = || RecordQuery {
        query: Some(q.clone()),
        fields: Some(SEARCH_FIELDS.to_owned()),
        limit: limit.clone(),
        ..RecordQuery::default()
    };
    let found = call.records("sc_cat_item", fetch()).await?;
    let mut producers = Value::Array(Vec::new());
    if includes_producers(args) {
        match call.records("sc_cat_item_producer", fetch()).await {
            Ok(v) => producers = v,
            Err(Fail::Cancelled) => return Err(Fail::Cancelled),
            // sc_cat_item_producer may not exist or be readable (6.2.0 logs and continues).
            Err(_) => {}
        }
    }
    let normalize = |r: &Value, default: &str| {
        let f = |k: &str| prop(Some(r), k).cloned();
        let item_type = if f("sys_class_name") == Some(json!("sc_cat_item_producer")) {
            "record_producer"
        } else {
            default
        };
        object(vec![
            ("sys_id", f("sys_id")),
            ("name", f("name")),
            ("short_description", f("short_description")),
            ("category", f("category")),
            ("active", f("active")),
            ("item_type", Some(json!(item_type))),
        ])
    };
    let mut out: Vec<Value> = array_method(&found, "items", "map")?
        .iter()
        .map(|i| normalize(i, "catalog_item"))
        .collect();
    out.extend(
        array_method(&producers, "producers", "map")?
            .iter()
            .map(|p| normalize(p, "record_producer")),
    );
    let out = Value::Array(out);
    Ok(format!(
        "Found {} catalog item(s):\n{}",
        super::js_out::length(&out),
        pretty(Some(&out))
    ))
}

fn submit_payload(args: &Value) -> Value {
    let mut m = Map::new();
    m.insert(
        "sysparm_quantity".into(),
        with_default(args, "quantity", json!(1)),
    );
    m.insert(
        "variables".into(),
        with_default(args, "variables", json!({})),
    );
    if truthy(args.get("requested_for")) {
        put(&mut m, "requested_for", args.get("requested_for"));
    }
    Value::Object(m)
}

pub(super) async fn submit<S: Sn>(call: &mut Call<'_, S>, args: &Value) -> Result<String, Fail> {
    let path = order_now_path(&tpl(args.get("sys_id"))).map_err(|e| call.not_sent(e))?;
    let payload = submit_payload(args);
    let data = call.mutate(call.sn.post(&path, &payload)).await?;
    let result = prop(Some(&data), "result");
    let number = or(
        or(prop(result, "request_number"), prop(result, "number")),
        prop(result, "sys_id"),
    );
    let number = if truthy(number) {
        tpl(number)
    } else {
        "unknown".to_owned()
    };
    Ok(format!(
        "✅ Catalog form submitted successfully!\n\nRequest: {number}\n\nFull result:\n{}",
        pretty(result)
    ))
}

fn categories_query(args: &Value) -> String {
    param(args.get("query")).unwrap_or_else(|| "active=true".to_owned())
}

pub(super) async fn categories<S: Sn>(call: &Call<'_, S>, args: &Value) -> Result<String, Fail> {
    let results = call
        .records(
            "sc_category",
            RecordQuery {
                query: Some(categories_query(args)),
                fields: Some(CATEGORY_FIELDS.to_owned()),
                limit: param(Some(&with_default(args, "limit", json!(50)))),
                ..RecordQuery::default()
            },
        )
        .await?;
    Ok(format!(
        "Found {} catalog categorie(s):\n{}",
        super::js_out::length(&results),
        pretty(Some(&results))
    ))
}

fn issue(
    severity: &str,
    component: &str,
    sys_id: Option<&Value>,
    text: String,
    fix: &str,
) -> Value {
    object(vec![
        ("severity", Some(json!(severity))),
        ("component", Some(json!(component))),
        ("sys_id", sys_id.cloned()),
        ("issue", Some(json!(text))),
        ("fix", Some(json!(fix))),
    ])
}

#[derive(Default)]
struct Validation {
    issues: Vec<Value>,
    warnings: u64,
    errors: u64,
}

impl Validation {
    fn error(&mut self, v: Value) {
        self.issues.push(v);
        self.errors += 1;
    }
}

async fn check<S: Sn>(
    call: &Call<'_, S>,
    item: &str,
    checks: &Value,
    r: &mut Validation,
) -> Result<(), Fail> {
    if checks.is_null() {
        return Err(Fail::Js(
            "Cannot read properties of null (reading 'variables')".to_owned(),
        ));
    }
    let cv = prop(Some(checks), "variables");
    if truthy(cv) {
        let variables = call
            .records(
                "item_option_new",
                RecordQuery {
                    query: Some(format!("cat_item={item}")),
                    limit: Some("1000".to_owned()),
                    ..RecordQuery::default()
                },
            )
            .await?;
        for v in for_of(&variables, "variables")?.iter() {
            let f = |k: &str| prop(Some(v), k);
            if truthy(prop(cv, "check_linked")) && !truthy(f("cat_item")) {
                r.error(issue(
                    "error",
                    "variable",
                    f("sys_id"),
                    format!("Variable {} is not linked to catalog item", tpl(f("name"))),
                    "Update cat_item field",
                ));
            }
            if truthy(f("tooltip"))
                && let Some(len) = js_length(f("tooltip")).filter(|l| *l > 40)
            {
                r.issues.push(issue(
                    "warning",
                    "variable",
                    f("sys_id"),
                    format!("Tooltip exceeds 40 characters and will be truncated ({len} chars)"),
                    "Move detailed help to help_text field",
                ));
                r.warnings += 1;
            }
            let ty = f("type").and_then(Value::as_str);
            if truthy(prop(cv, "check_choices")) && matches!(ty, Some("1" | "5")) {
                let choices = call
                    .records(
                        "question_choice",
                        RecordQuery {
                            query: Some(format!("question={}", tpl(f("sys_id")))),
                            limit: Some("1".to_owned()),
                            ..RecordQuery::default()
                        },
                    )
                    .await?;
                // `choices.length === 0` (TF-3): a missing result throws.
                let no_choices = match &choices {
                    Value::Null => {
                        return Err(Fail::Js(
                            "Cannot read properties of undefined (reading 'length')".to_owned(),
                        ));
                    }
                    Value::Array(a) => a.is_empty(),
                    Value::String(t) => t.is_empty(),
                    _ => false,
                };
                if no_choices {
                    let name = if ty == Some("1") {
                        "Choice"
                    } else {
                        "Select Box"
                    };
                    r.error(issue(
                        "error",
                        "variable",
                        f("sys_id"),
                        format!(
                            "Variable {} is type {name} but has no choices defined",
                            tpl(f("name"))
                        ),
                        "Add choices via question_choice table",
                    ));
                }
            }
        }
    }
    let cp = prop(Some(checks), "ui_policies");
    if truthy(cp) {
        let policies = call
            .records(
                "catalog_ui_policy",
                RecordQuery {
                    query: Some(format!("catalog_item={item}")),
                    limit: Some("1000".to_owned()),
                    ..RecordQuery::default()
                },
            )
            .await?;
        for p in for_of(&policies, "policies")?.iter() {
            if !truthy(prop(cp, "check_actions_linked")) {
                continue;
            }
            let actions = call
                .records(
                    "catalog_ui_policy_action",
                    RecordQuery {
                        query: Some(format!("ui_policy={}", tpl(prop(Some(p), "sys_id")))),
                        limit: Some("1000".to_owned()),
                        ..RecordQuery::default()
                    },
                )
                .await?;
            for a in for_of(&actions, "actions")?.iter() {
                if !truthy(prop(Some(a), "catalog_variable")) {
                    r.error(issue(
                        "error",
                        "ui_policy_action",
                        prop(Some(a), "sys_id"),
                        "catalog_variable field is empty - action not linked to policy".to_owned(),
                        "Run background script to set catalog_variable value",
                    ));
                }
            }
        }
    }
    Ok(())
}

pub(super) async fn validate<S: Sn>(call: &Call<'_, S>, args: &Value) -> Result<String, Fail> {
    let item = tpl(args.get("catalog_item"));
    let checks = with_default(args, "checks", json!({}));
    let mut r = Validation::default();
    match check(call, &item, &checks, &mut r).await {
        Ok(()) => {}
        Err(Fail::Cancelled) => return Err(Fail::Cancelled),
        Err(e) => {
            r.issues.push(object(vec![
                ("severity", Some(json!("error"))),
                ("component", Some(json!("validation"))),
                (
                    "issue",
                    Some(json!(format!("Validation failed: {}", e.message()))),
                ),
            ]));
            r.errors += 1;
        }
    }
    let valid = r.errors == 0;
    let results = json!({
        "valid": valid,
        "issues": r.issues,
        "warnings": r.warnings,
        "errors": r.errors,
    });
    Ok(format!(
        "Validation {}:\n{}",
        if valid { "PASSED" } else { "FAILED" },
        pretty(Some(&results))
    ))
}

pub(super) fn normalize(kind: Kind, args: &Value, input: &NormalizeInput<'_>) -> Vec<Operation> {
    let all = Vec::new;
    match kind {
        Kind::CatalogGetItem => {
            let id = tpl(args.get("sys_id"));
            vec![
                read(kind, "sc_cat_item", &[], all(), input),
                read(
                    kind,
                    "item_option_new",
                    &[&format!("cat_item={id}^active=true^ORDERBYorder")],
                    all(),
                    input,
                ),
                read(kind, "question_choice", &[], all(), input),
                read(
                    kind,
                    "io_set_item",
                    &[&format!("sc_cat_item={id}")],
                    all(),
                    input,
                ),
                read(
                    kind,
                    "catalog_ui_policy",
                    &[&format!("catalog_item={id}^active=true")],
                    all(),
                    input,
                ),
                read(
                    kind,
                    "catalog_script_client",
                    &[&format!("cat_item={id}^active=true")],
                    all(),
                    input,
                ),
                read(kind, "catalog_ui_policy_action", &[], all(), input),
            ]
        }
        Kind::CatalogSearchItems => {
            let q = search_query(args);
            let mut ops = vec![read(kind, "sc_cat_item", &[&q], all(), input)];
            if includes_producers(args) {
                ops.push(read(kind, "sc_cat_item_producer", &[&q], all(), input));
            }
            ops
        }
        Kind::CatalogGetCategories => {
            vec![read(
                kind,
                "sc_category",
                &[&categories_query(args)],
                all(),
                input,
            )]
        }
        Kind::CatalogSubmit => {
            // The order runs the item's flows; no single table is written.
            let vars = with_default(args, "variables", json!({}));
            vec![write(kind, Action::Create, None, &vars, input)]
        }
        _ => {
            let id = tpl(args.get("catalog_item"));
            vec![
                read(
                    kind,
                    "item_option_new",
                    &[&format!("cat_item={id}")],
                    all(),
                    input,
                ),
                read(kind, "question_choice", &[], all(), input),
                read(
                    kind,
                    "catalog_ui_policy",
                    &[&format!("catalog_item={id}")],
                    all(),
                    input,
                ),
                read(kind, "catalog_ui_policy_action", &[], all(), input),
            ]
        }
    }
}

#[cfg(test)]
mod tests {
    use hpm_types::tool::{MutationOutcome, ResultBody, ToolError};
    use serde_json::json;

    use super::super::fake::Fake;
    use super::super::run as run_tool;
    use super::*;

    fn text(done: &hpm_types::tool::HandlerCompletion) -> String {
        match &done.result {
            Ok(ResultBody::ToolResult { text, .. }) => text.clone(),
            other => panic!("not a tool result: {other:?}"),
        }
    }

    #[tokio::test]
    async fn search_defaults_and_tolerates_missing_producer_table() {
        let sn = Fake::new()
            .ok(json!([{"sys_id": "a", "name": "Laptop", "sys_class_name": "sc_cat_item"}]))
            .http(403, "403 no access");
        let done = run_tool(Kind::CatalogSearchItems, &sn, &json!({"keyword": "lap"})).await;
        assert_eq!(
            sn.calls()[0],
            format!(
                "GET sc_cat_item query=active=true^nameLIKElap^ORshort_descriptionLIKElap limit=25 fields={SEARCH_FIELDS}"
            )
        );
        assert_eq!(sn.calls().len(), 2);
        assert!(text(&done).starts_with("Found 1 catalog item(s):\n"));
        assert!(text(&done).contains("\"item_type\": \"catalog_item\""));

        let sn = Fake::new().ok(json!([]));
        run_tool(
            Kind::CatalogSearchItems,
            &sn,
            &json!({"active": null, "include_producers": false}),
        )
        .await;
        assert_eq!(
            sn.calls(),
            [format!(
                "GET sc_cat_item query=active=true limit=25 fields={SEARCH_FIELDS}"
            )]
        );
    }

    #[tokio::test]
    async fn submit_payload_request_number_and_errors() {
        let sn = Fake::new().ok(json!({"result": {"sys_id": "r1", "number": "REQ1"}}));
        let done = run_tool(
            Kind::CatalogSubmit,
            &sn,
            &json!({"sys_id": "abc", "variables": {"a": "1"}, "requested_for": "u1", "quantity": 2}),
        )
        .await;
        assert_eq!(
            sn.calls(),
            [
                r#"POST /api/sn_sc/servicecatalog/items/abc/order_now {"sysparm_quantity":2,"variables":{"a":"1"},"requested_for":"u1"}"#
            ]
        );
        assert!(text(&done).starts_with(
            "✅ Catalog form submitted successfully!\n\nRequest: REQ1\n\nFull result:\n{"
        ));
        assert_eq!(done.outcome, MutationOutcome::Committed);

        let sn = Fake::new().ok(json!({"result": {}}));
        let done = run_tool(
            Kind::CatalogSubmit,
            &sn,
            &json!({"sys_id": "abc", "variables": {}}),
        )
        .await;
        assert!(text(&done).contains("Request: unknown\n"));

        let sn = Fake::new().http(400, "400 Mandatory variables missing");
        let done = run_tool(
            Kind::CatalogSubmit,
            &sn,
            &json!({"sys_id": "abc", "variables": {}}),
        )
        .await;
        assert!(
            matches!(&done.result, Err(ToolError::Upstream { message, .. }) if message == "400 Mandatory variables missing")
        );
        assert_eq!(done.outcome, MutationOutcome::Failed);
    }

    #[tokio::test]
    async fn categories_default_query_and_limit() {
        let sn = Fake::new().ok(json!([]));
        let done = run_tool(Kind::CatalogGetCategories, &sn, &json!({})).await;
        assert_eq!(
            sn.calls(),
            [format!(
                "GET sc_category query=active=true limit=50 fields={CATEGORY_FIELDS}"
            )]
        );
        assert_eq!(text(&done), "Found 0 catalog categorie(s):\n[]");
    }

    #[tokio::test]
    async fn get_item_enriches_variables_choices_and_policies() {
        let sn = Fake::new()
            .ok(json!({"sys_id": "i1", "name": "Laptop", "sys_class_name": "sc_cat_item_producer"}))
            .ok(json!([
                {"sys_id": "v1", "name": "size", "question_text": "Size", "type": "5", "mandatory": "true", "order": "100", "default_value": ""},
                {"sys_id": "v2", "name": "x", "type": "99", "order": "abc"}
            ]))
            .ok(json!([{"question": {"value": "v1"}, "text": "Small", "value": "s"}]))
            .ok(json!([{"variable_set": {"value": "vs1", "display_value": "Set"}}]))
            .ok(json!([{"sys_id": "p1", "short_description": "Pol", "conditions": "c"}]))
            .ok(json!([{"script_name": "onLoad", "type": "onLoad"}]))
            .ok(json!([{"ui_policy": "p1", "catalog_variable": {"value": "v1"}, "mandatory": "true"}]));
        let done = run_tool(Kind::CatalogGetItem, &sn, &json!({"sys_id": "i1"})).await;
        let calls = sn.calls();
        assert_eq!(calls.len(), 7);
        assert!(calls[2].starts_with("GET question_choice query=question=INv1^ORDERBYorder"));
        assert!(calls[6].starts_with("GET catalog_ui_policy_action query=ui_policy=INp1"));
        let t = text(&done);
        let body: Value = serde_json::from_str(
            t.strip_prefix("Catalog item details for Laptop:\n")
                .expect("prefix"),
        )
        .expect("json");
        assert_eq!(body["item_type"], "record_producer");
        assert_eq!(body["variables"][0]["type"], "select_box");
        assert_eq!(body["variables"][0]["mandatory"], true);
        assert_eq!(body["variables"][0]["order"], 100);
        assert_eq!(body["variables"][0]["default_value"], Value::Null);
        assert_eq!(
            body["variables"][0]["choices"],
            json!([{"text": "Small", "value": "s"}])
        );
        assert_eq!(body["variables"][1]["type"], "unknown_99");
        assert_eq!(body["variables"][1]["order"], 0);
        assert_eq!(
            body["variable_sets"],
            json!([{"sys_id": "vs1", "display_value": "Set"}])
        );
        assert_eq!(
            body["ui_policies"][0]["actions"],
            json!([{"variable": "v1", "mandatory": "true"}])
        );
        assert_eq!(
            body["client_scripts"],
            json!([{"name": "onLoad", "type": "onLoad"}])
        );
    }

    #[tokio::test]
    async fn validate_reports_issues_and_catches_errors() {
        let long = "x".repeat(41);
        let sn = Fake::new()
            .ok(json!([{"sys_id": "v1", "name": "a", "type": "5", "tooltip": long}]))
            .ok(json!([]))
            .ok(json!([{"sys_id": "p1"}]))
            .ok(json!([{"sys_id": "a1", "catalog_variable": ""}]));
        let done = run_tool(
            Kind::ValidateConfiguration,
            &sn,
            &json!({"catalog_item": "c1", "checks": {
                "variables": {"check_linked": true, "check_choices": true},
                "ui_policies": {"check_actions_linked": true}}}),
        )
        .await;
        let t = text(&done);
        let body: Value =
            serde_json::from_str(t.strip_prefix("Validation FAILED:\n").expect("prefix"))
                .expect("json");
        assert_eq!(body["errors"], 3);
        assert_eq!(body["warnings"], 1);
        assert_eq!(
            body["issues"][1]["issue"],
            "Tooltip exceeds 40 characters and will be truncated (41 chars)"
        );
        assert_eq!(
            body["issues"][2]["issue"],
            "Variable a is type Select Box but has no choices defined"
        );

        let sn = Fake::new();
        let done = run_tool(
            Kind::ValidateConfiguration,
            &sn,
            &json!({"catalog_item": "c1"}),
        )
        .await;
        assert_eq!(
            text(&done),
            "Validation PASSED:\n{\n  \"valid\": true,\n  \"issues\": [],\n  \"warnings\": 0,\n  \"errors\": 0\n}"
        );
        assert!(sn.calls().is_empty());

        let sn = Fake::new().http(403, "403 denied");
        let done = run_tool(
            Kind::ValidateConfiguration,
            &sn,
            &json!({"catalog_item": "c1", "checks": {"variables": {}}}),
        )
        .await;
        assert!(text(&done).contains("\"issue\": \"Validation failed: 403 denied\""));
    }

    fn shown(done: &hpm_types::tool::HandlerCompletion) -> String {
        format!("{:?}", done.result)
    }

    #[test]
    fn parse_int_follows_js_past_i64() {
        assert_eq!(parse_int(Some(&json!("42abc"))), json!(42));
        assert_eq!(parse_int(Some(&json!(" -7"))), json!(-7));
        assert_eq!(parse_int(Some(&json!("x"))), json!(0));
        assert_eq!(parse_int(Some(&json!("-0"))), json!(0));
        let big = parse_int(Some(&json!("99999999999999999999")));
        assert_eq!(big.as_f64(), Some(1e20));
        assert_eq!(
            super::super::js_out::pretty(Some(&big)),
            "100000000000000000000"
        );
        assert_eq!(parse_int(Some(&json!("9".repeat(400)))), Value::Null);
    }

    #[tokio::test]
    async fn get_item_malformed_results_throw_the_620_text() {
        let id = json!({"sys_id": "0123456789abcdef0123456789abcdef"});
        // `variables.filter` on a missing result.
        let sn = Fake::new().ok(json!({"sys_id": "i"})).ok(Value::Null);
        let done = run_tool(Kind::CatalogGetItem, &sn, &id).await;
        assert!(
            shown(&done).contains("Cannot read properties of undefined (reading 'filter')"),
            "{}",
            shown(&done)
        );
        let sn = Fake::new().ok(json!({"sys_id": "i"})).ok(json!({"a": 1}));
        let done = run_tool(Kind::CatalogGetItem, &sn, &id).await;
        assert!(shown(&done).contains("variables.filter is not a function"));
        // `uiPolicies.length` on a missing result.
        let sn = Fake::new()
            .ok(json!({"sys_id": "i"}))
            .ok(json!([]))
            .ok(json!([]))
            .ok(Value::Null)
            .ok(json!([]));
        let done = run_tool(Kind::CatalogGetItem, &sn, &id).await;
        assert!(shown(&done).contains("Cannot read properties of undefined (reading 'length')"));
    }

    #[tokio::test]
    async fn get_item_sends_every_parallel_read_and_reports_the_first_failure() {
        let id = json!({"sys_id": "0123456789abcdef0123456789abcdef"});
        let sn = Fake::new()
            .ok(json!({"sys_id": "i"}))
            .ok(json!([]))
            .http(403, "403 io_set_item")
            .http(500, "500 policy")
            .ok(json!([]));
        let done = run_tool(Kind::CatalogGetItem, &sn, &id).await;
        let tables: Vec<String> = sn
            .calls()
            .iter()
            .map(|c| c.split(' ').nth(1).unwrap_or_default().to_owned())
            .collect();
        assert_eq!(
            tables,
            [
                "sc_cat_item/0123456789abcdef0123456789abcdef",
                "item_option_new",
                "io_set_item",
                "catalog_ui_policy",
                "catalog_script_client"
            ]
        );
        assert!(shown(&done).contains("403 io_set_item"), "{}", shown(&done));
    }

    #[tokio::test]
    async fn validate_choices_length_on_a_missing_result_throws() {
        let sn = Fake::new()
            .ok(json!([{"sys_id": "v", "name": "n", "cat_item": "c", "type": "5"}]))
            .ok(Value::Null);
        let done = run_tool(
            Kind::ValidateConfiguration,
            &sn,
            &json!({"catalog_item": "c", "checks": {"variables": {"check_choices": true}}}),
        )
        .await;
        assert!(
            shown(&done).contains("Cannot read properties of undefined (reading 'length')"),
            "{}",
            shown(&done)
        );
    }
}
