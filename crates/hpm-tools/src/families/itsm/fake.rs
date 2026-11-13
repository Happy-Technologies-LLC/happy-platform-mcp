//! Scripted ServiceNow for the unit tests: answers in order, logs every request.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use hpm_core::client::{ClientError, RecordParams, RecordQuery};
use serde_json::Value;

use super::Sn;
use crate::logic::js::json_stringify;

#[derive(Default)]
pub(super) struct Fake {
    answers: Mutex<VecDeque<Result<Value, ClientError>>>,
    log: Mutex<Vec<String>>,
    dead: AtomicBool,
}

impl Fake {
    pub(super) fn new() -> Self {
        Self::default()
    }
    pub(super) fn ok(self, v: Value) -> Self {
        self.answers.lock().expect("lock").push_back(Ok(v));
        self
    }
    /// An HTTP error as the client reports it.
    pub(super) fn http(self, status: u16, message: &str) -> Self {
        let e = ClientError::plain(message).with_status(Some(status));
        self.answers.lock().expect("lock").push_back(Err(e));
        self
    }
    /// A transport error (no status).
    pub(super) fn transport(self, message: &str) -> Self {
        self.answers
            .lock()
            .expect("lock")
            .push_back(Err(ClientError::plain(message)));
        self
    }
    /// Any client error, as-is.
    pub(super) fn err(self, e: ClientError) -> Self {
        self.answers.lock().expect("lock").push_back(Err(e));
        self
    }
    pub(super) fn cancelled(self) -> Self {
        self.dead.store(true, Ordering::SeqCst);
        self
    }
    pub(super) fn calls(&self) -> Vec<String> {
        self.log.lock().expect("lock").clone()
    }
    fn answer(&self, line: String) -> Result<Value, ClientError> {
        self.log.lock().expect("lock").push(line);
        self.answers
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or(Ok(Value::Null))
    }
}

impl Sn for Fake {
    fn live(&self) -> bool {
        !self.dead.load(Ordering::SeqCst)
    }
    async fn records(&self, table: &str, q: &RecordQuery) -> Result<Value, ClientError> {
        let mut line = format!("GET {table}");
        for (k, v) in [
            ("query", &q.query),
            ("limit", &q.limit),
            ("fields", &q.fields),
            ("offset", &q.offset),
            ("order_by", &q.order_by),
        ] {
            if let Some(v) = v {
                line.push_str(&format!(" {k}={v}"));
            }
        }
        self.answer(line)
    }
    async fn record(
        &self,
        table: &str,
        sys_id: &str,
        p: &RecordParams,
    ) -> Result<Value, ClientError> {
        let mut line = format!("GET {table}/{sys_id}");
        if let Some(f) = &p.fields {
            line.push_str(&format!(" fields={f}"));
        }
        self.answer(line)
    }
    async fn create(&self, table: &str, data: &Value) -> Result<Value, ClientError> {
        self.answer(format!("POST {table} {}", json_stringify(data)))
    }
    async fn update(&self, table: &str, sys_id: &str, data: &Value) -> Result<Value, ClientError> {
        self.answer(format!("PUT {table}/{sys_id} {}", json_stringify(data)))
    }
    async fn post(&self, path: &str, body: &Value) -> Result<Value, ClientError> {
        self.answer(format!("POST {path} {}", json_stringify(body)))
    }
}
