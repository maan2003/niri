//! Rho's local desktop endpoint. No network listener or agent-runtime dependency.
pub mod driver;
pub mod input;
pub mod video;
use std::cell::Cell;
use std::io;
use std::os::fd::AsRawFd;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use calloop::generic::Generic;
use calloop::{Interest, Mode, PostAction};
use futures_util::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use futures_util::AsyncBufReadExt;
use rho_desktop_proto::{Output, Request, Response, MAX_HEADER, VERSION};
use smithay::backend::allocator::Fourcc;
use smithay::utils::{Scale, Transform};

use crate::niri::State;
use crate::render_helpers::{render_to_vec, RenderTarget};

pub struct Socket {
    control: PathBuf,
    manifest: PathBuf,
    _lock: std::fs::File,
    runtime: Option<tokio::runtime::Runtime>,
}
impl Drop for Socket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.manifest);
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

pub(super) fn desktop_directory(runtime: PathBuf, agent: Option<&str>) -> Result<PathBuf> {
    let base = runtime.join("rho-desktop");
    match agent {
        Some(agent) => {
            anyhow::ensure!(
                !agent.is_empty()
                    && agent
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
                "invalid desktop agent"
            );
            Ok(base.join("agents").join(agent))
        }
        None => Ok(base),
    }
}

