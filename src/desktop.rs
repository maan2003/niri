//! Rho's local desktop endpoint. No network listener or agent-runtime dependency.
use std::cell::Cell;
use std::io;
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

pub struct Socket(PathBuf);
impl Drop for Socket {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Runtime directories and their contents belong to the invoking user, not a sandbox.
pub fn start(state: &mut State) -> Result<Socket> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is required")?;
    let metadata = std::fs::metadata(&runtime)?;
    anyhow::ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "XDG_RUNTIME_DIR must be a private directory owned by the current user"
    );
    let path = PathBuf::from(runtime).join(format!("rho-desktop-{}.sock", std::process::id()));
    let listener = UnixListener::bind(&path)?;
    let socket = Socket(path);
    std::fs::set_permissions(&socket.0, std::fs::Permissions::from_mode(0o600))?;
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
                let task = async move {
                    let _guard = guard;
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
                            event_loop.insert_idle(move |state| {
                                let result = match request {
                                    Request::Hello { version } if version == VERSION => {
                                        Ok((describe(state), Vec::new()))
                                    }
                                    Request::Hello { .. } => {
                                        Err(anyhow::anyhow!("desktop protocol version mismatch"))
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
    std::env::set_var(rho_desktop_proto::SOCKET_ENV, &socket.0);
    info!("Rho desktop listening on: {}", socket.0.display());
    Ok(socket)
}

fn describe(state: &State) -> Response {
    Response::Hello {
        version: VERSION,
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
    let size = output
        .current_transform()
        .transform_size(output.current_mode().unwrap().size);
    let width = size.w as u32;
    let height = size.h as u32;
    let expected =
        rho_desktop_proto::frame_len(width, height).context("output exceeds capture limits")?;
    state
        .backend
        .with_primary_renderer(|renderer| {
            state.niri.update_render_elements(Some(&output));
            let elements =
                state
                    .niri
                    .render::<_>(renderer, &output, false, RenderTarget::ScreenCapture);
            let pixels = render_to_vec(
                renderer,
                size,
                Scale::from(output.current_scale().fractional_scale()),
                Transform::Normal,
                Fourcc::Argb8888,
                elements.iter().rev(),
            )?;
            anyhow::ensure!(pixels.len() == expected, "unexpected capture stride");
            Ok((Response::Frame { width, height }, pixels))
        })
        .context("desktop renderer unavailable")?
}

/// Client-side lossless screenshot export; annotations can operate on this PNG.
pub fn save_capture(socket: Option<PathBuf>, output: String, path: PathBuf) -> Result<()> {
    use std::io::{BufRead, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;
    let socket = socket
        .or_else(|| std::env::var_os(rho_desktop_proto::SOCKET_ENV).map(PathBuf::from))
        .context("provide --socket or RHO_DESKTOP_SOCKET")?;
    let stream = UnixStream::connect(socket)?;
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
