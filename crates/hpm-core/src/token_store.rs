//! OAuth refresh-token store (lane A). Ports `src/token-store.js`: keychain by
//! default, file store when `SERVICENOW_TOKEN_STORE=file`, both behind
//! [`crate::keychain::SecretStore`] semantics and the same account grammar. Also
//! ports the refresh-token account identity (`oauthRefreshTokenAccount`,
//! `legacyRefreshTokenAccount`, `clearInstanceRefreshTokens` in
//! `src/servicenow-client.js`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use hpm_types::BoxFut;
use hpm_types::credential::Secret;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use url::Url;

use crate::config::{CodedError, Env, Object, home_dir, trim_trailing_slashes};
use crate::keychain::{SERVICE_NAME, SecretStore, SecretStoreError};
use crate::registry::resolve_oauth_endpoints;

/// `^[A-Za-z0-9_@.-]{1,200}$`, not `.`, no `..`.
pub fn is_valid_token_account(account: &str) -> bool {
    let b = account.as_bytes();
    (1..=200).contains(&b.len())
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'@' | b'.' | b'-'))
        && account != "."
        && !account.contains("..")
}

fn assert_account(account: &str) -> Result<(), CodedError> {
    if is_valid_token_account(account) {
        Ok(())
    } else {
        Err(CodedError::plain("unsafe account key"))
    }
}

/// Persists one OAuth refresh token per account key.
pub trait TokenStore: Send + Sync {
    fn get_refresh_token<'a>(
        &'a self,
        account: &'a str,
    ) -> BoxFut<'a, Result<Option<String>, CodedError>>;
    fn set_refresh_token<'a>(
        &'a self,
        account: &'a str,
        token: &'a str,
    ) -> BoxFut<'a, Result<(), CodedError>>;
    fn clear_refresh_token<'a>(&'a self, account: &'a str) -> BoxFut<'a, Result<(), CodedError>>;
}

/// No persistence across processes (tests).
#[derive(Default)]
pub struct InMemoryTokenStore(Mutex<HashMap<String, String>>);

impl InMemoryTokenStore {
    pub fn new() -> Self {
        Self::default()
    }
    fn map(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl TokenStore for InMemoryTokenStore {
    fn get_refresh_token<'a>(
        &'a self,
        account: &'a str,
    ) -> BoxFut<'a, Result<Option<String>, CodedError>> {
        Box::pin(async move {
            assert_account(account)?;
            Ok(self.map().get(account).cloned())
        })
    }
    fn set_refresh_token<'a>(
        &'a self,
        account: &'a str,
        token: &'a str,
    ) -> BoxFut<'a, Result<(), CodedError>> {
        Box::pin(async move {
            assert_account(account)?;
            self.map().insert(account.into(), token.into());
            Ok(())
        })
    }
    fn clear_refresh_token<'a>(&'a self, account: &'a str) -> BoxFut<'a, Result<(), CodedError>> {
        Box::pin(async move {
            assert_account(account)?;
            self.map().remove(account);
            Ok(())
        })
    }
}

fn keychain_error(e: SecretStoreError) -> CodedError {
    CodedError::new("KEYCHAIN_OPERATION_FAILED", e.to_string())
}

/// OS keychain (service [`SERVICE_NAME`], account = the token account).
pub struct KeychainTokenStore {
    store: Arc<dyn SecretStore>,
    service: String,
}

impl KeychainTokenStore {
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self {
            store,
            service: SERVICE_NAME.into(),
        }
    }
}

impl TokenStore for KeychainTokenStore {
    fn get_refresh_token<'a>(
        &'a self,
        account: &'a str,
    ) -> BoxFut<'a, Result<Option<String>, CodedError>> {
        Box::pin(async move {
            assert_account(account)?;
            // A real fault must fail loud, never masquerade as "no token".
            match self.store.get(&self.service, account).await {
                Ok(v) => Ok(v.map(|s| s.expose().to_owned())),
                Err(e) => {
                    eprintln!("Keychain read failed");
                    Err(keychain_error(e))
                }
            }
        })
    }
    fn set_refresh_token<'a>(
        &'a self,
        account: &'a str,
        token: &'a str,
    ) -> BoxFut<'a, Result<(), CodedError>> {
        Box::pin(async move {
            assert_account(account)?;
            self.store
                .set(&self.service, account, Secret::new(token.to_owned()))
                .await
                .map_err(keychain_error)
        })
    }
    fn clear_refresh_token<'a>(&'a self, account: &'a str) -> BoxFut<'a, Result<(), CodedError>> {
        Box::pin(async move {
            assert_account(account)?;
            self.store
                .delete(&self.service, account)
                .await
                .map(|_| ())
                .map_err(keychain_error)
        })
    }
}

