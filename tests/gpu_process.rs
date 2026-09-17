use niri::gpu::client::{GpuClient, Mode};
use niri::gpu::testing::run_smoke;

#[test]
fn smoke_in_separate_process() {
    let exe = std::path::Path::new(env!("CARGO_BIN_EXE_niri"));
    // The process seals itself before answering, so everything below runs under the seccomp
    // allowlist, like the real compositor.
    let client = GpuClient::spawn_process(exe, Mode::Headless, &[], None)
        .expect("spawning gpu process (needs EGL, e.g. llvmpipe)");
    run_smoke(client).unwrap();
}
