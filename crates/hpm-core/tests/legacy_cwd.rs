//! Review M2: the legacy registry is never discovered from the working directory.
//! Own test binary because it changes the process working directory.

use hpm_core::config::Env;
use hpm_core::registry::{InstanceRegistry, default_legacy_path, legacy_path_for_exe};
use serde_json::json;

#[test]
fn a_config_dir_in_the_working_directory_is_ignored() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let planted = project
        .path()
        .join("config")
        .join("servicenow-instances.json");
    std::fs::create_dir_all(planted.parent().unwrap()).unwrap();
    std::fs::write(
        &planted,
        json!({"version": 1, "instances": [{
            "name": "attacker", "url": "https://attacker.service-now.com", "authType": "basic",
            "username": "u", "password": "p", "default": true
        }]})
        .to_string(),
    )
    .unwrap();
    std::env::set_current_dir(project.path()).unwrap();

    let legacy = default_legacy_path().expect("test binary has an install root");
    assert_ne!(legacy.parent().unwrap(), planted.parent().unwrap());
    let registry =
        InstanceRegistry::new(Env::from_pairs(&[("HOME", home.path().to_str().unwrap())]));
    assert_ne!(
        registry.read_path().canonicalize().ok(),
        Some(planted.canonicalize().unwrap())
    );
    assert!(registry.list().unwrap().is_empty());
}

#[test]
fn legacy_path_is_the_install_root_of_the_executable() {
    assert_eq!(
        legacy_path_for_exe(std::path::Path::new("/opt/hpm/bin/servicenow-mcp-server")),
        Some("/opt/hpm/config/servicenow-instances.json".into())
    );
    assert_eq!(legacy_path_for_exe(std::path::Path::new("/exe")), None);
}
