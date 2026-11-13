//! Generated script helpers. Ports `src/generated-scripts.js`.
//!
//! Untrusted values (update-set names, descriptions, caller-chosen names) are
//! embedded in generated ServiceNow JavaScript only as serialized data
//! literals, never as raw source or block-comment text. Fix-script files are
//! created only as new, direct children of the scripts directory.

use super::js;
use serde_json::Value;
use std::io::Write as _;
use std::path::{Component, Path, PathBuf};

const DERIVED_NAME_MAX_LENGTH: usize = 64;

const INVALID_SCRIPT_NAME: &str = "Invalid script_name: use 1-100 letters, digits, \".\", \"_\" or \"-\", starting with a letter or digit (no path separators, \".\"/\"..\", drive or absolute paths, control characters, or Windows device names such as CON, NUL, COM1 or LPT1).";

/// Serializes a value as a JavaScript literal. JSON escapes quotes,
/// backslashes and control characters; U+2028/U+2029 are additionally
/// escaped because pre-ES2019 engines treat them as line terminators.
pub fn to_javascript_literal(value: &Value) -> String {
    escape_line_separators(js::json_stringify(value))
}

/// [`to_javascript_literal`] for a string value.
pub fn to_javascript_string_literal(value: &str) -> String {
    escape_line_separators(js::json_stringify_str(value))
}

fn escape_line_separators(json: String) -> String {
    if !json.contains(['\u{2028}', '\u{2029}']) {
        return json;
    }
    json.replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// `/^[A-Za-z0-9][A-Za-z0-9._-]{0,max_rest}$/` over ASCII bytes.
fn matches_safe_name(bytes: &[u8], max_rest: usize) -> bool {
    !bytes.is_empty()
        && bytes.len() <= max_rest + 1
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// `/^(?:con|prn|aux|nul|com[0-9]|lpt[0-9])(?:\.|$)/i`
fn is_windows_device_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let stem = |len: usize| matches!(bytes.get(len), None | Some(b'.'));
    let prefix = |word: &str| {
        bytes.len() >= word.len() && bytes[..word.len()].eq_ignore_ascii_case(word.as_bytes())
    };
    (["con", "prn", "aux", "nul"].into_iter().any(prefix) && stem(3))
        || (["com", "lpt"].into_iter().any(prefix)
            && bytes.get(3).is_some_and(u8::is_ascii_digit)
            && stem(4))
}

/// A fix-script name or file failed validation or could not be written.
#[derive(Debug, thiserror::Error)]
pub enum FixScriptError {
    #[error("{INVALID_SCRIPT_NAME}")]
    InvalidScriptName,
    #[error("Refusing to write fix script: generated file name is not a safe basename")]
    UnsafeFileName,
    #[error("Refusing to write fix script: scripts directory {} is a symbolic link", .0.display())]
    ScriptsDirectoryIsSymlink(PathBuf),
    #[error("Refusing to write fix script: scripts directory {} is not a directory", .0.display())]
    ScriptsDirectoryNotDirectory(PathBuf),
    #[error("Refusing to write fix script: target is not a direct child of the scripts directory")]
    NotDirectChild,
    #[error("Refusing to write fix script: {} already exists", .0.display())]
    AlreadyExists(PathBuf),
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

/// Validates a caller-supplied fix-script name: 1-100 characters of letters,
/// digits, '.', '_' or '-', starting with a letter or digit, and not a Windows
/// reserved device name. This excludes path separators, '.'/'..', drive or
/// absolute paths, control characters and device names.
pub fn validate_fix_script_name(name: Option<&Value>) -> Result<&str, FixScriptError> {
    match name {
        Some(Value::String(name))
            if matches_safe_name(name.as_bytes(), 99) && !is_windows_device_name(name) =>
        {
            Ok(name)
        }
        _ => Err(FixScriptError::InvalidScriptName),
    }
}

/// Reduces an untrusted value (e.g. an update-set name) to a bounded file-name
/// fragment: `String(value ?? '')`, every UTF-16 code unit outside
/// `[A-Za-z0-9]` replaced by '_', cut to 64 characters, or `unnamed`.
pub fn to_file_name_fragment(value: Option<&Value>) -> String {
    let text = match value {
        None | Some(Value::Null) => String::new(),
        Some(value) => js::to_js_string(value),
    };
    let mut fragment = String::with_capacity(DERIVED_NAME_MAX_LENGTH);
    for c in text.chars() {
        if fragment.len() >= DERIVED_NAME_MAX_LENGTH {
            break;
        }
        if c.is_ascii_alphanumeric() {
            fragment.push(c);
        } else {
            for _ in 0..c.len_utf16() {
                fragment.push('_');
            }
        }
    }
    fragment.truncate(DERIVED_NAME_MAX_LENGTH);
    if fragment.is_empty() {
        "unnamed".to_owned()
    } else {
        fragment
    }
}

/// File-name-safe UTC timestamp, e.g. 2026-10-06T01-02-03-004Z, for the
/// given milliseconds since the Unix epoch.
pub fn file_timestamp_at(millis: i64) -> Result<String, js::InvalidTimeValue> {
    Ok(js::to_iso_string(millis)?.replace([':', '.'], "-"))
}

/// File-name-safe UTC timestamp for the current time.
pub fn file_timestamp() -> String {
    // The system clock is always inside the JavaScript Date range.
    file_timestamp_at(js::now_millis()).unwrap_or_default()
}

/// Inputs of [`build_fix_script_file_content`].
#[derive(Debug, Clone, Copy)]
pub struct FixScriptFile<'a> {
    pub title: &'a str,
    pub created_at: &'a str,
    pub notes: &'a [&'a str],
    pub instructions: &'a [&'a str],
    pub metadata: Option<&'a Value>,
    pub body: &'a str,
}

