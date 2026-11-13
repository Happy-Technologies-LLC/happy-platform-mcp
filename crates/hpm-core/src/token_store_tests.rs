//! Port of `tests/token-store.test.js` (fs spy/race cases are not portable:
//! they monkey-patch Node's `fs` module).

use super::*;
use crate::keychain::MemorySecretStore;

fn token_name(account: &str) -> String {
    format!("token-{}", hex(&Sha256::digest(account.as_bytes())))
}

#[tokio::test]
async fn in_memory_round_trip_and_clear() {
    let store = InMemoryTokenStore::new();
    assert_eq!(store.get_refresh_token("acct").await.unwrap(), None);
    store.set_refresh_token("caleb@dev", "rt-1").await.unwrap();
    store.set_refresh_token("caleb@prod", "rt-2").await.unwrap();
    assert_eq!(
        store
            .get_refresh_token("caleb@dev")
            .await
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("rt-1")
    );
    assert_eq!(
        store
            .get_refresh_token("caleb@prod")
            .await
            .unwrap()
            .as_deref()
            .map(String::as_str),
        Some("rt-2")
    );
    store.clear_refresh_token("caleb@dev").await.unwrap();
    assert_eq!(store.get_refresh_token("caleb@dev").await.unwrap(), None);
}

#[test]
fn account_grammar() {
    for bad in [
        "", ".", "..", "a\\b", "C:dev", "a\n", "a\0", "a b", "../evil", "a/b",
    ] {
        assert!(!is_valid_token_account(bad), "{bad:?}");
    }
    assert!(!is_valid_token_account(&"a".repeat(201)));
    for ok in ["user@dev", "identity-v1-aaaa", &"x".repeat(200)] {
        assert!(is_valid_token_account(ok));
    }
}

#[tokio::test]
async fn keychain_store_round_trip_and_fail_loud() {
    let mem = Arc::new(MemorySecretStore::new());
    let store = KeychainTokenStore::new(mem.clone());
    assert_eq!(store.get_refresh_token("acct").await.unwrap(), None);
    store.set_refresh_token("acct", "rt").await.unwrap();
    assert_eq!(mem.value(SERVICE_NAME, "acct").as_deref(), Some("rt"));
    store.clear_refresh_token("acct").await.unwrap();
    store.clear_refresh_token("acct").await.unwrap();
    mem.fail_next(crate::keychain::MemoryOp::Get);
    assert!(store.get_refresh_token("acct").await.is_err());
    assert!(
        store
            .set_refresh_token("../x", "rt")
            .await
            .unwrap_err()
            .message
            .contains("unsafe account")
    );
}

#[test]
fn default_store_kind() {
    let env = Env::from_pairs(&[("HOME", "/tmp")]);
    let mem: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
    assert!(create_default_token_store(None, mem.clone(), &env).is_ok());
    assert!(create_default_token_store(Some("keychain"), mem.clone(), &env).is_ok());
    let err = create_default_token_store(Some("vault"), mem, &env)
        .err()
        .unwrap();
    assert_eq!(
        err.message,
        "SERVICENOW_TOKEN_STORE must be \"keychain\" or \"file\", got \"vault\""
    );
}

#[test]
fn rejects_unsafe_directory_paths() {
    for p in ["relative", "/", "/tmp/../escape", "/tmp/control\n"] {
        let err = FileTokenStore::new(Path::new(p)).err().unwrap();
        assert!(err.message.contains("unsafe token directory"), "{p:?}");
    }
}

#[cfg(unix)]
mod file {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn base() -> (tempfile::TempDir, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let canon = std::fs::canonicalize(root.path()).unwrap();
        (root, canon.join("tokens"))
    }

