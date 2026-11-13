//! Background-script execution: `/sys.scripts.do` (processor) and
//! `sys_trigger` (ports `executeScriptViaProcessor`, `executeScriptViaTrigger`
//! and their helpers in `src/servicenow-client.js`).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Map, Value, json};

use super::error::{self, ClientError, RawError};
use super::http::{
    HttpRequest, HttpResponse, Method, axios_query, form_encode, format_utc_datetime,
};
use super::pickers::{is_sys_id, truthy};
use super::script_processor::{
    CookieJar, PROCESSOR_DEFAULT_TIMEOUT_MS, ProcessorOutcome, SCRIPT_HISTORY_TABLE,
    SCRIPT_PROCESSOR_PATH, ScopeRequest, classify_processor_outcome, classify_scope, not_run_error,
    rollback_context_of, script_execution_links, unknown_outcome_error, with_reason,
};
use super::{AuthType, RecordQuery, ServiceNowClient, State, UI_TIMEOUT, UI_USER_AGENT};

/// Result of [`ServiceNowClient::execute_script_via_processor_or`].
#[derive(Debug)]
pub enum ProcessorRun<T> {
    /// The processor ran the script (the parsed result).
    Ran(Value),
    /// The processor provably did not run it; the fallback's output.
    NotRun(T),
}

#[derive(Clone, Debug)]
pub struct ProcessorOptions {
    pub scope: Option<String>,
    pub record_for_rollback: bool,
    pub reason: Option<String>,
    pub timeout_ms: u64,
}

impl Default for ProcessorOptions {
    fn default() -> Self {
        Self {
            scope: None,
            record_for_rollback: true,
            reason: None,
            timeout_ms: PROCESSOR_DEFAULT_TIMEOUT_MS,
        }
    }
}

#[derive(Clone, Debug)]
pub struct TriggerOptions {
    pub wait: bool,
    pub timeout_ms: u64,
    pub interval_ms: u64,
}

impl Default for TriggerOptions {
    fn default() -> Self {
        Self {
            wait: true,
            timeout_ms: 15_000,
            interval_ms: 500,
        }
    }
}

struct ScopeTarget {
    scope_id: String,
    label: String,
    inactive: bool,
}

struct ScriptSession {
    client: ServiceNowClient,
    state: Arc<State>,
    auth_header: String,
    jar: tokio::sync::Mutex<CookieJar>,
    /// Set once `/logout.do` was sent (RFC §4 per-operation UI logout).
    ended: std::sync::atomic::AtomicBool,
}

/// `GET /logout.do` for a UI session; the caller adds the cookie.
pub(crate) fn logout_request(base_url: &str, auth_header: &str) -> HttpRequest {
    let mut req = HttpRequest::new(Method::Get, format!("{base_url}/logout.do"));
    req.set_header("Authorization", auth_header);
    req.set_header("User-Agent", UI_USER_AGENT);
    req.timeout = Some(UI_TIMEOUT);
    req.max_redirects = 0;
    req
}

/// Best-effort logout when a session is dropped without [`ScriptSession::end`]
/// (early return, error or cancellation): sent on the current runtime.
pub(crate) fn spawn_logout(client: &ServiceNowClient, req: HttpRequest) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        let transport = client.deps().transport.clone();
        handle.spawn(async move {
            let _ = transport.send(req).await;
        });
    }
}

impl Drop for ScriptSession {
    fn drop(&mut self) {
        if self.ended.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        let mut req = logout_request(&self.state.url, &self.auth_header);
        let cookie = self.jar.get_mut().serialize();
        if !cookie.is_empty() {
            req.set_header("Cookie", cookie);
        }
        spawn_logout(&self.client, req);
    }
}

impl ScriptSession {
    /// Ends the UI session (`GET /logout.do`); failures are ignored.
    async fn end(&self) {
        self.ended.store(true, std::sync::atomic::Ordering::Release);
        let req = logout_request(&self.state.url, &self.auth_header);
        let _ = self.request(req).await;
    }