/// Builds a fix-script file. The header comment holds only fixed text;
/// untrusted values must reach the file only through `metadata` (emitted as a
/// `fixScriptMetadata` data literal) or a body built with
/// [`to_javascript_literal`].
pub fn build_fix_script_file_content(file: &FixScriptFile<'_>) -> String {
    let mut lines: Vec<String> = vec![
        "/**".to_owned(),
        format!(" * {}", file.title),
        format!(" * Created: {}", file.created_at),
        " *".to_owned(),
    ];
    lines.extend(file.notes.iter().map(|note| {
        if note.is_empty() {
            " *".to_owned()
        } else {
            format!(" * {note}")
        }
    }));
    if !file.notes.is_empty() {
        lines.push(" *".to_owned());
    }
    lines.extend(
        [
            " * Caller- or instance-supplied values appear only as data literals below,",
            " * never as executable code or comment text.",
            " *",
            " * INSTRUCTIONS:",
        ]
        .map(str::to_owned),
    );
    lines.extend(
        file.instructions
            .iter()
            .enumerate()
            .map(|(index, step)| format!(" * {}. {step}", index + 1)),
    );
    lines.push(" */".to_owned());
    lines.push(String::new());
    if let Some(metadata) = file.metadata {
        lines.push(format!(
            "var fixScriptMetadata = {};",
            to_javascript_literal(metadata)
        ));
        lines.push(String::new());
    }
    lines.push(file.body.to_owned());
    lines.push(String::new());
    lines.push("// End of script".to_owned());
    lines.push(String::new());
    lines.join("\n")
}

/// `path.resolve(value)`: absolute against the working directory, with `.`
/// and `..` removed lexically (symlinks are not resolved).
fn resolve(value: &Path) -> std::io::Result<PathBuf> {
    let absolute = if value.is_absolute() {
        value.to_path_buf()
    } else {
        std::env::current_dir()?.join(value)
    };
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            other => resolved.push(other),
        }
    }
    Ok(resolved)
}

/// Creates `file_name` as a new private file directly inside `scripts_dir`.
/// Rejects non-basename names, a scripts directory that is a symlink or not a
/// directory, and any existing target (file or symlink) without following it.
/// Returns the absolute path of the written file.
///
/// On Unix the directory is opened once (`O_DIRECTORY|O_NOFOLLOW`), checked
/// to be the same inode as the lstat-verified directory, and the file is
/// created relative to that descriptor (`openat`, `O_EXCL|O_NOFOLLOW`, mode
/// 0600), so a symlink swapped in after the checks cannot redirect the write
/// (review CO-7/TF-11). Elsewhere a path-based re-check remains.
///
/// This does blocking `std::fs` I/O: async tool handlers must call it through
/// `tokio::task::spawn_blocking`.
pub fn write_fix_script_file(
    scripts_dir: &Path,
    file_name: &str,
    content: &str,
) -> Result<PathBuf, FixScriptError> {
    let name = file_name.as_bytes();
    if name.len() < 4 || !name.ends_with(b".js") || !matches_safe_name(&name[..name.len() - 3], 199)
    {
        return Err(FixScriptError::UnsafeFileName);
    }

    let root = resolve(scripts_dir)?;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        // Owner-only for any directory created here (fix scripts are private).
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    if let Err(error) = builder.create(&root) {
        // An existing non-directory or dangling link is reported by lstat below.
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }
    let root_stat = std::fs::symlink_metadata(&root)?;
    if root_stat.file_type().is_symlink() {
        return Err(FixScriptError::ScriptsDirectoryIsSymlink(root));
    }
    if !root_stat.is_dir() {
        return Err(FixScriptError::ScriptsDirectoryNotDirectory(root));
    }

    let file_path = root.join(file_name);
    if file_path.parent() != Some(root.as_path())
        || file_path.file_name() != Some(std::ffi::OsStr::new(file_name))
    {
        return Err(FixScriptError::NotDirectChild);
    }

    write_in_dir(&root, file_name, |handle| {
        handle.write_all(content.as_bytes())
    })?;
    Ok(file_path)
}

