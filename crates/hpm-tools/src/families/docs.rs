//! Lane E: the five docs tools (6.2.0 `src/docs/tool-definitions.js`,
//! `tool-handlers.js`) and the two resources ([`crate::resources`]).
//!
//! Every call reads the docs settings afresh (6.2.0 `getDocsConfig()` per call:
//! environment through the [`ConfigManager`] env map, then the `docs` object of the
//! instances file) and builds its own docs client, like 6.2.0 `handleDocsTool`, so
//! the approved-family index is fetched once per call. The cache directory and the
//! raw-GitHub origin come from [`FamilyDeps`], never from `std::env`.

use std::path::PathBuf;
use std::sync::Arc;

use hpm_core::config::{ConfigManager, home_dir};
use hpm_docs::{
    DocsClient, DocsConfig, DocsError, DocsLimits, DocsStore, RequestOptions, ReqwestTransport,
    SyncOptions, VectorConfig, VectorIndex, assert_docs_family_name, get_docs_config,
    normalize_docs_document_path, read_system_docs_properties, sync_docs_family,
};
use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
use hpm_types::operation::{Action, Operation, OperationSet, Subject};
use hpm_types::tool::{
    HandlerCompletion, MutationOutcome, OutputClass, ResultBody, SideEffectClass, ToolDescriptor,
    ToolError, ToolHandler,
};
use serde_json::{Map, Value, json};

use crate::families::FamilyDeps;
use crate::registration::{RegistrationError, ToolRegistry};

mod resources;
pub mod telemetry;

/// 6.2.0 `DEFAULT_DOCS_FAMILY`.
const DEFAULT_FAMILY: &str = "australia";

const SEARCH_DISABLED: &str = "Local ServiceNow docs search is disabled. Use SN-Docs-Get for direct GitHub retrieval, or set docs.localIndexEnabled=true / HAPPY_DOCS_ENABLE_LOCAL_INDEX=true and run SN-Docs-Sync.";
const SYNC_DISABLED: &str = "Local ServiceNow docs indexing is disabled. Set docs.localIndexEnabled=true in config/servicenow-instances.json or HAPPY_DOCS_ENABLE_LOCAL_INDEX=true to enable SN-Docs-Sync.";

const FAMILIES_SCHEMA: &str = r#"{"type":"object","properties":{}}"#;
const STATUS_SCHEMA: &str = r#"{"type":"object","properties":{}}"#;
const SYNC_SCHEMA: &str = r#"{"type":"object","properties":{"family":{"type":"string","description":"Docs family listed by SN-Docs-Families, such as australia. Defaults to australia."},"branch":{"type":"string","description":"Optional branch assertion. Must equal the ref approved for the family by the ServiceNowDocs main/llms.txt index; any other value is rejected."}}}"#;
const SEARCH_SCHEMA: &str = r#"{"type":"object","properties":{"query":{"type":"string","description":"Search query."},"family":{"type":"string","description":"Optional docs family filter. Defaults to australia."},"limit":{"type":"number","description":"Maximum results to return.","default":10}},"required":["query"]}"#;
const GET_SCHEMA: &str = r#"{"type":"object","properties":{"family":{"type":"string","description":"Docs family listed by SN-Docs-Families, such as australia. Defaults to australia."},"path":{"type":"string","description":"Relative .md path inside the family branch, such as markdown/<publication>/index.md. No \"..\", percent-encoding, query, fragment or absolute URLs."}},"required":["path"]}"#;

#[derive(Clone, Copy)]
enum DocsTool {
    Families,
    Status,
    Sync,
    Search,
    Get,
}

/// What the handlers share: settings sources and the sandbox overrides.
struct DocsEnv {
    config: Arc<ConfigManager>,
    /// Raw-GitHub origin override (tests, mirrors); `None`: the pinned public origin.
    base_url: Option<String>,
    /// Cache directory override; `None`: the 6.2.0 settings chain.
    cache_dir: Option<PathBuf>,
}