    async fn request(
        &self,
        mut req: HttpRequest,
    ) -> Result<HttpResponse, super::http::TransportError> {
        req.set_header("Authorization", self.auth_header.clone());
        req.set_header("User-Agent", UI_USER_AGENT);
        if req.timeout.is_none() {
            req.timeout = Some(UI_TIMEOUT);
        }
        req.max_redirects = 0;
        let cookie = self.jar.lock().await.serialize();
        if !cookie.is_empty() {
            req.set_header("Cookie", cookie);
        }
        let response = self.client.deps().transport.send(req).await?;
        self.jar.lock().await.merge_set_cookies(&response);
        Ok(response)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.state.url)
    }

    async fn lookup_csrf(&self) -> Result<String, super::http::TransportError> {
        let mut req = HttpRequest::new(Method::Get, self.url("/api/now/ui/user/current_user"));
        req.set_header("Accept", "application/json");
        let current = self.request(req).await?;
        if !(200..300).contains(&current.status) {
            return Ok(String::new());
        }
        let session_id = self
            .jar
            .lock()
            .await
            .get("glide_session_store")
            .map(str::to_owned);
        let data = current.data();
        let user_name = data.pointer("/result/user_name").and_then(Value::as_str);
        let query = match (session_id, user_name) {
            (Some(id), _) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_alphanumeric()) => {
                format!("id={id}")
            }
            (_, Some(u)) if !u.is_empty() && !u.contains(['^', '\r', '\n']) => {
                format!("name={u}^ORDERBYDESClast_accessed")
            }
            _ => return Ok(String::new()),
        };
        let qs = axios_query(&[
            ("sysparm_fields", "csrf_token,id"),
            ("sysparm_query", &query),
            ("sysparm_limit", "1"),
        ]);
        let mut req = HttpRequest::new(
            Method::Get,
            self.url(&format!("/api/now/table/sys_user_session?{qs}")),
        );
        req.set_header("Accept", "application/json");
        let session = self.request(req).await?;
        if !(200..300).contains(&session.status) {
            return Ok(String::new());
        }
        Ok(session
            .data()
            .pointer("/result/0/csrf_token")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned())
    }
}

/// `generateScriptExecutionMarker`.
pub fn generate_script_execution_marker(now_ms: i64) -> String {
    let mut buf = [0u8; 4];
    // OS RNG failure is unrecoverable here.
    #[allow(clippy::expect_used)]
    getrandom::fill(&mut buf).expect("OS random number generator unavailable");
    let hex: String = buf.iter().map(|b| format!("{b:02x}")).collect();
    format!("MCP_{now_ms}_{hex}")
}

/// `buildBackgroundScriptWrapper`.
pub fn build_background_script_wrapper(
    script: &str,
    marker: &str,
    auto_delete: bool,
    trigger_sys_id: Option<&str>,
) -> String {
    let start = format!("{marker}:START");
    let end = format!("{marker}:END");
    let delete_block = if auto_delete {
        format!(
            "\n  var triggerGR = new GlideRecord('sys_trigger');\n  if (triggerGR.get('{}')) {{\n    triggerGR.deleteRecord();\n  }}",
            trigger_sys_id.unwrap_or("undefined")
        )
    } else {
        String::new()
    };
    format!(
        "\n// Auto-generated MCP script trigger\ngs.info('{start}');\ntry {{\n  {script}\n  gs.info('{end}' + JSON.stringify({{ ok: true }}));\n}} catch (e) {{\n  gs.info('{end}' + JSON.stringify({{ ok: false, error: e.message, stack: e.stack || '' }}));\n}} finally {{{delete_block}\n}}"
    )
}

#[derive(Clone, Debug, PartialEq)]
pub enum MarkerOutcome {
    Completed(Value),
    Failed { error: String, stack: Option<Value> },
}

