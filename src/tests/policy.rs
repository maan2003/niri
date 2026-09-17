//! A client is shown only the globals its UID's policy grants. The test client connects as our
//! own UID, so the policy entry for that UID decides what the registry advertises.

use niri_config::Config;
use niri_policy::{AppEntry, AppPolicy, Global, PolicyFile, PolicyStore};

use super::fixture::Fixture;

const PRIVILEGED: &[&str] = &[
    "zwlr_layer_shell_v1",
    "ext_session_lock_manager_v1",
    "zwlr_data_control_manager_v1",
    "ext_data_control_manager_v1",
    "zwlr_screencopy_manager_v1",
    "ext_image_copy_capture_manager_v1",
    "ext_output_image_capture_source_manager_v1",
    "zwlr_virtual_pointer_manager_v1",
    "zwp_virtual_keyboard_manager_v1",
    "zwp_input_method_manager_v2",
    "ext_foreign_toplevel_list_v1",
    "ext_workspace_manager_v1",
    "zwlr_output_manager_v1",
    "wp_security_context_manager_v1",
];

fn store(policy: AppPolicy) -> PolicyStore {
    let uid = rustix::process::getuid().as_raw();
    PolicyStore::new(PolicyFile {
        default: AppPolicy::unknown(),
        apps: vec![AppEntry {
            uid,
            uid_end: None,
            policy,
        }],
    })
    .unwrap()
}

fn advertised(policy: AppPolicy) -> Vec<String> {
    let mut f = Fixture::with_policy(Config::default(), store(policy));
    let id = f.add_client();
    f.roundtrip(id);
    let client = f.client(id);
    client
        .state
        .globals
        .iter()
        .map(|g| g.interface.clone())
        .collect()
}

#[test]
fn untrusted_client_sees_only_the_baseline() {
    let names = advertised(AppPolicy::unknown());
    assert!(names.iter().any(|n| n == "wl_compositor"));
    assert!(names.iter().any(|n| n == "xdg_wm_base"));
    assert!(names.iter().any(|n| n == "wl_shm"));
    for name in PRIVILEGED {
        assert!(!names.iter().any(|n| n == name), "{name} leaked");
    }
}

#[test]
fn trusted_client_sees_everything() {
    let names = advertised(AppPolicy::trusted("me"));
    for name in PRIVILEGED {
        assert!(names.iter().any(|n| n == name), "{name} missing");
    }
}

#[test]
fn granted_global_is_advertised() {
    let names = advertised(AppPolicy {
        globals: vec![Global::LayerShell],
        ..AppPolicy::unknown()
    });
    assert!(names.iter().any(|n| n == "zwlr_layer_shell_v1"));
    assert!(!names.iter().any(|n| n == "zwlr_screencopy_manager_v1"));
}

#[test]
fn unknown_uid_gets_the_default() {
    let mut f = Fixture::with_policy(
        Config::default(),
        PolicyStore::new(PolicyFile::default()).unwrap(),
    );
    let id = f.add_client();
    f.roundtrip(id);
    let names: Vec<_> = f
        .client(id)
        .state
        .globals
        .iter()
        .map(|g| g.interface.clone())
        .collect();
    assert!(!names.iter().any(|n| n == "zwlr_layer_shell_v1"));
}
