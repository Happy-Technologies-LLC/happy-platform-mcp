//! Lane A: CRUD, schema, natural-language search, batch (11 tools), ported from
//! 6.2.0 `mcp-server-consolidated.js` (defs 236-362, 669-700, 769-903; handlers
//! 1630-1794, 1976-2052, 2520-2592) and the matching `servicenow-client.js`
//! methods. Every ServiceNow request runs on the call's pinned client.

mod defs;
mod handlers;
mod json;
mod normalize;

use std::sync::Arc;

use hpm_core::client::PinnedClient;
use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
use hpm_types::credential::CredentialNeed;
use hpm_types::servicenow::ServiceNowAccess;
use hpm_types::tool::{
    AccessClass, HandlerCompletion, MutationOutcome, Normalizer, OutputClass, ResultBody,
    RetryEligibility, SideEffectClass, ToolDescriptor, ToolHandler,
};
use serde_json::Value;

use crate::families::FamilyDeps;
use crate::registration::{RegistrationError, ToolRegistry};
use handlers::{Done, Progress};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tool {
    QueryTable,
    CreateRecord,
    GetRecord,
    UpdateRecord,
    GetTableSchema,
    ListAvailableTables,
    DiscoverTableSchema,
    ExplainField,
    NaturalLanguageSearch,
    BatchCreate,
    BatchUpdate,
}

struct Handler {
    tool: Tool,
    servicenow: Arc<dyn ServiceNowAccess<Client = PinnedClient>>,
}

impl Handler {
    async fn run(&self, ctx: &RequestContext, args: Value) -> Done {
        if self.tool == Tool::ListAvailableTables {
            return handlers::list_available_tables(&args);
        }
        let client = match self.servicenow.client(ctx).await {
            Ok(c) => c,
            Err(e) => {
                let outcome = if matches!(
                    self.tool,
                    Tool::CreateRecord | Tool::UpdateRecord | Tool::BatchCreate | Tool::BatchUpdate
                ) {
                    MutationOutcome::NotSent
                } else {
                    MutationOutcome::NotAMutation
                };
                return Done {
                    outcome,
                    ops: Vec::new(),
                    result: Err(e),
                };
            }
        };
        let api: &dyn handlers::Api = &client;
        let emit = |progress: f64, total: Option<f64>, message: String| -> BoxFut<'_, ()> {
            ctx.progress().emit(progress, total, Some(message))
        };
        let cancelled = || !ctx.is_live();
        match self.tool {
            Tool::QueryTable => handlers::query_table(api, &args).await,
            Tool::CreateRecord => handlers::create_record(api, &args).await,
            Tool::GetRecord => handlers::get_record(api, &args).await,
            Tool::UpdateRecord => handlers::update_record(api, &args).await,
            Tool::GetTableSchema => handlers::get_table_schema(api, &args).await,
            Tool::ListAvailableTables => handlers::list_available_tables(&args),
            Tool::DiscoverTableSchema => handlers::discover_table_schema(api, &args).await,
            Tool::ExplainField => handlers::explain_field(api, &args).await,
            Tool::NaturalLanguageSearch => handlers::natural_language_search(api, &args).await,
            Tool::BatchCreate => {
                handlers::batch_create(api, &args, &mut Progress::new(&emit), &cancelled).await
            }
            Tool::BatchUpdate => {
                handlers::batch_update(api, &args, &mut Progress::new(&emit), &cancelled).await
            }
        }
    }
}

fn completion(done: Done) -> HandlerCompletion {
    HandlerCompletion::new(
        done.outcome,
        done.ops,
        done.result.map(|r| ResultBody::ToolResult {
            text: r.text,
            explicit_is_error_false: r.explicit_false,
        }),
    )
}

impl ToolHandler for Handler {
    fn call<'a>(&'a self, ctx: &'a RequestContext, args: Value) -> BoxFut<'a, HandlerCompletion> {
        Box::pin(async move {
            if !ctx.is_live() {
                return HandlerCompletion::new(
                    MutationOutcome::NotSent,
                    Vec::new(),
                    Err(hpm_types::tool::ToolError::Cancelled),
                );
            }
            completion(self.run(ctx, args).await)
        })
    }
}

fn render(body: &ResultBody) -> String {
    match body {
        ResultBody::ToolResult { text, .. } | ResultBody::Message(text) => text.clone(),
        other => format!("{other:?}"),
    }
}

