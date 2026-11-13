//! Port of tests/generated-script-safety.test.js: the parts that exercise
//! `generated_scripts` directly. The JS tests compose these helpers inside the
//! SN-Create-Fix-Script handler (`${script_name}_${fileTimestamp()}.js` in
//! `<cwd>/scripts`); the tests below repeat that composition.

use hpm_tools::logic::generated_scripts::*;
use serde_json::{Value, json};
use std::path::Path;

const HOSTILE_VALUES: &[(&str, &str)] = &[
    ("single quote", "Fix'); gs.print('pwned-quote'); ('"),
    (
        "backslash before quote",
        "Fix\\'); gs.print('pwned-backslash'); //",
    ),
    ("double quote", "Fix\" + gs.print(\"pwned-double\") + \""),
    ("newline", "Fix\ngs.print('pwned-newline');"),
    ("carriage return", "Fix\rgs.print('pwned-cr');"),
    (
        "block comment terminator",
        "Fix */ gs.print('pwned-comment'); /*",
    ),
    ("line separator", "Fix\u{2028}gs.print('pwned-ls');"),
    ("paragraph separator", "Fix\u{2029}gs.print('pwned-ps');"),
];

const CREATE_INSTRUCTIONS: &[&str] = &[
    "Copy the entire script below",
    "Navigate to ServiceNow: System Definition → Scripts - Background",
    "Paste the script",
    "Click \"Run script\"",
    "Verify output in the output panel",
];

/// SN-Create-Fix-Script composition.
fn create_fix_script(
    scripts_dir: &Path,
    script_name: &Value,
    description: Option<&str>,
) -> Result<std::path::PathBuf, FixScriptError> {
    let name = validate_fix_script_name(Some(script_name))?;
    let file_name = format!("{name}_{}.js", file_timestamp());
    let mut metadata = json!({ "script_name": name });
    if let Some(description) = description {
        metadata["description"] = json!(description);
    }
    let content = build_fix_script_file_content(&FixScriptFile {
        title: "Fix Script (Manual Execution Required)",
        created_at: "2026-10-06T01:02:03.004Z",
        notes: &[],
        instructions: CREATE_INSTRUCTIONS,
        metadata: Some(&metadata),
        body: "gs.info('hello');",
    });
    write_fix_script_file(scripts_dir, &file_name, &content)
}

fn list_files(dir: &Path) -> Vec<String> {
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() && !path.is_symlink() {
                files.extend(list_files(&path));
            } else {
                files.push(path.display().to_string());
            }
        }
    }
    files.sort();
    files
}

#[test]
fn rejects_invalid_script_names_before_writing_anything() {
    let temp = tempfile::tempdir().expect("tempdir");
    let outside = temp.path().join("outside").display().to_string();
    let invalid = [
        "../escape".to_owned(),
        "nested/../../escape".to_owned(),
        "..".to_owned(),
        ".".to_owned(),
        ".hidden".to_owned(),
        "sub/name".to_owned(),
        "sub\\name".to_owned(),
        "..\\..\\escape".to_owned(),
        format!("{outside}/escape"),
        "C:escape".to_owned(),
        "C:\\escape".to_owned(),
        "\\\\server\\share\\escape".to_owned(),
        "name\u{0}.txt".to_owned(),
        "name\nmore".to_owned(),
        "name\u{7f}".to_owned(),
        String::new(),
        "a".repeat(101),
        "CON".to_owned(),
        "con.x".to_owned(),
        "NUL.a".to_owned(),
        "Prn".to_owned(),
        "aux.backup".to_owned(),
        "COM1".to_owned(),
        "lpt9.txt".to_owned(),
    ];
    let scripts = temp.path().join("project").join("scripts");
    for name in invalid {
        let error = create_fix_script(&scripts, &json!(name), None).expect_err(&name);
        assert!(
            matches!(error, FixScriptError::InvalidScriptName),
            "{name:?}"
        );
        assert!(error.to_string().starts_with("Invalid script_name"));
        assert!(list_files(temp.path()).is_empty());
    }
}