/// `$XDG_CONFIG_HOME/happy-platform-mcp`, else `~/.config/happy-platform-mcp`.
pub fn default_token_dir(env: &Env) -> PathBuf {
    let base = env
        .get("XDG_CONFIG_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home_dir(env).join(".config"));
    base.join("happy-platform-mcp")
}

/// Optional plaintext POSIX store: private permissions, atomic replacement.
pub struct FileTokenStore {
    base_dir: PathBuf,
}

impl FileTokenStore {
    pub fn new(base_dir: &Path) -> Result<Self, CodedError> {
        let s = base_dir.to_string_lossy();
        if !base_dir.is_absolute()
            || s.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f)
            || s.split('/').any(|p| p == "." || p == "..")
            || crate::config::resolve_path(base_dir) == Path::new("/")
        {
            return Err(CodedError::plain("unsafe token directory path"));
        }
        Ok(Self {
            base_dir: crate::config::resolve_path(base_dir),
        })
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    fn file_name(account: &str) -> Result<String, CodedError> {
        assert_account(account)?;
        Ok(format!(
            "token-{}",
            hex(&Sha256::digest(account.as_bytes()))
        ))
    }
}

fn io(e: std::io::Error) -> CodedError {
    CodedError::new(
        e.raw_os_error()
            .map(|_| format!("{:?}", e.kind()))
            .unwrap_or_default(),
        e.to_string(),
    )
}

#[cfg(unix)]
mod posix {
    use std::fs::{self, Metadata};
    use std::io::{Read, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::path::{Path, PathBuf};

    use super::{CodedError, FileTokenStore, io};

    fn uid() -> u32 {
        nix::unistd::getuid().as_raw()
    }

    fn assert_private(stat: &Metadata, path: &Path, directory: bool) -> Result<(), CodedError> {
        let kind_ok = if directory {
            stat.is_dir()
        } else {
            stat.is_file()
        };
        if !kind_ok
            || stat.file_type().is_symlink()
            || stat.uid() != uid()
            || (stat.mode() & 0o7777) != if directory { 0o700 } else { 0o600 }
            || (!directory && stat.nlink() > 1)
        {
            let p = path.display();
            return Err(CodedError::plain(if directory {
                format!(
                    "unsafe token directory {p}: must be a real directory (not a symlink) owned by the current user with mode 0700"
                )
            } else {
                format!(
                    "unsafe token file {p}: must be a regular, singly linked file owned by the current user with mode 0600"
                )
            }));
        }
        Ok(())
    }

    fn assert_trusted_ancestor(stat: &Metadata, path: &Path) -> Result<(), CodedError> {
        if !stat.is_dir()
            || (stat.uid() != 0 && stat.uid() != uid())
            || ((stat.mode() & 0o022) != 0 && (stat.mode() & 0o1000) == 0)
        {
            let p = path.display();
            return Err(CodedError::plain(format!(
                "unsafe token directory ancestor {p}: must be a real directory owned by the current user or root and not group/world-writable unless sticky (for example, chmod go-w {p})"
            )));
        }
        Ok(())
    }

    impl FileTokenStore {
        fn trusted_parent(&self, create: bool) -> Result<PathBuf, CodedError> {
            let mut missing = Vec::new();
            let mut path = self
                .base_dir
                .parent()
                .unwrap_or(Path::new("/"))
                .to_path_buf();
            let mut canonical = loop {
                match fs::canonicalize(&path) {
                    Ok(c) => break c,
                    Err(e) => {
                        let nf = matches!(e.kind(), std::io::ErrorKind::NotFound)
                            || e.raw_os_error() == Some(nix::libc::ENOTDIR);
                        if !create || !nf {
                            return Err(io(e));
                        }
                        missing.insert(
                            0,
                            path.file_name().map(|n| n.to_owned()).unwrap_or_default(),
                        );
                        path = path.parent().unwrap_or(Path::new("/")).to_path_buf();
                    }
                }
            };
            let mut ancestor = canonical.clone();
            loop {
                assert_trusted_ancestor(&fs::symlink_metadata(&ancestor).map_err(io)?, &ancestor)?;
                match ancestor.parent() {
                    Some(p) => ancestor = p.to_path_buf(),
                    None => break,
                }
            }
            for name in missing {
                canonical = canonical.join(name);
                let mut b = fs::DirBuilder::new();
                std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
                if let Err(e) = b.create(&canonical)
                    && e.kind() != std::io::ErrorKind::AlreadyExists
                {
                    return Err(io(e));
                }
                assert_trusted_ancestor(
                    &fs::symlink_metadata(&canonical).map_err(io)?,
                    &canonical,
                )?;
            }
            Ok(canonical)
        }

        pub(super) fn directory(&self, create: bool) -> Result<Option<PathBuf>, CodedError> {
            let stat = match fs::symlink_metadata(&self.base_dir) {
                Ok(s) => Some(s),
                Err(e) => {
                    let notdir = e.raw_os_error() == Some(nix::libc::ENOTDIR);
                    if e.kind() != std::io::ErrorKind::NotFound && !(create && notdir) {
                        return Err(io(e));
                    }
                    if !create {
                        return Ok(None);
                    }
                    None
                }
            };
            if let Some(s) = &stat {
                assert_private(s, &self.base_dir, true)?;
            }
            let name = self.base_dir.file_name().unwrap_or_default().to_owned();
            let directory = self.trusted_parent(create && stat.is_none())?.join(name);
            if stat.is_none() {
                let mut b = fs::DirBuilder::new();
                std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
                match b.create(&directory) {
                    Ok(()) => fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
                        .map_err(io)?,
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(io(e)),
                }
            }
            assert_private(
                &fs::symlink_metadata(&directory).map_err(io)?,
                &directory,
                true,
            )?;
            Ok(Some(directory))
        }

        pub(super) fn existing(&self, file: &Path) -> Result<bool, CodedError> {
            match fs::symlink_metadata(file) {
                Ok(s) => assert_private(&s, file, false).map(|()| true),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(e) => Err(io(e)),
            }
        }

        pub(super) fn read(&self, file: &Path) -> Result<Option<String>, CodedError> {
            let nofollow = nix::fcntl::OFlag::O_NOFOLLOW.bits();
            let mut handle = match fs::OpenOptions::new()
                .read(true)
                .custom_flags(nofollow)
                .open(file)
            {
                Ok(h) => h,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(io(e)),
            };
            assert_private(&handle.metadata().map_err(io)?, file, false)?;
            let mut text = String::new();
            handle.read_to_string(&mut text).map_err(io)?;
            let token = crate::config::js_trim(&text);
            Ok((!token.is_empty()).then(|| token.to_owned()))
        }

        pub(super) fn write(
            &self,
            directory: &Path,
            file: &Path,
            token: &str,
        ) -> Result<(), CodedError> {
            let temporary = directory.join(format!(".token-{}.tmp", uuid::Uuid::now_v7()));
            let nofollow = nix::fcntl::OFlag::O_NOFOLLOW.bits();
            let mut handle = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(nofollow)
                .open(&temporary)
                .map_err(io)?;
            let result = (|| {
                handle
                    .set_permissions(fs::Permissions::from_mode(0o600))
                    .map_err(io)?;
                assert_private(&handle.metadata().map_err(io)?, &temporary, false)?;
                handle.write_all(token.as_bytes()).map_err(io)?;
                handle.sync_all().map_err(io)?;
                assert_private(
                    &fs::symlink_metadata(directory).map_err(io)?,
                    directory,
                    true,
                )?;
                self.existing(file)?;
                fs::rename(&temporary, file).map_err(io)
            })();
            if result.is_err() {
                let _ = fs::remove_file(&temporary);
            }
            result
        }
    }
}

impl FileTokenStore {
    #[cfg(unix)]
    fn get_sync(&self, account: &str) -> Result<Option<String>, CodedError> {
        let name = Self::file_name(account)?;
        let Some(directory) = self.directory(false)? else {
            return Ok(None);
        };
        let file = directory.join(name);
        if !self.existing(&file)? {
            return Ok(None);
        }
        self.read(&file)
    }
    #[cfg(unix)]
    fn set_sync(&self, account: &str, token: &str) -> Result<(), CodedError> {
        let name = Self::file_name(account)?;
        let directory = self
            .directory(true)?
            .ok_or_else(|| CodedError::plain("token directory unavailable"))?;
        let file = directory.join(name);
        self.existing(&file)?;
        self.write(&directory, &file, token)
    }
    #[cfg(unix)]
    fn clear_sync(&self, account: &str) -> Result<(), CodedError> {
        let name = Self::file_name(account)?;
        let Some(directory) = self.directory(false)? else {
            return Ok(());
        };
        let file = directory.join(name);
        if !self.existing(&file)? {
            return Ok(());
        }
        match std::fs::remove_file(&file) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(io(e)),
            _ => Ok(()),
        }
    }
    #[cfg(not(unix))]
    fn unsupported() -> CodedError {
        CodedError::plain(
            "File token storage requires POSIX ownership and permissions; use the keychain on Windows",
        )
    }
    #[cfg(not(unix))]
    fn get_sync(&self, account: &str) -> Result<Option<String>, CodedError> {
        Self::file_name(account)?;
        Err(Self::unsupported())
    }
    #[cfg(not(unix))]
    fn set_sync(&self, account: &str, _token: &str) -> Result<(), CodedError> {
        Self::file_name(account)?;
        Err(Self::unsupported())
    }
    #[cfg(not(unix))]
    fn clear_sync(&self, account: &str) -> Result<(), CodedError> {
        Self::file_name(account)?;
        Err(Self::unsupported())
    }
}

impl TokenStore for FileTokenStore {
    fn get_refresh_token<'a>(
        &'a self,
        account: &'a str,
    ) -> BoxFut<'a, Result<Option<String>, CodedError>> {
        Box::pin(async move { self.get_sync(account) })
    }
    fn set_refresh_token<'a>(
        &'a self,
        account: &'a str,
        token: &'a str,
    ) -> BoxFut<'a, Result<(), CodedError>> {
        Box::pin(async move { self.set_sync(account, token) })
    }
    fn clear_refresh_token<'a>(&'a self, account: &'a str) -> BoxFut<'a, Result<(), CodedError>> {
        Box::pin(async move { self.clear_sync(account) })
    }
}