fn docs_error(error: &DocsError) -> ToolError {
    ToolError::Message(error.message().to_owned())
}

impl DocsEnv {
    /// 6.2.0 `getDocsConfig()`: environment, then the `docs` object of the instances
    /// file, then the defaults.
    fn config(&self) -> Result<DocsConfig, ToolError> {
        let env = self.config.env();
        let properties = read_system_docs_properties(&self.config.registry().read_path())
            .map_err(|e| docs_error(&e))?;
        let home = home_dir(env);
        let mut config = get_docs_config(&|name| env.get(name), &properties, &home);
        if let Some(dir) = &self.cache_dir {
            config.cache_dir.clone_from(dir);
        }
        Ok(config)
    }

    /// A fresh client per call (6.2.0 `createServiceNowDocsClient()`).
    fn client(&self) -> Result<DocsClient<ReqwestTransport>, ToolError> {
        let transport = match &self.base_url {
            Some(url) => ReqwestTransport::with_origin(url.trim_end_matches('/')),
            None => ReqwestTransport::new(),
        }
        .map_err(|e| docs_error(&e))?;
        Ok(DocsClient::new(transport, DocsLimits::default()))
    }
}

/// 6.2.0 `withStore`: create the cache directory, open and initialize the store, run
/// `operation`, close. SQLite is blocking, so it runs off the async workers.
async fn with_store<R, F>(config: &DocsConfig, operation: F) -> Result<R, ToolError>
where
    R: Send + 'static,
    F: FnOnce(&DocsStore) -> Result<R, DocsError> + Send + 'static,
{
    let cache_dir = config.cache_dir.clone();
    let vector = VectorConfig::from(config);
    let job = move || -> Result<R, DocsError> {
        std::fs::create_dir_all(&cache_dir).map_err(|e| DocsError::plain(e.to_string()))?;
        let mut store = DocsStore::open(&cache_dir.join("index.sqlite"), &vector)?;
        store.initialize()?;
        let result = operation(&store);
        let closed = store.close();
        let value = result?;
        closed?;
        Ok(value)
    };
    tokio::task::spawn_blocking(job)
        .await
        .map_err(|_| ToolError::Message("ServiceNow docs cache task failed".to_owned()))?
        .map_err(|e| docs_error(&e))
}

fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// `args.family || DEFAULT_DOCS_FAMILY`.
fn family_arg(args: &Value) -> &str {
    string_arg(args, "family")
        .filter(|f| !f.is_empty())
        .unwrap_or(DEFAULT_FAMILY)
}

/// `args.limit || 10` bound as the SQLite `LIMIT` (negative: unlimited, as in SQLite;
/// a fraction is a SQLite type error).
fn limit_arg(args: &Value) -> Result<usize, ToolError> {
    let Some(limit) = args.get("limit").and_then(Value::as_f64) else {
        return Ok(10);
    };
    if limit == 0.0 {
        Ok(10)
    } else if limit.fract() != 0.0 {
        Err(ToolError::Message("datatype mismatch".to_owned()))
    } else if limit < 0.0 {
        Ok(usize::MAX)
    } else {
        // Saturating float-to-int conversion.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(limit as usize)
    }
}

fn ok(payload: Value) -> HandlerCompletion {
    HandlerCompletion::new(
        MutationOutcome::NotAMutation,
        Vec::new(),
        Ok(ResultBody::Json(payload)),
    )
}

fn failed(outcome: MutationOutcome, error: ToolError) -> HandlerCompletion {
    HandlerCompletion::new(outcome, Vec::new(), Err(error))
}

impl DocsEnv {
    async fn families(&self, options: RequestOptions<'_>) -> Result<Value, ToolError> {
        let client = self.client()?;
        let families = client
            .list_families(options)
            .await
            .map_err(|e| docs_error(&e))?;
        Ok(json!({ "families": families }))
    }

