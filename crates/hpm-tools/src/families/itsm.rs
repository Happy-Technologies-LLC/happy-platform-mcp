//! Lane B: ITSM, catalog, core listings (23 tools). Ports the 6.2.0 handlers in
//! `src/mcp-server-consolidated.js` (listings 1795-1975, ITSM operations
//! 2815-3272, catalog 3275-3330, `SN-Validate-Configuration` 2593) and the
//! catalog client methods of `src/servicenow-client.js` they call.
//!
//! Every handler runs on the client pinned to the call's credential binding
//! (`FamilyDeps::servicenow`); multi-step flows (lookup → update, catalog detail)
//! never re-resolve the instance between steps.

use std::sync::Arc;

use hpm_core::client::http::Method;
use hpm_core::client::{ClientError, PinnedClient, RecordParams, RecordQuery, path_segment};
use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
use hpm_types::credential::CredentialNeed;
use hpm_types::operation::{Action, Operation, OperationSet, Subject, is_script_value};
use hpm_types::servicenow::ServiceNowAccess;
use hpm_types::tool::{
    AccessClass, HandlerCompletion, MutationOutcome, NormalizeInput, Normalizer, OutputClass,
    ResultBody, RetryEligibility, SideEffectClass, ToolDescriptor, ToolError, ToolHandler,
};
use serde_json::{Map, Value};

use crate::families::FamilyDeps;
use crate::logic::js::{now_millis, to_iso_string, utf16_len};
use crate::registration::{RegistrationError, ToolRegistry};

mod catalog;
mod js_out;
mod ops;
mod schemas;

use crate::families::common::failed_outcome;
use crate::logic::encoded_query::{QueryInfo, analyze_query, query_value};
use js_out::{field_list, param, pretty, prop, tpl, truthy};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    ListIncidents,
    CreateIncident,
    GetIncident,
    ListSysUsers,
    ListCmdbCis,
    ListSysUserGroups,
    ListChangeRequests,
    ListProblems,
    ValidateConfiguration,
    AddComment,
    AddWorkNotes,
    AssignIncident,
    ResolveIncident,
    CloseIncident,
    AddChangeComment,
    AssignChange,
    ApproveChange,
    AddProblemComment,
    CloseProblem,
    CatalogGetItem,
    CatalogSearchItems,
    CatalogSubmit,
    CatalogGetCategories,
}

/// Same order as [`schemas::TOOLS`].
const KINDS: [Kind; 23] = [
    Kind::ListIncidents,
    Kind::CreateIncident,
    Kind::GetIncident,
    Kind::ListSysUsers,
    Kind::ListCmdbCis,
    Kind::ListSysUserGroups,
    Kind::ListChangeRequests,
    Kind::ListProblems,
    Kind::ValidateConfiguration,
    Kind::AddComment,
    Kind::AddWorkNotes,
    Kind::AssignIncident,
    Kind::ResolveIncident,
    Kind::CloseIncident,
    Kind::AddChangeComment,
    Kind::AssignChange,
    Kind::ApproveChange,
    Kind::AddProblemComment,
    Kind::CloseProblem,
    Kind::CatalogGetItem,
    Kind::CatalogSearchItems,
    Kind::CatalogSubmit,
    Kind::CatalogGetCategories,
];

/// D-05 shape of a tool.
enum Class {
    /// Read; output class.
    Read(OutputClass),
    /// Instance-data write; retry eligibility.
    Write(RetryEligibility),
}

impl Kind {
    fn class(self) -> Class {
        use Kind::*;
        match self {
            ListIncidents | GetIncident | ListSysUsers | ListCmdbCis | ListSysUserGroups
            | ListChangeRequests | ListProblems => Class::Read(OutputClass::Records),
            ValidateConfiguration | CatalogGetItem | CatalogSearchItems | CatalogGetCategories => {
                Class::Read(OutputClass::Schema)
            }
            CreateIncident | AddComment | AddWorkNotes | AddChangeComment | AddProblemComment
            | CatalogSubmit => Class::Write(RetryEligibility::Never),
            AssignIncident | ResolveIncident | CloseIncident | AssignChange | ApproveChange
            | CloseProblem => Class::Write(RetryEligibility::BeforeSendOnly),
        }
    }

    fn mutates(self) -> bool {
        matches!(self.class(), Class::Write(_))
    }

