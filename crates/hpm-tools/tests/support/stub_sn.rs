//! Rust port of `scripts/golden/lib/stub-sn.mjs`: deterministic loopback stand-in
//! for a ServiceNow instance (Table API, update-set/application pickers, session
//! bootstrap) and for raw.githubusercontent.com. Records every request in order.
//! Keep in step with the JS; `stub_selftest` in `golden_harness.rs` pins its answers.

use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use regex::Regex;
use serde_json::{Map, Value, json};

use super::http::{self, Described, RawRequest, Reply};
use super::sha1;

pub const FIXED_TIME: &str = "2026-01-15 12:00:00";

/// `sysId(kind, n)`: first 32 hex chars of `sha1("golden:<kind>:<n>")`.
pub fn sys_id(kind: &str, n: u32) -> String {
    let mut h = sha1::hex(format!("golden:{kind}:{n}").as_bytes());
    h.truncate(32);
    h
}

fn obj(v: Value) -> Map<String, Value> {
    match v {
        Value::Object(m) => m,
        _ => Map::new(),
    }
}

/// `makeRow(table, n, overrides)`: later keys replace in place, new keys append.
pub fn make_row(table: &str, n: u32, overrides: &Map<String, Value>) -> Map<String, Value> {
    let mut row = obj(json!({
        "sys_id": sys_id(table, n),
        "sys_created_on": FIXED_TIME,
        "sys_updated_on": FIXED_TIME,
        "sys_created_by": "golden.user",
        "sys_updated_by": "golden.user",
        "sys_class_name": table,
    }));
    let pad = format!("{n:07}");
    let extra = match table {
        "incident" => {
            json!({"number": format!("INC{pad}"), "short_description": format!("Golden incident {n}"), "state": "1", "priority": "3", "urgency": "2", "impact": "2", "category": "software", "caller_id": sys_id("sys_user", 1), "assigned_to": "", "assignment_group": ""})
        }
        "problem" => {
            json!({"number": format!("PRB{pad}"), "short_description": format!("Golden problem {n}"), "state": "1", "priority": "3"})
        }
        "change_request" => {
            json!({"number": format!("CHG{pad}"), "short_description": format!("Golden change {n}"), "state": "-5", "type": "normal", "approval": "not requested", "priority": "3"})
        }
        "sys_user" => {
            json!({"user_name": format!("golden.user{n}"), "name": format!("Golden User {n}"), "first_name": "Golden", "last_name": format!("User {n}"), "email": format!("golden.user{n}@example.invalid"), "active": "true"})
        }
        "sys_user_group" => {
            json!({"name": format!("Golden Group {n}"), "description": "Golden group", "active": "true"})
        }
        "cmdb_ci" => {
            json!({"name": format!("golden-ci-{n}"), "sys_class_name": "cmdb_ci_server", "operational_status": "1"})
        }
        "sys_update_set" => {
            json!({"name": format!("Golden Update Set {n}"), "description": "Golden update set", "state": "in progress", "application": sys_id("sys_scope", 1), "is_default": "false", "parent": "", "base_update_set": "", "sys_created_by": "golden.user"})
        }
        "sys_scope" | "sys_app" => {
            json!({"name": format!("Golden App {n}"), "scope": format!("x_golden_app{n}"), "version": "1.0.0", "active": "true"})
        }
        "sys_dictionary" => {
            json!({"name": "incident", "element": format!("field_{n}"), "column_label": format!("Field {n}"), "internal_type": "string", "max_length": "40", "mandatory": "false", "read_only": "false", "reference": "", "active": "true", "default_value": ""})
        }
        "sys_db_object" => {
            json!({"name": "incident", "label": "Incident", "super_class": sys_id("sys_db_object", 2), "sys_package": "", "is_extendable": "true"})
        }
        "sys_trigger" => {
            json!({"name": format!("golden trigger {n}"), "trigger_type": "0", "state": "0"})
        }
        "sc_cat_item" => {
            json!({"name": format!("Golden Item {n}"), "short_description": "Golden catalog item", "active": "true", "category": sys_id("sc_category", 1)})
        }
        "sc_category" => {
            json!({"title": format!("Golden Category {n}"), "description": "Golden category", "active": "true"})
        }
        _ => json!({"name": format!("golden-{table}-{n}")}),
    };
    for (k, v) in obj(extra).into_iter().chain(overrides.clone()) {
        row.insert(k, v);
    }
    row
}

