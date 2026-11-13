//! Port of tests/bulk-input-bounds.test.js: the validation and placeholder
//! parts, exercised directly on `bulk_limits` (the JS tests reach them
//! through ServiceNowClient; "zero writes" there means "validation fails").

use hpm_tools::logic::bulk_limits::*;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

fn hex(n: u64) -> String {
    format!("{n:032x}")
}

fn create_ops(count: usize) -> Vec<Value> {
    (0..count)
        .map(|_| json!({ "table": "incident", "data": { "short_description": "x" } }))
        .collect()
}

fn update_ops(count: u64) -> Vec<Value> {
    (0..count)
        .map(|i| json!({ "table": "incident", "sys_id": hex(i + 1), "data": { "state": 6 } }))
        .collect()
}

fn batch_create(ops: Vec<Value>) -> Result<Vec<String>, BulkInputError> {
    validate_batch_create_operations(Some(&Value::Array(ops)))
}

/// Mirrors ServiceNowClient.batchCreate: resolve, "create" (sys_id = hex(n)),
/// save under save_as or operation_i.
fn run_batch_create(ops: Vec<Value>) -> (Vec<Value>, HashMap<String, String>) {
    let serialized = batch_create(ops.clone()).expect("valid batch");
    let mut saved = HashMap::new();
    let mut posted = Vec::new();
    for (i, (op, data)) in ops.iter().zip(&serialized).enumerate() {
        let resolved = resolve_batch_placeholders(data, &saved);
        posted.push(serde_json::from_str::<Value>(&resolved).expect("valid JSON"));
        let key = op["save_as"]
            .as_str()
            .map_or(format!("operation_{i}"), str::to_owned);
        saved.insert(key, hex(i as u64 + 1));
    }
    (posted, saved)
}

#[test]
fn batch_create_rejects_invalid_input() {
    let mut oversized = create_ops(50);
    oversized.push(json!({ "table": "incident", "data": { "blob": "x".repeat(64 * 1024) } }));
    oversized.extend(create_ops(10));
    let mut bad_table = create_ops(99);
    bad_table.push(json!({ "table": "../sys_user", "data": {} }));
    let mut bad_data = create_ops(5);
    bad_data.push(json!({ "table": "incident", "data": "nope" }));
    let cases = [
        (
            create_ops(101),
            "operations accepts at most 100 entries (received 101)",
        ),
        (
            oversized,
            "operations[50].data must serialize to at most 65536 bytes",
        ),
        (
            bad_table,
            "operations[99].table must be a table name of 1-80 letters, digits or underscores",
        ),
        (
            bad_data,
            "operations[5].data must be an object of field values",
        ),
        (
            vec![json!({ "table": "incident", "data": {}, "save_as": "bad name}" })],
            "operations[0].save_as must match ^[A-Za-z_][A-Za-z0-9_]{0,63}$",
        ),
    ];
    for (ops, message) in cases {
        assert_eq!(batch_create(ops).expect_err(message).message, message);
    }
    assert_eq!(
        validate_batch_create_operations(None)
            .expect_err("undefined")
            .message,
        "operations must be an array"
    );
}