    fn normalizer(self) -> Normalizer {
        use Kind::*;
        macro_rules! n {
            ($k:ident) => {
                |a, i| normalize($k, a, i)
            };
        }
        match self {
            ListIncidents => n!(ListIncidents),
            CreateIncident => n!(CreateIncident),
            GetIncident => n!(GetIncident),
            ListSysUsers => n!(ListSysUsers),
            ListCmdbCis => n!(ListCmdbCis),
            ListSysUserGroups => n!(ListSysUserGroups),
            ListChangeRequests => n!(ListChangeRequests),
            ListProblems => n!(ListProblems),
            ValidateConfiguration => n!(ValidateConfiguration),
            AddComment => n!(AddComment),
            AddWorkNotes => n!(AddWorkNotes),
            AssignIncident => n!(AssignIncident),
            ResolveIncident => n!(ResolveIncident),
            CloseIncident => n!(CloseIncident),
            AddChangeComment => n!(AddChangeComment),
            AssignChange => n!(AssignChange),
            ApproveChange => n!(ApproveChange),
            AddProblemComment => n!(AddProblemComment),
            CloseProblem => n!(CloseProblem),
            CatalogGetItem => n!(CatalogGetItem),
            CatalogSearchItems => n!(CatalogSearchItems),
            CatalogSubmit => n!(CatalogSubmit),
            CatalogGetCategories => n!(CatalogGetCategories),
        }
    }
}

pub fn register(registry: &mut ToolRegistry, deps: &FamilyDeps) -> Result<(), RegistrationError> {
    for (kind, (name, description, schema)) in KINDS.iter().zip(schemas::TOOLS) {
        let handler = Arc::new(Handler {
            kind: *kind,
            servicenow: deps.servicenow.clone(),
        });
        let tool = ToolDescriptor::new(
            name,
            description,
            schema,
            kind.normalizer(),
            handler,
            render,
        )
        .needs_instance(true)
        .credentials(CredentialNeed::Rest);
        let tool = match kind.class() {
            Class::Read(output) => tool
                .access(AccessClass::Read)
                .output(output)
                .retry(RetryEligibility::Idempotent)
                .side_effect(SideEffectClass::None),
            Class::Write(retry) => tool
                .access(AccessClass::Write)
                .output(OutputClass::Records)
                .retry(retry)
                .side_effect(SideEffectClass::Remote),
        };
        registry.register_tool(tool)?;
    }
    Ok(())
}

/// Handlers return the finished 6.2.0 text.
fn render(body: &ResultBody) -> String {
    match body {
        ResultBody::ToolResult { text, .. } | ResultBody::Message(text) => text.clone(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// ServiceNow seam

/// The ServiceNow calls the handlers make; [`PinnedClient`] in production, a
/// scripted fake in the unit tests.
trait Sn: Sync {
    fn live(&self) -> bool;
    fn records(
        &self,
        table: &str,
        query: &RecordQuery,
    ) -> impl Future<Output = Result<Value, ClientError>> + Send;
    fn record(
        &self,
        table: &str,
        sys_id: &str,
        params: &RecordParams,
    ) -> impl Future<Output = Result<Value, ClientError>> + Send;
    fn create(
        &self,
        table: &str,
        data: &Value,
    ) -> impl Future<Output = Result<Value, ClientError>> + Send;
    fn update(
        &self,
        table: &str,
        sys_id: &str,
        data: &Value,
    ) -> impl Future<Output = Result<Value, ClientError>> + Send;
    /// POST returning the whole response body (`response.data`).
    fn post(
        &self,
        path: &str,
        body: &Value,
    ) -> impl Future<Output = Result<Value, ClientError>> + Send;
}

struct Live<'a> {
    client: PinnedClient,
    ctx: &'a RequestContext,
}

impl Sn for Live<'_> {
    fn live(&self) -> bool {
        self.ctx.is_live()
    }
    async fn records(&self, table: &str, query: &RecordQuery) -> Result<Value, ClientError> {
        self.client.get_records(table, query).await
    }
    async fn record(
        &self,
        table: &str,
        sys_id: &str,
        params: &RecordParams,
    ) -> Result<Value, ClientError> {
        self.client.get_record(table, sys_id, params).await
    }
    async fn create(&self, table: &str, data: &Value) -> Result<Value, ClientError> {
        self.client.create_record(table, data).await
    }
    async fn update(&self, table: &str, sys_id: &str, data: &Value) -> Result<Value, ClientError> {
        self.client.update_record(table, sys_id, data).await
    }
    async fn post(&self, path: &str, body: &Value) -> Result<Value, ClientError> {
        self.client.rest(Method::Post, path, Some(body)).await
    }
}

/// A handler failure: cancelled before a request, a client error, or a 6.2.0
/// thrown `Error` (`Error: {message}`).
#[derive(Debug)]
enum Fail {
    Cancelled,
    Client(ClientError),
    Js(String),
}

impl Fail {
    /// `error.message` as 6.2.0 catch blocks see it.
    fn message(&self) -> String {
        match self {
            Fail::Cancelled => "cancelled".to_owned(),
            Fail::Client(e) => e.message.clone(),
            Fail::Js(m) => m.clone(),
        }
    }
}

impl From<Fail> for ToolError {
    fn from(f: Fail) -> Self {
        match f {
            Fail::Cancelled => ToolError::Cancelled,
            Fail::Client(e) => e.into(),
            Fail::Js(m) => ToolError::Message(m),
        }
    }
}

/// One call: checks liveness before every request and tracks the mutation outcome.
struct Call<'a, S> {
    sn: &'a S,
    outcome: MutationOutcome,
}

