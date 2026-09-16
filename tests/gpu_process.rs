//! Spawns the real `niri gpu-process` binary and drives it over the socket.

use std::path::Path;

use niri::gpu::client::GpuClient;
use niri::gpu::testing::run_smoke;

#[test]
fn smoke_in_separate_process() {
    let exe = Path::new(env!("CARGO_BIN_EXE_niri"));
    let mut client = GpuClient::spawn_process(exe).unwrap();
    assert!(!client.renderer.is_empty());
    run_smoke(&mut client).unwrap();
    client.shutdown().unwrap();
}
