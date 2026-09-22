//! Exercises the file chooser from inside an app sandbox, as a GTK app would: asks its
//! private bus for a file, waits for the `Response`, reads what it was given, checks the
//! grant is read-only and its own, then saves a copy the same way. Results in
//! `$HOME/result.txt`.

use std::collections::HashMap;
use std::fs;

use anyhow::Context as _;
use drv_dbus_shim::{sender_component, PORTAL_NAME, PORTAL_PATH};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const FILE_CHOOSER: &str = "org.freedesktop.portal.FileChooser";
const REQUEST: &str = "org.freedesktop.portal.Request";

fn ask(
    conn: &Connection,
    method: &str,
    title: &str,
    token: &str,
    extra: &[(&str, Value<'static>)],
) -> anyhow::Result<(u32, Vec<String>)> {
    let me = sender_component(conn.unique_name().context("no unique name")?.as_str());
    let handle = format!("{PORTAL_PATH}/request/{me}/{token}");
    let request = Proxy::new(conn, PORTAL_NAME, handle.as_str(), REQUEST)?;
    let mut responses = request.receive_signal("Response")?;
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", Value::from(token));
    for (k, v) in extra {
        options.insert(k, v.clone());
    }
    let reply = conn.call_method(
        Some(PORTAL_NAME),
        PORTAL_PATH,
        Some(FILE_CHOOSER),
        method,
        &("", title, options),
    )?;
    let got: OwnedObjectPath = reply.body().deserialize()?;
    anyhow::ensure!(got.as_str() == handle, "handle {got} is not {handle}");
    let msg = responses.next().context("no Response")?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize()?;
    let uris = results
        .get("uris")
        .cloned()
        .and_then(|v| Vec::<String>::try_from(v).ok())
        .unwrap_or_default();
    Ok((code, uris))
}

fn path_of(uri: &str) -> String {
    let rest = uri.strip_prefix("file://").unwrap_or(uri).as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == b'%' && i + 2 < rest.len() {
            if let Ok(b) = u8::from_str_radix(std::str::from_utf8(&rest[i + 1..i + 3]).unwrap_or("zz"), 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(rest[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn main() -> anyhow::Result<()> {
    let home = std::env::var("HOME")?;
    let result = format!("{home}/out/result.txt");
    let conn = Connection::session()?;
    let mut out = String::new();
    let version: u32 = Proxy::new(&conn, PORTAL_NAME, PORTAL_PATH, FILE_CHOOSER)?.get_property("version")?;
    out += &format!("version: {version}\n");

    let (code, uris) = ask(&conn, "OpenFile", "Pick something to read", "probe_open", &[])?;
    out += &format!("open: response {code}, uris {uris:?}\n");
    if let Some(uri) = uris.first() {
        let path = path_of(uri);
        out += &format!("read {path}: {:?}\n", fs::read_to_string(&path).map_err(|e| e.to_string()));
        let listing = fs::read_dir("/run/drv/doc")
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect::<Vec<_>>())
            .map_err(|e| e.to_string());
        out += &format!("listing /run/drv/doc: {listing:?}\n");
        let write = fs::OpenOptions::new().write(true).open(&path).map(|_| ()).map_err(|e| e.to_string());
        out += &format!("open it for writing: {write:?}\n");
    }
    fs::write(&result, &out)?;

    let (code, uris) = ask(
        &conn,
        "SaveFile",
        "Save the result",
        "probe_save",
        &[("current_name", Value::from("result copy.txt"))],
    )?;
    out += &format!("save: response {code}, uris {uris:?}\n");
    if let Some(uri) = uris.first() {
        let path = path_of(uri);
        let r = fs::write(&path, &out)
            .map_err(|e| e.to_string())
            .and_then(|_| fs::metadata(&path).map(|m| m.len()).map_err(|e| e.to_string()));
        out += &format!("wrote {path}, length now {r:?}\n");
    }
    fs::write(&result, &out)?;
    Ok(())
}