impl<S: Sn> Call<'_, S> {
    fn check(&self) -> Result<(), Fail> {
        if self.sn.live() {
            Ok(())
        } else {
            Err(Fail::Cancelled)
        }
    }

    async fn records(&self, table: &str, query: RecordQuery) -> Result<Value, Fail> {
        self.check()?;
        self.sn.records(table, &query).await.map_err(Fail::Client)
    }

    async fn record(&self, table: &str, sys_id: &str, params: RecordParams) -> Result<Value, Fail> {
        self.check()?;
        self.sn
            .record(table, sys_id, &params)
            .await
            .map_err(Fail::Client)
    }

    /// The one mutation of a call; sets the outcome from how it ended.
    async fn mutate(
        &mut self,
        fut: impl Future<Output = Result<Value, ClientError>>,
    ) -> Result<Value, Fail> {
        self.check()?;
        match fut.await {
            Ok(v) => {
                self.outcome = MutationOutcome::Committed;
                Ok(v)
            }
            Err(e) => {
                self.outcome = failed_outcome(&e);
                Err(Fail::Client(e))
            }
        }
    }

    /// A write whose path failed to build: nothing was sent.
    fn not_sent(&mut self, e: ClientError) -> Fail {
        self.outcome = MutationOutcome::NotSent;
        Fail::Client(e)
    }
}

fn query(q: impl Into<String>, limit: &str) -> RecordQuery {
    RecordQuery {
        query: Some(q.into()),
        limit: Some(limit.to_owned()),
        ..RecordQuery::default()
    }
}

fn iso_now() -> String {
    to_iso_string(now_millis()).unwrap_or_default()
}

/// `!list || list.length === 0` → `not_found`; else `list[0]`.
fn first(list: &Value, not_found: impl FnOnce() -> String) -> Result<Value, Fail> {
    match list {
        Value::Null => Err(Fail::Js(not_found())),
        Value::Array(a) => match a.first() {
            None => Err(Fail::Js(not_found())),
            Some(Value::Null) => Err(Fail::Js(
                "Cannot read properties of null (reading 'sys_id')".to_owned(),
            )),
            Some(v) => Ok(v.clone()),
        },
        Value::String(s) if s.is_empty() => Err(Fail::Js(not_found())),
        Value::Bool(false) => Err(Fail::Js(not_found())),
        _ => Err(Fail::Js(
            "Cannot read properties of undefined (reading 'sys_id')".to_owned(),
        )),
    }
}

struct Handler {
    kind: Kind,
    servicenow: Arc<dyn ServiceNowAccess<Client = PinnedClient>>,
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, ctx: &'a RequestContext, args: Value) -> BoxFut<'a, HandlerCompletion> {
        Box::pin(async move {
            let client = match self.servicenow.client(ctx).await {
                Ok(c) => c,
                Err(e) => {
                    let outcome = if self.kind.mutates() {
                        MutationOutcome::NotSent
                    } else {
                        MutationOutcome::NotAMutation
                    };
                    return HandlerCompletion::new(outcome, Vec::new(), Err(e));
                }
            };
            run(self.kind, &Live { client, ctx }, &args).await
        })
    }
}