    #[tokio::test]
    async fn round_trip_perms_overwrite_clear() {
        let (_r, dir) = base();
        let store = FileTokenStore::new(&dir).unwrap();
        assert_eq!(store.get_refresh_token("dev").await.unwrap(), None);
        store.set_refresh_token("dev", "rt-1").await.unwrap();
        store.set_refresh_token("prod", "rt-prod").await.unwrap();
        store.set_refresh_token("dev", "rt-2").await.unwrap();
        assert_eq!(
            store
                .get_refresh_token("dev")
                .await
                .unwrap()
                .as_deref()
                .map(String::as_str),
            Some("rt-2")
        );
        assert_eq!(
            store
                .get_refresh_token("prod")
                .await
                .unwrap()
                .as_deref()
                .map(String::as_str),
            Some("rt-prod")
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join(token_name("dev"))), 0o600);
        assert_eq!(mode(&dir), 0o700);
        store.clear_refresh_token("dev").await.unwrap();
        assert_eq!(store.get_refresh_token("dev").await.unwrap(), None);
    }

    #[tokio::test]
    async fn rejects_unsafe_bases() {
        let (_r, dir) = base();
        std::fs::write(&dir, "fake").unwrap();
        assert!(
            FileTokenStore::new(&dir)
                .unwrap()
                .set_refresh_token("dev", "f")
                .await
                .is_err()
        );
        let (_r, dir) = base();
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            FileTokenStore::new(&dir)
                .unwrap()
                .set_refresh_token("dev", "f")
                .await
                .is_err()
        );
        let (_r, dir) = base();
        std::os::unix::fs::symlink(std::env::temp_dir(), &dir).unwrap();
        assert!(
            FileTokenStore::new(&dir)
                .unwrap()
                .set_refresh_token("dev", "f")
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn rejects_multiply_linked_and_unsafe_files() {
        let (_r, dir) = base();
        let store = FileTokenStore::new(&dir).unwrap();
        store.set_refresh_token("dev", "inert").await.unwrap();
        std::fs::hard_link(dir.join(token_name("dev")), dir.join("alias")).unwrap();
        let err = store.get_refresh_token("dev").await.unwrap_err();
        assert!(err.message.contains("unsafe token file"));
        assert!(store.set_refresh_token("dev", "x").await.is_err());
        std::fs::remove_file(dir.join("alias")).unwrap();
        let target = dir.join(token_name("dev"));
        std::fs::remove_file(&target).unwrap();
        std::os::unix::fs::symlink("missing", &target).unwrap();
        assert!(store.get_refresh_token("dev").await.is_err());
        assert!(store.set_refresh_token("dev", "x").await.is_err());
        std::fs::remove_file(&target).unwrap();
        std::fs::write(&target, "fake").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.get_refresh_token("dev").await.is_err());
    }

    #[tokio::test]
    async fn case_distinct_files_and_no_legacy_names() {
        let (_r, dir) = base();
        let store = FileTokenStore::new(&dir).unwrap();
        store.set_refresh_token("u@Dev", "upper").await.unwrap();
        assert_eq!(store.get_refresh_token("u@dev").await.unwrap(), None);
        store.set_refresh_token("u@dev", "lower").await.unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        let mut want = vec![token_name("u@Dev"), token_name("u@dev")];
        want.sort();
        assert_eq!(names, want);
        std::fs::write(dir.join("token-dev"), "legacy").unwrap();
        std::fs::set_permissions(
            dir.join("token-dev"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert_eq!(store.get_refresh_token("dev").await.unwrap(), None);
        store.clear_refresh_token("dev").await.unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("token-dev")).unwrap(),
            "legacy"
        );
    }

    #[tokio::test]
    async fn ancestor_validation() {
        for mode in [0o777, 0o775, 0o757] {
            let (_r, dir) = base();
            let exposed = dir.parent().unwrap().join("exposed");
            std::fs::create_dir(&exposed).unwrap();
            std::fs::set_permissions(&exposed, std::fs::Permissions::from_mode(mode)).unwrap();
            let store = FileTokenStore::new(&exposed.join("config/happy-platform-mcp")).unwrap();
            let err = store
                .set_refresh_token("dev", "inert-secret-token")
                .await
                .unwrap_err();
            assert!(err.message.contains(&format!(
                "unsafe token directory ancestor {}",
                exposed.display()
            )));
            assert!(err.message.contains("owned by the current user or root"));
            assert!(err.message.contains("chmod go-w"));
            assert!(!err.message.contains("inert-secret-token"));
            assert_eq!(std::fs::read_dir(&exposed).unwrap().count(), 0);
        }
        let (_r, dir) = base();
        let sticky = dir.parent().unwrap().join("sticky");
        std::fs::create_dir(&sticky).unwrap();
        std::fs::set_permissions(&sticky, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let store = FileTokenStore::new(&sticky.join("happy-platform-mcp")).unwrap();
        store.set_refresh_token("dev", "sticky").await.unwrap();
        assert_eq!(
            store
                .get_refresh_token("dev")
                .await
                .unwrap()
                .as_deref()
                .map(String::as_str),
            Some("sticky")
        );
    }

    #[tokio::test]
    async fn env_selected_file_store() {
        let (_r, dir) = base();
        let xdg = dir.to_string_lossy().into_owned();
        let env = Env::from_pairs(&[("XDG_CONFIG_HOME", xdg.as_str())]);
        let mem: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
        let a = create_default_token_store(Some("file"), mem.clone(), &env).unwrap();
        a.set_refresh_token("synthetic-user@dev", "inert")
            .await
            .unwrap();
        let b = create_default_token_store(Some("file"), mem, &env).unwrap();
        assert_eq!(
            b.get_refresh_token("synthetic-user@dev")
                .await
                .unwrap()
                .as_deref()
                .map(String::as_str),
            Some("inert")
        );
        b.clear_refresh_token("synthetic-user@dev").await.unwrap();
        assert_eq!(
            a.get_refresh_token("synthetic-user@dev").await.unwrap(),
            None
        );
    }
}

#[test]
fn refresh_token_accounts() {
    let mut inst = Object::new();
    inst.insert("name".into(), json!("dev"));
    inst.insert("url".into(), json!("https://dev.service-now.com/"));
    inst.insert("clientId".into(), json!("cid"));
    let a = oauth_refresh_token_account(&inst, "alice").unwrap();
    assert!(a.starts_with(REFRESH_TOKEN_IDENTITY_PREFIX));
    assert_eq!(a.len(), REFRESH_TOKEN_IDENTITY_PREFIX.len() + 64);
    assert_ne!(a, oauth_refresh_token_account(&inst, "bob").unwrap());
    assert_eq!(
        legacy_refresh_token_account("dev", "alice").as_deref(),
        Some("alice@dev")
    );
    assert_eq!(legacy_refresh_token_account("", "alice"), None);
}
