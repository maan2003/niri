use super::client::{GpuClient, Mode};
use super::testing::run_smoke;

#[test]
fn smoke_in_thread() {
    let client = GpuClient::spawn_thread(Mode::Headless)
        .expect("gpu server thread (needs Vulkan, e.g. Lavapipe)");
    run_smoke(client).unwrap();
}