/// Arguments a tool interpolates into an encoded query (owner decision on
/// review B M3): each must stay one operand, so `^` and control characters
/// are refused before any request.
fn query_args(kind: Kind) -> &'static [&'static str] {
    use Kind::*;
    match kind {
        AddComment | AddWorkNotes | ResolveIncident | CloseIncident => &["incident_number"],
        AssignIncident => &["incident_number", "assigned_to", "assignment_group"],
        AddChangeComment | ApproveChange => &["change_number"],
        AssignChange => &["change_number", "assigned_to", "assignment_group"],
        AddProblemComment | CloseProblem => &["problem_number"],
        CatalogGetItem => &["sys_id"],
        CatalogSearchItems => &["keyword", "category"],
        ValidateConfiguration => &["catalog_item"],
        _ => &[],
    }
}

fn guard_query_args(kind: Kind, args: &Value) -> Result<(), ToolError> {
    for name in query_args(kind) {
        if let Some(Value::String(v)) = args.get(*name) {
            query_value(name, v)?;
        }
    }
    Ok(())
}

async fn run<S: Sn>(kind: Kind, sn: &S, args: &Value) -> HandlerCompletion {
    if let Err(e) = guard_query_args(kind, args) {
        let outcome = if kind.mutates() {
            MutationOutcome::NotSent
        } else {
            MutationOutcome::NotAMutation
        };
        return HandlerCompletion::new(outcome, Vec::new(), Err(e));
    }
    let mut call = Call {
        sn,
        outcome: if kind.mutates() {
            MutationOutcome::Failed
        } else {
            MutationOutcome::NotAMutation
        },
    };
    let result = dispatch(kind, &mut call, args).await;
    // A cancelled call that never reached its mutation sent none.
    let outcome = match (&result, call.outcome) {
        (Err(Fail::Cancelled), MutationOutcome::Failed) => MutationOutcome::NotSent,
        (_, o) => o,
    };
    HandlerCompletion::new(
        outcome,
        Vec::new(),
        result
            .map(|text| ResultBody::ToolResult {
                text,
                explicit_is_error_false: false,
            })
            .map_err(ToolError::from),
    )
}

async fn dispatch<S: Sn>(kind: Kind, call: &mut Call<'_, S>, args: &Value) -> Result<String, Fail> {
    use Kind::*;
    match kind {
        ListIncidents | ListSysUsers | ListCmdbCis | ListSysUserGroups | ListChangeRequests
        | ListProblems => list(kind, call, args).await,
        CreateIncident => {
            let data = without_instance(args);
            let result = call.mutate(call.sn.create("incident", &data)).await?;
            Ok(format!(
                "Created Incident: {}\n{}",
                tpl(prop(Some(&result), "number")),
                pretty(Some(&result))
            ))
        }
        GetIncident => {
            let result = call
                .record(
                    "incident",
                    &tpl(args.get("sys_id")),
                    RecordParams::default(),
                )
                .await?;
            Ok(pretty(Some(&result)))
        }
        ValidateConfiguration => catalog::validate(call, args).await,
        CatalogGetItem => catalog::get_item(call, args).await,
        CatalogSearchItems => catalog::search(call, args).await,
        CatalogSubmit => catalog::submit(call, args).await,
        CatalogGetCategories => catalog::categories(call, args).await,
        AddComment | AddWorkNotes | AssignIncident | ResolveIncident | CloseIncident
        | AddChangeComment | AssignChange | ApproveChange | AddProblemComment | CloseProblem => {
            ops::run(kind, call, args).await
        }
    }
}

/// `const { instance: _instance, ...data } = args`.
fn without_instance(args: &Value) -> Value {
    let mut data = args.as_object().cloned().unwrap_or_default();
    data.shift_remove("instance");
    Value::Object(data)
}

// ---------------------------------------------------------------------------
// Listings

struct Listing {
    table: &'static str,
    label: &'static str,
    /// Arguments appended as `name=value` unless the query already names them.
    filters: &'static [&'static str],
}