/// `parseBackgroundScriptMarker`.
pub fn parse_background_script_marker(record: &Value, marker: &str) -> MarkerOutcome {
    let message = record.get("message").and_then(Value::as_str).unwrap_or("");
    let json_part = message
        .find(marker)
        .map_or(message, |i| &message[i + marker.len()..]);
    let Ok(payload) = serde_json::from_str::<Value>(json_part) else {
        return MarkerOutcome::Failed {
            error: format!("Could not parse script outcome from syslog message: {message}"),
            stack: None,
        };
    };
    if payload.get("ok").is_some_and(truthy) {
        let mut output = payload.as_object().cloned().unwrap_or_default();
        output.remove("ok");
        return MarkerOutcome::Completed(Value::Object(output));
    }
    let error = payload
        .get("error")
        .filter(|v| truthy(v))
        .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
        .unwrap_or_else(|| "Script execution failed".to_owned());
    MarkerOutcome::Failed {
        error,
        stack: payload.get("stack").cloned(),
    }
}

/// Any execution's start/end marker line: `^MCP_\d+_[0-9a-f]+:(START|END)`.
fn is_marker_line(message: &str) -> bool {
    let Some(rest) = message.strip_prefix("MCP_") else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits == 0 {
        return false;
    }
    let Some(rest) = rest[digits..].strip_prefix('_') else {
        return false;
    };
    let hex = rest
        .bytes()
        .take_while(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
        .count();
    hex > 0 && (rest[hex..].starts_with(":START") || rest[hex..].starts_with(":END"))
}

impl ServiceNowClient {
    /// `executeScriptViaProcessor`: the script POST is sent exactly once.
    pub async fn execute_script_via_processor(
        &self,
        script: &str,
        options: &ProcessorOptions,
    ) -> Result<Value, ClientError> {
        match self
            .execute_script_via_processor_or(script, options, |e| async move { e })
            .await?
        {
            ProcessorRun::Ran(v) => Ok(v),
            ProcessorRun::NotRun(e) => Err(e),
        }
    }

    /// The processor run with a fallback for the provable not-run case
    /// (`SCRIPT_PROCESSOR_NOT_RUN`): `on_not_run` receives that error and runs
    /// while the UI session is still open (e.g. the `sys_trigger` fallback), and
    /// the session's single `/logout.do` is sent after it finishes. Any other
    /// failure, including an unknown outcome, is `Err` and runs no fallback.
    pub async fn execute_script_via_processor_or<T, F, Fut>(
        &self,
        script: &str,
        options: &ProcessorOptions,
        on_not_run: F,
    ) -> Result<ProcessorRun<T>, ClientError>
    where
        F: FnOnce(ClientError) -> Fut,
        Fut: Future<Output = T>,
    {
        if script.trim().is_empty() {
            return Err(ClientError::plain("script must be a non-empty string"));
        }
        let state = self.state();
        let target = match &options.scope {
            Some(scope) => Some(self.resolve_script_scope(scope).await?),
            None => None,
        };
        let session = self.open_script_processor_session(&state).await?;
        let user_token = if state.cfg.auth_type == AuthType::Basic {
            let mut req = HttpRequest::new(
                Method::Post,
                session.url("/angular.do?sysparm_type=get_user"),
            );
            req.set_header("Accept", "application/json, text/plain, */*");
            req.set_header("Content-Type", "application/x-www-form-urlencoded");
            session
                .request(req)
                .await
                .ok()
                .and_then(|r| r.header("x-usertoken-response").map(str::to_owned))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let sysparm_ck = if user_token.is_empty() {
            session.lookup_csrf().await.unwrap_or_default()
        } else {
            user_token.clone()
        };
        let scripted = with_reason(script, options.reason.as_deref());
        let mut form: Vec<(&str, &str)> = vec![
            ("script", &scripted),
            ("sysparm_ck", &sysparm_ck),
            ("runscript", "Run script"),
            ("quota_managed_transaction", "on"),
        ];
        if let Some(t) = &target {
            form.push(("sys_scope", &t.scope_id));
        }
        if options.record_for_rollback {
            form.push(("record_for_rollback", "on"));
        }
        self.assert_current(&state)?;
        let mut req = HttpRequest::new(Method::Post, session.url(SCRIPT_PROCESSOR_PATH));
        req.set_header(
            "Content-Type",
            "application/x-www-form-urlencoded; charset=UTF-8",
        );
        req.set_header("Accept", "*/*");
        if !user_token.is_empty() {
            req.set_header("X-UserToken", user_token.clone());
        }
        req.body = Some(form_encode(&form).into_bytes());
        req.timeout = Some(Duration::from_millis(options.timeout_ms));
        let result = match session.request(req).await {
            Ok(response) => classify_processor_outcome(&Ok(response)),
            Err(e) => classify_processor_outcome(&Err(e)),
        };
        // The UI logout (intentional 7.0 difference, RFC §4) is the operation's
        // last request: after the execution-history read, never before it.
        let result = match result {
            ProcessorOutcome::Ran(r) => r,
            ProcessorOutcome::NotRun(reason) => {
                let fallback = on_not_run(not_run_error(&reason)).await;
                session.end().await;
                return Ok(ProcessorRun::NotRun(fallback));
            }
            ProcessorOutcome::Unknown(reason) => {
                session.end().await;
                return Err(unknown_outcome_error(&reason));
            }
        };
        let mut warnings = result.warnings.clone();
        if let Some(t) = target.as_ref().filter(|t| t.inactive) {
            warnings.push(format!("Application '{}' is inactive.", t.label));
        }
        if let Some(t) = target.as_ref().filter(|t| t.label != result.scope) {
            warnings.push(format!(
                "Requested scope '{}' but the instance reported running in '{}'.",
                t.label, result.scope
            ));
        }
        let mut rollback = None;
        if let Some(history) = &result.execution_history_id {
            match self.script_rollback_context_id(history).await {
                Ok(id) => rollback = id,
                Err(e) => {
                    warnings.push("Could not read the rollback context for this run (reading script execution history needs the admin role).".to_owned());
                    eprintln!("Could not read the script rollback context: {}", e.message);
                }
            }
        }
        let mut out = Map::new();
        out.insert("ok".into(), json!(result.ok));
        out.insert(
            "status".into(),
            json!(if result.ok { "completed" } else { "failed" }),
        );
        out.insert("method".into(), json!("processor"));
        out.insert("output".into(), json!(result.output));
        out.insert("errors".into(), json!(result.errors));
        out.insert("log".into(), json!(result.log));
        out.insert("warnings".into(), json!(warnings));
        if result.truncated {
            out.insert("truncated".into(), json!(true));
        }
        out.insert("scope".into(), json!(result.scope));
        let scope_id = match &target {
            Some(t) => json!(t.scope_id),
            None if result.scope == "global" => json!("global"),
            None => Value::Null,
        };
        out.insert("scopeId".into(), scope_id);
        out.insert(
            "recordForRollback".into(),
            json!(options.record_for_rollback),
        );
        out.insert(
            "executionHistoryId".into(),
            json!(result.execution_history_id),
        );
        out.insert("rollbackContextId".into(), json!(rollback));
        out.insert(
            "links".into(),
            script_execution_links(result.execution_history_id.as_deref(), rollback.as_deref()),
        );
        session.end().await;
        Ok(ProcessorRun::Ran(Value::Object(out)))
    }

    async fn resolve_script_scope(&self, scope: &str) -> Result<ScopeTarget, ClientError> {
        let (query, value) = match classify_scope(scope)? {
            ScopeRequest::Global => {
                return Ok(ScopeTarget {
                    scope_id: "global".into(),
                    label: "global".into(),
                    inactive: false,
                });
            }
            ScopeRequest::SysId(v) => (format!("sys_id={v}"), v),
            ScopeRequest::Name(v) => (format!("scope={v}"), v),
        };
        let records = self
            .get_records(
                "sys_scope",
                &RecordQuery {
                    query: Some(query),
                    fields: Some("sys_id,scope,name,active".into()),
                    limit: Some("2".into()),
                    ..RecordQuery::default()
                },
            )
            .await?;
        let records = records.as_array().cloned().unwrap_or_default();
        if records.is_empty() {
            return Err(ClientError::plain(format!(
                "No application found with scope '{scope}' on the instance"
            )));
        }
        if records.len() > 1 {
            return Err(ClientError::plain(format!(
                "Multiple applications found with scope '{scope}'; resolve the duplicate installations first"
            )));
        }
        let app = &records[0];
        let sys_id = app.get("sys_id").and_then(Value::as_str).unwrap_or("");
        if !is_sys_id(sys_id) && sys_id != "global" {
            return Err(ClientError::plain(format!(
                "The instance returned no usable sys_id for scope '{scope}'"
            )));
        }
        let label = app
            .get("scope")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map_or(value, str::to_owned);
        let inactive = matches!(app.get("active"), Some(Value::Bool(false)))
            || app.get("active") == Some(&json!("false"));
        Ok(ScopeTarget {
            scope_id: sys_id.to_owned(),
            label,
            inactive,
        })
    }

    async fn script_rollback_context_id(
        &self,
        history_id: &str,
    ) -> Result<Option<String>, ClientError> {
        let records = self
            .get_records(
                SCRIPT_HISTORY_TABLE,
                &RecordQuery {
                    query: Some(format!("sys_id={history_id}")),
                    fields: Some("rollback_context".into()),
                    limit: Some("1".into()),
                    ..RecordQuery::default()
                },
            )
            .await?;
        Ok(records
            .as_array()
            .and_then(|r| rollback_context_of(r.first())))
    }

    async fn open_script_processor_session(
        &self,
        state: &Arc<State>,
    ) -> Result<ScriptSession, ClientError> {
        let auth_header = self.auth_header_for(state).await?;
        Ok(ScriptSession {
            client: self.clone(),
            state: state.clone(),
            auth_header,
            jar: tokio::sync::Mutex::new(CookieJar::default()),
            ended: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// `executeScriptViaTrigger`.
    pub async fn execute_script_via_trigger(
        &self,
        script: &str,
        description: &str,
        auto_delete: bool,
        options: &TriggerOptions,
    ) -> Result<Value, ClientError> {
        self.execute_script_via_trigger_inner(script, description, auto_delete, options)
            .await
            .map_err(|e| {
                let mut wrapped =
                    ClientError::plain(format!("Failed to create script trigger: {}", e.message));
                wrapped.status = e.status;
                wrapped.code = e.code;
                wrapped
            })
    }

    async fn execute_script_via_trigger_inner(
        &self,
        script: &str,
        description: &str,
        auto_delete: bool,
        options: &TriggerOptions,
    ) -> Result<Value, ClientError> {
        // Once the trigger row exists the script may run regardless of what
        // fails next, so every later error is tagged "result unknown".
        let inserted = |mut e: ClientError| {
            e.code = Some(error::TRIGGER_INSERTED_RESULT_UNKNOWN_CODE.into());
            e
        };
        let now = self.now_ms();
        let next_action = format_utc_datetime(now + 1000);
        let marker = generate_script_execution_marker(self.now_ms());
        let end_marker = format!("{marker}:END");
        let poll_since = format_utc_datetime(now - 2000);
        let description = if description.is_empty() {
            "Automated script execution via MCP"
        } else {
            description
        };
        let base = json!({
            "name": format!("MCP_Script_{}", self.now_ms()),
            "next_action": next_action,
            "trigger_type": "0",
            "state": "0",
            "description": description,
        });
        let with_script = |s: String| {
            let mut v = base.clone();
            v["script"] = json!(s);
            v
        };
        let trigger = if auto_delete {
            let trigger = self
                .create_record("sys_trigger", &with_script(script.to_owned()))
                .await?;
            let sys_id = trigger
                .get("sys_id")
                .and_then(Value::as_str)
                .unwrap_or("undefined")
                .to_owned();
            let wrapped = build_background_script_wrapper(script, &marker, true, Some(&sys_id));
            self.update_record("sys_trigger", &sys_id, &json!({"script": wrapped}))
                .await
                .map_err(inserted)?;
            trigger
        } else {
            let wrapped = build_background_script_wrapper(script, &marker, false, None);
            self.create_record("sys_trigger", &with_script(wrapped))
                .await?
        };
        let trigger_sys_id = trigger.get("sys_id").cloned().unwrap_or(Value::Null);
        let mut out = Map::new();
        out.insert("trigger_sys_id".into(), trigger_sys_id.clone());
        out.insert(
            "trigger_name".into(),
            trigger.get("name").cloned().unwrap_or(Value::Null),
        );
        out.insert("next_action".into(), json!(next_action));
        out.insert("auto_delete".into(), json!(auto_delete));
        let with = |mut m: Map<String, Value>, head: [(&str, Value); 2]| {
            let mut ordered = Map::new();
            for (k, v) in head {
                ordered.insert(k.into(), v);
            }
            ordered.append(&mut m);
            ordered
        };

        if !options.wait {
            let mut o = with(
                out,
                [("success", json!(true)), ("status", json!("scheduled"))],
            );
            o.insert(
                "message".into(),
                json!(format!(
                    "Script scheduled to run at {next_action}. {}",
                    if auto_delete {
                        "Trigger will auto-delete after execution."
                    } else {
                        "Trigger will remain after execution."
                    }
                )),
            );
            return Ok(Value::Object(o));
        }

        let deadline = self.now_ms() + i64::try_from(options.timeout_ms).unwrap_or(i64::MAX);
        let found = loop {
            let records = self
                .get_records(
                    "syslog",
                    &RecordQuery {
                        query: Some(format!(
                            "messageLIKE{end_marker}^sys_created_on>={poll_since}^ORDERBYDESCsys_created_on"
                        )),
                        fields: Some("message,sys_created_on".into()),
                        limit: Some("1".into()),
                        ..RecordQuery::default()
                    },
                )
                .await
                .map_err(inserted)?;
            if let Some(first) = records.as_array().and_then(|r| r.first()).cloned() {
                break Some(first);
            }
            if self.now_ms() >= deadline {
                break None;
            }
            self.sleep_ms(options.interval_ms).await;
        };

        let Some(record) = found else {
            let mut o = with(
                out,
                [("success", json!(false)), ("status", json!("timeout"))],
            );
            o.insert(
                "message".into(),
                json!(format!(
                    "Timed out after {}ms waiting for the script's completion marker in syslog. Trigger {} may still be pending, may have run without producing a marker, or may have failed to fire — inspect it manually.",
                    options.timeout_ms,
                    trigger_sys_id.as_str().unwrap_or("undefined")
                )),
            );
            return Ok(Value::Object(o));
        };
        let outcome = parse_background_script_marker(&record, &end_marker);
        let until = record
            .get("sys_created_on")
            .and_then(Value::as_str)
            .unwrap_or("undefined")
            .to_owned();
        let (logs, truncated, unverified) = self
            .collect_background_script_logs(&end_marker, &next_action, &until)
            .await
            .map_err(inserted)?;
        let mut o = match &outcome {
            MarkerOutcome::Completed(output) => {
                let mut o = with(
                    out,
                    [("success", json!(true)), ("status", json!("completed"))],
                );
                o.insert("output".into(), output.clone());
                o
            }
            MarkerOutcome::Failed { error, stack } => {
                let mut o = with(
                    out,
                    [("success", json!(false)), ("status", json!("failed"))],
                );
                o.insert("error".into(), json!(error));
                if let Some(stack) = stack {
                    o.insert("stack".into(), stack.clone());
                }
                o
            }
        };
        o.insert("logs".into(), json!(logs));
        if truncated {
            o.insert("logsTruncated".into(), json!(true));
        }
        if unverified {
            o.insert("logsWindowUnverified".into(), json!(true));
        }
        let message = match &outcome {
            MarkerOutcome::Completed(_) => "Script executed successfully.".to_owned(),
            MarkerOutcome::Failed { error, .. } => format!("Script execution failed: {error}"),
        };
        o.insert("message".into(), json!(message));
        Ok(Value::Object(o))
    }

    /// Cookie jar + CSRF token for a fresh UI session of the current
    /// generation (the same lookup `/sys.scripts.do` uses). Returns
    /// `(cookie_header, csrf_token, generation)`.
    pub async fn open_ui_session(&self) -> Result<(String, String, u64), ClientError> {
        let state = self.state();
        let session = self.open_script_processor_session(&state).await?;
        let mut token = String::new();
        if state.cfg.auth_type == AuthType::Basic {
            let mut req = HttpRequest::new(
                Method::Post,
                session.url("/angular.do?sysparm_type=get_user"),
            );
            req.set_header("Accept", "application/json, text/plain, */*");
            req.set_header("Content-Type", "application/x-www-form-urlencoded");
            token = session
                .request(req)
                .await
                .ok()
                .and_then(|r| r.header("x-usertoken-response").map(str::to_owned))
                .unwrap_or_default();
        }
        if token.is_empty() {
            token = session.lookup_csrf().await.unwrap_or_default();
        }
        self.assert_current(&state)?;
        let cookie = session.jar.lock().await.serialize();
        // Ownership passes to the caller, who ends it with `logout_ui_session`.
        session
            .ended
            .store(true, std::sync::atomic::Ordering::Release);
        Ok((cookie, token, state.generation))
    }

    /// `GET /logout.do` in the session's jar, on this client's (pinned) instance.
    /// When that generation has moved, the session is still ended on its own
    /// host, with its cookie only: no credential of any generation is attached.
    pub async fn logout_ui_session(&self, cookie: &str) -> Result<(), ClientError> {
        let state = self.state();
        let mut req = HttpRequest::new(Method::Get, format!("{}/logout.do", state.url));
        req.set_header("User-Agent", UI_USER_AGENT);
        if !cookie.is_empty() {
            req.set_header("Cookie", cookie);
        }
        req.timeout = Some(UI_TIMEOUT);
        if self.assert_current(&state).is_err() {
            return match self.deps().transport.send(req).await {
                Ok(_) => Ok(()),
                Err(e) => Err(self.sanitize(
                    &state,
                    RawError {
                        message: e.message,
                        code: e.code,
                        ..RawError::default()
                    },
                )),
            };
        }
        let header = self.auth_header_for(&state).await?;
        req.set_header("Authorization", header);
        self.send(&state, req).await.map(|_| ())
    }

    /// `_collectBackgroundScriptLogs` (maxLines 100).
    async fn collect_background_script_logs(
        &self,
        end_marker: &str,
        since: &str,
        until: &str,
    ) -> Result<(Vec<String>, bool, bool), ClientError> {
        const MAX_LINES: usize = 100;
        let records = self
            .get_records(
                "syslog",
                &RecordQuery {
                    query: Some(format!(
                        "source=*** Script^sys_created_on>={since}^sys_created_on<={until}^ORDERBYsys_created_on"
                    )),
                    fields: Some("message,sys_created_on".into()),
                    limit: Some((MAX_LINES + 10).to_string()),
                    ..RecordQuery::default()
                },
            )
            .await?;
        let messages: Vec<String> = records
            .as_array()
            .map(|r| {
                r.iter()
                    .map(|rec| {
                        rec.get("message")
                            .filter(|v| truthy(v))
                            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_owned))
                            .unwrap_or_default()
                    })
                    .collect()
            })
            .unwrap_or_default();
        let verified = messages.iter().any(|m| m.starts_with(end_marker));
        let mut lines: Vec<String> = messages
            .into_iter()
            .filter(|m| !is_marker_line(m))
            .collect();
        let truncated = lines.len() > MAX_LINES;
        lines.truncate(MAX_LINES);
        Ok((lines, truncated, !verified))
    }
}
