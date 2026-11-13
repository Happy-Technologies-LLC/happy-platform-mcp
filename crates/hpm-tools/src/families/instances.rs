//! Lane D: instance tools. `SN-Set-Instance` / `SN-Get-Current-Instance`
//! (6.2.0 `mcp-server-consolidated.js:215-235,1569-1629`) and
//! `SN-Register-Instance` (`instance-tools.js`).
//!
//! The "current instance" is the caller's selected default in the registry view
//! (`RegistryStore`): `SN-Set-Instance` changes that selection for later calls and
//! never moves an in-flight call's binding.

use std::path::Path;
use std::sync::Arc;

use hpm_core::config::{CodedError, resolve_path};
use hpm_core::credential::{InstanceCredentialStore, credential_ref_for};
use hpm_core::registry::{RegisterOptions, canonicalize_instance_url};
use hpm_types::BoxFut;
use hpm_types::context::RequestContext;
use hpm_types::credential::CredentialNeed;
use hpm_types::operation::{Action, Operation, OperationSet, Subject};
use hpm_types::registry::{RegistryChange, RegistryError, RegistryView};
use hpm_types::tool::{
    AccessClass, HandlerCompletion, MutationOutcome, NormalizeInput, OpCompletion, OutputClass,
    ResultBody, RetryEligibility, SideEffectClass, ToolDescriptor, ToolError, ToolHandler,
};
use serde_json::{Map, Value, json};

use crate::families::{FamilyDeps, InstanceRegistryAccess};
use crate::registration::{RegistrationError, ToolRegistry};

type Object = Map<String, Value>;

const SET: &str = "SN-Set-Instance";
const CURRENT: &str = "SN-Get-Current-Instance";
const REGISTER: &str = "SN-Register-Instance";

pub fn register(registry: &mut ToolRegistry, deps: &FamilyDeps) -> Result<(), RegistrationError> {
    registry.register_tool(
        ToolDescriptor::new(
            SET,
            "Switch to a different ServiceNow instance. Use this at the start of your session to target a specific instance (dev, test, prod, etc.). Lists available instances if no name provided.",
            include_str!("instances/SN-Set-Instance.schema.json"),
            normalize_set,
            Arc::new(SetInstance {
                config: deps.instances.clone(),
            }),
            render,
        )
        .access(AccessClass::Read)
        .output(OutputClass::Schema)
        .side_effect(SideEffectClass::Local)
        .retry(RetryEligibility::Idempotent)
        .credentials(CredentialNeed::None),
    )?;
    registry.register_tool(
        ToolDescriptor::new(
            CURRENT,
            "Get information about the currently active ServiceNow instance",
            include_str!("instances/SN-Get-Current-Instance.schema.json"),
            |_, i| Ok(op(CURRENT, Action::Read, i)),
            Arc::new(CurrentInstance {
                config: deps.instances.clone(),
            }),
            render,
        )
        .access(AccessClass::Read)
        .output(OutputClass::Schema)
        .side_effect(SideEffectClass::None)
        .retry(RetryEligibility::Idempotent)
        .credentials(CredentialNeed::None),
    )?;
    registry.register_tool(
        ToolDescriptor::new(
            REGISTER,
            "Register non-secret ServiceNow instance metadata using credentials already stored by the local CLI.",
            include_str!("instances/SN-Register-Instance.schema.json"),
            |_, i| Ok(op(REGISTER, Action::ConfigWrite, i)),
            Arc::new(RegisterInstance {
                config: deps.instances.clone(),
                credentials: deps.credentials.clone(),
                docs_only: deps.docs_only,
            }),
            render,
        )
        .access(AccessClass::Config)
        .output(OutputClass::Schema)
        .side_effect(SideEffectClass::Local)
        .retry(RetryEligibility::BeforeSendOnly)
        .credentials(CredentialNeed::None)
        // The handler refuses unknown, secret-shaped and nested fields with the
        // 6.2.0 INVALID_INSTANCE_CONFIG texts; the registry validates values.
        .handler_validates_input(true),
    )
}

fn op(name: &'static str, action: Action, input: &NormalizeInput<'_>) -> OperationSet {
    OperationSet(vec![
        Operation::new(Subject::Tool(name), action)
            .instance_id(input.instance.map(|i| i.instance_id.clone())),
    ])
}

/// Arg-dependent (D-05): no `instance_name` lists instances; a name selects one.
fn normalize_set(args: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
    let action = if set_target(args).is_some() {
        Action::ConfigWrite
    } else {
        Action::Read
    };
    Ok(op(SET, action, input))
}

