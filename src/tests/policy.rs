//! A client is shown only the globals its UID's policy grants. The test client connects as our
//! own UID, so the policy entry for that UID decides what the registry advertises. The daemon
//! side runs on a thread over a socket pair, so the real protocol is exercised.

use std::os::unix::net::UnixStream;
use std::thread;

use niri_config::Config;
use niri_policy::rpc::{self, Request, Response};
use niri_policy::{AppEntry, AppPolicy, Global, PolicyClient, PolicyFile};

use super::fixture::{serve_policy as serve, Fixture};

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

fn for_our_uid(policy: AppPolicy) -> PolicyClient {
    let uid = rustix::process::getuid().as_raw();
    serve(PolicyFile {
        default: AppPolicy::unknown(),
        apps: vec![AppEntry {
            uid,
            uid_end: None,
            policy,
        }],
    })
}

fn advertised_with(client: PolicyClient) -> Vec<String> {
    let mut f = Fixture::with_policy(Config::default(), client);
    let id = f.add_client();
    f.roundtrip(id);
    f.client(id)
        .state
        .globals
        .iter()
        .map(|g| g.interface.clone())
        .collect()
}

fn advertised(policy: AppPolicy) -> Vec<String> {
    advertised_with(for_our_uid(policy))
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
    let names = advertised_with(serve(PolicyFile::default()));
    assert!(names.iter().any(|n| n == "wl_compositor"));
    assert!(!names.iter().any(|n| n == "zwlr_layer_shell_v1"));
}

/// A daemon that dies after the handshake must not leave clients trusted.
#[test]
fn dead_daemon_fails_closed() {
    let (ours, theirs) = UnixStream::pair().unwrap();
    thread::spawn(move || {
        let request: Request = rpc::read_msg(&theirs).unwrap();
        assert!(matches!(request, Request::Hello { .. }));
        rpc::write_msg(
            &theirs,
            &Response::Hello {
                version: rpc::VERSION,
            },
        )
        .unwrap();
        // Dropped here: every later request sees EOF.
    });
    let client = PolicyClient::from_stream(ours).unwrap();

    let names = advertised_with(client);
    assert!(names.iter().any(|n| n == "wl_compositor"));
    assert!(!names.iter().any(|n| n == "zwlr_layer_shell_v1"));
}
