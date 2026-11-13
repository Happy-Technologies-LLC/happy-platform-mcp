//! Docs configuration and path safety (port of `src/docs/config.js`).
//!
//! Resolving *which* config file to read (`HAPPY_CONFIG_PATH` → user path → legacy path) is
//! the caller's job (hpm-core's config-path logic); this module reads a path it is handed.

use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use crate::error::DocsError;
use crate::text::utf16_len;

pub const MAX_DOCS_PATH_LENGTH: usize = 1024;
pub const MAX_DOCS_PATH_DEPTH: usize = 32;

/// Family markdown lives under its own subdirectory so no family name can address the
/// index database (`index.sqlite`, `-wal`, `-shm`) in the cache directory.
pub const DOCS_FILES_DIR: &str = "files";

/// Resolved docs settings. Never carries a GitHub credential (the docs client is anonymous).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DocsConfig {
    pub cache_dir: PathBuf,
    pub local_index_enabled: bool,
    pub enable_vector: bool,
    pub embedding_provider: String,
}

/// The `docs` object of the instances config file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DocsSystemProperties {
    pub cache_dir: Option<String>,
    pub local_index_enabled: bool,
    pub enable_vector: bool,
    pub embedding_provider: Option<String>,
}

fn boolean_property(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(Value::String(text)) => matches!(text.to_lowercase().as_str(), "true" | "1" | "yes"),
        _ => false,
    }
}

fn string_property(value: Option<&Value>) -> Option<String> {
    match value {
        Some(Value::String(text)) if !text.is_empty() => Some(text.clone()),
        _ => None,
    }
}

impl DocsSystemProperties {
    /// Reads the properties out of a parsed config document (`config.docs || {}`).
    #[must_use]
    pub fn from_config(config: &Value) -> Self {
        let Some(docs) = config.get("docs") else {
            return Self::default();
        };
        Self {
            cache_dir: string_property(docs.get("cacheDir")),
            local_index_enabled: boolean_property(docs.get("localIndexEnabled")),
            enable_vector: boolean_property(docs.get("enableVector")),
            embedding_provider: string_property(docs.get("embeddingProvider")),
        }
    }
}

/// Loads `docs` from the config file at `config_path`. A missing or syntactically invalid
/// file yields defaults; any other failure is an error (as in 6.2.0).
pub fn read_system_docs_properties(config_path: &Path) -> Result<DocsSystemProperties, DocsError> {
    let text = match std::fs::read_to_string(config_path) {
        Ok(text) => text,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Ok(DocsSystemProperties::default());
        }
        Err(error) => {
            return Err(DocsError::plain(format!(
                "Failed to load docs system properties: {error}"
            )));
        }
    };
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Null) => Err(DocsError::plain(
            "Failed to load docs system properties: Cannot read properties of null (reading 'docs')",
        )),
        Ok(config) => Ok(DocsSystemProperties::from_config(&config)),
        Err(_) => Ok(DocsSystemProperties::default()),
    }
}

/// `env` looks a variable up by name; `Some("")` means "present but empty", which counts as
/// present for the boolean flags (JS `hasOwnProperty`) and as absent for the string ones.
#[must_use]
pub fn get_docs_config(
    env: &dyn Fn(&str) -> Option<String>,
    system_properties: &DocsSystemProperties,
    home_dir: &Path,
) -> DocsConfig {
    let non_empty = |name: &str| env(name).filter(|value| !value.is_empty());
    let flag = |name: &str, configured: bool| match env(name) {
        Some(value) => boolean_property(Some(&Value::String(value))),
        None => configured,
    };

    DocsConfig {
        cache_dir: non_empty("HAPPY_DOCS_CACHE_DIR")
            .or_else(|| system_properties.cache_dir.clone())
            .map_or_else(|| default_cache_dir(home_dir), PathBuf::from),
        local_index_enabled: flag(
            "HAPPY_DOCS_ENABLE_LOCAL_INDEX",
            system_properties.local_index_enabled,
        ),
        enable_vector: flag("HAPPY_DOCS_ENABLE_VECTOR", system_properties.enable_vector),
        embedding_provider: non_empty("HAPPY_DOCS_EMBEDDING_PROVIDER")
            .or_else(|| system_properties.embedding_provider.clone())
            .unwrap_or_else(|| "none".to_owned()),
    }
}