    async fn status(&self) -> Result<Value, ToolError> {
        let config = self.config()?;
        // 6.2.0 reports the index state of a configuration without a database.
        let vector = VectorIndex::new(&VectorConfig::from(&config), false);
        let cache_dir = config.cache_dir.to_string_lossy().into_owned();
        if !config.local_index_enabled {
            let mut out = Map::new();
            out.insert("cacheDir".into(), json!(cache_dir));
            out.insert("localIndexEnabled".into(), json!(false));
            out.insert("ftsAvailable".into(), json!(false));
            out.insert("sqliteAvailable".into(), json!(true));
            out.insert("sqliteReason".into(), json!("Local docs index disabled"));
            out.insert("vectorAvailable".into(), json!(vector.available()));
            if let Some(reason) = vector.reason() {
                out.insert("vectorReason".into(), json!(reason));
            }
            out.insert("families".into(), json!([]));
            return Ok(Value::Object(out));
        }
        let status = with_store(&config, |store| store.status()).await?;
        let mut out = Map::new();
        out.insert("cacheDir".into(), json!(cache_dir));
        out.insert("localIndexEnabled".into(), json!(true));
        out.insert("sqliteAvailable".into(), json!(true));
        if let Value::Object(fields) = json!(status) {
            out.extend(fields);
        }
        // Spread order: the store's keys keep their place, the values come from `vector`.
        out.insert("vectorAvailable".into(), json!(vector.available()));
        match vector.reason() {
            Some(reason) => out.insert("vectorReason".into(), json!(reason)),
            None => out.remove("vectorReason"),
        };
        Ok(Value::Object(out))
    }

    async fn sync(&self, options: RequestOptions<'_>, args: &Value) -> Result<Value, ToolError> {
        let config = self.config()?;
        if !config.local_index_enabled {
            return Ok(json!({ "synced": false, "message": SYNC_DISABLED }));
        }
        let client = self.client()?;
        let result = sync_docs_family(SyncOptions {
            family: family_arg(args),
            branch: string_arg(args, "branch"),
            cache_dir: &config.cache_dir,
            client: &client,
            vector_config: VectorConfig::from(&config),
            limits: DocsLimits::default(),
            cancel: options.cancel,
        })
        .await
        .map_err(|e| docs_error(&e))?;
        Ok(json!(result))
    }

    async fn search(&self, args: &Value) -> Result<Value, ToolError> {
        let config = self.config()?;
        let query = string_arg(args, "query").unwrap_or_default().to_owned();
        let family = family_arg(args).to_owned();
        if !config.local_index_enabled {
            return Ok(json!({
                "query": query,
                "family": family,
                "results": [],
                "message": SEARCH_DISABLED,
            }));
        }
        let limit = limit_arg(args)?;
        let (q, f) = (query.clone(), family.clone());
        let results = with_store(&config, move |store| store.search(&q, Some(&f), limit)).await?;
        let message = if results.is_empty() {
            "No local docs results found. If this family has not been synced, run SN-Docs-Sync first."
        } else {
            "Found locally indexed ServiceNow documentation results."
        };
        Ok(json!({
            "query": query,
            "family": family,
            "results": results,
            "message": message,
        }))
    }

    async fn get(&self, options: RequestOptions<'_>, args: &Value) -> Result<Value, ToolError> {
        let config = self.config()?;
        // Validate before any cache or network use.
        let family = assert_docs_family_name(family_arg(args)).map_err(|e| docs_error(&e))?;
        let path = normalize_docs_document_path(string_arg(args, "path").unwrap_or_default())
            .map_err(|e| docs_error(&e))?;

        if config.local_index_enabled {
            let (f, p) = (family.to_owned(), path.to_owned());
            let document = with_store(&config, move |store| store.get_document(&f, &p)).await?;
            if let Some(document) = document {
                return Ok(json!({ "source": "local-cache", "document": document }));
            }
        }

        let client = self.client()?;
        let branch = client
            .resolve_family(family, options)
            .await
            .map_err(|e| docs_error(&e))?
            .branch;
        let markdown = client
            .get_markdown(family, path, options)
            .await
            .map_err(|e| docs_error(&e))?;
        Ok(json!({
            "source": "github",
            "document": {
                "family": family,
                "branch": branch,
                "path": path,
                "markdown": markdown,
            },
        }))
    }
}