/// 6.2.0 `if (!instance_name)`: a falsy value lists instances.
fn set_target(args: &Value) -> Option<String> {
    match args.get("instance_name")? {
        Value::Null | Value::Bool(false) => None,
        Value::String(s) if s.is_empty() => None,
        Value::Number(n) if n.as_f64() == Some(0.0) => None,
        Value::String(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

fn render(body: &ResultBody) -> String {
    match body {
        ResultBody::ToolResult { text, .. } => text.clone(),
        _ => String::new(),
    }
}

/// 6.2.0 `JSON.stringify(payload, null, 2)`.
fn pretty(payload: &Value) -> String {
    serde_json::to_string_pretty(payload).unwrap_or_default()
}

fn text(payload: &Value) -> Result<ResultBody, ToolError> {
    Ok(ResultBody::ToolResult {
        text: pretty(payload),
        explicit_is_error_false: false,
    })
}

fn committed(result: Result<ResultBody, ToolError>) -> HandlerCompletion {
    outcome(MutationOutcome::Committed, result)
}

fn outcome(outcome: MutationOutcome, result: Result<ResultBody, ToolError>) -> HandlerCompletion {
    HandlerCompletion::new(
        outcome,
        vec![OpCompletion {
            op_index: 0,
            outcome,
        }],
        result,
    )
}

/// `serviceNowClient.getCurrentInstance()`: the caller's selected default.
fn current_instance(view: &RegistryView) -> Value {
    let entry = view.selected_default.as_deref().and_then(|id| {
        view.instances
            .iter()
            .find(|e| e.instance_id == id || e.name == id)
    });
    match entry {
        Some(e) => json!({
            "name": e.name,
            "url": e.canonical_url,
            "authType": e.auth.get("authType").and_then(Value::as_str).unwrap_or("basic"),
        }),
        None => json!({
            "name": view.selected_default,
            "url": Value::Null,
            "authType": "basic",
        }),
    }
}

// ---------------------------------------------------------------------------
// SN-Set-Instance / SN-Get-Current-Instance

struct SetInstance {
    config: Arc<dyn InstanceRegistryAccess>,
}

impl ToolHandler for SetInstance {
    fn call<'a>(&'a self, ctx: &'a RequestContext, args: Value) -> BoxFut<'a, HandlerCompletion> {
        Box::pin(async move { self.run(ctx, &args).await })
    }
}

/// Conflicting concurrent selections retry on a fresh view this many times.
const SELECT_ATTEMPTS: usize = 3;

impl SetInstance {
    async fn run(&self, ctx: &RequestContext, args: &Value) -> HandlerCompletion {
        let Some(name) = set_target(args) else {
            return HandlerCompletion::not_a_mutation(self.list(ctx));
        };
        let instance = match self.config.get_instance(&name) {
            Ok(i) => i,
            Err(e) => {
                return outcome(MutationOutcome::NotSent, Err(ToolError::Message(e.message)));
            }
        };
        let instance_name = instance
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or(&name)
            .to_owned();
        if ctx.cancel().is_cancelled() {
            return outcome(MutationOutcome::NotSent, Err(ToolError::Cancelled));
        }
        let mut last = None;
        for _ in 0..SELECT_ATTEMPTS {
            let version = self.config.registry_version();
            let change = RegistryChange::SetPrincipalDefault {
                principal: ctx.principal().clone(),
                instance_id: instance_name.clone(),
            };
            match self.config.apply(version, change).await {
                Ok(_) => {
                    last = None;
                    break;
                }
                Err(e @ RegistryError::Conflict { .. }) => last = Some(e),
                Err(e) => {
                    return outcome(
                        MutationOutcome::Failed,
                        Err(ToolError::Message(e.to_string())),
                    );
                }
            }
        }
        if let Some(e) = last {
            return outcome(
                MutationOutcome::Failed,
                Err(ToolError::Message(e.to_string())),
            );
        }
        let url = instance.get("url").cloned().unwrap_or(Value::Null);
        eprintln!(
            "🔄 Switched to instance: {instance_name} ({})",
            url.as_str().unwrap_or_default()
        );
        let mut summary = Object::new();
        summary.insert("name".into(), instance_name.clone().into());
        summary.insert("url".into(), url);
        if let Some(d) = instance.get("description") {
            summary.insert("description".into(), d.clone());
        }
        committed(text(&json!({
            "success": true,
            "message": format!("Switched to ServiceNow instance: {instance_name}"),
            "instance": summary,
        })))
    }

    fn list(&self, ctx: &RequestContext) -> Result<ResultBody, ToolError> {
        let instances = self
            .config
            .list_instances()
            .map_err(|e| ToolError::Message(e.message))?;
        let view = self.config.snapshot_for(ctx.principal());
        let instances: Vec<Value> = instances
            .into_iter()
            .map(|i| {
                json!({
                    "name": i.name,
                    "url": i.url,
                    "default": i.default,
                    "description": i.description,
                })
            })
            .collect();
        text(&json!({
            "message": "Available ServiceNow instances",
            "current_instance": current_instance(&view),
            "instances": instances,
        }))
    }
}

struct CurrentInstance {
    config: Arc<dyn InstanceRegistryAccess>,
}

impl ToolHandler for CurrentInstance {
    fn call<'a>(&'a self, ctx: &'a RequestContext, _: Value) -> BoxFut<'a, HandlerCompletion> {
        Box::pin(async move {
            let view = self.config.snapshot_for(ctx.principal());
            let current = current_instance(&view);
            let label = |k: &str| match current.get(k) {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Null) | None => "undefined".to_owned(),
                Some(other) => other.to_string(),
            };
            let message = format!(
                "Currently connected to: {} ({})",
                label("name"),
                label("url")
            );
            HandlerCompletion::not_a_mutation(text(&json!({
                "current_instance": current,
                "message": message,
            })))
        })
    }
}

// ---------------------------------------------------------------------------
// SN-Register-Instance (`instance-tools.js`)

const ALLOWED_FIELDS: [&str; 13] = [
    "name",
    "url",
    "authType",
    "grantType",
    "username",
    "clientId",
    "scope",
    "authorizeUrl",
    "tokenUrl",
    "redirectPort",
    "callbackPath",
    "description",
    "makeDefault",
];

/// `/(?:password|secret|token|credential|api[_-]?key|private[_-]?key)/i`.
fn is_secret_key(key: &str) -> bool {
    let k = key.to_lowercase();
    ["password", "secret", "token", "credential"]
        .iter()
        .any(|p| k.contains(p))
        || ["api", "private"].iter().any(|p| {
            ["key", "_key", "-key"]
                .iter()
                .any(|s| k.contains(&format!("{p}{s}")))
        })
}

/// `/(?:password|secret|token|private|api[_-]?key)/i` (registry detail filter).
fn is_secret_detail(value: &str) -> bool {
    let k = value.to_lowercase();
    [
        "password", "secret", "token", "private", "apikey", "api_key", "api-key",
    ]
    .iter()
    .any(|p| k.contains(p))
}

fn error_text(code: &str, message: &str, details: Object) -> String {
    let mut payload = Object::new();
    payload.insert("success".into(), false.into());
    payload.insert("code".into(), code.into());
    payload.insert("message".into(), message.into());
    payload.extend(details);
    pretty(&Value::Object(payload))
}

fn error_response(code: &str, message: &str, details: Object) -> ToolError {
    ToolError::Text(error_text(code, message, details))
}

fn invalid(message: &str) -> ToolError {
    error_response("INVALID_INSTANCE_CONFIG", message, Object::new())
}

fn obj(pairs: Vec<(&str, Value)>) -> Object {
    pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
}

/// `validateInputKeys`.
fn validate_input_keys(value: &Value, depth: usize) -> Result<(), ToolError> {
    match value {
        Value::Array(items) => items
            .iter()
            .try_for_each(|i| validate_input_keys(i, depth + 1)),
        Value::Object(map) => {
            for (key, nested) in map {
                let known = depth == 0 && ALLOWED_FIELDS.contains(&key.as_str());
                if !known && is_secret_key(key) {
                    return Err(invalid(
                        "Secret-shaped fields are not accepted by this tool; use the local credential command",
                    ));
                }
                if depth == 0 && !known {
                    return Err(invalid("Unknown instance metadata field"));
                }
                if depth > 0 {
                    return Err(invalid("Nested instance metadata is not supported"));
                }
                validate_input_keys(nested, depth + 1)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// One credential the auth configuration needs: `(reference, type)`.
type Requirement = (String, &'static str);

struct Canonical {
    metadata: Value,
    requirements: Vec<Requirement>,
}

/// `canonicalMetadata` + `credentialRequirements`. `Err` = `credentialRefFor` threw.
fn canonical_metadata(input: &Object) -> Result<Canonical, ()> {
    let auth_type = input
        .get("authType")
        .cloned()
        .unwrap_or_else(|| "basic".into());
    let grant_type = if auth_type == "oauth" {
        Some(input.get("grantType").cloned().unwrap_or_else(|| {
            if input.contains_key("username") {
                "password".into()
            } else {
                "client_credentials".into()
            }
        }))
    } else {
        None
    };
    let mut metadata = Object::new();
    metadata.insert(
        "name".into(),
        input.get("name").cloned().unwrap_or(Value::Null),
    );
    metadata.insert(
        "url".into(),
        input.get("url").cloned().unwrap_or(Value::Null),
    );
    metadata.insert("authType".into(), auth_type.clone());
    if let Some(g) = &grant_type {
        metadata.insert("grantType".into(), g.clone());
    }
    for field in [
        "username",
        "clientId",
        "scope",
        "authorizeUrl",
        "tokenUrl",
        "redirectPort",
        "callbackPath",
        "description",
    ] {
        if let Some(v) = input.get(field) {
            metadata.insert(field.into(), v.clone());
        }
    }
    // `credentialRefFor(input.name, …)` throws for a missing or invalid name.
    let reference = |kind: &str| {
        input
            .get("name")
            .and_then(Value::as_str)
            .and_then(|n| credential_ref_for(n, kind).ok())
            .ok_or(())
    };
    let requirements: Vec<Requirement> = if auth_type == "basic" {
        let r = reference("password")?;
        metadata.insert("credentialRef".into(), r.clone().into());
        vec![(r, "password")]
    } else if grant_type
        .as_ref()
        .is_some_and(|g| g == "client_credentials")
    {
        let r = reference("client-secret")?;
        metadata.insert("credentialRef".into(), r.clone().into());
        vec![(r, "client-secret")]
    } else if grant_type.as_ref().is_some_and(|g| g == "password") {
        let p = reference("password")?;
        let s = reference("client-secret")?;
        metadata.insert(
            "credentialRef".into(),
            json!({"password": p, "clientSecret": s}),
        );
        vec![(p, "password"), (s, "client-secret")]
    } else {
        Vec::new()
    };
    Ok(Canonical {
        metadata: Value::Object(metadata),
        requirements,
    })
}

/// `outputMetadata`: registered fields first, then the input's.
fn output_metadata(instance: &Object, input: &Object) -> Value {
    let mut output = Object::new();
    for field in ALLOWED_FIELDS {
        let from_instance = if field == "makeDefault" {
            instance.get("default")
        } else {
            instance.get(field)
        };
        if let Some(v) = from_instance.or_else(|| input.get(field)) {
            output.insert(field.into(), v.clone());
        }
    }
    Value::Object(output)
}

fn display_name(name: Option<&Value>) -> String {
    match name {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) => "null".into(),
        None => "undefined".into(),
        Some(other) => other.to_string(),
    }
}

/// The working path for a missing credential: `instance add` registers the
/// instance and prompts for its secret(s) together. (6.2.0 pointed to
/// `instance credential set`, which refuses an unregistered name.)
fn credential_commands(name: &str) -> Value {
    json!([format!("happy-platform-mcp instance add --name {name}")])
}

/// Origin of a canonical instance URL (`scheme://host[:port]`), the form the
/// credential origin binding stores.
fn url_origin(canonical_url: &str) -> &str {
    let after_scheme = canonical_url.find("://").map_or(0, |i| i + 3);
    canonical_url[after_scheme..]
        .find('/')
        .map_or(canonical_url, |i| &canonical_url[..after_scheme + i])
}

const ORIGIN_MISMATCH: &str = "CREDENTIAL_ORIGIN_MISMATCH";

fn origin_mismatch(name: &str) -> ToolError {
    error_response(
        ORIGIN_MISMATCH,
        "A credential is already stored for this name and is bound to a different instance URL; it will not be reused. Re-save it locally for the new URL first.",
        obj(vec![(
            "commands",
            json!([format!("happy-platform-mcp instance credential set {name}")]),
        )]),
    )
}

const ORIGIN_UNBOUND: &str = "CREDENTIAL_ORIGIN_UNBOUND";

/// A secret stored without an origin binding (6.x, or left behind by a
/// removed registration) is never reused for an agent-supplied URL: only the
/// operator can bind it (registered name) or replace it (`instance add`).
fn origin_unbound(name: &str) -> ToolError {
    error_response(
        ORIGIN_UNBOUND,
        "A credential is already stored for this name but is not bound to an instance URL; it will not be reused for a URL registered here. Bind or re-save it locally first.",
        obj(vec![(
            "commands",
            json!([
                format!("happy-platform-mcp instance credential bind {name}"),
                format!("happy-platform-mcp instance add --name {name}"),
            ]),
        )]),
    )
}

/// `safeRegistryDetails`.
fn safe_registry_details(error: &CodedError) -> Object {
    let mut details = Object::new();
    if let Some(field) = error.detail_str("field") {
        let mut chars = field.chars();
        let shaped = chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
        if shaped && !is_secret_detail(field) {
            details.insert("field".into(), field.into());
        }
    }
    if let Some(path) = error.detail_str("path").filter(|p| !p.is_empty()) {
        let resolved = resolve_path(Path::new(path)).to_string_lossy().into_owned();
        if !is_secret_detail(&resolved) {
            details.insert("path".into(), resolved.into());
        }
    }
    details
}

const CREDENTIAL_NOT_FOUND_TEXT: &str =
    "Required credential is missing; store it with the local credential command and retry.";
const KEYCHAIN_UNAVAILABLE_TEXT: &str =
    "Credential store unavailable; unlock or configure the local keychain and retry.";
const KEYCHAIN_FAILED_TEXT: &str =
    "Credential store operation failed; verify local keychain access and retry.";

/// `registryError`.
fn registry_error(error: &CodedError) -> ToolError {
    let code = error.code.as_str();
    match code {
        "INSTANCE_ALREADY_EXISTS" => error_response(
            code,
            "An instance with this name already exists; use the local CLI update command.",
            obj(vec![(
                "commands",
                json!([format!(
                    "happy-platform-mcp instance update {}",
                    error
                        .detail_str("name")
                        .filter(|n| !n.is_empty())
                        .unwrap_or("the-instance")
                )]),
            )]),
        ),
        "INVALID_INSTANCE_CONFIG" => error_response(
            code,
            "Instance metadata failed canonical validation",
            obj(vec![(
                "details",
                Value::Object(safe_registry_details(error)),
            )]),
        ),
        "CREDENTIAL_NOT_FOUND" => error_response(
            code,
            CREDENTIAL_NOT_FOUND_TEXT,
            safe_registry_details(error),
        ),
        "KEYCHAIN_UNAVAILABLE" => error_response(
            code,
            KEYCHAIN_UNAVAILABLE_TEXT,
            safe_registry_details(error),
        ),
        c if c.starts_with("KEYCHAIN_") => error_response(
            "KEYCHAIN_OPERATION_FAILED",
            KEYCHAIN_FAILED_TEXT,
            safe_registry_details(error),
        ),
        "REGISTRY_ROLLBACK_REQUIRED" => rollback_required(),
        ORIGIN_MISMATCH => origin_mismatch(error.detail_str("name").unwrap_or("the-instance")),
        ORIGIN_UNBOUND => origin_unbound(error.detail_str("name").unwrap_or("the-instance")),
        "REGISTRY_RELOAD_FAILED" | "REGISTRY_EMPTY" => {
            let message = if code == "REGISTRY_EMPTY" {
                "No registered instances are available. Register an instance with the local CLI, then restart the MCP server."
            } else {
                "The instance registry could not be loaded. Repair the registry file or restart the MCP server after fixing it."
            };
            let details = safe_registry_details(error);
            let mut out = Object::new();
            if let Some(p) = details.get("path") {
                out.insert("path".into(), p.clone());
            }
            error_response(code, message, out)
        }
        "REGISTRY_WRITE_FAILED" => error_response(
            code,
            "The instance registry could not be updated. No registration was committed.",
            obj(vec![(
                "details",
                Value::Object(safe_registry_details(error)),
            )]),
        ),
        "LEGACY_MIGRATION_REQUIRED" => error_response(
            code,
            "Migrate the legacy instance registry with the local CLI before registering another instance.",
            Object::new(),
        ),
        _ => error_response(
            "INSTANCE_REGISTRATION_FAILED",
            "The instance registration could not be completed.",
            Object::new(),
        ),
    }
}

fn rollback_required() -> ToolError {
    error_response(
        "REGISTRY_ROLLBACK_REQUIRED",
        "The registration could not be rolled back safely because it changed concurrently; manual rollback is required.",
        obj(vec![
            ("persisted", true.into()),
            ("partial", true.into()),
            ("rollbackRequired", true.into()),
            ("restartRequired", true.into()),
        ]),
    )
}

/// `logRegistrationDiagnostic`: code-only JSON on stderr.
fn log_diagnostic(operation: &str, code: &str, name: Option<&Value>) {
    let mut diagnostic = obj(vec![("operation", operation.into()), ("code", code.into())]);
    if let Some(n) = name.and_then(Value::as_str).filter(|n| {
        !n.is_empty()
            && n.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }) {
        diagnostic.insert("name".into(), n.into());
    }
    eprintln!("{}", Value::Object(diagnostic));
}

/// `checkCredentials`: `Err((code, response))`. A stored secret bound to
/// another origin (`CREDENTIAL_ORIGIN_MISMATCH`) or to none at all
/// (`CREDENTIAL_ORIGIN_UNBOUND`: a 6.x or orphaned secret) fails closed:
/// registering its name at a new URL would otherwise send that secret there.
async fn check_credentials(
    store: &InstanceCredentialStore,
    requirements: &[Requirement],
    name: &str,
    origin: &str,
) -> Result<(), (String, ToolError)> {
    let mut missing = false;
    for (reference, _) in requirements {
        let present = match store.has_secret(reference).await {
            Err(e) if e.code() == "CREDENTIAL_NOT_FOUND" => Ok(false),
            other => other,
        };
        // `Ok(Some(bound))`: stored and bound to `bound` (`""`: unbound).
        let binding = match present {
            Ok(true) => store
                .bound_origin(reference)
                .await
                .map(|bound| Some(bound.unwrap_or_default())),
            Ok(false) => {
                missing = true;
                Ok(None)
            }
            Err(e) => Err(e),
        };
        match binding {
            Ok(None) => {}
            Ok(Some(b)) if b == origin => {}
            Ok(Some(b)) if b.is_empty() => {
                return Err((ORIGIN_UNBOUND.to_owned(), origin_unbound(name)));
            }
            Ok(Some(_)) => return Err((ORIGIN_MISMATCH.to_owned(), origin_mismatch(name))),
            Err(e) => {
                let code = if e.code().starts_with("KEYCHAIN_") {
                    e.code()
                } else {
                    "KEYCHAIN_UNAVAILABLE"
                };
                return Err((
                    code.to_owned(),
                    error_response(
                        code,
                        "The local credential store could not be checked; no registration was committed.",
                        Object::new(),
                    ),
                ));
            }
        }
    }
    if !missing {
        return Ok(());
    }
    Err((
        "CREDENTIAL_NOT_FOUND".to_owned(),
        error_response(
            "CREDENTIAL_NOT_FOUND",
            "No credential is stored for this instance. Register it with the local CLI, which prompts for the credential(s) and saves them with the instance.",
            obj(vec![("commands", credential_commands(name))]),
        ),
    ))
}

struct RegisterInstance {
    config: Arc<dyn InstanceRegistryAccess>,
    credentials: InstanceCredentialStore,
    /// Docs-only mode: live tools load only after a restart (6.2.0 `docsOnly`).
    docs_only: bool,
}

impl ToolHandler for RegisterInstance {
    fn call<'a>(&'a self, ctx: &'a RequestContext, args: Value) -> BoxFut<'a, HandlerCompletion> {
        Box::pin(async move { self.run(&|| ctx.cancel().is_cancelled(), args).await })
    }
}

/// What `register` reported: the committed entry and the prior default.
struct Registered {
    entry: Object,
    prior_default: Option<(String, bool)>,
}

impl RegisterInstance {
    async fn run(&self, cancelled: &(dyn Fn() -> bool + Sync), args: Value) -> HandlerCompletion {
        let not_sent = |e| outcome(MutationOutcome::NotSent, Err(e));
        let input = match args {
            Value::Object(o) => o,
            _ => return not_sent(invalid("Instance metadata must be an object")),
        };
        let input_value = Value::Object(input);
        if let Err(e) = validate_input_keys(&input_value, 0) {
            return not_sent(e);
        }
        let Value::Object(input) = input_value else {
            return not_sent(invalid("Instance metadata must be an object"));
        };
        if input.get("makeDefault").is_some_and(|v| !v.is_boolean()) {
            return not_sent(invalid("makeDefault must be a boolean"));
        }
        let basic = input.get("authType").is_none_or(|a| a == "basic");
        if basic && input.contains_key("grantType") {
            return not_sent(invalid("Basic authentication must not specify grantType"));
        }
        let Ok(canonical) = canonical_metadata(&input) else {
            return not_sent(invalid("Instance metadata failed canonical validation"));
        };
        let registry = self.config.registration_registry();
        if let Err(e) = registry.validate(&canonical.metadata) {
            return not_sent(registry_error(&e));
        }
        // `canonical_metadata` succeeded only with a valid string name, or with
        // no credential requirements (authorization_code), where the registry
        // validation above rejects a missing name.
        let name = display_name(input.get("name"));
        match registry.get(&name) {
            Ok(_) => {
                return not_sent(error_response(
                    "INSTANCE_ALREADY_EXISTS",
                    "An instance with this name already exists; use the local CLI update command.",
                    obj(vec![(
                        "commands",
                        json!([format!("happy-platform-mcp instance update {name}")]),
                    )]),
                ));
            }
            Err(e) if e.code == "INSTANCE_NOT_FOUND" => {}
            Err(e) => return not_sent(registry_error(&e)),
        }
        let origin = match canonical
            .metadata
            .get("url")
            .and_then(Value::as_str)
            .map(canonicalize_instance_url)
        {
            Some(Ok(url)) => url_origin(&url).to_owned(),
            Some(Err(e)) => return not_sent(registry_error(&e)),
            None => return not_sent(invalid("Instance metadata failed canonical validation")),
        };
        if let Err((_, e)) =
            check_credentials(&self.credentials, &canonical.requirements, &name, &origin).await
        {
            return not_sent(e);
        }
        if cancelled() {
            return not_sent(ToolError::Cancelled);
        }

        let job = Commit {
            config: self.config.clone(),
            credentials: self.credentials.clone(),
            docs_only: self.docs_only,
            registry,
            canonical,
            name,
            origin,
            input,
        };
        // CR-7: the commit and its tail (credential re-check, compensation,
        // reload) run in their own task, so dropping this call future (a
        // session teardown) cannot leave a committed registration unchecked.
        match tokio::spawn(job.run()).await {
            Ok(completion) => completion,
            Err(_) => outcome(
                MutationOutcome::Unknown,
                Err(ToolError::Internal(
                    "instance registration task failed".into(),
                )),
            ),
        }
    }
}

/// Everything the registration commit needs, owned so it can run detached.
struct Commit {
    config: Arc<dyn InstanceRegistryAccess>,
    credentials: InstanceCredentialStore,
    docs_only: bool,
    registry: Arc<hpm_core::registry::InstanceRegistry>,
    canonical: Canonical,
    name: String,
    origin: String,
    input: Object,
}

impl Commit {
    async fn run(self) -> HandlerCompletion {
        let Commit {
            config,
            credentials,
            docs_only,
            registry,
            canonical,
            name,
            origin,
            input,
        } = self;
        let not_sent = |e| outcome(MutationOutcome::NotSent, Err(e));
        let make_default = input.get("makeDefault") == Some(&Value::Bool(true));
        let registered = {
            let registry = registry.clone();
            let store = credentials.clone();
            let metadata = canonical.metadata.clone();
            let requirements = canonical.requirements.clone();
            let name = name.clone();
            let origin = origin.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                let mut prior_default = None;
                let mut capture = |previous: &Value, _: &Value| {
                    prior_default = previous
                        .get("instances")
                        .and_then(Value::as_array)
                        .and_then(|list| {
                            list.iter()
                                .find(|i| i.get("default") == Some(&Value::Bool(true)))
                        })
                        .and_then(|i| i.get("name").and_then(Value::as_str))
                        .filter(|n| !n.is_empty())
                        .map(|n| (n.to_owned(), true));
                    Ok(())
                };
                let mut precommit = |_: &Value| {
                    handle
                        .block_on(check_credentials(&store, &requirements, &name, &origin))
                        .map_err(|(code, _)| {
                            CodedError::new(code, "credential check failed")
                                .with("name", name.as_str())
                        })
                };
                let entry = registry.register(
                    &metadata,
                    RegisterOptions {
                        make_default,
                        precommit: Some(&mut precommit),
                        capture_context: Some(&mut capture),
                    },
                )?;
                Ok::<_, CodedError>(Registered {
                    entry,
                    prior_default,
                })
            })
            .await
        };
        let registered = match registered {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return not_sent(registry_error(&e)),
            Err(_) => {
                return outcome(
                    MutationOutcome::Unknown,
                    Err(ToolError::Internal(
                        "instance registration task failed".into(),
                    )),
                );
            }
        };
        let expected = Value::Object(registered.entry.clone());
        let prior = registered
            .prior_default
            .as_ref()
            .map(|(n, d)| (n.as_str(), *d));

        if let Err((_, response)) =
            check_credentials(&credentials, &canonical.requirements, &name, &origin).await
        {
            return match registry.compensate_registration(&name, Some(&expected), prior) {
                Ok(()) => outcome(MutationOutcome::Failed, Err(response)),
                Err(e) => outcome(MutationOutcome::Committed, Err(registry_error(&e))),
            };
        }

        if config.reload().is_err() {
            log_diagnostic(
                "registration_reload",
                "REGISTRY_RELOAD_FAILED",
                input.get("name"),
            );
            if registry
                .compensate_registration(&name, Some(&expected), prior)
                .is_err()
            {
                log_diagnostic(
                    "registration_rollback",
                    "REGISTRY_ROLLBACK_REQUIRED",
                    input.get("name"),
                );
                return outcome(MutationOutcome::Committed, Err(rollback_required()));
            }
            let restore_failed = config.reload().is_err();
            if restore_failed {
                log_diagnostic(
                    "registration_restore_reload",
                    "REGISTRY_RELOAD_FAILED",
                    input.get("name"),
                );
            }
            return outcome(
                MutationOutcome::Failed,
                Err(error_response(
                    "REGISTRY_RELOAD_FAILED",
                    "Configuration reload failed; the persisted registration was rolled back.",
                    obj(vec![
                        ("persisted", false.into()),
                        ("rolledBack", true.into()),
                        ("restartRequired", restore_failed.into()),
                        ("metadata", output_metadata(&registered.entry, &input)),
                    ]),
                )),
            );
        }

        committed(text(&success_payload(
            docs_only,
            output_metadata(&registered.entry, &input),
        )))
    }
}

/// 6.2.0 success body: docs-only servers need a restart to load live tools.
fn success_payload(restart_required: bool, metadata: Value) -> Value {
    let mut payload = obj(vec![
        ("success", true.into()),
        ("restartRequired", restart_required.into()),
    ]);
    if restart_required {
        payload.insert(
            "message".into(),
            "Instance registered. Restart the MCP server to enable live ServiceNow tools.".into(),
        );
    }
    payload.insert("metadata".into(), metadata);
    Value::Object(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_of(e: ToolError) -> Value {
        match e {
            ToolError::Text(t) => serde_json::from_str(&t).unwrap_or(Value::Null),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn secret_shaped_keys_match_the_6_2_0_pattern() {
        for k in [
            "password",
            "clientSecret",
            "accessToken",
            "Credential",
            "apiKey",
            "api_key",
            "private-key",
            "PRIVATEKEY",
        ] {
            assert!(is_secret_key(k), "{k}");
        }
        for k in ["name", "url", "apiary", "privacy"] {
            assert!(!is_secret_key(k), "{k}");
        }
    }

    #[test]
    fn input_key_validation_orders_secret_unknown_nested() {
        let secret =
            text_of(validate_input_keys(&json!({"name":"a","password":"x"}), 0).unwrap_err());
        assert_eq!(secret["code"], "INVALID_INSTANCE_CONFIG");
        assert_eq!(
            secret["message"],
            "Secret-shaped fields are not accepted by this tool; use the local credential command"
        );
        let unknown = text_of(validate_input_keys(&json!({"bogus":1}), 0).unwrap_err());
        assert_eq!(unknown["message"], "Unknown instance metadata field");
        let nested = text_of(validate_input_keys(&json!({"description":{"a":1}}), 0).unwrap_err());
        assert_eq!(
            nested["message"],
            "Nested instance metadata is not supported"
        );
        let nested_secret =
            text_of(validate_input_keys(&json!({"description":{"token":1}}), 0).unwrap_err());
        assert_eq!(
            nested_secret["message"],
            "Secret-shaped fields are not accepted by this tool; use the local credential command"
        );
        assert!(validate_input_keys(&json!({"name":"a","description":{}}), 0).is_ok());
    }

    #[test]
    fn canonical_metadata_derives_grant_and_credential_refs() {
        let basic =
            canonical_metadata(json!({"name":"dev","url":"u"}).as_object().unwrap()).unwrap();
        assert_eq!(basic.metadata["authType"], "basic");
        assert_eq!(
            basic.metadata["credentialRef"],
            "keychain:instance/dev/password"
        );
        assert_eq!(basic.requirements.len(), 1);

        let pw = canonical_metadata(
            json!({"name":"dev","url":"u","authType":"oauth","username":"x","clientId":"c"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(pw.metadata["grantType"], "password");
        assert_eq!(
            pw.metadata["credentialRef"],
            json!({"password":"keychain:instance/dev/password","clientSecret":"keychain:instance/dev/client-secret"})
        );
        assert_eq!(pw.requirements.len(), 2);

        let cc = canonical_metadata(
            json!({"name":"dev","url":"u","authType":"oauth","clientId":"c"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(cc.metadata["grantType"], "client_credentials");

        let code = canonical_metadata(
            json!({"name":"dev","url":"u","authType":"oauth","grantType":"authorization_code"})
                .as_object()
                .unwrap(),
        )
        .unwrap();
        assert!(code.metadata.get("credentialRef").is_none());
        assert!(code.requirements.is_empty());

        assert!(canonical_metadata(json!({"url":"u"}).as_object().unwrap()).is_err());
    }

    #[test]
    fn registry_errors_map_to_safe_6_2_0_responses() {
        let exists = text_of(registry_error(
            &CodedError::new("INSTANCE_ALREADY_EXISTS", "x").with("name", "dev"),
        ));
        assert_eq!(
            exists["commands"],
            json!(["happy-platform-mcp instance update dev"])
        );

        let invalid = text_of(registry_error(
            &CodedError::new("INVALID_INSTANCE_CONFIG", "raw").with("field", "clientSecret"),
        ));
        assert_eq!(invalid["details"], json!({}));

        let keychain = text_of(registry_error(&CodedError::new("KEYCHAIN_LOCKED", "raw")));
        assert_eq!(keychain["code"], "KEYCHAIN_OPERATION_FAILED");

        let other = text_of(registry_error(&CodedError::plain("boom")));
        assert_eq!(other["code"], "INSTANCE_REGISTRATION_FAILED");
        assert_eq!(
            other["message"],
            "The instance registration could not be completed."
        );

        let rollback = text_of(registry_error(&CodedError::new(
            "REGISTRY_ROLLBACK_REQUIRED",
            "x",
        )));
        assert_eq!(rollback["persisted"], true);
        assert_eq!(rollback["restartRequired"], true);
    }

    #[test]
    fn output_metadata_prefers_registered_values() {
        let out = output_metadata(
            json!({"name":"dev","url":"https://dev.example.com","authType":"basic","default":true,"credentialRef":"r"})
                .as_object()
                .unwrap(),
            json!({"name":"dev","url":"https://DEV.example.com/","makeDefault":false,"description":"d"})
                .as_object()
                .unwrap(),
        );
        assert_eq!(
            out,
            json!({"name":"dev","url":"https://dev.example.com","authType":"basic","description":"d","makeDefault":true})
        );
    }

    #[test]
    fn set_target_treats_falsy_as_list() {
        assert_eq!(set_target(&json!({})), None);
        assert_eq!(set_target(&json!({"instance_name":""})), None);
        assert_eq!(
            set_target(&json!({"instance_name":"prod"})).as_deref(),
            Some("prod")
        );
    }

    fn store() -> InstanceCredentialStore {
        InstanceCredentialStore::new(Arc::new(hpm_core::keychain::MemorySecretStore::new()))
    }

    const REF: &str = "keychain:instance/dev/password";

    fn reqs() -> Vec<Requirement> {
        vec![(REF.to_owned(), "password")]
    }

    #[tokio::test]
    async fn reregistering_a_name_at_a_new_url_fails_closed() {
        let store = store();
        store
            .set_secret(
                REF,
                hpm_types::credential::Secret::new("pw".into()),
                "https://old.example.com",
            )
            .await
            .unwrap();
        let (code, e) = check_credentials(&store, &reqs(), "dev", "https://new.example.com")
            .await
            .unwrap_err();
        assert_eq!(code, ORIGIN_MISMATCH);
        let body = text_of(e);
        assert_eq!(body["code"], ORIGIN_MISMATCH);
        assert_eq!(
            body["commands"],
            json!(["happy-platform-mcp instance credential set dev"])
        );
        // Same origin is accepted.
        assert!(
            check_credentials(&store, &reqs(), "dev", "https://old.example.com")
                .await
                .is_ok()
        );
    }

    /// H1: a 6.x or orphaned secret (no binding) is never adopted by an
    /// agent-supplied URL, at any origin; only the operator's CLI binds it.
    #[tokio::test]
    async fn unbound_secret_refuses_registration_at_any_url() {
        let mem = Arc::new(hpm_core::keychain::MemorySecretStore::new());
        mem.insert(hpm_core::keychain::SERVICE_NAME, REF, "pw");
        let unbound = InstanceCredentialStore::new(mem.clone());
        for url in ["https://attacker.example", "https://dev.example.com"] {
            let (code, e) = check_credentials(&unbound, &reqs(), "dev", url)
                .await
                .unwrap_err();
            assert_eq!(code, ORIGIN_UNBOUND);
            let body = text_of(e);
            assert_eq!(body["code"], ORIGIN_UNBOUND);
            assert_eq!(
                body["commands"],
                json!([
                    "happy-platform-mcp instance credential bind dev",
                    "happy-platform-mcp instance add --name dev"
                ])
            );
        }
        assert_eq!(unbound.bound_origin(REF).await.unwrap(), None);
        let mapped = text_of(registry_error(
            &CodedError::new(ORIGIN_UNBOUND, "x").with("name", "dev"),
        ));
        assert_eq!(mapped["code"], ORIGIN_UNBOUND);
        // Precommit's error re-maps to the same response.
        let mapped = text_of(registry_error(
            &CodedError::new(ORIGIN_MISMATCH, "x").with("name", "dev"),
        ));
        assert_eq!(mapped["code"], ORIGIN_MISMATCH);
    }

    #[tokio::test]
    async fn missing_credential_points_to_instance_add() {
        let (code, e) = check_credentials(&store(), &reqs(), "dev", "https://dev.example.com")
            .await
            .unwrap_err();
        assert_eq!(code, "CREDENTIAL_NOT_FOUND");
        assert_eq!(
            text_of(e)["commands"],
            json!(["happy-platform-mcp instance add --name dev"])
        );
    }

    #[test]
    fn url_origin_drops_the_path() {
        assert_eq!(
            url_origin("https://a.example.com/sub"),
            "https://a.example.com"
        );
        assert_eq!(url_origin("http://127.0.0.1:8080"), "http://127.0.0.1:8080");
    }

    #[test]
    fn docs_only_success_requires_restart() {
        let m = json!({"name":"dev"});
        assert_eq!(
            pretty(&success_payload(true, m.clone())),
            pretty(&json!({
                "success": true,
                "restartRequired": true,
                "message": "Instance registered. Restart the MCP server to enable live ServiceNow tools.",
                "metadata": {"name":"dev"},
            }))
        );
        assert_eq!(
            pretty(&success_payload(false, m)),
            pretty(&json!({"success": true, "restartRequired": false, "metadata": {"name":"dev"}}))
        );
    }

    // --- CR-6 / CR-7: registration through the handler ----------------------

    use hpm_core::config::{ConfigManager, Env};
    use hpm_core::keychain::{MemorySecretStore, SERVICE_NAME, SecretStore, SecretStoreError};
    use hpm_core::registry::InstanceRegistry;
    use hpm_types::credential::Secret;
    use tokio::sync::Notify;

    const DEV_URL: &str = "https://dev.example.com";

    fn registry_fixture() -> (tempfile::TempDir, std::path::PathBuf, Arc<ConfigManager>) {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("instances.json");
        std::fs::write(
            &file,
            json!({"version": 1, "instances": [
                {"name": "prod", "url": "https://prod.example.com", "authType": "oauth",
                 "grantType": "authorization_code", "clientId": "c", "default": true}]})
            .to_string(),
        )
        .unwrap();
        let env = Env::from_pairs(&[]);
        let cm = Arc::new(ConfigManager::new(
            Arc::new(InstanceRegistry::at_paths(&file, &file, env.clone())),
            env,
        ));
        cm.load_instances().unwrap();
        (dir, file, cm)
    }

    fn dev_args() -> Value {
        json!({"name": "dev", "url": DEV_URL, "authType": "basic", "username": "admin"})
    }

    fn handler(cm: &Arc<ConfigManager>, store: Arc<dyn SecretStore>) -> RegisterInstance {
        RegisterInstance {
            config: cm.clone(),
            credentials: InstanceCredentialStore::new(store),
            docs_only: false,
        }
    }

    /// CR-6: the next call after a registration sees a new registry version
    /// (no `GenerationMoved` until an unrelated `apply`).
    #[tokio::test(flavor = "multi_thread")]
    async fn registration_bumps_the_registry_version() {
        let (_dir, _file, cm) = registry_fixture();
        let mem = Arc::new(MemorySecretStore::new());
        let h = handler(&cm, mem.clone());
        h.credentials
            .set_secret(REF, Secret::new("pw".into()), DEV_URL)
            .await
            .unwrap();
        let before = cm.registry_version();
        let done = h.run(&|| false, dev_args()).await;
        assert!(
            matches!(done.outcome, MutationOutcome::Committed),
            "{:?}",
            done.result
        );
        assert!(cm.registry_version() > before);
        assert!(cm.get_instance("dev").is_ok());
    }

    /// Pauses the first `get` made after the registry file names `dev`
    /// (the post-commit credential re-check) until released.
    struct GateAfterCommit {
        inner: Arc<MemorySecretStore>,
        file: std::path::PathBuf,
        gated: std::sync::atomic::AtomicBool,
        reached: Notify,
        release: Notify,
    }

    impl SecretStore for GateAfterCommit {
        fn get<'a>(
            &'a self,
            service: &'a str,
            account: &'a str,
        ) -> BoxFut<'a, Result<Option<Secret>, SecretStoreError>> {
            Box::pin(async move {
                let committed =
                    std::fs::read_to_string(&self.file).is_ok_and(|t| t.contains("\"dev\""));
                if committed && !self.gated.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    self.reached.notify_one();
                    self.release.notified().await;
                }
                self.inner.get(service, account).await
            })
        }
        fn set<'a>(
            &'a self,
            service: &'a str,
            account: &'a str,
            value: Secret,
        ) -> BoxFut<'a, Result<(), SecretStoreError>> {
            self.inner.set(service, account, value)
        }
        fn delete<'a>(
            &'a self,
            service: &'a str,
            account: &'a str,
        ) -> BoxFut<'a, Result<bool, SecretStoreError>> {
            self.inner.delete(service, account)
        }
    }

    /// CR-7: the call future is dropped after the commit, while the
    /// credential re-check is in flight; the binding then disappears. The
    /// detached tail still runs and compensates the registration.
    #[tokio::test(flavor = "multi_thread")]
    async fn dropped_registration_still_runs_its_tail() {
        let (_dir, file, cm) = registry_fixture();
        let mem = Arc::new(MemorySecretStore::new());
        let gate = Arc::new(GateAfterCommit {
            inner: mem.clone(),
            file: file.clone(),
            gated: std::sync::atomic::AtomicBool::new(false),
            reached: Notify::new(),
            release: Notify::new(),
        });
        let h = handler(&cm, gate.clone());
        h.credentials
            .set_secret(REF, Secret::new("pw".into()), DEV_URL)
            .await
            .unwrap();
        let args = dev_args();
        tokio::select! {
            _ = h.run(&|| false, args) => panic!("call finished before the gate"),
            () = gate.reached.notified() => {}
        }
        // The call future is gone; the binding is removed behind its back.
        assert!(
            mem.delete(
                SERVICE_NAME,
                &hpm_core::keychain::origin_binding_account(REF)
            )
            .await
            .unwrap()
        );
        gate.release.notify_one();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let text = std::fs::read_to_string(&file).unwrap();
            if !text.contains("\"dev\"") {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "registration was not compensated: {text}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }
}