fn listing(kind: Kind) -> Listing {
    let (table, label, filters): (_, _, &'static [&'static str]) = match kind {
        Kind::ListIncidents => ("incident", "Incident", &["state", "priority"]),
        Kind::ListSysUsers => ("sys_user", "Sys User", &[]),
        Kind::ListCmdbCis => ("cmdb_ci", "Cmdb Ci", &[]),
        Kind::ListSysUserGroups => ("sys_user_group", "Sys User Group", &[]),
        Kind::ListChangeRequests => ("change_request", "Change Request", &["state", "type"]),
        _ => ("problem", "Problem", &[]),
    };
    Listing {
        table,
        label,
        filters,
    }
}

/// `sysparm_query` as the handler builds it.
fn list_query(l: &Listing, args: &Value) -> Option<String> {
    if l.filters.is_empty() {
        return param(args.get("query"));
    }
    let mut q = param(args.get("query")).unwrap_or_default();
    for f in l.filters {
        let v = args.get(*f);
        if truthy(v) && !q.contains(f) {
            if !q.is_empty() {
                q.push('^');
            }
            q.push_str(&format!("{f}={}", tpl(v)));
        }
    }
    (!q.is_empty()).then_some(q)
}

fn list_params(l: &Listing, args: &Value) -> RecordQuery {
    let limit = match args.get("limit") {
        None => Some("25".to_owned()),
        v => param(v),
    };
    RecordQuery {
        query: list_query(l, args),
        limit,
        fields: param(args.get("fields")),
        offset: param(args.get("offset")),
        order_by: param(args.get("order_by")),
    }
}

async fn list<S: Sn>(kind: Kind, call: &Call<'_, S>, args: &Value) -> Result<String, Fail> {
    let l = listing(kind);
    let results = call.records(l.table, list_params(&l, args)).await?;
    Ok(format!(
        "Found {} {}(s):\n{}",
        js_out::length(&results),
        l.label,
        pretty(Some(&results))
    ))
}

// ---------------------------------------------------------------------------
// Normalization

fn op(kind: Kind, action: Action, table: &str, input: &NormalizeInput<'_>) -> Operation {
    Operation::new(Subject::Tool(schemas::TOOLS[kind_index(kind)].0), action)
        .on_table(table, input.classification)
        .instance_id(input.instance.map(|i| i.instance_id.clone()))
}

fn kind_index(kind: Kind) -> usize {
    KINDS.iter().position(|k| *k == kind).unwrap_or(0)
}

/// A read with the facts of the queries it sends.
fn read(
    kind: Kind,
    table: &str,
    queries: &[&str],
    returned: Vec<String>,
    input: &NormalizeInput<'_>,
) -> Operation {
    let mut facts = QueryInfo::default();
    for q in queries {
        let info = analyze_query(q);
        // Fields are a set; every script operand is reported.
        for (into, from) in [
            (&mut facts.predicate_fields, info.predicate_fields),
            (&mut facts.ordering_fields, info.ordering_fields),
        ] {
            for v in from {
                if !into.contains(&v) {
                    into.push(v);
                }
            }
        }
        facts.script_operands.extend(info.script_operands);
    }
    op(kind, Action::Read, table, input)
        .returned_fields(returned)
        .predicate_fields(facts.predicate_fields)
        .ordering_fields(facts.ordering_fields)
        .query_script_operands(facts.script_operands)
}

/// A write of `data`'s fields; script values are reported (escalated centrally).
fn write(
    kind: Kind,
    action: Action,
    table: Option<&str>,
    data: &Value,
    input: &NormalizeInput<'_>,
) -> Operation {
    let fields: Vec<String> = data
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default();
    let scripts: Vec<String> = data
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, v)| v.as_str().is_some_and(is_script_value))
        .map(|(k, _)| k.clone())
        .collect();
    let base = match table {
        Some(t) => op(kind, action, t, input),
        None => Operation::new(Subject::Tool(schemas::TOOLS[kind_index(kind)].0), action)
            .instance_id(input.instance.map(|i| i.instance_id.clone())),
    };
    base.written_fields(fields).written_script_fields(scripts)
}