#[must_use]
pub fn default_cache_dir(home_dir: &Path) -> PathBuf {
    home_dir
        .join(".happy-platform-mcp")
        .join("docs")
        .join("servicenow")
}

fn is_name_byte(byte: u8, first: bool) -> bool {
    byte.is_ascii_alphanumeric() || (!first && matches!(byte, b'.' | b'_' | b'-'))
}

/// `^[A-Za-z0-9][A-Za-z0-9._-]{0,99}$` and no `..`.
fn is_safe_name(value: &str) -> bool {
    let bytes = value.as_bytes();
    (1..=100).contains(&bytes.len())
        && bytes
            .iter()
            .enumerate()
            .all(|(index, &byte)| is_name_byte(byte, index == 0))
        && !value.contains("..")
}

/// Family names and git refs share one conservative single-segment syntax.
pub fn assert_docs_family_name(family: &str) -> Result<&str, DocsError> {
    if is_safe_name(family) {
        Ok(family)
    } else {
        Err(DocsError::plain(
            "Invalid ServiceNow docs family: expected 1-100 letters, digits, \".\", \"_\" or \"-\"",
        ))
    }
}

pub fn assert_docs_branch_name(branch: &str) -> Result<&str, DocsError> {
    if is_safe_name(branch) {
        Ok(branch)
    } else {
        Err(DocsError::plain(
            "Invalid ServiceNow docs branch: expected 1-100 letters, digits, \".\", \"_\" or \"-\"",
        ))
    }
}

/// `^[A-Za-z0-9_~+=,@()-][A-Za-z0-9._~+=,@()-]{0,254}$`
fn is_safe_segment(segment: &str) -> bool {
    let bytes = segment.as_bytes();
    (1..=255).contains(&bytes.len())
        && bytes.iter().enumerate().all(|(index, &byte)| {
            byte.is_ascii_alphanumeric()
                || b"_~+=,@()-".contains(&byte)
                || (index > 0 && byte == b'.')
        })
}

fn unsafe_path(reason: &str) -> DocsError {
    DocsError::plain(format!("Unsafe docs path: {reason}"))
}

/// Canonical relative markdown path inside a docs branch. Rejects instead of normalizing: no
/// percent-encoding, dot segments, empty segments, separators other than "/", controls,
/// query/fragment markers, or drive/scheme colons.
pub fn normalize_docs_document_path(document_path: &str) -> Result<&str, DocsError> {
    if document_path.is_empty() {
        return Err(unsafe_path("a relative markdown path is required"));
    }
    if utf16_len(document_path) > MAX_DOCS_PATH_LENGTH {
        return Err(unsafe_path(&format!(
            "longer than {MAX_DOCS_PATH_LENGTH} characters"
        )));
    }
    if document_path.split('/').count() > MAX_DOCS_PATH_DEPTH {
        return Err(unsafe_path(&format!(
            "deeper than {MAX_DOCS_PATH_DEPTH} segments"
        )));
    }
    if !document_path.split('/').all(is_safe_segment) {
        return Err(unsafe_path(
            "only relative paths of letters, digits and ._~+=,@()- segments are allowed",
        ));
    }
    if !document_path.ends_with(".md") {
        return Err(unsafe_path("only .md documents are allowed"));
    }
    Ok(document_path)
}