/// Runtime directories and their contents belong to the invoking user, not a sandbox.
pub fn start(state: &mut State, name: &str) -> Result<Socket> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is required")?;
    let metadata = std::fs::metadata(&runtime)?;
    anyhow::ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "XDG_RUNTIME_DIR must be a private directory owned by the current user"
    );
    let path = PathBuf::from(format!(
        "@rho-desktop-{}-{:016x}",
        std::process::id(),
        fastrand::u64(..)
    ));
    anyhow::ensure!(
        !name.is_empty()
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "invalid desktop session name"
    );
    let agent = std::env::var("RHO_AGENT_ID").ok();
    let directory = std::env::var_os("RHO_DESKTOP_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or(desktop_directory(
            PathBuf::from(&runtime),
            agent.as_deref(),
        )?);
    std::fs::create_dir_all(&directory)?;
    std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(directory.join(format!("{name}.lock")))?;
    anyhow::ensure!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "desktop session already running"
    );
    let manifest = directory.join(format!("{name}.json"));
    if manifest.exists() {
        let old: serde_json::Value = serde_json::from_slice(&std::fs::read(&manifest)?)?;
        if let Some(old) = old["socket"].as_str() {
            anyhow::ensure!(
                connect_local(old).is_err(),
                "desktop session already running"
            );
        }
    }
    let listener = bind_local(&path)?;
    let media_path = path.with_extension("moq");
    let media_listener = bind_local(&media_path)?;
    media_listener.set_nonblocking(true)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let quality = std::sync::Arc::new(video::Quality::default());
    let (commands, receive) = calloop::channel::channel();
    state
        .niri
        .event_loop
        .insert_source(receive, |event, _, state| {
            if let calloop::channel::Event::Msg(command) = event {
                match command {
                    video::Command::Start(frames, quality) => {
                        state.backend.headless().video = Some(video::Video::new(frames, quality));
                        state.niri.queue_redraw_all();
                    }
                    video::Command::Stop => state.backend.headless().video = None,
                }
            }
        })
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let q = quality.clone();
    let _enter = runtime.enter();
    let media_listener = tokio::net::UnixListener::from_std(media_listener)?;
    runtime.spawn(async move {
        if let Err(error) = video::run(media_listener, commands, q).await {
            error!("desktop media: {error:#}");
        }
    });
    drop(_enter);
    let temporary = manifest.with_extension("json.tmp");
    std::fs::write(
        &temporary,
        serde_json::to_vec(
            &serde_json::json!({"agent":agent,"name":name,"socket":path,"pid":std::process::id(),"ipc_socket":state.niri.ipc_server.as_ref().and_then(|ipc|ipc.socket_path.as_ref()),"wayland_display":state.niri.socket_name}),
        )?,
    )?;
    std::fs::rename(temporary, &manifest)?;
    let socket = Socket {
        control: path,
        manifest,
        _lock: lock,
        runtime: Some(runtime),
    };
    listener.set_nonblocking(true)?;
    let active = Rc::new(Cell::new(0usize));
    state
        .niri
        .event_loop
        .insert_source(
            Generic::new(listener, Interest::READ, Mode::Level),
            move |_, listener, state| {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        return Ok(PostAction::Continue)
                    }
                    Err(e) => return Err(e),
                };
                if peer_uid(&stream)? != unsafe { libc::geteuid() } {
                    return Ok(PostAction::Continue);
                }
                // Bound clients holding requests or unread full-resolution screenshots.
                if active.get() >= 4 {
                    return Ok(PostAction::Continue);
                }
                active.set(active.get() + 1);
                struct Active(Rc<Cell<usize>>);
                impl Drop for Active {
                    fn drop(&mut self) {
                        self.0.set(self.0.get() - 1);
                    }
                }
                let guard = Active(active.clone());
                let stream = state.niri.event_loop.adapt_io(stream)?;
                let event_loop = state.niri.event_loop.clone();
                let quality = quality.clone();
                let media_path = media_path.clone();
                let held = Rc::new(std::cell::RefCell::new(input::Held::default()));
                let task = async move {
                    let _guard = guard;
                    let cleanup_loop = event_loop.clone();
                    let cleanup_held = held.clone();
                    let cleanup_quality = quality.clone();
                    let result = async move {
                        let (read, mut write) = stream.split();
                        let mut read = BufReader::new(read);
                        let mut ready = false;
                        loop {
                            let mut line = Vec::new();
                            (&mut read)
                                .take(MAX_HEADER)
                                .read_until(b'\n', &mut line)
                                .await?;
                            if line.is_empty() {
                                break;
                            }
                            if line.last() != Some(&b'\n') {
                                bail!("oversized desktop request");
                            }
                            let request: Request = serde_json::from_slice(&line)?;
                            let (tx, rx) = async_channel::bounded(1);
                            let was_ready = ready;
                            let held = held.clone();
                            let quality = quality.clone();
                            let media_path = media_path.clone();
                            event_loop.insert_idle(move |state| {
                                let result = match request {
                                    Request::Hello { version } if version == VERSION => Ok((
                                        describe(state, media_path.to_string_lossy().into_owned()),
                                        Vec::new(),
                                    )),
                                    Request::Hello { .. } => {
                                        Err(anyhow::anyhow!("desktop protocol version mismatch"))
                                    }
                                    Request::Status if was_ready => {
                                        use std::sync::atomic::Ordering;
                                        Ok((
                                            Response::Status {
                                                streaming: state.backend.headless().video.is_some(),
                                                composed: quality.composed.load(Ordering::Relaxed),
                                                encoded: quality.encoded.load(Ordering::Relaxed),
                                            },
                                            Vec::new(),
                                        ))
                                    }
                                    Request::Input { input } if was_ready => {
                                        input::apply(state, &mut held.borrow_mut(), &quality, input)
                                            .map(|()| (Response::Done, Vec::new()))
                                    }
                                    Request::Capture { output } if was_ready => {
                                        capture(state, &output)
                                    }
                                    _ => {
                                        Err(anyhow::anyhow!("Hello must precede desktop requests"))
                                    }
                                };
                                let _ = tx.try_send(result);
                            });
                            let result = rx.recv().await?;
                            let (header, pixels) = match result {
                                Ok(value) => value,
                                Err(error) => (
                                    Response::Error {
                                        message: format!("{error:#}"),
                                    },
                                    Vec::new(),
                                ),
                            };
                            let failed = matches!(header, Response::Error { .. });
                            ready |= matches!(header, Response::Hello { .. });
                            let mut bytes = serde_json::to_vec(&header)?;
                            bytes.push(b'\n');
                            write.write_all(&bytes).await?;
                            write.write_all(&pixels).await?;
                            if failed {
                                break;
                            }
                        }
                        Ok::<(), anyhow::Error>(())
                    }
                    .await;
                    cleanup_loop.insert_idle(move |state| {
                        let _ = input::apply(
                            state,
                            &mut cleanup_held.borrow_mut(),
                            &cleanup_quality,
                            rho_desktop_proto::Input::ReleaseAll,
                        );
                    });
                    if let Err(error) = result {
                        debug!("desktop client: {error:#}");
                    }
                };
                state
                    .niri
                    .scheduler
                    .schedule(task)
                    .map_err(io::Error::other)?;
                Ok(PostAction::Continue)
            },
        )
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    std::env::set_var(rho_desktop_proto::SOCKET_ENV, &socket.control);
    info!("Rho desktop listening on: {}", socket.control.display());
    Ok(socket)
}

fn describe(state: &State, media_socket: String) -> Response {
    Response::Hello {
        version: VERSION,
        media_socket,
        outputs: state
            .niri
            .global_space
            .outputs()
            .map(|output| {
                let size = output
                    .current_transform()
                    .transform_size(output.current_mode().unwrap().size);
                Output {
                    name: output.name(),
                    width: size.w as u32,
                    height: size.h as u32,
                    scale: output.current_scale().fractional_scale(),
                }
            })
            .collect(),
    }
}