/// `SERVICENOW_TOKEN_STORE`: unset/empty/`keychain` → keychain, `file` → file store.
pub fn create_default_token_store(
    kind: Option<&str>,
    secrets: Arc<dyn SecretStore>,
    env: &Env,
) -> Result<Arc<dyn TokenStore>, CodedError> {
    match kind {
        None | Some("" | "keychain") => Ok(Arc::new(KeychainTokenStore::new(secrets))),
        Some("file") => Ok(Arc::new(FileTokenStore::new(&default_token_dir(env))?)),
        Some(other) => Err(CodedError::plain(format!(
            "SERVICENOW_TOKEN_STORE must be \"keychain\" or \"file\", got {}",
            Value::String(other.to_owned())
        ))),
    }
}

// ---------------------------------------------------------------------------
// Refresh-token identity (src/servicenow-client.js)

pub const REFRESH_TOKEN_IDENTITY_PREFIX: &str = "identity-v1-";
pub const REFRESH_TOKEN_TUPLE_TAG: &str = "happy-platform-mcp/oauth-refresh-token";

/// `os.userInfo().username`: the passwd entry of the real uid (not `$USER`).
pub fn os_user() -> Result<String, CodedError> {
    #[cfg(unix)]
    {
        nix::unistd::User::from_uid(nix::unistd::getuid())
            .ok()
            .flatten()
            .map(|u| u.name)
            .ok_or_else(|| CodedError::plain("Unable to determine the OS user"))
    }
    #[cfg(not(unix))]
    {
        std::env::var("USERNAME").map_err(|_| CodedError::plain("Unable to determine the OS user"))
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn invalid_url() -> CodedError {
    CodedError::new("ERR_INVALID_URL", "Invalid URL")
}

/// `identity-v1-<sha256(JSON [tag, 1, osUser, canonicalUrl, clientId, authorizeHref, tokenHref])>`.
pub fn oauth_refresh_token_account(instance: &Object, os_user: &str) -> Result<String, CodedError> {
    let url = instance.get("url").and_then(Value::as_str).unwrap_or("");
    let parsed = Url::parse(url).map_err(|_| invalid_url())?;
    let canonical = format!(
        "{}{}",
        parsed.origin().ascii_serialization(),
        trim_trailing_slashes(parsed.path())
    );
    let (authorize, token) = resolve_oauth_endpoints(
        &canonical,
        instance.get("authorizeUrl").and_then(Value::as_str),
        instance.get("tokenUrl").and_then(Value::as_str),
    );
    let authorize = Url::parse(&authorize).map_err(|_| invalid_url())?;
    let token = Url::parse(&token).map_err(|_| invalid_url())?;
    let tuple = json!([
        REFRESH_TOKEN_TUPLE_TAG,
        1,
        os_user,
        canonical,
        instance.get("clientId").cloned().unwrap_or(Value::Null),
        authorize.as_str(),
        token.as_str(),
    ])
    .to_string();
    Ok(format!(
        "{REFRESH_TOKEN_IDENTITY_PREFIX}{}",
        hex(&Sha256::digest(tuple.as_bytes()))
    ))
}

/// Pre-identity key `<os user>@<instance name>`; `None` when not addressable.
pub fn legacy_refresh_token_account(instance_name: &str, os_user: &str) -> Option<String> {
    let account = format!("{os_user}@{instance_name}");
    (!instance_name.is_empty() && is_valid_token_account(&account)).then_some(account)
}

/// Clears an instance's identity key and, when addressable, its legacy key.
pub async fn clear_instance_refresh_tokens(
    store: &dyn TokenStore,
    instance: &Object,
    os_user: &str,
) -> Result<(), CodedError> {
    store
        .clear_refresh_token(&oauth_refresh_token_account(instance, os_user)?)
        .await?;
    let name = instance.get("name").and_then(Value::as_str).unwrap_or("");
    if let Some(legacy) = legacy_refresh_token_account(name, os_user) {
        store.clear_refresh_token(&legacy).await?;
    }
    Ok(())
}

#[cfg(test)]
#[path = "token_store_tests.rs"]
mod tests;
