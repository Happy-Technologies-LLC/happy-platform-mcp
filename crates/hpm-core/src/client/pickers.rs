//! Update-set and application pickers through `/api/now/ui/concoursepicker/*`
//! in one cookie session (ports `_openUiPickerSession`, `setCurrentUpdateSet`,
//! `getCurrentUpdateSet`, `setCurrentApplication`).

use std::sync::Arc;

use serde_json::{Value, json};

use super::error::{ClientError, RawError};
use super::http::{HttpRequest, Method, format_iso};
use super::{RecordParams, RecordQuery, ServiceNowClient, State, UI_USER_AGENT};

const UPDATE_SET_PICKER: &str = "/api/now/ui/concoursepicker/updateset";
const APPLICATION_PICKER: &str = "/api/now/ui/concoursepicker/application";

/// `isSysId` (bulk-limits): 32 lowercase hex characters.
pub fn is_sys_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) struct PickerSession {
    client: ServiceNowClient,
    state: Arc<State>,
    pub auth_header: String,
    cookie: String,
}

impl PickerSession {
    fn request(&self, method: Method, path: &str) -> HttpRequest {
        let mut req = HttpRequest::new(method, format!("{}{path}", self.state.url));
        req.set_header("Authorization", self.auth_header.clone());
        req.set_header("Content-Type", "application/json");
        req.set_header("Accept", "application/json");
        req.set_header("User-Agent", UI_USER_AGENT);
        if !self.cookie.is_empty() {
            req.set_header("Cookie", self.cookie.clone());
        }
        req.max_redirects = 5;
        req
    }

    fn fail(&self, raw: RawError) -> ClientError {
        let mut redactor = self.state.redactor();
        redactor.add_authorization_value(&self.auth_header);
        raw.into_safe_servicenow(redactor)
    }

    async fn exchange(&self, req: HttpRequest) -> Result<Value, ClientError> {
        let response = self.client.send(&self.state, req).await?;
        if (200..300).contains(&response.status) {
            Ok(response.data())
        } else {
            Err(self.fail(RawError::http(response.status, response.data())))
        }
    }

    pub async fn get(&self, path: &str) -> Result<Value, ClientError> {
        self.exchange(self.request(Method::Get, path)).await
    }

    pub async fn put(&self, path: &str, body: &Value) -> Result<Value, ClientError> {
        let mut req = self.request(Method::Put, path);
        req.body = Some(body.to_string().into_bytes());
        self.exchange(req).await
    }
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    path.iter().try_fold(v, |acc, k| acc.get(k))?.as_str()
}

impl ServiceNowClient {
    /// `_openUiPickerSession`: landing-page cookie, then reuse it.
    pub(crate) async fn open_ui_picker_session(&self) -> Result<PickerSession, ClientError> {
        let state = self.state();
        let auth_header = self.auth_header_for(&state).await?;
        let mut session = PickerSession {
            client: self.clone(),
            state,
            auth_header,
            cookie: String::new(),
        };
        let mut landing = session.request(Method::Get, "/");
        landing.set_header("Accept", "text/html");
        let response = self.send(&session.state, landing).await?;
        if !(200..300).contains(&response.status) {
            return Err(session.fail(RawError::http(response.status, response.data())));
        }
        session.cookie = response
            .header_all("set-cookie")
            .map(|e| e.split(';').next().unwrap_or(""))
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join("; ");
        Ok(session)
    }

