//! Replays `fixtures/golden/tools/**` (captured from 6.2.0 by
//! `scripts/golden/stages/tools.mjs`) through the real dispatcher and MCP stdio
//! handler against Rust ports of the loopback ServiceNow and raw-GitHub stubs.
//!
//! One test per family: `cargo test -p hpm-tools --test golden_tools -- crud::`.
//! A lane that ports a family adds its `register` to [`register_ported`] and removes
//! the family's `#[ignore]`. A golden whose tool is not registered fails with
//! `not ported: <tool>`.

mod support;

use hpm_tools::registration::{RegistrationError, ToolRegistry};
use support::harness::{self, Sandbox};

/// Every ported family's registration. Lanes append their `register` here.
fn register_ported(
    _registry: &mut ToolRegistry,
    _sandbox: &Sandbox,
) -> Result<(), RegistrationError> {
    Ok(())
}

macro_rules! family {
    ($module:ident, $dir:literal) => {
        mod $module {
            #[tokio::test]
            #[ignore = "W3: family not ported yet; remove when its lane registers it"]
            async fn replay() {
                super::harness::replay_family($dir, super::register_ported).await;
            }
        }
    };
}

family!(atf, "atf");
family!(batch, "batch");
family!(catalog, "catalog");
family!(crud, "crud");
family!(docs, "docs");
family!(instances, "instances");
family!(itsm, "itsm");
family!(itsm_ops, "itsm-ops");
family!(schema, "schema");
family!(scripts, "scripts");
family!(update_sets, "update-sets");
family!(workflow, "workflow");

/// Every golden directory has a family test above.
#[test]
fn every_family_has_a_test() {
    let mut dirs: Vec<String> = std::fs::read_dir(harness::tools_dir())
        .expect("tools goldens")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    dirs.sort();
    assert_eq!(
        dirs,
        [
            "atf",
            "batch",
            "catalog",
            "crud",
            "docs",
            "instances",
            "itsm",
            "itsm-ops",
            "schema",
            "scripts",
            "update-sets",
            "workflow"
        ]
    );
}

/// The harness itself: a test-only SN-Query-Table handler that mimics 6.2.0
/// replays real goldens end to end, and mismatches are reported.
mod selftest {
    use std::sync::{Arc, LazyLock};

    use hpm_tools::registration::{RegistrationError, ToolRegistry};
    use hpm_types::BoxFut;
    use hpm_types::classification::ToolClassificationSet;
    use hpm_types::context::RequestContext;
    use hpm_types::credential::{CredentialKind, CredentialNeed, CredentialProvider};
    use hpm_types::operation::{Action, Operation, OperationSet, Subject};
    use hpm_types::tool::{
        HandlerCompletion, NormalizeInput, ResultBody, SideEffectClass, ToolDescriptor, ToolError,
        ToolHandler,
    };
    use serde_json::{Value, json};

    use super::support::harness::{self, Sandbox, SandboxCredentials, World};

    const TOOL: &str = "SN-Query-Table";

    struct QueryTable {
        credentials: Arc<SandboxCredentials>,
        /// Negative control: sends `sysparm_limit` one higher than asked.
        wrong_limit: bool,
    }

    /// JS truthiness of a request argument rendered as a query value.
    fn param(v: Option<&Value>) -> Option<String> {
        match v? {
            Value::String(s) if !s.is_empty() => Some(s.clone()),
            Value::Number(n) if n.as_f64() != Some(0.0) => Some(n.to_string()),
            _ => None,
        }
    }