#[test]
fn rejects_a_non_string_script_name() {
    for name in [json!(["..", "escape"]), json!(null), json!(7)] {
        assert!(matches!(
            validate_fix_script_name(Some(&name)),
            Err(FixScriptError::InvalidScriptName)
        ));
    }
    assert!(validate_fix_script_name(None).is_err());
}

#[test]
fn writes_a_valid_name_as_a_private_direct_child_of_the_scripts_directory() {
    let temp = tempfile::tempdir().expect("tempdir");
    let scripts = temp.path().join("project").join("scripts");
    let path =
        create_fix_script(&scripts, &json!("link_ui_policy-actions.v2"), None).expect("written");
    assert_eq!(path.parent(), Some(scripts.as_path()));
    let base = path.file_name().and_then(|n| n.to_str()).expect("name");
    let stamp = base
        .strip_prefix("link_ui_policy-actions.v2_")
        .and_then(|rest| rest.strip_suffix("Z.js"))
        .expect("pattern");
    assert!(stamp.len() > 11 && stamp.as_bytes()[10] == b'T', "{base}");
    assert!(
        stamp
            .bytes()
            .all(|b| b.is_ascii_digit() || b == b'-' || b == b'T')
    );
    assert_eq!(list_files(temp.path()).len(), 1);
    let content = std::fs::read_to_string(&path).expect("read");
    assert!(content.contains("gs.info('hello');"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}

#[cfg(unix)]
#[test]
fn refuses_a_scripts_directory_that_is_a_symlink() {
    let temp = tempfile::tempdir().expect("tempdir");
    let outside = temp.path().join("outside");
    let project = temp.path().join("project");
    std::fs::create_dir_all(&outside).expect("mkdir");
    std::fs::create_dir_all(&project).expect("mkdir");
    std::os::unix::fs::symlink(&outside, project.join("scripts")).expect("symlink");
    let error = create_fix_script(&project.join("scripts"), &json!("valid_name"), None)
        .expect_err("symlink");
    assert!(matches!(
        error,
        FixScriptError::ScriptsDirectoryIsSymlink(_)
    ));
    assert!(error.to_string().contains("scripts directory"));
    assert!(error.to_string().ends_with("is a symbolic link"));
    assert_eq!(std::fs::read_dir(&outside).expect("read").count(), 0);
}

#[test]
fn accepts_names_that_only_resemble_windows_device_names() {
    for name in [
        "console",
        "nullable",
        "com10",
        "lpt",
        "auxiliary.fix",
        "my_con",
    ] {
        let temp = tempfile::tempdir().expect("tempdir");
        let scripts = temp.path().join("scripts");
        let path = create_fix_script(&scripts, &json!(name), None).expect(name);
        let base = path.file_name().and_then(|n| n.to_str()).expect("name");
        assert!(base.starts_with(&format!("{name}_")), "{base}");
        assert_eq!(list_files(temp.path()).len(), 1);
    }
}

#[test]
fn refuses_a_scripts_path_that_is_not_a_directory() {
    let temp = tempfile::tempdir().expect("tempdir");
    let scripts = temp.path().join("scripts");
    std::fs::write(&scripts, "not a directory").expect("write");
    let error = create_fix_script(&scripts, &json!("valid_name"), None).expect_err("not a dir");
    let message = error.to_string();
    assert!(message.starts_with("Refusing to write fix script: scripts directory "));
    assert!(message.ends_with(" is not a directory"));
    assert_eq!(
        std::fs::read_to_string(&scripts).expect("read"),
        "not a directory"
    );
}

#[cfg(unix)]
#[test]
fn never_follows_or_replaces_an_existing_target() {
    let temp = tempfile::tempdir().expect("tempdir");
    let scripts = temp.path().join("scripts");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&scripts).expect("mkdir");
    std::fs::create_dir_all(&outside).expect("mkdir");
    let victim = outside.join("victim.txt");
    std::fs::write(&victim, "original").expect("write");
    let stamp = file_timestamp_at(1_791_248_523_004).expect("timestamp");
    assert_eq!(stamp, "2026-10-06T01-02-03-004Z");
    std::os::unix::fs::symlink(&victim, scripts.join(format!("linked_{stamp}.js")))
        .expect("symlink");
    std::fs::write(scripts.join(format!("existing_{stamp}.js")), "keep me").expect("write");

    let via_symlink = write_fix_script_file(
        &scripts,
        &format!("linked_{stamp}.js"),
        "gs.info('overwrite');",
    )
    .expect_err("symlink target");
    let via_file = write_fix_script_file(
        &scripts,
        &format!("existing_{stamp}.js"),
        "gs.info('overwrite');",
    )
    .expect_err("file target");
    assert!(via_symlink.to_string().ends_with("already exists"));
    assert!(matches!(via_file, FixScriptError::AlreadyExists(_)));
    assert_eq!(std::fs::read_to_string(&victim).expect("read"), "original");
    assert_eq!(
        std::fs::read_to_string(scripts.join(format!("existing_{stamp}.js"))).expect("read"),
        "keep me"
    );
}

#[test]
fn refuses_unsafe_generated_file_names() {
    let temp = tempfile::tempdir().expect("tempdir");
    for name in [
        "../x.js",
        "x",
        ".js",
        "a/b.js",
        "x.JS",
        &format!("{}.js", "a".repeat(201)),
    ] {
        let error = write_fix_script_file(temp.path(), name, "").expect_err(name);
        assert_eq!(
            error.to_string(),
            "Refusing to write fix script: generated file name is not a safe basename"
        );
    }
    assert!(write_fix_script_file(temp.path(), &format!("{}.js", "a".repeat(200)), "").is_ok());
    assert!(list_files(temp.path()).len() == 1);
}

/// `expectDataAssignment`: `variable` is assigned exactly one literal that
/// decodes to `expected`, on one line under every JS line terminator; returns
/// the other lines.
fn expect_data_assignment(source: &str, variable: &str, expected: &Value) -> String {
    let prefix = format!("var {variable} = ");
    let lines: Vec<&str> = source
        .split("\r\n")
        .flat_map(|chunk| chunk.split(['\n', '\r', '\u{2028}', '\u{2029}']))
        .collect();
    let assignments: Vec<&&str> = lines
        .iter()
        .filter(|line| line.starts_with(&prefix))
        .collect();
    assert_eq!(assignments.len(), 1);
    let literal = assignments[0][prefix.len()..]
        .strip_suffix(';')
        .unwrap_or(&assignments[0][prefix.len()..]);
    let decoded: Value = serde_json::from_str(literal).expect("one complete literal");
    assert_eq!(&decoded, expected);
    lines
        .iter()
        .filter(|line| !line.starts_with(&prefix))
        .copied()
        .collect::<Vec<_>>()
        .join("\n")
}

fn leading_block_comment(source: &str) -> &str {
    let start = source.find("/*").expect("comment");
    let end = source[start + 2..].find("*/").expect("end") + start + 4;
    &source[start..end]
}

#[test]
fn fix_script_metadata_stays_out_of_executable_code() {
    for (label, description) in HOSTILE_VALUES {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = create_fix_script(temp.path(), &json!("metadata_check"), Some(description))
            .expect(label);
        let content = std::fs::read_to_string(path).expect("read");
        assert!(
            !leading_block_comment(&content).contains("pwned"),
            "{label}"
        );
        let remainder = expect_data_assignment(
            &content,
            "fixScriptMetadata",
            &json!({ "script_name": "metadata_check", "description": description }),
        );
        assert!(!remainder.contains("pwned"), "{label}");
        assert!(remainder.contains("gs.info('hello');"));
    }
}

#[test]
fn update_set_names_are_serialized_as_data() {
    // Pure half of the VULN-009 manual fix-script fallback (SN-Set-Update-Set).
    let sys_id = "0123456789abcdef0123456789abcdef";
    for (label, name) in HOSTILE_VALUES {
        let body = format!(
            "// Set current update set using GlideUpdateSet API\nvar updateSetSysId = {};\nvar updateSetName = {};\nvar gus = new GlideUpdateSet();\ngus.set(updateSetSysId);\ngs.info('✅ Update set changed to: ' + updateSetName);",
            to_javascript_string_literal(sys_id),
            to_javascript_string_literal(name)
        );
        let content = build_fix_script_file_content(&FixScriptFile {
            title: "Fix Script: Set Current Update Set",
            created_at: "2026-10-06T01:02:03.004Z",
            notes: &[
                "Note: Automated methods failed. Manual execution required.",
                "",
            ],
            instructions: &["Copy the script below this comment"],
            metadata: None,
            body: &body,
        });
        assert!(
            !leading_block_comment(&content).contains("pwned"),
            "{label}"
        );
        expect_data_assignment(&content, "updateSetSysId", &json!(sys_id));
        let remainder = expect_data_assignment(&content, "updateSetName", &json!(name));
        assert!(!remainder.contains("pwned"), "{label}");
        assert!(remainder.contains("gus.set(updateSetSysId);"));
        let file_name = format!(
            "set_update_set_{}_{}.js",
            to_file_name_fragment(Some(&json!(name))),
            file_timestamp()
        );
        assert!(
            file_name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"_-.".contains(&b)),
            "{file_name}"
        );
    }
}

