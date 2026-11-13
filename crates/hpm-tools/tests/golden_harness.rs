//! Self-tests of the golden replay harness (`support/`): a test-only
//! `SN-Query-Table` that mimics 6.2.0 replays real goldens end to end through the
//! single-user credential provider and pinned clients, every comparison the harness
//! makes is shown to catch a deliberate mismatch, and the Rust stubs answer as
//! `scripts/golden/lib/stub-sn.mjs` does.

#[allow(dead_code, reason = "support is shared with golden_tools.rs")]
mod support;

mod selftest {
    use std::sync::{Arc, LazyLock};

    use hpm_core::client::http::Method;
    use hpm_core::credential::provider::{InstanceClients, SingleUserCredentialProvider};
    use hpm_tools::dispatcher::{Dispatcher, Services};
    use hpm_tools::registration::ToolRegistry;
    use hpm_tools::schema::CompiledSchemaValidator;
    use hpm_tools::single_user::{AllowAll, PassThrough, StderrAudit, UnloadedClassification};
    use hpm_types::BoxFut;
    use hpm_types::classification::ToolClassificationSet;
    use hpm_types::context::{AdmissionKey, RequestContext};
    use hpm_types::credential::CredentialNeed;
    use hpm_types::operation::{Action, Operation, OperationSet, ScriptOperandGuard, Subject};
    use hpm_types::servicenow::ServiceNowAccess;
    use hpm_types::tool::{
        HandlerCompletion, NormalizeInput, ResultBody, SideEffectClass, ToolDescriptor, ToolError,
        ToolHandler,
    };
    use serde_json::{Value, json};

    use super::support::harness::{self, Sandbox, World};

    const TOOL: &str = "SN-Query-Table";

    /// Deliberate deviations, one per comparison the harness makes.
    #[derive(Clone, Copy, PartialEq)]
    enum Variant {
        Faithful,
        /// Outbound: `sysparm_limit` one higher than asked.
        WrongLimit,
        /// Instance: ignores the per-call `instance` and queries the default.
        IgnoreInstance,
    }

    struct QueryTable {
        clients: Arc<InstanceClients>,
        variant: Variant,
    }