fn request_options(ctx: &RequestContext) -> RequestOptions<'_> {
    RequestOptions {
        cancel: Some(ctx.cancel()),
        budget: None,
    }
}

struct DocsHandler {
    tool: DocsTool,
    env: Arc<DocsEnv>,
}

impl ToolHandler for DocsHandler {
    fn call<'a>(&'a self, ctx: &'a RequestContext, args: Value) -> BoxFut<'a, HandlerCompletion> {
        Box::pin(async move {
            let env = &*self.env;
            let result = match self.tool {
                DocsTool::Families => env.families(request_options(ctx)).await,
                DocsTool::Status => env.status().await,
                DocsTool::Sync => env.sync(request_options(ctx), &args).await,
                DocsTool::Search => env.search(&args).await,
                DocsTool::Get => env.get(request_options(ctx), &args).await,
            };
            match (self.tool, result) {
                (DocsTool::Sync, Ok(payload)) => {
                    // A disabled index changed nothing; an enabled sync rewrote the cache.
                    let synced = payload.get("synced") != Some(&Value::Bool(false));
                    let outcome = if synced {
                        MutationOutcome::Committed
                    } else {
                        MutationOutcome::NotAMutation
                    };
                    HandlerCompletion::new(outcome, Vec::new(), Ok(ResultBody::Json(payload)))
                }
                // Errors before or during a sync report as ordinary failures: a partly
                // written cache is a cache, never a ServiceNow record (6.2.0 text kept).
                (DocsTool::Sync, Err(e)) => failed(MutationOutcome::Failed, e),
                (_, Ok(payload)) => ok(payload),
                (_, Err(e)) => failed(MutationOutcome::NotAMutation, e),
            }
        })
    }
}

/// 6.2.0 `JSON.stringify(payload, null, 2)`.
fn render(body: &ResultBody) -> String {
    match body {
        ResultBody::Json(v) | ResultBody::Schema(v) => {
            serde_json::to_string_pretty(v).unwrap_or_default()
        }
        ResultBody::Message(text) => text.clone(),
        _ => String::new(),
    }
}

fn metadata_ops(name: &'static str) -> Result<OperationSet, ToolError> {
    Ok(OperationSet(vec![Operation::new(
        Subject::Tool(name),
        Action::Metadata,
    )]))
}