#[test]
fn bounds_the_file_name_derived_from_a_very_long_update_set_name() {
    let fragment = to_file_name_fragment(Some(&json!("x".repeat(400))));
    assert_eq!(fragment, "x".repeat(64));
    let file_name = format!("set_update_set_{fragment}_{}.js", file_timestamp());
    assert!(file_name.len() <= 120);
}

#[test]
fn javascript_literal_and_fragment_semantics() {
    assert_eq!(
        to_javascript_string_literal("a\u{2028}b\u{2029}\"\n"),
        "\"a\\u2028b\\u2029\\\"\\n\""
    );
    assert_eq!(
        to_javascript_literal(&json!({ "n": 1.0, "s": "\u{1}" })),
        r#"{"n":1,"s":"\u0001"}"#
    );
    assert_eq!(to_file_name_fragment(None), "unnamed");
    assert_eq!(to_file_name_fragment(Some(&json!(null))), "unnamed");
    assert_eq!(to_file_name_fragment(Some(&json!(""))), "unnamed");
    assert_eq!(
        to_file_name_fragment(Some(&json!("My Update-Set.v2"))),
        "My_Update_Set_v2"
    );
    assert_eq!(to_file_name_fragment(Some(&json!("a\u{1F600}b"))), "a__b");
    assert_eq!(to_file_name_fragment(Some(&json!(false))), "false");
    assert_eq!(to_file_name_fragment(Some(&json!({}))), "_object_Object_");
}

#[test]
fn builds_the_exact_javascript_file_layout() {
    let metadata = json!({ "description": "d" });
    let content = build_fix_script_file_content(&FixScriptFile {
        title: "T",
        created_at: "C",
        notes: &["n1", ""],
        instructions: &["s1", "s2"],
        metadata: Some(&metadata),
        body: "B",
    });
    assert_eq!(
        content,
        "/**\n * T\n * Created: C\n *\n * n1\n *\n *\n * Caller- or instance-supplied values appear only as data literals below,\n * never as executable code or comment text.\n *\n * INSTRUCTIONS:\n * 1. s1\n * 2. s2\n */\n\nvar fixScriptMetadata = {\"description\":\"d\"};\n\nB\n\n// End of script\n"
    );
}
