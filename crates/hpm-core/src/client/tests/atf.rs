//! Port of tests/cicd-atf.test.js: real client against a fake transport.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicUsize;

use super::*;
use crate::client::cicd_atf::handle_atf_tool;

const PASSWORD: &str = "fake-password-xyz";
const TEST_ID: &str = "0123456789abcdef0123456789abcdef";
const SUITE_ID: &str = "1123456789abcdef0123456789abcdef";
const PROGRESS_ID: &str = "2123456789abcdef0123456789abcdef";
const RESULT_ID: &str = "3123456789abcdef0123456789abcdef";

#[derive(Clone, Debug)]
struct Req {
    method: Method,
    path: String,
    query: BTreeMap<String, String>,
}

fn parse(r: &HttpRequest) -> Req {
    let rest = r.url.trim_start_matches(URL);
    let (path, qs) = rest.split_once('?').unwrap_or((rest, ""));
    Req {
        method: r.method,
        path: path.to_owned(),
        query: url::form_urlencoded::parse(qs.as_bytes())
            .into_owned()
            .collect(),
    }
}

enum Body {
    Json(Value),
    Html(&'static str),
}

type Answer = Option<(u16, Body)>;
type AnswerFn = fn() -> Answer;

fn fake(routes: impl Fn(&Req) -> Answer + Send + Sync + 'static) -> Harness {
    let transport = FakeTransport::new(move |r| {
        let req = parse(r);
        let (status, body) = routes(&req).unwrap_or_else(|| {
            (404, Body::Json(json!({"error": {"message": format!("unexpected {:?} {}", req.method, req.path)}})))
        });
        Ok(match body {
            Body::Json(v) => HttpResponse::json(status, &v),
            Body::Html(h) => HttpResponse::new(status, h).with_header("content-type", "text/html"),
        })
    });
    harness(
        basic(Some(PASSWORD)),
        transport,
        FakePoster::new(vec![]),
        Value::Null,
    )
}

fn reqs(h: &Harness) -> Vec<Req> {
    h.transport.requests().iter().map(parse).collect()
}

fn ok(v: Value) -> Answer {
    Some((200, Body::Json(v)))
}

fn started() -> Answer {
    ok(json!({"result": {"status": "0", "links": {"progress": {"id": PROGRESS_ID, "url": "x"}}}}))
}

fn payload(result: &Value) -> Value {
    serde_json::from_str(result["content"][0]["text"].as_str().unwrap_or("null"))
        .unwrap_or_default()
}

fn q(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
        .collect()
}

/// `toMatchObject`: every key in `expected` matches recursively.
fn assert_subset(actual: &Value, expected: &Value) {
    match (actual, expected) {
        (Value::Object(a), Value::Object(e)) => {
            for (k, ev) in e {
                let av = a
                    .get(k)
                    .unwrap_or_else(|| panic!("missing key {k} in {actual}"));
                assert_subset(av, ev);
            }
        }
        (Value::Array(a), Value::Array(e)) => {
            assert_eq!(a.len(), e.len(), "{actual} vs {expected}");
            for (av, ev) in a.iter().zip(e) {
                assert_subset(av, ev);
            }
        }
        _ => assert_eq!(actual, expected),
    }
}

async fn err_of(h: &Harness, tool: &str, args: Value) -> ClientError {
    handle_atf_tool(tool, &args, &h.client)
        .await
        .err()
        .unwrap_or_else(|| panic!("{tool} should fail"))
}

// ---- SN-Run-ATF-Test --------------------------------------------------------

#[tokio::test]
async fn run_test_by_sys_id_polls_to_completion() {
    let n = Arc::new(AtomicUsize::new(0));
    let h = fake(move |r| {
        if r.method == Method::Post && r.path == "/api/sn_cicd/tests/run_test" {
            return started();
        }
        if r.path == format!("/api/sn_cicd/progress/{PROGRESS_ID}") {
            return if n.fetch_add(1, Ordering::SeqCst) == 0 {
                ok(
                    json!({"result": {"status": "1", "percent_complete": 40, "status_message": "Running"}}),
                )
            } else {
                ok(
                    json!({"result": {"status": "2", "percent_complete": 100, "links": {"results": {"id": RESULT_ID, "url": "https://x/api/sn_cicd/tests/test/results/r"}}}}),
                )
            };
        }
        if r.path == format!("/api/sn_cicd/tests/test/results/{RESULT_ID}") {
            return ok(
                json!({"result": {"test_name": "Login works", "test_status": "success", "output": "all good"}}),
            );
        }
        None
    });
    let result = handle_atf_tool(
        "SN-Run-ATF-Test",
        &json!({"test_sys_id": TEST_ID, "run_in_cloud": true}),
        &h.client,
    )
    .await
    .unwrap_or_default();
    let body = payload(&result);
    assert_eq!(result["isError"], false);
    assert_subset(
        &body,
        &json!({
            "status": "successful", "successful": true, "progress_id": PROGRESS_ID,
            "results": {"result_type": "test", "result_id": RESULT_ID, "test_name": "Login works", "test_status": "success", "output": "all good"}
        }),
    );
    assert_eq!(
        body["results"]["links"]["record"],
        format!("{URL}/sys_atf_test_result.do?sys_id={RESULT_ID}")
    );
    assert_eq!(
        body["results"]["links"]["api"],
        format!("{URL}/api/sn_cicd/tests/test/results/{RESULT_ID}")
    );
    let all = reqs(&h);
    let run = all.iter().find(|r| r.method == Method::Post).cloned();
    assert_eq!(
        run.map(|r| r.query),
        Some(q(&[("test_sys_id", TEST_ID), ("run_in_cloud", "true")]))
    );
    assert!(!all.iter().any(|r| r.path.contains("/table/sys_atf_test")));
    assert_eq!(h.clock.sleeps(), vec![2000]);
}

#[tokio::test]
async fn run_test_resolves_name_escaping_separators() {
    let h = fake(|r| {
        if r.path == "/api/now/table/sys_atf_test" {
            return ok(json!({"result": [{"sys_id": TEST_ID, "name": "a^b"}]}));
        }
        if r.method == Method::Post {
            return started();
        }
        None
    });
    let result = handle_atf_tool(
        "SN-Run-ATF-Test",
        &json!({"test_name": "a^b", "wait": false}),
        &h.client,
    )
    .await
    .unwrap_or_default();
    assert_subset(
        &payload(&result),
        &json!({"status": "started", "progress_id": PROGRESS_ID, "target": {"test_sys_id": TEST_ID, "test_name": "a^b"}}),
    );
    let all = reqs(&h);
    assert_eq!(all[0].query["sysparm_query"], "name=a^^b");
    assert_eq!(all[1].query["test_sys_id"], TEST_ID);
}

#[tokio::test]
async fn run_test_rejects_unresolvable_names_without_running() {
    for (rows, expected) in [
        (json!([]), "No ATF test found"),
        (
            json!([{"sys_id": TEST_ID}, {"sys_id": SUITE_ID}]),
            "Multiple ATF tests found",
        ),
    ] {
        let h = fake(move |r| {
            if r.path == "/api/now/table/sys_atf_test" {
                ok(json!({"result": rows.clone()}))
            } else {
                started()
            }
        });
        let e = err_of(&h, "SN-Run-ATF-Test", json!({"test_name": "Login"})).await;
        assert!(e.message.contains(expected), "{}", e.message);
        if expected.starts_with("Multiple") {
            assert!(e.message.contains("test_sys_id"));
        }
        assert!(!reqs(&h).iter().any(|r| r.method == Method::Post));
    }
}

#[tokio::test]
async fn run_test_rejects_bad_input_before_any_request() {
    for (args, expected) in [
        (
            json!({"test_sys_id": TEST_ID, "test_name": "Login"}),
            "mutually exclusive",
        ),
        (json!({}), "Provide either test_sys_id or test_name"),
        (
            json!({"test_sys_id": "not-a-sys-id"}),
            "test_sys_id must be a 32-character",
        ),
        (
            json!({"test_sys_id": TEST_ID, "timeout_seconds": 601}),
            "timeout_seconds must be a number from 1 to 600",
        ),
    ] {
        let h = fake(|_| started());
        let e = err_of(&h, "SN-Run-ATF-Test", args).await;
        assert!(e.message.contains(expected), "{}", e.message);
        assert!(h.transport.requests().is_empty());
    }
}

#[tokio::test]
async fn run_test_timeout_returns_progress_id() {
    let h = fake(|r| {
        if r.method == Method::Post {
            return started();
        }
        if r.path == format!("/api/sn_cicd/progress/{PROGRESS_ID}") {
            return ok(json!({"result": {"status": "1", "percent_complete": 10}}));
        }
        None
    });
    let result = handle_atf_tool(
        "SN-Run-ATF-Test",
        &json!({"test_sys_id": TEST_ID, "timeout_seconds": 5, "poll_interval_seconds": 2}),
        &h.client,
    )
    .await
    .unwrap_or_default();
    let body = payload(&result);
    assert_eq!(result["isError"], false);
    assert_subset(
        &body,
        &json!({"status": "timeout", "progress_id": PROGRESS_ID, "progress": {"state": "running", "percent_complete": 10, "finished": false}}),
    );
    assert!(
        body["next_step"]
            .as_str()
            .unwrap_or("")
            .contains("SN-Get-CICD-Progress")
    );
    assert_eq!(h.clock.sleeps(), vec![2000, 2000, 1000]);
}

#[tokio::test]
async fn run_test_progress_failure_keeps_progress_id() {
    let h = fake(|r| {
        if r.method == Method::Post {
            return started();
        }
        if r.path.starts_with("/api/sn_cicd/progress/") {
            return Some((500, Body::Json(json!({"error": {"message": "boom"}}))));
        }
        None
    });
    let result = handle_atf_tool(
        "SN-Run-ATF-Test",
        &json!({"test_sys_id": TEST_ID}),
        &h.client,
    )
    .await
    .unwrap_or_default();
    assert_eq!(result["isError"], true);
    assert_subset(
        &payload(&result),
        &json!({"status": "unknown", "progress_id": PROGRESS_ID}),
    );
}

// ---- SN-Run-ATF-Suite -------------------------------------------------------

#[tokio::test]
async fn run_suite_reports_counts_and_rows_on_failure() {
    let h = fake(|r| {
        if r.method == Method::Post && r.path == "/api/sn_cicd/testsuite/run" {
            return started();
        }
        if r.path == format!("/api/sn_cicd/progress/{PROGRESS_ID}") {
            return ok(
                json!({"result": {"status": "3", "status_message": "Test suite failed", "links": {"results": {"id": RESULT_ID}}}}),
            );
        }
        if r.path == format!("/api/sn_cicd/testsuite/results/{RESULT_ID}") {
            return ok(json!({"result": {
                "test_suite_name": "Smoke", "test_suite_status": "failure",
                "rolledup_test_success_count": 3, "rolledup_test_failure_count": 1,
                "rolledup_test_error_count": 0, "rolledup_test_skip_count": 2
            }}));
        }
        if r.path == "/api/now/table/sys_atf_test_result" {
            return ok(json!({"result": [
                {"sys_id": TEST_ID, "test": "Login works", "status": "Success"},
                {"sys_id": SUITE_ID, "test": "Checkout", "status": "Failure", "output": "assert failed"}
            ]}));
        }
        None
    });
    let result = handle_atf_tool(
        "SN-Run-ATF-Suite",
        &json!({"test_suite_name": "Smoke", "browser_name": "chrome"}),
        &h.client,
    )
    .await
    .unwrap_or_default();
    let body = payload(&result);
    assert_eq!(result["isError"], true);
    assert_subset(
        &body,
        &json!({
            "status": "failed", "successful": false,
            "results": {
                "result_type": "suite", "test_suite_name": "Smoke",
                "counts": {"passed": 3, "failed": 1, "errored": 0, "skipped": 2},
                "tests": [
                    {"test_name": "Login works", "status": "Success"},
                    {"test_name": "Checkout", "status": "Failure", "output": "assert failed"}
                ]
            }
        }),
    );
    let all = reqs(&h);
    assert_eq!(
        all.iter()
            .find(|r| r.method == Method::Post)
            .map(|r| r.query.clone()),
        Some(q(&[
            ("test_suite_name", "Smoke"),
            ("browser_name", "chrome")
        ]))
    );
    assert_eq!(
        body["results"]["links"],
        json!({
            "api": format!("{URL}/api/sn_cicd/testsuite/results/{RESULT_ID}"),
            "record": format!("{URL}/sys_atf_test_suite_result.do?sys_id={RESULT_ID}")
        })
    );
    let rows = all
        .iter()
        .find(|r| r.path == "/api/now/table/sys_atf_test_result");
    assert!(
        rows.is_some_and(|r| r.query["sysparm_query"].starts_with(&format!("parent={RESULT_ID}")))
    );
}

#[tokio::test]
async fn run_suite_rejects_bad_input_before_any_request() {
    let h = fake(|_| started());
    let e = err_of(
        &h,
        "SN-Run-ATF-Suite",
        json!({"test_suite_sys_id": SUITE_ID, "test_suite_name": "Smoke"}),
    )
    .await;
    assert!(e.message.contains("mutually exclusive"));
    let e = err_of(
        &h,
        "SN-Run-ATF-Suite",
        json!({"test_suite_sys_id": SUITE_ID, "browser_name": "netscape"}),
    )
    .await;
    assert!(e.message.contains("browser_name must be one of"));
    assert!(h.transport.requests().is_empty());
}

// ---- availability errors ---------------------------------------------------

#[tokio::test]
async fn names_the_plugin_when_the_api_is_missing() {
    let answers: [fn() -> Answer; 3] = [
        || {
            Some((
                400,
                Body::Json(
                    json!({"error": {"message": "Requested URI does not represent any resource", "detail": null}}),
                ),
            ))
        },
        || Some((404, Body::Json(json!({})))),
        || Some((200, Body::Html("<html><body>Log in</body></html>"))),
    ];
    for answer in answers {
        let h = fake(move |_| answer());
        let e = err_of(
            &h,
            "SN-Run-ATF-Suite",
            json!({"test_suite_sys_id": SUITE_ID}),
        )
        .await;
        assert_eq!(e.code(), Some("CICD_PLUGIN_INACTIVE"));
        assert!(
            e.message.contains("com.glide.continuousdelivery"),
            "{}",
            e.message
        );
    }
}

#[tokio::test]
async fn names_the_role_on_403_without_leaking_credentials() {
    let h = fake(|_| {
        Some((
            403,
            Body::Json(
                json!({"error": {"message": "User Not Authorized", "detail": format!("basic {PASSWORD}")}}),
            ),
        ))
    });
    let e = err_of(
        &h,
        "SN-Get-CICD-Progress",
        json!({"progress_id": PROGRESS_ID}),
    )
    .await;
    assert_eq!(e.code(), Some("CICD_ROLE_MISSING"));
    assert!(e.message.contains("sn_cicd.sys_ci_automation"));
    assert!(!e.message.contains(PASSWORD));
}

#[tokio::test]
async fn reports_operation_errors_as_themselves() {
    let h = fake(|_| {
        Some((
            400,
            Body::Json(json!({"result": {"error": "Invalid test_sys_id", "status": "3"}})),
        ))
    });
    let e = err_of(&h, "SN-Run-ATF-Test", json!({"test_sys_id": TEST_ID})).await;
    assert_eq!(e.code(), Some("CICD_REQUEST_FAILED"));
    assert!(e.message.contains("Invalid test_sys_id"));
}

#[tokio::test]
async fn start_failure_hints_runner_disabled() {
    for (tool, args, message) in [
        (
            "SN-Run-ATF-Test",
            json!({"test_sys_id": TEST_ID}),
            "Start ATF test Failed with the given test ID",
        ),
        (
            "SN-Run-ATF-Suite",
            json!({"test_suite_sys_id": SUITE_ID}),
            "Start ATF test suite failed with the given test suite ID",
        ),
    ] {
        let h = fake(move |_| {
            Some((
                400,
                Body::Json(json!({"error": {"message": message, "detail": null}})),
            ))
        });
        let e = err_of(&h, tool, args).await;
        assert_eq!(e.code(), Some("ATF_RUNNER_DISABLED"));
        assert!(e.message.contains(message));
        let hint = e.message.find("may be disabled").unwrap_or(usize::MAX);
        let prop = e.message.find("sn_atf.runner.enabled").unwrap_or(0);
        let must = e.message.find("must be true").unwrap_or(0);
        assert!(hint < prop && prop < must, "{}", e.message);
    }
}

#[tokio::test]
async fn malformed_progress_id_says_the_run_was_dispatched() {
    let h = fake(|r| {
        (r.path == "/api/sn_cicd/testsuite/run").then(|| {
            (
                200,
                Body::Json(json!({"result": {"status": "0", "links": {"progress": {"id": "../x", "url": "x"}}}})),
            )
        })
    });
    let e = err_of(
        &h,
        "SN-Run-ATF-Suite",
        json!({"test_suite_sys_id": SUITE_ID}),
    )
    .await;
    assert_eq!(e.code(), Some("CICD_NO_PROGRESS_ID"));
    assert!(e.message.contains("dispatched the"), "{}", e.message);
    assert!(e.message.contains(SUITE_ID), "{}", e.message);
    assert!(e.message.contains("malformed progress id"), "{}", e.message);
    assert!(!e.message.contains("may not have started"), "{}", e.message);
    // Never polled with the bad id.
    assert!(reqs(&h).iter().all(|r| !r.path.contains("/progress/")));
}

// ---- progress and results --------------------------------------------------

#[tokio::test]
async fn single_progress_read_does_not_poll() {
    let h = fake(|_| {
        ok(
            json!({"result": {"status": "2", "percent_complete": 100, "links": {"results": {"id": RESULT_ID}}}}),
        )
    });
    let result = handle_atf_tool(
        "SN-Get-CICD-Progress",
        &json!({"progress_id": PROGRESS_ID}),
        &h.client,
    )
    .await
    .unwrap_or_default();
    assert_subset(
        &payload(&result),
        &json!({"progress_id": PROGRESS_ID, "state": "successful", "finished": true, "results": {"id": RESULT_ID}}),
    );
    assert_eq!(h.transport.requests().len(), 1);
    assert!(h.clock.sleeps().is_empty());
}

#[tokio::test]
async fn results_require_a_valid_result_type() {
    let h = fake(|_| ok(json!({"result": {}})));
    let e = err_of(
        &h,
        "SN-Get-ATF-Results",
        json!({"result_id": RESULT_ID, "result_type": "all"}),
    )
    .await;
    assert!(
        e.message
            .contains("result_type must be one of: test, suite")
    );
    assert!(h.transport.requests().is_empty());
}

// ---- logs -------------------------------------------------------------------

#[tokio::test]
async fn logs_send_pattern_and_limit() {
    let h = fake(|r| {
        (r.path == format!("/api/sn_cicd/tests/test/results/{RESULT_ID}/logs")).then(|| {
            (
                200,
                Body::Json(json!({"result": {"logs": ["ERROR step 2 failed"]}})),
            )
        })
    });
    let result = handle_atf_tool(
        "SN-Get-ATF-Test-Logs",
        &json!({"result_id": RESULT_ID, "pattern": "error|warn", "limit": 20}),
        &h.client,
    )
    .await
    .unwrap_or_default();
    assert_eq!(
        reqs(&h)[0].query,
        q(&[("sysparm_regex", "error|warn"), ("sysparm_limit", "20")])
    );
    assert_subset(
        &payload(&result),
        &json!({"result_id": RESULT_ID, "pattern": "error|warn", "limit": 20, "logs": {"logs": ["ERROR step 2 failed"]}}),
    );
}

#[tokio::test]
async fn no_logs_404_is_an_empty_success() {
    let h = fake(|_| {
        Some((
            404,
            Body::Json(
                json!({"error": {"message": format!("No logs found for test result {RESULT_ID}"), "detail": null}}),
            ),
        ))
    });
    let result = handle_atf_tool(
        "SN-Get-ATF-Test-Logs",
        &json!({"result_id": RESULT_ID}),
        &h.client,
    )
    .await
    .unwrap_or_default();
    assert_eq!(result["isError"], false);
    let body = payload(&result);
    assert_subset(
        &body,
        &json!({"result_id": RESULT_ID, "limit": 100, "logs": []}),
    );
    assert!(
        body["note"]
            .as_str()
            .unwrap_or("")
            .contains("capture_node_logs: true")
    );
}

#[tokio::test]
async fn other_log_404s_stay_errors() {
    let answers: [(AnswerFn, &str); 2] = [
        (
            || {
                Some((
                    404,
                    Body::Json(json!({"error": {"message": "No Record found", "detail": null}})),
                ))
            },
            "CICD_REQUEST_FAILED",
        ),
        (
            || Some((404, Body::Json(json!({})))),
            "CICD_PLUGIN_INACTIVE",
        ),
    ];
    for (answer, code) in answers {
        let h = fake(move |_| answer());
        let e = err_of(&h, "SN-Get-ATF-Test-Logs", json!({"result_id": RESULT_ID})).await;
        assert_eq!(e.code(), Some(code));
    }
}

#[tokio::test]
async fn logs_reject_bad_input_before_any_request() {
    for (extra, expected) in [
        (
            json!({"pattern": "(unclosed"}),
            "not a valid regular expression",
        ),
        (
            json!({"pattern": "a".repeat(257)}),
            "at most 256 characters",
        ),
        (json!({"pattern": "(a+)+$"}), "nested quantifier"),
        (
            json!({"limit": 1001}),
            "limit must be an integer from 1 to 1000",
        ),
    ] {
        let h = fake(|_| ok(json!({"result": {}})));
        let mut args = json!({"result_id": RESULT_ID});
        if let (Value::Object(a), Value::Object(e)) = (&mut args, extra) {
            a.extend(e);
        }
        let e = err_of(&h, "SN-Get-ATF-Test-Logs", args).await;
        assert!(e.message.contains(expected), "{}", e.message);
        assert!(h.transport.requests().is_empty());
    }
}

#[test]
fn invalid_log_pattern_reads_like_v8() {
    use crate::client::cicd_atf::validate_log_pattern;
    for (pattern, reason) in [
        ("(", "Unterminated group"),
        ("[a", "Unterminated character class"),
    ] {
        let msg = validate_log_pattern(Some(&json!(pattern)))
            .err()
            .map(|e| e.message)
            .unwrap_or_default();
        assert_eq!(
            msg,
            format!(
                "pattern is not a valid regular expression: Invalid regular expression: /{pattern}/i: {reason}"
            )
        );
    }
}

#[test]
fn log_pattern_rejects_unbounded_constructs_promptly() {
    use crate::client::cicd_atf::validate_log_pattern;
    for pattern in [
        r"(a|b\g<1>\g<1>\g<1>)",
        r"(?P<x>a|b(?P>x)(?P>x))",
        r"(a|(?R))",
        r"(a)\1",
        r"(?<n>a)\k<n>",
        r"a(?=b)",
        r"a(?!b)",
        r"(?<=a)b",
        r"(?<!a)b",
    ] {
        let start = std::time::Instant::now();
        let e = validate_log_pattern(Some(&json!(pattern))).err();
        assert!(
            start.elapsed() < std::time::Duration::from_millis(100),
            "{pattern}"
        );
        let msg = e.map(|e| e.message).unwrap_or_default();
        assert!(
            msg.starts_with("pattern is not a valid regular expression"),
            "{pattern}: {msg}"
        );
    }
    for pattern in [r"(?<name>err)or", r"\\1", r"fail(ed|ure)"] {
        assert_eq!(
            validate_log_pattern(Some(&json!(pattern)))
                .ok()
                .flatten()
                .as_deref(),
            Some(pattern)
        );
    }
}

#[tokio::test]
async fn stalled_progress_request_times_out_at_the_deadline() {
    let h = fake(|r| (r.method == Method::Post).then(started).flatten());
    *h.transport.stall.lock().unwrap_or_else(|e| e.into_inner()) = Some("/api/sn_cicd/progress/");
    let start = std::time::Instant::now();
    let result = handle_atf_tool(
        "SN-Run-ATF-Test",
        &json!({"test_sys_id": TEST_ID, "timeout_seconds": 1}),
        &h.client,
    )
    .await
    .unwrap_or_default();
    assert!(start.elapsed() < std::time::Duration::from_secs(5));
    let body = payload(&result);
    assert_subset(
        &body,
        &json!({"status": "timeout", "progress_id": PROGRESS_ID}),
    );
}
