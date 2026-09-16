use super::client::GpuClient;
use super::testing::run_smoke;

#[test]
fn smoke_in_thread() {
    let mut client = GpuClient::spawn_thread().unwrap();
    run_smoke(&mut client).unwrap();
    client.shutdown().unwrap();
}

#[test]
fn rejects_unsealed_pool() {
    use std::os::fd::AsFd;
    use super::protocol::ShmDesc;
    use super::scene::BufferId;

    let mut client = GpuClient::spawn_thread().unwrap();
    let fd = rustix::fs::memfd_create("unsealed", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    let file = std::fs::File::from(fd);
    file.set_len(4096).unwrap();
    let err = client
        .register_shm(
            BufferId(7),
            file.as_fd(),
            ShmDesc {
                size: 4096,
                offset: 0,
                stride: 64,
                width: 16,
                height: 16,
                format: smithay::backend::allocator::Fourcc::Argb8888 as u32,
            },
        )
        .unwrap_err();
    assert!(err.to_string().contains("sealed"), "{err}");
    client.shutdown().unwrap();
}