/// Opens `root` once and creates `file_name` relative to that descriptor; a
/// failed `write` unlinks it through the same descriptor.
#[cfg(unix)]
fn write_in_dir(
    root: &Path,
    file_name: &str,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<(), FixScriptError> {
    use nix::errno::Errno;
    use nix::fcntl::{OFlag, open, openat};
    use nix::sys::stat::Mode;
    use nix::unistd::{UnlinkatFlags, unlinkat};
    use std::os::unix::fs::MetadataExt as _;

    let expected = std::fs::symlink_metadata(root)?;
    let dir = match open(
        root,
        OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::ELOOP | Errno::ENOTDIR) => {
            return Err(FixScriptError::ScriptsDirectoryIsSymlink(root.into()));
        }
        Err(e) => return Err(std::io::Error::from(e).into()),
    };
    let dir = std::fs::File::from(dir);
    let opened = dir.metadata()?;
    if (opened.dev(), opened.ino()) != (expected.dev(), expected.ino()) {
        // An ancestor was swapped between the lstat and the open.
        return Err(FixScriptError::ScriptsDirectoryIsSymlink(root.into()));
    }
    let fd = match openat(
        &dir,
        file_name,
        OFlag::O_WRONLY | OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    ) {
        Ok(fd) => fd,
        Err(Errno::EEXIST | Errno::ELOOP) => {
            return Err(FixScriptError::AlreadyExists(root.join(file_name)));
        }
        Err(e) => return Err(std::io::Error::from(e).into()),
    };
    let mut handle = std::fs::File::from(fd);
    if let Err(error) = write(&mut handle) {
        drop(handle);
        let _ = unlinkat(&dir, file_name, UnlinkatFlags::NoRemoveDir);
        return Err(error.into());
    }
    Ok(())
}

/// Non-Unix: write through the canonical directory, then re-check that the
/// created file still sits directly in it; otherwise remove it.
#[cfg(not(unix))]
fn write_in_dir(
    root: &Path,
    file_name: &str,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> Result<(), FixScriptError> {
    let canonical = std::fs::canonicalize(root)?;
    let real_path = canonical.join(file_name);
    let mut handle = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&real_path)
    {
        Ok(handle) => handle,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Err(FixScriptError::AlreadyExists(real_path));
        }
        Err(error) => return Err(error.into()),
    };
    if let Err(error) = write(&mut handle) {
        drop(handle);
        let _ = std::fs::remove_file(&real_path);
        return Err(error.into());
    }
    let parent = real_path.parent().map(std::fs::canonicalize).transpose()?;
    if parent.as_deref() != Some(canonical.as_path()) {
        let _ = std::fs::remove_file(&real_path);
        return Err(FixScriptError::ScriptsDirectoryIsSymlink(root.into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_write_removes_the_partial_file() {
        let temp = tempfile::tempdir().expect("tempdir");
        let error = write_in_dir(temp.path(), "write_fails.js", |handle| {
            handle.write_all(b"gs.info('partial');")?;
            Err(std::io::Error::other("simulated disk full"))
        })
        .expect_err("write failure must surface");
        assert!(matches!(error, FixScriptError::Io(_)));
        assert_eq!(error.to_string(), "simulated disk full");
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_refused_without_following_and_mode_is_private() {
        let temp = tempfile::tempdir().expect("tempdir");
        let real = temp.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = temp.path().join("scripts");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            write_fix_script_file(&link, "a.js", "x"),
            Err(FixScriptError::ScriptsDirectoryIsSymlink(_))
        ));
        assert_eq!(std::fs::read_dir(&real).unwrap().count(), 0);

        let outside = temp.path().join("outside.js");
        std::os::unix::fs::symlink(&outside, real.join("b.js")).unwrap();
        assert!(matches!(
            write_fix_script_file(&real, "b.js", "x"),
            Err(FixScriptError::AlreadyExists(_))
        ));
        assert!(!outside.exists());

        let path = write_fix_script_file(&real, "c.js", "ok").expect("write");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "ok");
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}