/// D-05 class of a tool, as the registry checks it.
struct Class {
    access: AccessClass,
    output: OutputClass,
    side_effect: SideEffectClass,
    retry: RetryEligibility,
    credentials: CredentialNeed,
}

const READ: Class = Class {
    access: AccessClass::Read,
    output: OutputClass::Records,
    side_effect: SideEffectClass::None,
    retry: RetryEligibility::Idempotent,
    credentials: CredentialNeed::Rest,
};
const SCHEMA: Class = Class {
    output: OutputClass::Schema,
    ..READ
};
const LOCAL_LIST: Class = Class {
    output: OutputClass::None,
    credentials: CredentialNeed::None,
    ..READ
};
const CREATE: Class = Class {
    access: AccessClass::Write,
    side_effect: SideEffectClass::Remote,
    retry: RetryEligibility::Never,
    ..READ
};
const UPDATE: Class = Class {
    retry: RetryEligibility::BeforeSendOnly,
    ..CREATE
};

pub fn register(registry: &mut ToolRegistry, deps: &FamilyDeps) -> Result<(), RegistrationError> {
    use defs::*;
    let tools: [(
        &'static str,
        &'static str,
        &'static str,
        Tool,
        Normalizer,
        Class,
    ); 11] = [
        (
            "SN-Query-Table",
            QUERY_TABLE_DESC,
            QUERY_TABLE_SCHEMA,
            Tool::QueryTable,
            normalize::query_table,
            READ,
        ),
        (
            "SN-Create-Record",
            CREATE_RECORD_DESC,
            CREATE_RECORD_SCHEMA,
            Tool::CreateRecord,
            normalize::create_record,
            CREATE,
        ),
        (
            "SN-Get-Record",
            GET_RECORD_DESC,
            GET_RECORD_SCHEMA,
            Tool::GetRecord,
            normalize::get_record,
            READ,
        ),
        (
            "SN-Update-Record",
            UPDATE_RECORD_DESC,
            UPDATE_RECORD_SCHEMA,
            Tool::UpdateRecord,
            normalize::update_record,
            UPDATE,
        ),
        (
            "SN-Get-Table-Schema",
            GET_TABLE_SCHEMA_DESC,
            GET_TABLE_SCHEMA_SCHEMA,
            Tool::GetTableSchema,
            normalize::get_table_schema,
            SCHEMA,
        ),
        (
            "SN-List-Available-Tables",
            LIST_AVAILABLE_TABLES_DESC,
            LIST_AVAILABLE_TABLES_SCHEMA,
            Tool::ListAvailableTables,
            normalize::list_available_tables,
            LOCAL_LIST,
        ),
        (
            "SN-Discover-Table-Schema",
            DISCOVER_TABLE_SCHEMA_DESC,
            DISCOVER_TABLE_SCHEMA_SCHEMA,
            Tool::DiscoverTableSchema,
            normalize::discover_table_schema,
            SCHEMA,
        ),
        (
            "SN-Explain-Field",
            EXPLAIN_FIELD_DESC,
            EXPLAIN_FIELD_SCHEMA,
            Tool::ExplainField,
            normalize::explain_field,
            SCHEMA,
        ),
        (
            "SN-Natural-Language-Search",
            NATURAL_LANGUAGE_SEARCH_DESC,
            NATURAL_LANGUAGE_SEARCH_SCHEMA,
            Tool::NaturalLanguageSearch,
            normalize::natural_language_search,
            READ,
        ),
        (
            "SN-Batch-Create",
            BATCH_CREATE_DESC,
            BATCH_CREATE_SCHEMA,
            Tool::BatchCreate,
            normalize::batch_create,
            CREATE,
        ),
        (
            "SN-Batch-Update",
            BATCH_UPDATE_DESC,
            BATCH_UPDATE_SCHEMA,
            Tool::BatchUpdate,
            normalize::batch_update,
            UPDATE,
        ),
    ];
    for (name, description, schema, tool, normalizer, class) in tools {
        let handler = Arc::new(Handler {
            tool,
            servicenow: deps.servicenow.clone(),
        });
        registry.register_tool(
            ToolDescriptor::new(name, description, schema, normalizer, handler, render)
                .access(class.access)
                .output(class.output)
                .side_effect(class.side_effect)
                .retry(class.retry)
                .needs_instance(true)
                .credentials(class.credentials),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