#[test]
fn batch_create_accepts_100_operations_at_the_64_kib_boundary() {
    let filler = "y".repeat(64 * 1024 - r#"{"blob":""}"#.len());
    let ops: Vec<Value> = (0..100)
        .map(|_| json!({ "table": "incident", "data": { "blob": filler } }))
        .collect();
    let serialized = batch_create(ops).expect("boundary accepted");
    assert_eq!(serialized.len(), 100);
    assert_eq!(serialized[0].len(), 64 * 1024);
}

#[test]
fn resolves_references_to_previously_saved_ids_and_preserves_literal_values() {
    let (posted, saved) = run_batch_create(vec![
        json!({ "table": "incident", "data": { "short_description": "parent" }, "save_as": "parent" }),
        json!({ "table": "incident", "data": { "short_description": "sibling" } }),
        json!({
            "table": "incident",
            "data": {
                "parent": "${parent}",
                "sibling": "${operation_1}",
                "both": "p=${parent};p=${parent};s=${operation_1}",
                "future": "${child}",
                "unknown": "${not_saved}",
                "malformed": "${parent",
                "spaced": "${ parent }",
                "dollar": "$${parent}}",
                "quote": "say \"${parent}\"",
                "nested": { "list": ["${parent}", 7, null, true] },
                "number": 42
            },
            "save_as": "child"
        }),
    ]);
    let (parent, sibling) = (hex(1), hex(2));
    assert_eq!(saved["parent"], parent);
    assert_eq!(saved["operation_1"], sibling);
    assert_eq!(
        posted[2],
        json!({
            "parent": parent,
            "sibling": sibling,
            "both": format!("p={parent};p={parent};s={sibling}"),
            "future": "${child}",
            "unknown": "${not_saved}",
            "malformed": "${parent",
            "spaced": "${ parent }",
            "dollar": format!("${parent}}}"),
            "quote": format!("say \"{parent}\""),
            "nested": { "list": [parent, 7, null, true] },
            "number": 42
        })
    );
}

#[test]
fn rejects_reserved_and_duplicate_save_as_names() {
    let op = |save_as: &str| json!({ "table": "incident", "data": {}, "save_as": save_as });
    let plain = json!({ "table": "incident", "data": {} });
    let cases = [
        (
            vec![op("__proto__")],
            "operations[0].save_as '__proto__' is reserved",
        ),
        (
            vec![op("constructor")],
            "operations[0].save_as 'constructor' is reserved",
        ),
        (
            vec![op("prototype")],
            "operations[0].save_as 'prototype' is reserved",
        ),
        (
            vec![op("a"), op("a")],
            "operations[1].save_as 'a' duplicates an earlier save_as",
        ),
        (
            vec![plain, op("operation_0")],
            "operations[1].save_as 'operation_0' is reserved",
        ),
        (
            vec![op("operation_0")],
            "operations[0].save_as 'operation_0' is reserved",
        ),
    ];
    for (ops, message) in cases {
        let error = batch_create(ops).expect_err(message);
        assert_eq!(error.message, message);
        assert!(error.message.contains("save_as"));
    }
}

#[test]
fn saved_ids_never_resolve_inherited_names() {
    let (posted, saved) = run_batch_create(vec![
        json!({ "table": "incident", "data": {}, "save_as": "toString" }),
        json!({ "table": "incident", "data": { "a": "${toString}", "b": "${hasOwnProperty}", "c": "${valueOf}" } }),
    ]);
    assert_eq!(saved["toString"], hex(1));
    assert_eq!(
        posted[1],
        json!({ "a": hex(1), "b": "${hasOwnProperty}", "c": "${valueOf}" })
    );
}

#[test]
fn placeholder_dense_operations_resolve_correctly() {
    // Correctness half of the JS scaling test (timing ratio is not ported).
    let ops: Vec<Value> = (0..100)
        .map(|i| {
            let refs: Vec<String> = (0..i).map(|k| format!("${{k{k}}}")).collect();
            let unit = if refs.is_empty() {
                "none".to_owned()
            } else {
                refs.join(",")
            };
            let repeat = ((50 * 1024) / (unit.len() + 1)).max(1);
            json!({
                "table": "incident",
                "save_as": format!("k{i}"),
                "data": { "refs": vec![unit; repeat].join("|"), "stray": "${".repeat(5000) }
            })
        })
        .collect();
    let (posted, _) = run_batch_create(ops);
    let refs = posted[99]["refs"].as_str().expect("refs");
    assert!(refs.starts_with(&format!("{},{}", hex(1), hex(2))));
    assert!(!refs.contains("${k"));
    assert_eq!(posted[99]["stray"], "${".repeat(5000));
}

#[test]
fn batch_update_rejects_invalid_input() {
    let with = |mut ops: Vec<Value>, extra: Value| {
        ops.push(extra);
        Value::Array(ops)
    };
    let upper = hex(1).to_uppercase().replace('0', "A");
    let cases = [
        (
            Value::Array(update_ops(101)),
            "updates accepts at most 100 entries (received 101)",
        ),
        (
            with(
                update_ops(10),
                json!({ "table": "incident", "sys_id": "abc", "data": {} }),
            ),
            "updates[10].sys_id must be a 32-character lowercase hexadecimal sys_id",
        ),
        (
            json!([{ "table": "incident", "sys_id": upper, "data": {} }]),
            "updates[0].sys_id must be a 32-character lowercase hexadecimal sys_id",
        ),
        (
            with(
                update_ops(3),
                json!({ "table": "incident", "sys_id": hex(9), "data": { "blob": "x".repeat(64 * 1024) } }),
            ),
            "updates[3].data must serialize to at most 65536 bytes",
        ),
        (
            json!([{ "table": "incident/../../x", "sys_id": hex(1), "data": {} }]),
            "updates[0].table must be a table name of 1-80 letters, digits or underscores",
        ),
    ];
    for (updates, message) in cases {
        assert_eq!(
            validate_batch_updates(Some(&updates))
                .expect_err(message)
                .message,
            message
        );
    }
    assert!(validate_batch_updates(Some(&Value::Array(update_ops(100)))).is_ok());
}

fn activities(n: usize) -> Value {
    (0..n)
        .map(|i| json!({ "name": format!("A{i}"), "script": "gs.info(1);" }))
        .collect()
}

fn transitions(n: usize, max: usize) -> Value {
    (0..n)
        .map(|i| json!({ "from": i % max, "to": (i + 1) % max }))
        .collect()
}

fn spec(extra: Value) -> Value {
    let mut base = json!({ "name": "WF", "table": "incident", "activities": activities(3), "transitions": transitions(2, 3) });
    if let (Some(base), Value::Object(extra)) = (base.as_object_mut(), extra) {
        base.extend(extra);
    }
    base
}

#[test]
fn workflow_rejects_out_of_bounds_input() {
    let big: Value = (0..17)
        .map(|i| json!({ "name": format!("A{i}"), "script": "s".repeat(250 * 1024) }))
        .collect();
    let cases = [
        (
            spec(json!({ "activities": activities(101) })),
            "activities accepts at most 100 entries (received 101)",
        ),
        (
            spec(json!({ "activities": activities(100), "transitions": transitions(201, 100) })),
            "transitions accepts at most 200 entries (received 201)",
        ),
        (
            spec(
                json!({ "activities": [{ "name": "big", "script": "s".repeat(256 * 1024 + 1) }] }),
            ),
            "activities[0].script must be at most 262144 bytes",
        ),
        (
            spec(
                json!({ "transitions": [{ "from": 0, "to": 1, "condition": "c".repeat(256 * 1024 + 1) }] }),
            ),
            "transitions[0].condition must be at most 262144 bytes",
        ),
        (
            spec(json!({ "activities": big })),
            "workflow specification must serialize to at most 4194304 bytes",
        ),
        (
            spec(json!({ "name": "n".repeat(256) })),
            "name must be a non-empty string of at most 255 characters",
        ),
        (
            spec(json!({ "activities": [{ "name": "n".repeat(256) }] })),
            "activities[0].name must be a non-empty string of at most 255 characters",
        ),
        (
            spec(json!({ "transitions": [{ "from": 0, "to": 3 }] })),
            "transitions[0].to must be an activity index from 0 to 2",
        ),
        (
            spec(json!({ "table": "incident;drop" })),
            "table must be a table name of 1-80 letters, digits or underscores",
        ),
        (
            spec(json!({ "activities": [{ "name": "a", "activity_definition_sys_id": "nope" }] })),
            "activities[0].activity_definition_sys_id must be a 32-character lowercase hexadecimal sys_id",
        ),
    ];
    for (workflow, message) in cases {
        assert_eq!(
            validate_workflow_spec(Some(&workflow))
                .expect_err(message)
                .message,
            message
        );
    }
}

#[test]
fn workflow_accepts_100_activities_and_200_transitions_at_the_script_boundary() {
    let mut acts = activities(100);
    acts[0]["script"] = json!("s".repeat(256 * 1024));
    let workflow = json!({
        "name": "n".repeat(255),
        "table": "incident",
        "activities": acts,
        "transitions": transitions(200, 100),
        "publish": true
    });
    let references = validate_workflow_spec(Some(&workflow)).expect("boundary accepted");
    assert_eq!(references.len(), 100);
}

#[test]
fn workflow_rejects_unresolvable_activity_references() {
    let cases = [
        (
            spec(json!({ "transitions": [{ "from": "Nope", "to": 1 }] })),
            "transitions[0].from must reference a declared activity id or name, an activity index, or a 32-character lowercase hex sys_id",
        ),
        (
            spec(json!({ "transitions": [{ "from": "__proto__", "to": 1 }] })),
            "transitions[0].from must reference a declared activity id or name, an activity index, or a 32-character lowercase hex sys_id",
        ),
        (
            spec(json!({ "transitions": [{ "from": "activity_0", "to": 1 }] })),
            "transitions[0].from must reference a declared activity id or name, an activity index, or a 32-character lowercase hex sys_id",
        ),
        (
            spec(json!({ "start_activity": "nope", "publish": true })),
            "start_activity must reference a declared activity id or name, an activity index, or a 32-character lowercase hex sys_id",
        ),
        (
            spec(
                json!({ "activities": [{ "id": "x", "name": "A" }, { "id": "x", "name": "B" }], "transitions": [] }),
            ),
            "activities[1].id 'x' duplicates an earlier activity id",
        ),
        (
            spec(
                json!({ "activities": [{ "name": "Same" }, { "name": "Same" }], "transitions": [{ "from": "Same", "to": 0 }] }),
            ),
            "transitions[0].from 'Same' matches more than one activity id or name",
        ),
    ];
    for (workflow, message) in cases {
        let error = validate_workflow_spec(Some(&workflow)).expect_err(message);
        assert_eq!(error.message, message);
        assert!(error.message.contains("activit"));
    }
}

#[test]
fn workflow_resolves_declared_ids_unique_names_and_external_sys_ids() {
    let external = hex(0xe0);
    let workflow = json!({
        "name": "WF",
        "table": "change_request",
        "activities": [{ "id": "check", "name": "Check Risk" }, { "name": "Auto Approve" }],
        "transitions": [
            { "from": "check", "to": "Auto Approve" },
            { "from": "Auto Approve", "to": external },
            { "from": 1, "to": 0 }
        ],
        "start_activity": "Auto Approve",
        "publish": true
    });
    let references = validate_workflow_spec(Some(&workflow)).expect("valid");
    let expected: HashMap<String, usize> = [("check", 0), ("Check Risk", 0), ("Auto Approve", 1)]
        .map(|(k, v)| (k.to_owned(), v))
        .into();
    assert_eq!(references, expected);
}

fn move_options(value: Value) -> Map<String, Value> {
    value.as_object().cloned().unwrap_or_default()
}

#[test]
fn update_set_move_rejects_invalid_input() {
    let target = json!(hex(0xabc));
    let ids: Vec<String> = (1..=201).map(hex).collect();
    let range = json!({ "start": "2025-01-01 00:00:00", "end": "2025-01-02 00:00:00" });
    let cases = [
        (
            json!({ "record_sys_ids": ids }),
            "record_sys_ids accepts at most 200 entries (received 201)",
        ),
        (
            json!({ "record_sys_ids": [hex(1), "not-a-sys-id"] }),
            "record_sys_ids[1] must be a 32-character lowercase hexadecimal sys_id",
        ),
        (
            json!({ "record_sys_ids": [format!("{}^ORsys_id!=x", hex(1))] }),
            "record_sys_ids[0] must be a 32-character lowercase hexadecimal sys_id",
        ),
        (
            json!({ "time_range": { "start": "2025-01-01 00:00:00^ORactive=true", "end": "2025-01-02 00:00:00" } }),
            "time_range.start must use the format YYYY-MM-DD HH:MM:SS",
        ),
        (
            json!({ "time_range": range, "source_update_set": "Default^ORname!=x" }),
            "source_update_set must not contain '^' or control characters",
        ),
        (
            json!({ "record_sys_ids": [hex(1)], "table": "sys_update_xml/../sys_user" }),
            "table must be a table name of 1-80 letters, digits or underscores",
        ),
    ];
    for (options, message) in cases {
        let error =
            validate_update_set_move(Some(&target), &move_options(options)).expect_err(message);
        assert_eq!(error.message, message);
    }
    let error = validate_update_set_move(
        Some(&json!("Default")),
        &move_options(json!({ "record_sys_ids": [hex(1)] })),
    )
    .expect_err("bad target");
    assert_eq!(
        error.message,
        "update_set_id must be a 32-character lowercase hexadecimal sys_id"
    );
}

#[test]
fn update_set_move_accepts_and_deduplicates_ids() {
    let mut with_duplicates: Vec<String> = (1..=199).map(hex).collect();
    with_duplicates.insert(0, hex(5));
    let result = validate_update_set_move(
        Some(&json!(hex(0xabc))),
        &move_options(json!({ "record_sys_ids": with_duplicates })),
    )
    .expect("valid");
    assert_eq!(result.len(), 199);
    assert_eq!(result[0], hex(5));
    assert_eq!(result[198], hex(199));
    let ranged = validate_update_set_move(
        Some(&json!(hex(0xabc))),
        &move_options(json!({ "time_range": { "start": "2025-01-01 00:00:00", "end": "2025-01-02 00:00:00" }, "source_update_set": "Default" })),
    )
    .expect("valid");
    assert!(ranged.is_empty());
}

#[test]
fn update_set_clone_rejects_invalid_source_ids_and_names() {
    let cases = [
        (
            json!("Default"),
            json!("copy"),
            "source_update_set_id must be a 32-character lowercase hexadecimal sys_id",
        ),
        (
            json!(hex(1)),
            json!("n".repeat(256)),
            "new_name must be a non-empty string of at most 255 characters",
        ),
        (
            json!(hex(1)),
            json!(""),
            "new_name must be a non-empty string of at most 255 characters",
        ),
    ];
    for (source, name, message) in cases {
        assert_eq!(
            validate_update_set_clone(Some(&source), Some(&name))
                .expect_err(message)
                .message,
            message
        );
    }
    assert!(validate_update_set_clone(Some(&json!(hex(1))), Some(&json!("Copy"))).is_ok());
}

#[test]
fn exports_the_javascript_limits() {
    assert_eq!(MAX_BATCH_OPERATIONS, 100);
    assert_eq!(MAX_WORKFLOW_ACTIVITIES, 100);
    assert_eq!(MAX_WORKFLOW_TRANSITIONS, 200);
    assert_eq!(MAX_UPDATE_SET_RECORD_IDS, 200);
    assert_eq!(MAX_OPERATION_DATA_BYTES, 65536);
    assert_eq!(MAX_SCRIPT_BYTES, 262144);
    assert_eq!(MAX_WORKFLOW_INPUT_BYTES, 4194304);
    assert_eq!(MAX_NAME_LENGTH, 255);
    assert_eq!(MAX_TABLE_NAME_LENGTH, 80);
    assert_eq!(MAX_SAVE_AS_LENGTH, 64);
    assert_eq!(UPDATE_SET_ID_CHUNK_SIZE, 100);
    assert_eq!(UPDATE_SET_QUERY_PAGE_SIZE, 1000);
    assert_eq!(MAX_UPDATE_SET_QUERY_RECORDS, 10000);
    assert_eq!(SYS_ID_PATTERN, "^[0-9a-f]{32}$");
    assert_eq!(BulkInputError::CODE, "BULK_INPUT_INVALID");
    assert!(is_sys_id(&hex(5)));
    assert!(!is_sys_id(&hex(5).to_uppercase().replace('0', "A")));
}