/// `[status, body, headers]`. `Body::None` is JS `undefined`.
pub struct Answer {
    pub status: u16,
    pub body: Body,
    pub headers: Vec<(String, String)>,
}

pub enum Body {
    Json(Value),
    Text(String),
    None,
}

pub fn json_answer(status: u16, body: Value) -> Answer {
    Answer {
        status,
        body: Body::Json(body),
        headers: Vec::new(),
    }
}

/// Per-scenario mutable state (`state` in the JS).
#[derive(Default)]
pub struct State {
    pub seq: u32,
    pub picker_update_set: Option<Value>,
    pub picker_app: Option<Value>,
    pub sessions: u32,
    pub trigger_marker: Option<String>,
    requests: Vec<Described>,
}

/// Scenario override, tried before the default routes (`stub` in `scenarios/tools.mjs`).
pub type Routes = Arc<dyn Fn(&Described, &mut State) -> Option<Answer> + Send + Sync>;

#[derive(Default)]
struct Shared {
    routes: Option<Routes>,
    state: State,
}

#[derive(Clone)]
pub struct StubSn {
    pub port: u16,
    shared: Arc<Mutex<Shared>>,
}

impl StubSn {
    pub async fn start() -> Self {
        let shared: Arc<Mutex<Shared>> = Arc::default();
        let s = shared.clone();
        let port = http::serve(Arc::new(move |raw: RawRequest| {
            let described = http::describe(&raw.method, &raw.target, &raw.headers, &raw.body);
            let mut guard = s.lock().unwrap_or_else(PoisonError::into_inner);
            let Shared { routes, state } = &mut *guard;
            state.requests.push(described.clone());
            // JS turns a throwing route into a 500 `stub failure: …`.
            let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                routes
                    .as_ref()
                    .and_then(|r| r(&described, state))
                    .unwrap_or_else(|| default_routes(&described, state))
            }))
            .unwrap_or_else(|panic| {
                let why = panic
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
                    .unwrap_or_default();
                Answer {
                    status: 500,
                    body: Body::Json(json!({"error": {"message": format!("stub failure: {why}"), "detail": "golden stub"}})),
                    headers: Vec::new(),
                }
            });
            to_reply(answer)
        }))
        .await;
        Self { port, shared }
    }

    pub fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// `reset(routes)`: fresh state for one scenario.
    pub fn reset(&self, routes: Option<Routes>) {
        let mut g = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        g.routes = routes;
        g.state = State::default();
    }

    /// `take()`: requests since the last take, in arrival order.
    pub fn take(&self) -> Vec<Described> {
        std::mem::take(
            &mut self
                .shared
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .state
                .requests,
        )
    }
}

fn to_reply(a: Answer) -> Reply {
    let (content_type, body) = match a.body {
        Body::Text(t) => ("text/html;charset=UTF-8", Some(t.into_bytes())),
        Body::Json(v) => (
            "application/json;charset=UTF-8",
            Some(serde_json::to_vec(&v).expect("serialize stub body")),
        ),
        Body::None => ("application/json;charset=UTF-8", None),
    };
    let mut headers = vec![("content-type".to_owned(), content_type.to_owned())];
    for (k, v) in a.headers {
        if k.eq_ignore_ascii_case("content-type") {
            headers.retain(|(h, _)| h != "content-type");
        }
        headers.push((k, v));
    }
    Reply {
        status: a.status,
        headers,
        body: if a.status == 204 { None } else { body },
    }
}

fn next_created(state: &mut State, table: &str, body: Option<&Value>) -> Value {
    state.seq += 1;
    let mut row = make_row(table, 100 + state.seq, &Map::new());
    if let Some(Value::Object(b)) = body {
        for (k, v) in b {
            row.insert(k.clone(), v.clone());
        }
    }
    row.insert(
        "sys_id".into(),
        json!(sys_id(&format!("created:{table}"), state.seq)),
    );
    Value::Object(row)
}