/// Absolute path of a family directory (`document_path == None`) or one of its documents
/// under `<cache_dir>/files/<family>/…`.
pub fn resolve_docs_cache_path(
    cache_dir: &Path,
    family: &str,
    document_path: Option<&str>,
) -> Result<PathBuf, DocsError> {
    assert_docs_family_name(family)?;
    let segments: Vec<&str> = match document_path {
        Some(path) => normalize_docs_document_path(path)?.split('/').collect(),
        None => Vec::new(),
    };

    let cache_root = std::path::absolute(cache_dir)
        .map_err(|_| DocsError::plain("Unsafe docs path: the docs cache directory is invalid"))?;
    let root = cache_root.join(DOCS_FILES_DIR);
    let mut resolved = root.join(family);
    resolved.extend(&segments);

    let contained = resolved.starts_with(&root)
        && resolved != root
        && resolved
            .components()
            .all(|part| !matches!(part, Component::ParentDir));
    if !contained {
        return Err(DocsError::plain(
            "Unsafe docs path: resolves outside the docs cache",
        ));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    // Ported from tests/docs-config.test.js. The JS tests mutate `process.env`; here the
    // environment is an injected lookup so tests stay parallel-safe.

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |name| map.get(name).cloned()
    }

    #[test]
    fn uses_the_default_cache_directory_under_the_user_home() {
        let home = Path::new("/home/someone");
        let config = get_docs_config(&env_of(&[]), &DocsSystemProperties::default(), home);
        assert_eq!(
            config.cache_dir,
            home.join(".happy-platform-mcp/docs/servicenow")
        );
        assert!(!config.local_index_enabled);
        assert!(!config.enable_vector);
        assert_eq!(config.embedding_provider, "none");
    }

    #[test]
    fn allows_cache_directory_and_vector_flag_through_env_vars() {
        let env = env_of(&[
            ("HAPPY_DOCS_CACHE_DIR", "/tmp/happy-docs"),
            ("HAPPY_DOCS_ENABLE_LOCAL_INDEX", "true"),
            ("HAPPY_DOCS_ENABLE_VECTOR", "true"),
        ]);
        let config = get_docs_config(&env, &DocsSystemProperties::default(), Path::new("/h"));
        assert_eq!(config.cache_dir, PathBuf::from("/tmp/happy-docs"));
        assert!(config.local_index_enabled);
        assert!(config.enable_vector);
    }

    #[test]
    fn loads_docs_system_properties_from_the_local_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::json!({
                "docs": {
                    "cacheDir": "/tmp/from-config",
                    "localIndexEnabled": true,
                    "enableVector": true,
                    "embeddingProvider": "local",
                    "githubToken": "ghp_from_config"
                }
            })
            .to_string(),
        )
        .unwrap();

        let properties = read_system_docs_properties(&config_path).unwrap();
        let config = get_docs_config(
            &env_of(&[("GITHUB_TOKEN", "ghp_ambient")]),
            &properties,
            Path::new("/h"),
        );

        assert_eq!(
            config,
            DocsConfig {
                cache_dir: PathBuf::from("/tmp/from-config"),
                local_index_enabled: true,
                enable_vector: true,
                embedding_provider: "local".to_owned(),
            }
        );
    }

    #[test]
    fn env_overrides_system_properties_including_explicit_false() {
        let properties = DocsSystemProperties {
            cache_dir: Some("/from-config".to_owned()),
            local_index_enabled: true,
            enable_vector: true,
            embedding_provider: Some("local".to_owned()),
        };
        let env = env_of(&[
            ("HAPPY_DOCS_ENABLE_LOCAL_INDEX", "no"),
            ("HAPPY_DOCS_ENABLE_VECTOR", ""),
            ("HAPPY_DOCS_CACHE_DIR", ""),
            ("HAPPY_DOCS_EMBEDDING_PROVIDER", "other"),
        ]);
        let config = get_docs_config(&env, &properties, Path::new("/h"));
        assert!(!config.local_index_enabled);
        assert!(!config.enable_vector);
        assert_eq!(config.cache_dir, PathBuf::from("/from-config"));
        assert_eq!(config.embedding_provider, "other");
    }

    #[test]
    fn missing_or_invalid_config_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            read_system_docs_properties(&dir.path().join("absent.json")).unwrap(),
            DocsSystemProperties::default()
        );
        let broken = dir.path().join("broken.json");
        std::fs::write(&broken, "{ not json").unwrap();
        assert_eq!(
            read_system_docs_properties(&broken).unwrap(),
            DocsSystemProperties::default()
        );
        // A directory is not ENOENT/SyntaxError: surfaced as an error.
        assert!(
            read_system_docs_properties(dir.path())
                .unwrap_err()
                .message()
                .starts_with("Failed to load docs system properties:")
        );
    }

    #[test]
    fn never_surfaces_ambient_github_credentials_in_docs_config() {
        let config = get_docs_config(
            &env_of(&[("GITHUB_TOKEN", "ghp_ambient")]),
            &DocsSystemProperties::default(),
            Path::new("/h"),
        );
        assert!(!serde_json::to_string(&config).unwrap().contains("ghp_"));
    }

    #[test]
    fn normalizes_safe_nested_document_paths_and_rejects_unsafe_ones() {
        assert_eq!(
            normalize_docs_document_path("markdown/pub/nested/foo.md").unwrap(),
            "markdown/pub/nested/foo.md"
        );
        for unsafe_path in [
            "../secret.md",
            "/absolute.md",
            "a/./b.md",
            "a/%2e%2e/b.md",
            "a\\b.md",
            "C:/x.md",
            "a/b.md?x",
            "a b.md",
            "",
        ] {
            let error = normalize_docs_document_path(unsafe_path).unwrap_err();
            assert!(
                error.message().starts_with("Unsafe docs path"),
                "{unsafe_path:?}: {error}"
            );
        }
        let long = format!("{}.md", "x".repeat(1100));
        assert!(
            normalize_docs_document_path(&long)
                .unwrap_err()
                .message()
                .starts_with("Unsafe docs path")
        );
    }

    #[test]
    fn rejects_controls_depth_and_non_markdown() {
        for unsafe_path in [
            "markdown/x\u{0000}.md",
            "markdown/x\n.md",
            "markdown//x.md",
            "markdown/x.txt",
            "é.md",
        ] {
            assert!(
                normalize_docs_document_path(unsafe_path).is_err(),
                "{unsafe_path:?}"
            );
        }
        let deep = format!("{}x.md", "a/".repeat(40));
        assert!(
            normalize_docs_document_path(&deep)
                .unwrap_err()
                .message()
                .contains("deeper than")
        );
        assert!(normalize_docs_document_path(".hidden.md").is_err());
        assert!(normalize_docs_document_path("a/.x.md").is_err());
        assert!(normalize_docs_document_path("a.b/c.md").is_ok());
    }

    #[test]
    fn validates_family_names() {
        assert_eq!(assert_docs_family_name("australia").unwrap(), "australia");
        let too_long = "x".repeat(101);
        for unsafe_name in ["", "../x", "a/b", "a..b", ".hidden", too_long.as_str()] {
            let error = assert_docs_family_name(unsafe_name).unwrap_err();
            assert!(
                error
                    .message()
                    .starts_with("Invalid ServiceNow docs family"),
                "{unsafe_name:?}"
            );
        }
        assert!(
            assert_docs_branch_name("feature/x")
                .unwrap_err()
                .message()
                .starts_with("Invalid ServiceNow docs branch")
        );
        assert!(assert_docs_family_name(&"x".repeat(100)).is_ok());
    }

    #[test]
    fn resolves_family_cache_paths_under_a_dedicated_files_directory_never_the_index_database() {
        let cache = Path::new("/tmp/cache");
        assert_eq!(
            resolve_docs_cache_path(cache, "australia", Some("foo/bar.md")).unwrap(),
            Path::new("/tmp/cache/files/australia/foo/bar.md")
        );
        assert_eq!(
            resolve_docs_cache_path(cache, "australia", None).unwrap(),
            Path::new("/tmp/cache/files/australia")
        );
        assert_eq!(
            resolve_docs_cache_path(cache, "index.sqlite", None).unwrap(),
            Path::new("/tmp/cache/files/index.sqlite")
        );
        let error = resolve_docs_cache_path(cache, "..", Some("x.md")).unwrap_err();
        assert!(
            error
                .message()
                .starts_with("Invalid ServiceNow docs family")
        );
    }
}
