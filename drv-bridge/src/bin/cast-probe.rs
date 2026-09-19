//! Exercises screen sharing from inside an app sandbox, as a browser would: creates a
//! portal session on its private bus, asks to start (the person consents at drv-portal),
//! opens the PipeWire remote and lists what that connection can see, which should be the
//! core and the one node. Then holds the cast for a while, or until the desktop ends it
//! (Session.Closed), and closes the session. Results in `$HOME/cast.txt` as they come.

use std::collections::HashMap;
use std::fs;
use std::os::fd::OwnedFd;
use std::time::{Duration, Instant};

use anyhow::Context as _;
use drv_bridge::{sender_component, PORTAL_NAME, PORTAL_PATH};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const SCREEN_CAST: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";

/// One request/response round: the call with `handle_token`, then its `Response`.
fn ask(
    conn: &Connection,
    method: &str,
    token: &str,
    args: impl FnOnce(HashMap<&'static str, Value<'static>>) -> Vec<Value<'static>>,
) -> anyhow::Result<(u32, HashMap<String, OwnedValue>)> {
    let me = sender_component(conn.unique_name().context("no unique name")?.as_str());
    let handle = format!("{PORTAL_PATH}/request/{me}/{token}");
    let request = Proxy::new(conn, PORTAL_NAME, handle.as_str(), REQUEST)?;
    let mut responses = request.receive_signal("Response")?;
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", Value::from(token.to_owned()));
    let mut builder = zbus::zvariant::StructureBuilder::new();
    for arg in args(options) {
        builder = builder.append_field(arg);
    }
    let body = builder.build()?;
    let reply = conn.call_method(Some(PORTAL_NAME), PORTAL_PATH, Some(SCREEN_CAST), method, &body)?;
    let got: OwnedObjectPath = reply.body().deserialize()?;
    anyhow::ensure!(got.as_str() == handle, "handle {got} is not {handle}");
    let msg = responses.next().context("no Response")?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
    Ok((code, results))
}

/// What a PipeWire connection on `fd` can see: `(id, type)` per global.
fn globals(fd: OwnedFd) -> anyhow::Result<Vec<(u32, String)>> {
    use pipewire::context::ContextRc;
    use pipewire::core::PW_ID_CORE;
    use pipewire::loop_::Timeout;
    use pipewire::main_loop::MainLoopRc;
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    let main_loop = MainLoopRc::new(None)?;
    let context = ContextRc::new(&main_loop, None)?;
    let core = context.connect_fd_rc(fd, None).context("connecting on the remote fd")?;
    let registry = core.get_registry_rc()?;
    let seen = Rc::new(RefCell::new(Vec::new()));
    let _reg = {
        let seen = seen.clone();
        registry
            .add_listener_local()
            .global(move |g| seen.borrow_mut().push((g.id, format!("{:?}", g.type_))))
            .register()
    };
    let done = Rc::new(Cell::new(false));
    let pending = core.sync(0)?;
    let _core = {
        let done = done.clone();
        core.add_listener_local()
            .done(move |id, seq| {
                if id == PW_ID_CORE && seq == pending {
                    done.set(true);
                }
            })
            .register()
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done.get() && Instant::now() < deadline {
        main_loop.loop_().iterate(Timeout::Finite(Duration::from_millis(200)));
    }
    anyhow::ensure!(done.get(), "PipeWire did not answer");
    let out = seen.borrow().clone();
    Ok(out)
}

fn main() {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_owned());
    let result = format!("{home}/cast.txt");
    let mut out = String::new();
    if let Err(err) = run(&result, &mut out) {
        out += &format!("error: {err:#}\n");
        let _ = fs::write(&result, &out);
        std::process::exit(1);
    }
}

fn run(result: &str, out: &mut String) -> anyhow::Result<()> {
    let conn = Connection::session()?;
    let save = |out: &str| fs::write(result, out);

    let portal = Proxy::new(&conn, PORTAL_NAME, PORTAL_PATH, SCREEN_CAST)?;
    let version: u32 = portal.get_property("version")?;
    let types: u32 = portal.get_property("AvailableSourceTypes")?;
    let cursors: u32 = portal.get_property("AvailableCursorModes")?;
    *out += &format!("version {version}, source types {types}, cursor modes {cursors}\n");
    save(out)?;

    let (code, results) = ask(&conn, "CreateSession", "probe_create", |mut o| {
        o.insert("session_handle_token", Value::from("probe_session"));
        vec![Value::from(o)]
    })?;
    let session: String = results
        .get("session_handle")
        .cloned()
        .and_then(|v| String::try_from(v).ok())
        .context("no session_handle")?;
    *out += &format!("CreateSession: response {code}, session {session}\n");
    let session_path = OwnedObjectPath::try_from(session.as_str())?;

    let (code, _) = ask(&conn, "SelectSources", "probe_select", |mut o| {
        o.insert("types", Value::U32(1 | 2));
        o.insert("cursor_mode", Value::U32(2));
        vec![Value::from(session_path.clone()), Value::from(o)]
    })?;
    *out += &format!("SelectSources: response {code}\n");
    save(out)?;

    let (code, results) = ask(&conn, "Start", "probe_start", |o| {
        vec![Value::from(session_path.clone()), Value::from(""), Value::from(o)]
    })?;
    *out += &format!("Start: response {code}, streams {:?}\n", results.get("streams"));
    save(out)?;
    if code != 0 {
        return Ok(());
    }

    let reply = conn.call_method(
        Some(PORTAL_NAME),
        PORTAL_PATH,
        Some(SCREEN_CAST),
        "OpenPipeWireRemote",
        &(session_path.clone(), HashMap::<&str, Value<'_>>::new()),
    )?;
    let fd: zbus::zvariant::OwnedFd = reply.body().deserialize()?;
    *out += &format!("OpenPipeWireRemote: fd {:?}\n", fd);
    let fd: OwnedFd = fd.into();
    match globals(fd) {
        Ok(list) => *out += &format!("the remote sees: {list:?}\n"),
        Err(err) => *out += &format!("the remote: {err:#}\n"),
    }
    save(out)?;

    // Hold the cast so the indicator can be seen: 20 s, or until the desktop ends it (a
    // revoke sends Session.Closed and the session is gone; Close would only fail).
    let session_proxy = Proxy::new(&conn, PORTAL_NAME, session_path.clone(), SESSION)?;
    let closed = session_proxy.receive_signal("Closed")?;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        if closed.into_iter().next().is_some() {
            let _ = tx.send(());
        }
    });
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(()) => *out += "Closed by the desktop\n",
        Err(_) => match session_proxy.call_method("Close", &()) {
            Ok(_) => *out += "Close: ok\n",
            Err(err) => *out += &format!("Close: {err}\n"),
        },
    }
    save(out)?;
    Ok(())
}