static TABLE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^/api/now/table/([^/]+)(?:/([^/]+))?$").expect("table re"));
static NUMBER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"number=([A-Z]+[0-9]+)").expect("number re"));
static SYS_ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"sys_id=([0-9a-f]{32})").expect("sys_id re"));

/// JS `Number(x) || 3` for `sysparm_limit`, then `Math.min(limit, 3)` rows.
fn row_count(limit: Option<&str>) -> u32 {
    let n = match limit {
        None => 3.0,
        Some(s) => {
            let t = s.trim();
            if t.is_empty() {
                0.0
            } else {
                t.parse::<f64>().unwrap_or(f64::NAN)
            }
        }
    };
    let n = if n.is_nan() || n == 0.0 { 3.0 } else { n };
    let c = n.min(3.0);
    if c >= 1.0 { c.floor() as u32 } else { 0 }
}

/// `defaultRoutes(ctx)`.
pub fn default_routes(req: &Described, state: &mut State) -> Answer {
    let method = req.method.as_str();
    let path = req.path.as_str();

    if method == "GET" && path == "/" {
        state.sessions += 1;
        return Answer {
            status: 200,
            body: Body::Text("<html><body>golden</body></html>".into()),
            headers: vec![
                (
                    "set-cookie".into(),
                    format!(
                        "JSESSIONID=golden-session-{}; Path=/; HttpOnly",
                        state.sessions
                    ),
                ),
                (
                    "set-cookie".into(),
                    "glide_user_route=golden-route; Path=/".into(),
                ),
            ],
        };
    }

    if let Some(m) = TABLE_RE.captures(path) {
        let table = m.get(1).map_or("", |t| t.as_str());
        let id = m.get(2).map(|i| i.as_str());
        match (method, id) {
            ("GET", None) => {
                let query = req.query_value("sysparm_query").unwrap_or("");
                let num = NUMBER_RE.captures(query).map(|c| c[1].to_owned());
                let sid = SYS_ID_RE.captures(query).map(|c| c[1].to_owned());
                let count = if num.is_some() || sid.is_some() {
                    1
                } else {
                    row_count(req.query_value("sysparm_limit"))
                };
                let rows: Vec<Value> = (1..=count)
                    .map(|i| {
                        let mut o = Map::new();
                        if let Some(n) = &num {
                            o.insert("number".into(), json!(n));
                        }
                        if let Some(s) = &sid {
                            o.insert("sys_id".into(), json!(s));
                        }
                        Value::Object(make_row(table, i, &o))
                    })
                    .collect();
                let total = rows.len().to_string();
                return Answer {
                    status: 200,
                    body: Body::Json(json!({ "result": rows })),
                    headers: vec![("x-total-count".into(), total)],
                };
            }
            ("GET", Some(id)) => {
                return json_answer(
                    200,
                    json!({"result": make_row(table, 1, &obj(json!({"sys_id": id})))}),
                );
            }
            ("POST", _) => {
                return json_answer(
                    201,
                    json!({"result": next_created(state, table, req.body.as_ref())}),
                );
            }
            ("PATCH" | "PUT", _) => {
                let mut row = make_row(table, 1, &obj(json!({"sys_id": id})));
                // JS `{sys_id: undefined}` drops the key from the JSON.
                if id.is_none() {
                    row.remove("sys_id");
                }
                if let Some(Value::Object(b)) = &req.body {
                    for (k, v) in b {
                        row.insert(k.clone(), v.clone());
                    }
                }
                return json_answer(200, json!({ "result": row }));
            }
            ("DELETE", _) => {
                return Answer {
                    status: 204,
                    body: Body::None,
                    headers: Vec::new(),
                };
            }
            _ => {}
        }
    }

    let body_field = |k: &str| {
        req.body
            .as_ref()
            .and_then(|b| b.get(k))
            .cloned()
            .unwrap_or(Value::Null)
    };
    if path == "/api/now/ui/concoursepicker/updateset" {
        state.picker_update_set.get_or_insert_with(
            || json!({"sysId": sys_id("picker", 1), "name": "Golden current update set"}),
        );
        if method == "PUT" {
            // `{ sysId: undefined }` serializes without the key.
            let mut cur = Map::new();
            for k in ["sysId", "name"] {
                let v = body_field(k);
                if !v.is_null() || req.body.as_ref().and_then(|b| b.get(k)).is_some() {
                    cur.insert(k.into(), v);
                }
            }
            state.picker_update_set = Some(Value::Object(cur));
            return json_answer(200, json!({"result": {"ok": true}}));
        }
        return json_answer(
            200,
            json!({"result": {"current": state.picker_update_set.clone()}}),
        );
    }
    if path == "/api/now/ui/concoursepicker/application" {
        // `??=` also replaces a null/undefined left by an earlier PUT.
        if matches!(state.picker_app, None | Some(Value::Null)) {
            state.picker_app = Some(Value::String("global".into()));
        }
        if method == "PUT" {
            state.picker_app = Some(body_field("app_id"));
            return json_answer(200, json!({"result": {"ok": true}}));
        }
        let current = state.picker_app.clone().unwrap_or(Value::Null);
        return json_answer(
            200,
            json!({"result": {"current": current, "list": app_list()}}),
        );
    }

    json_answer(
        404,
        json!({"error": {"message": format!("No Processor or Table API for {method} {path}"), "detail": "golden stub: unrouted"}, "status": "failure"}),
    )
}