fn capture(state: &mut State, name: &str) -> Result<(Response, Vec<u8>)> {
    let output = state
        .niri
        .global_space
        .outputs()
        .find(|o| o.name() == name)
        .cloned()
        .context("unknown desktop output")?;
    state
        .backend
        .with_primary_renderer(|renderer| {
            let image = capture_pixels(&mut state.niri, renderer, &output)?;
            Ok((
                Response::Frame {
                    width: image.width as u32,
                    height: image.height as u32,
                },
                image.bgra,
            ))
        })
        .context("desktop renderer unavailable")?
}

pub fn capture_pixels(
    niri: &mut crate::niri::Niri,
    renderer: &mut smithay::backend::renderer::gles::GlesRenderer,
    output: &smithay::output::Output,
) -> Result<rho_desktop_media::codec::Image> {
    let size = output
        .current_transform()
        .transform_size(output.current_mode().unwrap().size);
    let expected = rho_desktop_proto::frame_len(size.w as u32, size.h as u32)
        .context("output exceeds capture limits")?;
    niri.update_render_elements(Some(output));
    let elements = niri.render::<_>(renderer, output, false, RenderTarget::ScreenCapture);
    let pixels = render_to_vec(
        renderer,
        size,
        Scale::from(output.current_scale().fractional_scale()),
        Transform::Normal,
        Fourcc::Argb8888,
        elements.iter().rev(),
    )?;
    anyhow::ensure!(pixels.len() == expected, "unexpected capture stride");
    Ok(rho_desktop_media::codec::Image {
        width: size.w as usize,
        height: size.h as usize,
        bgra: pixels,
    })
}

/// Client-side lossless screenshot export; annotations can operate on this PNG.
pub fn save_capture(socket: Option<PathBuf>, output: String, path: PathBuf) -> Result<()> {
    use std::io::{BufRead, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let socket = socket
        .or_else(|| std::env::var_os(rho_desktop_proto::SOCKET_ENV).map(PathBuf::from))
        .context("provide --socket or RHO_DESKTOP_SOCKET")?;
    let stream = connect_local(&socket.to_string_lossy())?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut stream = std::io::BufReader::new(stream);
    fn request(stream: &mut std::io::BufReader<UnixStream>, request: Request) -> Result<Response> {
        let mut bytes = serde_json::to_vec(&request)?;
        bytes.push(b'\n');
        stream.get_mut().write_all(&bytes)?;
        let mut line = Vec::new();
        stream.take(MAX_HEADER).read_until(b'\n', &mut line)?;
        anyhow::ensure!(line.last() == Some(&b'\n'), "invalid desktop response");
        let response = serde_json::from_slice(&line)?;
        if let Response::Error { message } = response {
            bail!("{message}");
        }
        Ok(response)
    }
    anyhow::ensure!(
        matches!(
            request(&mut stream, Request::Hello { version: VERSION })?,
            Response::Hello {
                version: VERSION,
                ..
            }
        ),
        "desktop protocol version mismatch"
    );
    let Response::Frame { width, height } = request(&mut stream, Request::Capture { output })?
    else {
        bail!("expected a screenshot");
    };
    let size =
        rho_desktop_proto::frame_len(width, height).context("invalid screenshot dimensions")?;
    let mut pixels = vec![0; size];
    stream.read_exact(&mut pixels)?;
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let mut encoder = png::Encoder::new(std::fs::File::create(path)?, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.write_header()?.write_image_data(&pixels)?;
    Ok(())
}

fn bind_local(path: &std::path::Path) -> std::io::Result<UnixListener> {
    let name = path.to_string_lossy();
    let address = std::os::unix::net::SocketAddr::from_abstract_name(
        name.strip_prefix('@').expect("abstract desktop address"),
    )?;
    UnixListener::bind_addr(&address)
}
pub fn connect_local(address: &str) -> std::io::Result<std::os::unix::net::UnixStream> {
    if let Some(name) = address.strip_prefix('@') {
        let address = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
        std::os::unix::net::UnixStream::connect_addr(&address)
    } else {
        std::os::unix::net::UnixStream::connect(address)
    }
}

fn peer_uid(stream: &std::os::unix::net::UnixStream) -> std::io::Result<u32> {
    let mut cred = std::mem::MaybeUninit::<libc::ucred>::uninit();
    let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            cred.as_mut_ptr().cast(),
            &mut size,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { cred.assume_init() }.uid)
}