    /// JS truthiness of an argument rendered as a query value.
    fn param(v: Option<&Value>) -> Option<String> {
        match v? {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) if n.as_f64() != Some(0.0) => Some(n.to_string()),
            _ => None,
        }
    }

    impl QueryTable {
        async fn run(&self, ctx: &RequestContext, args: &Value) -> Result<ResultBody, ToolError> {
            let client = if self.variant == Variant::IgnoreInstance {
                // The default (`dev`) client, whatever the call resolved.
                let mut dev = ctx
                    .instance()
                    .ok_or(ToolError::Internal("no instance".into()))?
                    .clone();
                dev.instance_id = "dev".into();
                let c = self.clients.client_for(&dev)?;
                c.pinned(c.generation())?
            } else {
                self.clients.client(ctx).await?
            };
            let table = args["table_name"].as_str().unwrap_or_default();
            // `const { limit = 25 } = args`, then `getRecords` param order.
            let mut limit = args.get("limit").cloned().unwrap_or(json!(25));
            if self.variant == Variant::WrongLimit {
                limit = json!(limit.as_u64().unwrap_or(0) + 1);
            }
            let mut url = reqwest::Url::parse("http://x.invalid/").expect("base url");
            url.set_path(&format!("/api/now/table/{table}"));
            {
                let mut q = url.query_pairs_mut();
                for (key, value) in [
                    ("sysparm_query", param(args.get("query"))),
                    ("sysparm_limit", param(Some(&limit))),
                    ("sysparm_fields", param(args.get("fields"))),
                    ("sysparm_offset", param(args.get("offset"))),
                    ("sysparm_order_by", param(args.get("order_by"))),
                ] {
                    if let Some(v) = value {
                        q.append_pair(key, &v);
                    }
                }
            }
            let path = match url.query() {
                Some(q) => format!("{}?{q}", url.path()),
                None => url.path().to_owned(),
            };
            let body = client.rest(Method::Get, &path, None).await?;
            Ok(ResultBody::Json(
                json!({"table": table, "result": body["result"]}),
            ))
        }
    }

    impl ToolHandler for QueryTable {
        fn call<'a>(
            &'a self,
            ctx: &'a RequestContext,
            args: Value,
        ) -> BoxFut<'a, HandlerCompletion> {
            Box::pin(async move { HandlerCompletion::not_a_mutation(self.run(ctx, &args).await) })
        }
    }

    fn normalize(args: &Value, input: &NormalizeInput<'_>) -> Result<OperationSet, ToolError> {
        let table = args["table_name"].as_str().unwrap_or_default();
        Ok(OperationSet(vec![
            Operation::new(Subject::Tool(TOOL), Action::Read).on_table(table, input.classification),
        ]))
    }

    /// `Found ${results.length} records in ${table_name}:\n${JSON.stringify(results, null, 2)}`
    fn render(body: &ResultBody) -> String {
        let ResultBody::Json(v) = body else {
            return String::new();
        };
        let rows = v["result"].as_array().map_or(0, Vec::len);
        format!(
            "Found {rows} records in {}:\n{}",
            v["table"].as_str().unwrap_or_default(),
            serde_json::to_string_pretty(&v["result"]).unwrap_or_default()
        )
    }

    /// 6.2.0 description and schema bytes, read once from the tools list.
    static SPEC: LazyLock<(String, String)> = LazyLock::new(|| {
        let list: Value = serde_json::from_str(
            &std::fs::read_to_string(harness::repo_root().join("fixtures/tools-list-6.2.0.json"))
                .expect("tools list"),
        )
        .expect("tools list json");
        let spec = list["tools"]
            .as_array()
            .and_then(|t| t.iter().find(|t| t["name"] == TOOL))
            .expect("SN-Query-Table in the 6.2.0 tools list");
        (
            spec["description"].as_str().unwrap_or_default().to_owned(),
            spec["inputSchema"].to_string(),
        )
    });

    /// The single-user composition (`hpm_cli::SingleUser::dispatcher`) with the
    /// test tool as the only registration.
    fn compose(
        sandbox: &Sandbox,
        key: AdmissionKey,
        variant: Variant,
    ) -> Result<Arc<Dispatcher>, String> {
        let clients = Arc::new(InstanceClients::from_config(
            sandbox.config.clone(),
            sandbox.client_deps(),
        ));
        let classes = ToolClassificationSet::bundled().map_err(|e| e.to_string())?;
        let row = classes.get(TOOL).ok_or("D-05 row")?;
        let descriptor = ToolDescriptor::new(
            TOOL,
            SPEC.0.as_str(),
            SPEC.1.as_str(),
            normalize,
            Arc::new(QueryTable {
                clients: clients.clone(),
                variant,
            }),
            render,
        )
        .access(row.descriptor_floor())
        .output(row.output)
        .retry(row.retry.eligibility())
        .side_effect(SideEffectClass::None)
        .needs_instance(true)
        .credentials(CredentialNeed::Rest);
        let mut registry = ToolRegistry::new().map_err(|e| e.to_string())?;
        registry
            .register_tool(descriptor)
            .map_err(|e| e.to_string())?;
        let services = Services {
            validator: Arc::new(
                CompiledSchemaValidator::new(registry.tools()).map_err(|e| e.to_string())?,
            ),
            classification: Arc::new(UnloadedClassification),
            policy: Arc::new(AllowAll),
            protector: Arc::new(PassThrough),
            credentials: Arc::new(SingleUserCredentialProvider::new(clients)),
            audit: Arc::new(StderrAudit),
            script_guard: ScriptOperandGuard {
                shared_account: false,
            },
            usage: None,
        };
        Ok(Arc::new(Dispatcher::new(registry, services, key)))
    }

    fn faithful(s: &Sandbox, k: AdmissionKey) -> Result<Arc<Dispatcher>, String> {
        compose(s, k, Variant::Faithful)
    }
    fn wrong_limit(s: &Sandbox, k: AdmissionKey) -> Result<Arc<Dispatcher>, String> {
        compose(s, k, Variant::WrongLimit)
    }
    fn ignore_instance(s: &Sandbox, k: AdmissionKey) -> Result<Arc<Dispatcher>, String> {
        compose(s, k, Variant::IgnoreInstance)
    }

    fn golden(rel: &str) -> std::path::PathBuf {
        harness::tools_dir().join(rel)
    }

    #[tokio::test]
    async fn query_table_goldens_replay_clean() {
        let world = World::start().await;
        let mut failures = Vec::new();
        for rel in [
            "crud/SN-Query-Table__basic.json",
            "crud/SN-Query-Table__offset-defaults.json",
            "crud/SN-Query-Table__invalid-table-arg.json",
            "crud/SN-Query-Table__http-403.json",
            "instances/SN-Query-Table__per-call-instance-override.json",
        ] {
            if let Err(e) = harness::replay_with(&world, &golden(rel), faithful).await {
                failures.push(e);
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    }

    #[tokio::test]
    async fn outbound_mismatch_is_reported() {
        let world = World::start().await;
        let err = harness::replay_with(
            &world,
            &golden("crud/SN-Query-Table__basic.json"),
            wrong_limit,
        )
        .await
        .expect_err("a wrong sysparm_limit must not replay clean");
        assert!(err.contains("step 0 outbound"), "{err}");
        assert!(err.contains("sysparm_limit=3"), "{err}");
        assert!(err.contains("sysparm_limit=2"), "{err}");
    }

    #[tokio::test]
    async fn unported_tool_is_reported() {
        // A name no 6.2.0 tool (or lane) can ever register; the golden is
        // synthesized so the selftest never depends on fixtures/** or lane state.
        const FAKE: &str = "Test-Only-Never-A-Tool";
        assert!(hpm_tools::tool_order::rank(FAKE).is_none());
        let world = World::start().await;
        let file =
            std::env::temp_dir().join(format!("hpm-selftest-unported-{}.json", std::process::id()));
        let golden = serde_json::json!({
            "id": "selftest/unported",
            "steps": [{"request": {"method": "tools/call",
                "params": {"name": FAKE, "arguments": {}}}}]
        });
        std::fs::write(&file, golden.to_string()).expect("write synthetic golden");
        let err = harness::replay_with(&world, &file, harness::production)
            .await
            .expect_err("a never-real tool cannot be registered");
        let _ = std::fs::remove_file(&file);
        assert!(err.ends_with(&format!("not ported: {FAKE}")), "{err}");
    }

    #[tokio::test]
    async fn wrong_instance_is_reported() {
        let world = World::start().await;
        let err = harness::replay_with(
            &world,
            &golden("instances/SN-Query-Table__per-call-instance-override.json"),
            ignore_instance,
        )
        .await
        .expect_err("a request to dev for instance=prod must not replay clean");
        assert!(
            err.ends_with(
                "step 0 instance: a ServiceNow request reached {{INSTANCE_HOST}}, expected {{INSTANCE_HOST_ALT}}"
            ),
            "{err}"
        );
    }

    #[test]
    fn only_a_final_cookie_logout_is_ignored() {
        let entry = |path: &str, cookie: bool| {
            json!({"target": "servicenow", "method": "GET", "path": path,
                "credentials": {"scheme": "basic", "sessionCookie": cookie}})
        };
        let read = entry("/api/now/table/sys_script_execution_history/x", false);
        let logout = entry("/logout.do", true);
        // Final cookie-session logout: the intentional 7.0 difference, dropped.
        assert_eq!(
            harness::without_final_ui_logout(vec![read.clone(), logout.clone()]),
            vec![read.clone()]
        );
        // A logout before a later read stays and is compared.
        let early = vec![logout.clone(), read.clone()];
        assert_eq!(harness::without_final_ui_logout(early.clone()), early);
        // Without the session cookie it is not the UI logout.
        let plain = vec![entry("/logout.do", false)];
        assert_eq!(harness::without_final_ui_logout(plain.clone()), plain);
    }

    #[test]
    fn absent_key_differs_from_null() {
        let got = json!({"result": {"content": []}, "error": null});
        let want = json!({"result": {"content": []}});
        let diff = harness::first_difference(&got, &want, String::new()).expect("reported");
        assert!(diff.starts_with("at /error: only in got"), "{diff}");
        let diff = harness::first_difference(&want, &got, String::new()).expect("reported");
        assert!(diff.starts_with("at /error: only in golden"), "{diff}");
        assert!(harness::first_difference(&got, &got, String::new()).is_none());
    }
}

mod stub_selftest {
    use serde_json::{Value, json};

    use super::support::scrub::Scrubber;
    use super::support::stub_sn::{RawStub, StubSn, sys_id};

    #[test]
    fn sys_id_matches_js() {
        assert_eq!(sys_id("arg", 1), "8dce91cd01ccb903cce2773e4168a30b");
        assert_eq!(sys_id("arg", 10), "ed62390adb712ed83486d9105c442b81");
        assert_eq!(sys_id("incident", 1), "2fc8aa1ec6d67779a3ab923ac2cc7238");
        assert_eq!(
            sys_id("created:problem", 1),
            "1b47749ea402cb0ed158428466570829"
        );
    }

    struct Got {
        status: u16,
        content_type: String,
        total: Option<String>,
        cookies: Vec<String>,
        body: String,
    }

    async fn call(base: &str, method: &str, path: &str, body: Option<&str>) -> Got {
        let client = reqwest::Client::new();
        let mut req = client.request(
            reqwest::Method::from_bytes(method.as_bytes()).expect("method"),
            format!("{base}{path}"),
        );
        if let Some(b) = body {
            req = req
                .header("content-type", "application/json")
                .body(b.to_owned());
        }
        let res = req.send().await.expect("stub answers");
        let header = |n: &str| {
            res.headers()
                .get(n)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        Got {
            status: res.status().as_u16(),
            content_type: header("content-type").unwrap_or_default(),
            total: header("x-total-count"),
            cookies: res
                .headers()
                .get_all("set-cookie")
                .iter()
                .filter_map(|v| v.to_str().ok().map(str::to_owned))
                .collect(),
            body: res.text().await.expect("body"),
        }
    }

    const JSON_CT: &str = "application/json;charset=UTF-8";

    #[tokio::test]
    async fn servicenow_stub_matches_js_answers() {
        let stub = StubSn::start().await;
        stub.reset(None);
        let base = stub.origin();

        let list = call(
            &base,
            "GET",
            "/api/now/table/incident?sysparm_limit=2",
            None,
        )
        .await;
        assert_eq!((list.status, list.content_type.as_str()), (200, JSON_CT));
        assert_eq!(list.total.as_deref(), Some("2"));
        let rows: Value = serde_json::from_str(&list.body).expect("json");
        assert_eq!(
            rows["result"][0]["sys_id"],
            "2fc8aa1ec6d67779a3ab923ac2cc7238"
        );
        assert_eq!(
            rows["result"][1]["sys_id"],
            "f2bd9ee077a023364ec9be5301e9482c"
        );
        assert_eq!(
            rows["result"][0]["caller_id"],
            "8d1cba9bbb9a19d3a8ab22c62b5c2df1"
        );

        let by_number = call(
            &base,
            "GET",
            "/api/now/table/incident?sysparm_query=number%3DINC0010001",
            None,
        )
        .await;
        assert_eq!(by_number.total.as_deref(), Some("1"));
        let rows: Value = serde_json::from_str(&by_number.body).expect("json");
        assert_eq!(rows["result"][0]["number"], "INC0010001");

        let one = call(
            &base,
            "GET",
            "/api/now/table/sys_update_set/ed62390adb712ed83486d9105c442b81",
            None,
        )
        .await;
        assert_eq!(
            one.body,
            r#"{"result":{"sys_id":"ed62390adb712ed83486d9105c442b81","sys_created_on":"2026-01-15 12:00:00","sys_updated_on":"2026-01-15 12:00:00","sys_created_by":"golden.user","sys_updated_by":"golden.user","sys_class_name":"sys_update_set","name":"Golden Update Set 1","description":"Golden update set","state":"in progress","application":"09d04d9cdba21c4d3e07fd98f37a1db9","is_default":"false","parent":"","base_update_set":""}}"#
        );

        let created = call(
            &base,
            "POST",
            "/api/now/table/problem",
            Some(r#"{"short_description":"x"}"#),
        )
        .await;
        assert_eq!(created.status, 201);
        assert_eq!(
            created.body,
            r#"{"result":{"sys_id":"1b47749ea402cb0ed158428466570829","sys_created_on":"2026-01-15 12:00:00","sys_updated_on":"2026-01-15 12:00:00","sys_created_by":"golden.user","sys_updated_by":"golden.user","sys_class_name":"problem","number":"PRB0000101","short_description":"x","state":"1","priority":"3"}}"#
        );

        let patched = call(
            &base,
            "PATCH",
            "/api/now/table/incident/abc",
            Some(r#"{"state":"2"}"#),
        )
        .await;
        let row: Value = serde_json::from_str(&patched.body).expect("json");
        assert_eq!(
            (
                row["result"]["sys_id"].clone(),
                row["result"]["state"].clone()
            ),
            (json!("abc"), json!("2"))
        );

        let deleted = call(&base, "DELETE", "/api/now/table/incident/abc", None).await;
        assert_eq!((deleted.status, deleted.body.as_str()), (204, ""));

        let app = call(
            &base,
            "GET",
            "/api/now/ui/concoursepicker/application",
            None,
        )
        .await;
        assert_eq!(
            app.body,
            r#"{"result":{"current":"global","list":[{"sysId":"global","name":"Global"},{"sysId":"c6a53127a812c575a4c21a03ce479278","name":"Golden App 1"}]}}"#
        );

        let unrouted = call(&base, "GET", "/nope", None).await;
        assert_eq!(unrouted.status, 404);
        assert_eq!(
            unrouted.body,
            r#"{"error":{"message":"No Processor or Table API for GET /nope","detail":"golden stub: unrouted"},"status":"failure"}"#
        );

        let root = call(&base, "GET", "/", None).await;
        assert_eq!(root.content_type, "text/html;charset=UTF-8");
        assert_eq!(root.body, "<html><body>golden</body></html>");
        assert_eq!(
            root.cookies,
            [
                "JSESSIONID=golden-session-1; Path=/; HttpOnly",
                "glide_user_route=golden-route; Path=/"
            ]
        );

        // Recorded requests, as the JS `take()` reported them.
        let recorded: Vec<Value> = stub
            .take()
            .iter()
            .map(|r| {
                let mut v = r.to_json("servicenow");
                v.as_object_mut().expect("object").remove("headers");
                v
            })
            .collect();
        let none = json!({"scheme": "none", "sessionCookie": false, "userToken": false});
        assert_eq!(recorded.len(), 9);
        assert_eq!(
            recorded[1],
            json!({"target": "servicenow", "method": "GET", "path": "/api/now/table/incident", "rawQuery": "sysparm_query=number%3DINC0010001", "query": [["sysparm_query", "number=INC0010001"]], "credentials": none, "body": null})
        );
        assert_eq!(recorded[3]["body"], json!({"short_description": "x"}));
    }

    #[tokio::test]
    async fn raw_github_stub_matches_js_answers() {
        let raw = RawStub::start(&[("/a/b.txt", "hello\n")]).await;
        let ok = call(&raw.base(), "GET", "/a/b.txt", None).await;
        assert_eq!(
            (ok.status, ok.content_type.as_str(), ok.body.as_str()),
            (200, "text/plain; charset=utf-8", "hello\n")
        );
        let missing = call(&raw.base(), "GET", "/zz", None).await;
        assert_eq!(
            (
                missing.status,
                missing.content_type.as_str(),
                missing.body.as_str()
            ),
            (404, "text/plain", "not found")
        );
        let paths: Vec<String> = raw.take().into_iter().map(|r| r.path).collect();
        assert_eq!(paths, ["/a/b.txt", "/zz"]);
    }

    #[test]
    fn scrubber_applies_normalization_rules() {
        let s = Scrubber::new(vec![("/tmp/root".into(), "{{TMP}}".into())]);
        let cases = [
            ("at 2026-01-15 12:00:00", "at 2026-01-15 12:00:00"),
            ("at 2026-02-01 09:30:00", "at {{SN_DATETIME}}"),
            ("q=2026-01-15+12%3A00%3A00", "q=2026-01-15+12%3A00%3A00"),
            ("q=2026-01-01+00%3A00%3A00", "q={{SN_DATETIME_ENCODED}}"),
            (
                "/tmp/root/scripts/x_2026-01-01T00-00-00-000Z.js",
                "{{TMP}}/scripts/x_{{FILE_TIMESTAMP}}.js",
            ),
            ("12026-01-01T00-00-00-000Z", "12026-01-01T00-00-00-000Z"),
            ("took 12 ms", "took {{N}}ms"),
            (
                "MCP_1767225600000_abcdef12 done",
                "MCP_{{EPOCH_MS}}_{{MARKER}} done",
            ),
            ("Basic Z29sZGVuOnBhc3M=", "Basic {{CREDENTIAL}}"),
            ("JSESSIONID=abc; x", "JSESSIONID={{COOKIE}}; x"),
            ("dev451274.service-now.com", "{{SN_HOST}}"),
        ];
        for (input, want) in cases {
            assert_eq!(s.string(input), want, "{input}");
        }
        assert_eq!(
            s.deep(&json!({"2026-03-03T01:02:03.004Z": ["/tmp/root"]})),
            json!({"{{ISO_TIMESTAMP}}": ["{{TMP}}"]})
        );
    }
}