    /// `setCurrentUpdateSet`.
    pub async fn set_current_update_set(&self, sys_id: &str) -> Result<Value, ClientError> {
        if !is_sys_id(sys_id) {
            return Err(ClientError::plain(
                "update_set_sys_id must be a 32-character lowercase hexadecimal sys_id",
            ));
        }
        let update_set = self
            .get_record("sys_update_set", sys_id, &RecordParams::default())
            .await?;
        let session = self.open_ui_picker_session().await?;
        let previous = session.get(UPDATE_SET_PICKER).await?;
        let previous = previous
            .pointer("/result/current")
            .filter(|v| !v.is_null())
            .cloned();
        let name = update_set.get("name").cloned().unwrap_or(Value::Null);
        let response = session
            .put(UPDATE_SET_PICKER, &json!({"name": name, "sysId": sys_id}))
            .await?;
        self.sleep_ms(500).await;
        let current = session.get(UPDATE_SET_PICKER).await?;
        let current_id = str_at(&current, &["result", "current", "sysId"]);
        if current_id != Some(sys_id) {
            return Err(ClientError::plain(format!(
                "Update set verification failed: current is {}, expected {sys_id}",
                current_id.filter(|s| !s.is_empty()).unwrap_or("unknown")
            )));
        }
        let prev = previous.map_or(Value::Null, |p| {
            json!({
                "name": p.get("name").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null),
                "sys_id": p.get("sysId").filter(|v| truthy(v)).cloned().unwrap_or(Value::Null),
            })
        });
        Ok(json!({
            "success": true,
            "update_set": name,
            "sys_id": sys_id,
            "method": "ui_api",
            "verified": true,
            "previous_update_set": prev,
            "response": response,
        }))
    }

    /// `getCurrentUpdateSet`.
    pub async fn get_current_update_set(&self) -> Result<Value, ClientError> {
        let session = self.open_ui_picker_session().await?;
        let data = session.get(UPDATE_SET_PICKER).await?;
        let current = data
            .pointer("/result/current")
            .cloned()
            .unwrap_or(Value::Null);
        let Some(sys_id) = current.get("sysId").filter(|v| truthy(v)).cloned() else {
            return Err(ClientError::plain(
                "Current update set was not returned by the ServiceNow update-set picker",
            ));
        };
        let name = current
            .get("name")
            .filter(|v| truthy(v))
            .cloned()
            .unwrap_or(Value::Null);
        Ok(json!({"result": {"name": name, "value": sys_id, "sys_id": sys_id}}))
    }

    pub async fn list_update_sets(&self, query: &RecordQuery) -> Result<Value, ClientError> {
        self.get_records("sys_update_set", query).await
    }

    /// `setCurrentApplication`. Failures are reduced to a redacted summary.
    pub async fn set_current_application(&self, app_sys_id: &str) -> Result<Value, ClientError> {
        let start = self.now_ms();
        let mut auth_header: Option<String> = None;
        match self
            .set_current_application_inner(app_sys_id, start, &mut auth_header)
            .await
        {
            Ok(v) => Ok(v),
            Err(error) => {
                let state = self.state();
                let mut redactor = state.redactor();
                if let Some(h) = &auth_header {
                    redactor.add_authorization_value(h);
                }
                let summary_message = redactor.redact(&error.message);
                let status = error.status;
                let message = match status {
                    Some(401) => "Authentication failed. Please check your credentials.".to_owned(),
                    Some(403) => "Access denied. Please verify:\n1. You have admin or developer role\n2. You have access to the application\n3. The application is active".to_owned(),
                    Some(404) => format!("Application not found with sys_id: {app_sys_id}"),
                    Some(s) if s >= 500 => format!("ServiceNow server error ({s}). Please try again later."),
                    _ => summary_message,
                };
                eprintln!("Failed to set current application: {message}");
                let mut safe =
                    ClientError::plain(format!("Failed to set current application: {message}"));
                safe.code = error.code;
                safe.status = status;
                Err(safe)
            }
        }
    }

    async fn set_current_application_inner(
        &self,
        app_sys_id: &str,
        start: i64,
        auth_header: &mut Option<String>,
    ) -> Result<Value, ClientError> {
        if app_sys_id.is_empty() {
            return Err(ClientError::plain("app_sys_id is required"));
        }
        let is_global = app_sys_id == "global";
        if !is_global && !is_sys_id(app_sys_id) {
            return Err(ClientError::plain(format!(
                "Invalid sys_id format: {app_sys_id}. Must be \"global\" or a 32-character lowercase hexadecimal sys_id."
            )));
        }
        let table = if is_global { "sys_scope" } else { "sys_app" };
        let app = match self
            .get_record(table, app_sys_id, &RecordParams::default())
            .await
        {
            Ok(app) => app,
            Err(e) if e.status == Some(404) => {
                return Err(ClientError::plain(format!(
                    "Application not found with sys_id: {app_sys_id}. Please verify the sys_id is correct."
                )));
            }
            Err(e) => return Err(e),
        };
        let session = self.open_ui_picker_session().await?;
        *auth_header = Some(session.auth_header.clone());

        let previous_scope = match session.get(APPLICATION_PICKER).await {
            Ok(previous) => {
                let result = previous.get("result").cloned().unwrap_or(Value::Null);
                match result.get("current").filter(|v| truthy(v)) {
                    Some(current) => {
                        let name = result
                            .get("list")
                            .and_then(Value::as_array)
                            .and_then(|list| {
                                list.iter().find(|c| {
                                    let id = c
                                        .get("sysId")
                                        .filter(|v| truthy(v))
                                        .or_else(|| c.get("sys_id"));
                                    id == Some(current)
                                })
                            })
                            .and_then(|e| e.get("name"))
                            .filter(|v| truthy(v))
                            .cloned()
                            .unwrap_or(Value::Null);
                        json!({"sys_id": current, "name": name})
                    }
                    None => Value::Null,
                }
            }
            Err(e) => {
                eprintln!("Could not retrieve previous scope: {}", e.message);
                Value::Null
            }
        };

        let response = session
            .put(APPLICATION_PICKER, &json!({"app_id": app_sys_id}))
            .await?;
        self.sleep_ms(500).await;
        let (verified, verification_error) = match session.get(APPLICATION_PICKER).await {
            Ok(v) => {
                let current = str_at(&v, &["result", "current"]);
                if current == Some(app_sys_id) {
                    (true, None)
                } else {
                    (
                        false,
                        Some(format!(
                            "Verification failed: Current app is {}, expected {app_sys_id}",
                            current.filter(|s| !s.is_empty()).unwrap_or("unknown")
                        )),
                    )
                }
            }
            Err(e) => (
                false,
                Some(format!("Verification query failed: {}", e.message)),
            ),
        };
        let now = self.now_ms();
        let mut warnings = Vec::new();
        if !verified {
            warnings.push("Could not verify scope change - please check ServiceNow UI".to_owned());
        }
        if let Some(e) = &verification_error {
            warnings.push(e.clone());
        }
        let scope = app
            .get("scope")
            .filter(|v| truthy(v))
            .cloned()
            .unwrap_or_else(|| json!("global"));
        Ok(json!({
            "success": true,
            "application": app.get("name").cloned().unwrap_or(Value::Null),
            "scope": scope,
            "sys_id": app_sys_id,
            "previous_scope": previous_scope,
            "verified": verified,
            "verification_error": verification_error,
            "timestamp": format_iso(now),
            "execution_time_ms": now - start,
            "method": "ui_api",
            "endpoint": APPLICATION_PICKER,
            "response": response,
            "warnings": warnings,
        }))
    }
}

/// JS truthiness for JSON values.
pub(crate) fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}