fn app_list() -> Value {
    json!([{"sysId": "global", "name": "Global"}, {"sysId": sys_id("sys_app", 1), "name": "Golden App 1"}])
}

/// `DEFAULT_DOCS_ROUTES` (`scripts/golden/lib/world.mjs`).
pub const DEFAULT_DOCS_ROUTES: &[(&str, &str)] = &[
    (
        "/ServiceNow/ServiceNowDocs/main/llms.txt",
        "# ServiceNow Product Documentation\n\n- Family-to-branch mapping is:\n    - \"australia\" : \"australia\" -- australia family\n",
    ),
    (
        "/ServiceNow/ServiceNowDocs/australia/llms.txt",
        "- [Flow](platform/flow.md)\n- [Tables](platform/tables.md)\n",
    ),
    (
        "/ServiceNow/ServiceNowDocs/australia/platform/flow.md",
        "# Flow\n\nCreate flow actions and subflows.\n",
    ),
    (
        "/ServiceNow/ServiceNowDocs/australia/platform/tables.md",
        "# Tables\n\nTables store records.\n",
    ),
];

/// `startRawStub(routes)`: static route table keyed by the full request target.
#[derive(Clone)]
pub struct RawStub {
    pub port: u16,
    requests: Arc<Mutex<Vec<Described>>>,
}

impl RawStub {
    pub async fn start(routes: &'static [(&'static str, &'static str)]) -> Self {
        let requests: Arc<Mutex<Vec<Described>>> = Arc::default();
        let r = requests.clone();
        let port = http::serve(Arc::new(move |raw: RawRequest| {
            // The JS records `describeRequest(req, '')`: the body is never kept.
            let described = http::describe(&raw.method, &raw.target, &raw.headers, "");
            r.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(described);
            match routes.iter().find(|(p, _)| *p == raw.target) {
                Some((_, body)) => Reply {
                    status: 200,
                    headers: vec![("content-type".into(), "text/plain; charset=utf-8".into())],
                    body: Some(body.as_bytes().to_vec()),
                },
                None => Reply {
                    status: 404,
                    headers: vec![("content-type".into(), "text/plain".into())],
                    body: Some(b"not found".to_vec()),
                },
            }
        }))
        .await;
        Self { port, requests }
    }

    pub fn base(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    pub fn take(&self) -> Vec<Described> {
        std::mem::take(&mut *self.requests.lock().unwrap_or_else(PoisonError::into_inner))
    }
}