fn normalize(
    kind: Kind,
    args: &Value,
    input: &NormalizeInput<'_>,
) -> Result<OperationSet, ToolError> {
    use Kind::*;
    let ops = match kind {
        ListIncidents | ListSysUsers | ListCmdbCis | ListSysUserGroups | ListChangeRequests
        | ListProblems => {
            let l = listing(kind);
            let q = list_query(&l, args).unwrap_or_default();
            let mut o = read(kind, l.table, &[&q], field_list(args.get("fields")), input);
            for f in field_list(args.get("order_by")) {
                if !o.ordering_fields.contains(&f) {
                    o.ordering_fields.push(f);
                }
            }
            vec![o]
        }
        GetIncident => vec![read(kind, "incident", &[], Vec::new(), input)],
        CreateIncident => vec![write(
            kind,
            Action::Create,
            Some("incident"),
            &without_instance(args),
            input,
        )],
        ValidateConfiguration
        | CatalogGetItem
        | CatalogSearchItems
        | CatalogGetCategories
        | CatalogSubmit => catalog::normalize(kind, args, input),
        _ => ops::normalize(kind, args, input),
    };
    Ok(OperationSet(ops))
}

/// Shared by the catalog submodule: JS `/^[0-9a-f]{32}$/i`.
fn is_sys_id(s: &str) -> bool {
    s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `str.length` for a JSON value, if it has one (strings, arrays).
fn js_length(v: Option<&Value>) -> Option<usize> {
    match v {
        Some(Value::String(s)) => Some(utf16_len(s)),
        Some(Value::Array(a)) => Some(a.len()),
        _ => None,
    }
}

fn object(pairs: Vec<(&str, Option<Value>)>) -> Value {
    let mut m = Map::new();
    for (k, v) in pairs {
        if let Some(v) = v {
            m.insert(k.to_owned(), v);
        }
    }
    Value::Object(m)
}

/// `/api/sn_sc/servicecatalog/items/{sys_id}/order_now`.
fn order_now_path(sys_id: &str) -> Result<String, ClientError> {
    Ok(format!(
        "/api/sn_sc/servicecatalog/items/{}/order_now",
        path_segment(sys_id, "sys_id")?
    ))
}

#[cfg(test)]
mod fake;

#[cfg(test)]
mod tests {
    use super::fake::Fake;
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn list_incidents_appends_filters_unless_named_and_defaults_limit() {
        let sn = Fake::new().ok(json!([{"number": "INC1"}]));
        let done = run(
            Kind::ListIncidents,
            &sn,
            &json!({"query": "active=true", "state": "2", "priority": 1}),
        )
        .await;
        assert_eq!(
            sn.calls(),
            ["GET incident query=active=true^state=2^priority=1 limit=25"]
        );
        let text = done.result.expect("ok");
        assert!(
            matches!(text, ResultBody::ToolResult { text: t, .. } if t.starts_with("Found 1 Incident(s):\n[\n  {\n    \"number\": \"INC1\""))
        );

        let sn = Fake::new().ok(json!([]));
        run(
            Kind::ListChangeRequests,
            &sn,
            &json!({"query": "state=1", "state": "3", "type": "normal", "limit": 0, "order_by": "-number"}),
        )
        .await;
        assert_eq!(
            sn.calls(),
            ["GET change_request query=state=1^type=normal order_by=-number"]
        );
    }

    #[tokio::test]
    async fn create_incident_drops_instance_argument() {
        let sn = Fake::new().ok(json!({"number": "INC9", "sys_id": "a"}));
        let done = run(
            Kind::CreateIncident,
            &sn,
            &json!({"short_description": "x", "instance": "dev"}),
        )
        .await;
        assert_eq!(sn.calls(), [r#"POST incident {"short_description":"x"}"#]);
        assert_eq!(done.outcome, MutationOutcome::Committed);
    }

    #[test]
    fn list_normalizer_reports_query_script_operands_and_fields() {
        let snap = Default::default();
        let input = NormalizeInput::new(None, &snap);
        let ops = normalize(
            Kind::ListIncidents,
            &json!({"query": "caller_id=javascript:gs.getUserID()", "fields": "number,short_description", "order_by": "-opened_at"}),
            &input,
        )
        .expect("ops");
        let o = &ops.0[0];
        assert_eq!(o.action, Action::Read);
        assert_eq!(o.table.as_deref(), Some("incident"));
        assert_eq!(o.query_script_operands, ["gs.getUserID()"]);
        assert_eq!(o.returned_fields, ["number", "short_description"]);
        assert_eq!(o.ordering_fields, ["opened_at"]);
    }

    #[tokio::test]
    async fn query_arguments_with_caret_or_control_chars_are_refused_unsent() {
        let cases = [
            (
                Kind::AddComment,
                json!({"incident_number": "INC1^ORnumberSTARTSWITHINC", "comment": "c"}),
                "incident_number must not contain '^' (it would add encoded-query conditions)",
            ),
            (
                Kind::AssignIncident,
                json!({"incident_number": "INC1", "assigned_to": "bob\nx"}),
                "assigned_to must not contain line breaks or other control characters",
            ),
            (
                Kind::AssignChange,
                json!({"change_number": "CHG1", "assigned_to": "bob", "assignment_group": "a^NQx"}),
                "assignment_group must not contain '^' (it would add encoded-query conditions)",
            ),
            (
                Kind::CloseProblem,
                json!({"problem_number": "P^ORDERBYnumber", "resolution_notes": "n"}),
                "problem_number must not contain '^' (it would add encoded-query conditions)",
            ),
            (
                Kind::CatalogSearchItems,
                json!({"keyword": "lap^active=false"}),
                "keyword must not contain '^' (it would add encoded-query conditions)",
            ),
            (
                Kind::CatalogGetItem,
                json!({"sys_id": "a^ORactive=false"}),
                "sys_id must not contain '^' (it would add encoded-query conditions)",
            ),
            (
                Kind::ValidateConfiguration,
                json!({"catalog_item": "a\u{0}"}),
                "catalog_item must not contain line breaks or other control characters",
            ),
        ];
        for (kind, args, text) in cases {
            let sn = Fake::new();
            let done = run(kind, &sn, &args).await;
            assert!(
                matches!(&done.result, Err(ToolError::InvalidParams(m)) if m == text),
                "{kind:?}: {:?}",
                done.result
            );
            let want = if kind.mutates() {
                MutationOutcome::NotSent
            } else {
                MutationOutcome::NotAMutation
            };
            assert_eq!(done.outcome, want, "{kind:?}");
            assert!(sn.calls().is_empty(), "{kind:?}");
        }
    }

    #[tokio::test]
    async fn plain_values_pass_the_guard() {
        let sn = Fake::new().ok(json!([{"sys_id": "abc"}]));
        let done = run(
            Kind::AddComment,
            &sn,
            &json!({"incident_number": "INC 0010001", "comment": "a^b is fine here"}),
        )
        .await;
        assert_eq!(done.outcome, MutationOutcome::Committed);
        assert_eq!(
            sn.calls()[0],
            "GET incident query=number=INC 0010001 limit=1"
        );
    }

    #[test]
    fn registers_23_tools_each_bound_to_its_kind() {
        let expected = [
            (Kind::ListIncidents, "SN-List-Incidents"),
            (Kind::CreateIncident, "SN-Create-Incident"),
            (Kind::GetIncident, "SN-Get-Incident"),
            (Kind::ListSysUsers, "SN-List-SysUsers"),
            (Kind::ListCmdbCis, "SN-List-CmdbCis"),
            (Kind::ListSysUserGroups, "SN-List-SysUserGroups"),
            (Kind::ListChangeRequests, "SN-List-ChangeRequests"),
            (Kind::ListProblems, "SN-List-Problems"),
            (Kind::ValidateConfiguration, "SN-Validate-Configuration"),
            (Kind::AddComment, "SN-Add-Comment"),
            (Kind::AddWorkNotes, "SN-Add-Work-Notes"),
            (Kind::AssignIncident, "SN-Assign-Incident"),
            (Kind::ResolveIncident, "SN-Resolve-Incident"),
            (Kind::CloseIncident, "SN-Close-Incident"),
            (Kind::AddChangeComment, "SN-Add-Change-Comment"),
            (Kind::AssignChange, "SN-Assign-Change"),
            (Kind::ApproveChange, "SN-Approve-Change"),
            (Kind::AddProblemComment, "SN-Add-Problem-Comment"),
            (Kind::CloseProblem, "SN-Close-Problem"),
            (Kind::CatalogGetItem, "SN-Catalog-Get-Item"),
            (Kind::CatalogSearchItems, "SN-Catalog-Search-Items"),
            (Kind::CatalogSubmit, "SN-Catalog-Submit"),
            (Kind::CatalogGetCategories, "SN-Catalog-Get-Categories"),
        ];
        assert_eq!(KINDS.len(), schemas::TOOLS.len());
        assert_eq!(expected.len(), KINDS.len());
        for (kind, name) in expected {
            assert_eq!(schemas::TOOLS[kind_index(kind)].0, name, "{kind:?}");
        }
    }
}