    impl QueryTable {
        async fn run(&self, ctx: &RequestContext, args: &Value) -> Result<ResultBody, ToolError> {
            let instance = ctx
                .instance()
                .ok_or_else(|| ToolError::Internal("no instance".into()))?;
            let binding = ctx
                .credential()
                .ok_or_else(|| ToolError::Internal("no credential".into()))?;
            let credential = self.credentials.authorize(binding).await?;
            let CredentialKind::Authorization(auth) = credential.kind else {
                return Err(ToolError::Internal("unexpected credential".into()));
            };
            let table = args["table_name"].as_str().unwrap_or_default();
            // `const { limit = 25 } = args`, then `getRecords` param order.
            let mut limit = args.get("limit").cloned().unwrap_or(json!(25));
            if self.wrong_limit {
                limit = json!(limit.as_u64().unwrap_or(0) + 1);
            }
            let mut url =
                reqwest::Url::parse(&format!("{}/api/now/table/{table}", instance.canonical_url))
                    .map_err(|e| ToolError::Internal(e.to_string()))?;
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
            let response = reqwest::Client::new()
                .get(url)
                .header("authorization", auth.expose())
                .header("accept", "application/json")
                .header("content-type", "application/json")
                .send()
                .await
                .map_err(|e| ToolError::Internal(e.to_string()))?;
            let status = response.status().as_u16();
            let body: Value = response
                .json()
                .await
                .map_err(|e| ToolError::Internal(e.to_string()))?;
            if status >= 400 {
                return Err(ToolError::Upstream {
                    status,
                    message: body["error"]["message"].as_str().unwrap_or("").to_owned(),
                });
            }
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

    fn descriptor(sandbox: &Sandbox, wrong_limit: bool) -> ToolDescriptor {
        let classes = ToolClassificationSet::bundled().expect("D-05 classification");
        let row = classes.get(TOOL).expect("D-05 row");
        ToolDescriptor::new(
            TOOL,
            SPEC.0.as_str(),
            SPEC.1.as_str(),
            normalize,
            Arc::new(QueryTable {
                credentials: sandbox.credentials.clone(),
                wrong_limit,
            }),
            render,
        )
        .access(row.descriptor_floor())
        .output(row.output)
        .retry(row.retry.eligibility())
        .side_effect(SideEffectClass::None)
        .needs_instance(true)
        .credentials(CredentialNeed::Rest)
    }

    fn register_query_table(r: &mut ToolRegistry, s: &Sandbox) -> Result<(), RegistrationError> {
        r.register_tool(descriptor(s, false))
    }

    fn register_wrong_limit(r: &mut ToolRegistry, s: &Sandbox) -> Result<(), RegistrationError> {
        r.register_tool(descriptor(s, true))
    }

    fn register_nothing(_: &mut ToolRegistry, _: &Sandbox) -> Result<(), RegistrationError> {
        Ok(())
    }

    fn golden(rel: &str) -> std::path::PathBuf {
        harness::tools_dir().join(rel)
    }

    /// Known protocol gap (base-owned `hpm-protocol` handler): success results carry
    /// `isError: false`, 6.2.0 omits the key. Text, outbound list, notifications and
    /// files all replay clean, so this is the only remaining diff. When the handler
    /// is fixed this test fails; then assert `replay(..).is_ok()` instead.
    const IS_ERROR_GAP: &str = "step 0 response at /result/isError:\n  got:  false\n  want: null";

    #[tokio::test]
    async fn query_table_goldens_replay_end_to_end() {
        let world = World::start().await;
        let mut unexpected = Vec::new();
        for rel in [
            "crud/SN-Query-Table__basic.json",
            "crud/SN-Query-Table__offset-defaults.json",
            "crud/SN-Query-Table__invalid-table-arg.json",
        ] {
            match harness::replay(&world, &golden(rel), register_query_table).await {
                Err(e) if e == format!("{rel}\n{IS_ERROR_GAP}") => {}
                other => unexpected.push(format!("{rel}: {other:?}")),
            }
        }
        assert!(unexpected.is_empty(), "{}", unexpected.join("\n\n"));
    }

    #[tokio::test]
    async fn outbound_mismatch_is_reported() {
        let world = World::start().await;
        let err = harness::replay(
            &world,
            &golden("crud/SN-Query-Table__basic.json"),
            register_wrong_limit,
        )
        .await
        .expect_err("a wrong sysparm_limit must not replay clean");
        assert!(err.contains("step 0 outbound"), "{err}");
        assert!(err.contains("sysparm_limit=3"), "{err}");
        assert!(err.contains("sysparm_limit=2"), "{err}");
    }

    #[tokio::test]
    async fn unported_tool_is_reported() {
        let world = World::start().await;
        let err = harness::replay(
            &world,
            &golden("crud/SN-Get-Record__basic.json"),
            register_nothing,
        )
        .await
        .expect_err("no handler registered");
        assert!(err.ends_with("not ported: SN-Get-Record"), "{err}");
    }
}

/// The Rust stubs answer as `scripts/golden/lib/stub-sn.mjs` does. Expected values
/// were taken from the JS stub (`startStub` / `startRawStub`) for the same requests.
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