pub fn register(registry: &mut ToolRegistry, deps: &FamilyDeps) -> Result<(), RegistrationError> {
    let env = Arc::new(DocsEnv {
        config: deps.config.clone(),
        base_url: deps.docs_base_url.clone(),
        cache_dir: deps.docs_cache_dir.clone(),
    });
    let handler = |tool| -> Arc<dyn ToolHandler> {
        Arc::new(DocsHandler {
            tool,
            env: env.clone(),
        })
    };

    registry.register_tool(ToolDescriptor::new(
        "SN-Docs-Families",
        "List available ServiceNow documentation families/releases from the official ServiceNowDocs GitHub repository.",
        FAMILIES_SCHEMA,
        |_, _| metadata_ops("SN-Docs-Families"),
        handler(DocsTool::Families),
        render,
    ))?;
    registry.register_tool(
        ToolDescriptor::new(
            "SN-Docs-Status",
            "Show local ServiceNow docs cache, FTS index, and optional vector index status.",
            STATUS_SCHEMA,
            |_, _| metadata_ops("SN-Docs-Status"),
            handler(DocsTool::Status),
            render,
        )
        .output(OutputClass::Schema),
    )?;
    registry.register_tool(
        ToolDescriptor::new(
            "SN-Docs-Sync",
            "Download and index a ServiceNowDocs family into the local SQLite FTS cache.",
            SYNC_SCHEMA,
            |_, _| {
                // Rewrites the local docs cache: a local-disk mutation, no instance.
                Ok(OperationSet(vec![Operation::new(
                    Subject::Tool("SN-Docs-Sync"),
                    Action::Update,
                )]))
            },
            handler(DocsTool::Sync),
            render,
        )
        .access(hpm_types::tool::AccessClass::Write)
        .output(OutputClass::Schema)
        .side_effect(SideEffectClass::Local),
    )?;
    registry.register_tool(ToolDescriptor::new(
        "SN-Docs-Search",
        "Search locally synced ServiceNow documentation using SQLite FTS, with optional vector search when enabled.",
        SEARCH_SCHEMA,
        |_, _| metadata_ops("SN-Docs-Search"),
        handler(DocsTool::Search),
        render,
    ))?;
    registry.register_tool(ToolDescriptor::new(
        "SN-Docs-Get",
        "Retrieve a ServiceNow documentation markdown document by family and path.",
        GET_SCHEMA,
        |_, _| metadata_ops("SN-Docs-Get"),
        handler(DocsTool::Get),
        render,
    ))?;
    resources::register(registry, deps)
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use hpm_core::config::Env;
    use hpm_core::registry::InstanceRegistry;
    use hpm_docs::{Chunk, DocumentRecord};
    use tempfile::TempDir;

    use super::*;

    struct Fixture {
        _dir: TempDir,
        env: DocsEnv,
        cache: PathBuf,
    }

    fn fixture(enabled: bool) -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = dir.path().join("cache");
        let registry_path = dir.path().join("instances.json");
        let home = dir.path().to_string_lossy().into_owned();
        let pairs = [
            ("HOME", home.as_str()),
            ("USERPROFILE", home.as_str()),
            ("HAPPY_DOCS_CACHE_DIR", &*cache.to_string_lossy()),
            (
                "HAPPY_DOCS_ENABLE_LOCAL_INDEX",
                if enabled { "true" } else { "false" },
            ),
        ];
        let env = Env::from_pairs(&pairs);
        let registry = Arc::new(InstanceRegistry::at_paths(
            registry_path.clone(),
            registry_path,
            env.clone(),
        ));
        Fixture {
            _dir: dir,
            env: DocsEnv {
                config: Arc::new(ConfigManager::new(registry, env)),
                base_url: None,
                cache_dir: None,
            },
            cache,
        }
    }

    fn seed(cache: &std::path::Path) {
        std::fs::create_dir_all(cache).unwrap();
        let mut store =
            DocsStore::open(&cache.join("index.sqlite"), &VectorConfig::default()).unwrap();
        store.initialize().unwrap();
        store.begin_family_sync("australia", "zurich").unwrap();
        let path = "markdown/flow/designer.md";
        let document = DocumentRecord {
            family: "australia".into(),
            branch: "zurich".into(),
            path: path.into(),
            sha: None,
            title: "Flow Designer".into(),
            markdown: "# Flow Designer\nBuild flows with triggers.".into(),
        };
        let chunk = Chunk {
            family: "australia".into(),
            path: path.into(),
            title: "Flow Designer".into(),
            heading: "Flow Designer".into(),
            start_line: 1,
            end_line: 2,
            body: "Build flows with triggers.".into(),
        };
        store.replace_document(&document, &[chunk]).unwrap();
        store
            .complete_family_sync(
                "australia",
                "zurich",
                "2026-01-15T12:00:00.000Z",
                &HashSet::from([path.to_owned()]),
            )
            .unwrap();
        store.close().unwrap();
    }

    fn message(result: Result<Value, ToolError>) -> String {
        match result {
            Err(ToolError::Message(text)) => text,
            other => panic!("expected a message error, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_validates_family_and_path_before_any_cache_or_network_use() {
        let f = fixture(true);
        let options = RequestOptions::default();
        let family = message(
            f.env
                .get(options, &json!({"family": "a/b", "path": "x.md"}))
                .await,
        );
        assert_eq!(
            family,
            assert_docs_family_name("a/b").unwrap_err().message()
        );
        let path = message(f.env.get(options, &json!({"path": "../x.md"})).await);
        assert_eq!(
            path,
            normalize_docs_document_path("../x.md")
                .unwrap_err()
                .message()
        );
        // Neither call touched the cache.
        assert!(!f.cache.exists());
    }

    #[tokio::test]
    async fn get_serves_a_synced_document_from_the_local_cache() {
        let f = fixture(true);
        seed(&f.cache);
        let got = f
            .env
            .get(
                RequestOptions::default(),
                &json!({"path": "markdown/flow/designer.md"}),
            )
            .await
            .unwrap();
        assert_eq!(got["source"], "local-cache");
        assert_eq!(got["document"]["family"], "australia");
        assert_eq!(got["document"]["branch"], "zurich");
        assert_eq!(got["document"]["title"], "Flow Designer");
    }

    #[tokio::test]
    async fn search_reports_disabled_empty_and_found() {
        let disabled = fixture(false);
        let out = disabled
            .env
            .search(&json!({"query": "flow"}))
            .await
            .unwrap();
        assert_eq!(out["results"], json!([]));
        assert_eq!(out["family"], "australia");
        assert_eq!(out["message"], SEARCH_DISABLED);

        let f = fixture(true);
        let out = f
            .env
            .search(&json!({"query": "flow", "family": "other"}))
            .await
            .unwrap();
        assert_eq!(out["family"], "other");
        assert_eq!(
            out["message"],
            "No local docs results found. If this family has not been synced, run SN-Docs-Sync first."
        );

        seed(&f.cache);
        let out = f
            .env
            .search(&json!({"query": "triggers", "limit": 5}))
            .await
            .unwrap();
        assert_eq!(
            out["message"],
            "Found locally indexed ServiceNow documentation results."
        );
        assert_eq!(out["results"][0]["path"], "markdown/flow/designer.md");
        assert!(
            out["results"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("<mark>triggers</mark>")
        );
    }

    #[tokio::test]
    async fn sync_when_disabled_changes_nothing_and_says_how_to_enable() {
        let f = fixture(false);
        let out = f
            .env
            .sync(RequestOptions::default(), &json!({}))
            .await
            .unwrap();
        assert_eq!(out, json!({"synced": false, "message": SYNC_DISABLED}));
        assert!(!f.cache.exists());
    }

    #[tokio::test]
    async fn status_reflects_the_configuration_and_synced_families() {
        let disabled = fixture(false);
        let out = disabled.env.status().await.unwrap();
        let keys: Vec<_> = out
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            keys[..5],
            [
                "cacheDir",
                "localIndexEnabled",
                "ftsAvailable",
                "sqliteAvailable",
                "sqliteReason"
            ]
        );
        assert_eq!(out["families"], json!([]));

        let f = fixture(true);
        seed(&f.cache);
        let out = f.env.status().await.unwrap();
        assert_eq!(out["localIndexEnabled"], true);
        assert_eq!(out["ftsAvailable"], true);
        assert_eq!(out["families"][0]["name"], "australia");
        assert_eq!(out["families"][0]["branch"], "zurich");
        assert!(out.get("sqliteReason").is_none());
    }

    #[test]
    fn limit_defaults_to_ten_and_rejects_fractions() {
        assert_eq!(limit_arg(&json!({})).unwrap(), 10);
        assert_eq!(limit_arg(&json!({"limit": 0})).unwrap(), 10);
        assert_eq!(limit_arg(&json!({"limit": 3})).unwrap(), 3);
        assert!(limit_arg(&json!({"limit": 2.5})).is_err());
    }
}
