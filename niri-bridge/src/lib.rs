//! What crosses from an app's private session bus to the human's session.
//!
//! `niri-bridge serve` runs as the human. Every connection is keyed on the peer UID
//! (`SO_PEERCRED`) and the identity daemon's answer for it; the app never names itself.
//! `niri-bridge app` runs in the app's UID on its private bus and claims the desktop names
//! apps expect (`org.freedesktop.Notifications`, `org.freedesktop.portal.Desktop`); it is a
//! convenience, never a boundary.
//!
//! Shim and server speak D-Bus peer to peer over the server's socket, so bodies and file
//! descriptors cross unchanged. Portal object paths carry the caller's unique bus name
//! (`/org/freedesktop/portal/desktop/request/<sender>/<token>`); the server renames that part
//! between the app's bus and the human's bus.

use zbus::zvariant::{Array, ObjectPath, Str, Structure, StructureBuilder, Value};

/// Where apps find the server's socket.
pub const SOCKET_ENV: &str = "NIRI_BRIDGE_SOCKET";
/// Portal app ids for manifest names: `niri.app.<name>`, so the human's launcher entries and
/// the portal dialogs agree on who is asking.
pub const APP_ID_PREFIX: &str = "niri.app.";

pub const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
pub const NOTIFICATIONS_NAME: &str = "org.freedesktop.Notifications";
pub const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";

/// The part of a unique name that portals put in object paths: `:1.7` becomes `1_7`.
pub fn sender_component(unique: &str) -> String {
    unique.trim_start_matches(':').replace('.', "_")
}

/// The reverse of [`sender_component`].
pub fn unique_from_component(component: &str) -> String {
    format!(":{}", component.replace('_', "."))
}

/// `(kind, sender, token)` of a portal request or session handle, `kind` being `/request/` or
/// `/session/`.
pub fn handle_parts(path: &str) -> Option<(&'static str, &str, &str)> {
    let rest = path.strip_prefix(PORTAL_PATH)?;
    let (kind, rest) = ["/request/", "/session/"]
        .into_iter()
        .find_map(|kind| rest.strip_prefix(kind).map(|rest| (kind, rest)))?;
    let (sender, token) = rest.split_once('/')?;
    Some((kind, sender, token))
}

/// `s` with the sender part of a portal handle renamed, if it is one owned by `from`.
pub fn rewrite_handle(s: &str, from: &str, to: &str) -> Option<String> {
    let (kind, sender, token) = handle_parts(s)?;
    (sender == from).then(|| format!("{PORTAL_PATH}{kind}{to}/{token}"))
}

/// Renames portal handles in every string and object path inside `value`.
pub fn rewrite_value(value: &mut Value<'_>, from: &str, to: &str) -> zbus::zvariant::Result<()> {
    match value {
        Value::Str(s) => {
            if let Some(new) = rewrite_handle(s.as_str(), from, to) {
                *value = Value::Str(Str::from(new));
            }
        }
        Value::ObjectPath(p) => {
            if let Some(new) = rewrite_handle(p.as_str(), from, to) {
                *value = Value::ObjectPath(ObjectPath::try_from(new)?);
            }
        }
        Value::Value(inner) => rewrite_value(inner, from, to)?,
        Value::Dict(dict) => {
            for (_, v) in dict.iter_mut() {
                rewrite_value(v, from, to)?;
            }
        }
        Value::Array(_) | Value::Structure(_) => {
            let taken = std::mem::replace(value, Value::U8(0));
            *value = match taken {
                Value::Array(array) => {
                    let mut out = Array::new(array.element_signature());
                    for v in array.inner() {
                        let mut v = v.try_clone()?;
                        rewrite_value(&mut v, from, to)?;
                        out.append(v)?;
                    }
                    Value::Array(out)
                }
                Value::Structure(structure) => {
                    let mut builder = StructureBuilder::new();
                    for mut field in structure.into_fields() {
                        rewrite_value(&mut field, from, to)?;
                        builder = builder.append_field(field);
                    }
                    Value::Structure(builder.build()?)
                }
                _ => unreachable!(),
            };
        }
        _ => {}
    }
    Ok(())
}

/// [`rewrite_value`] over a whole message body.
pub fn rewrite_structure(
    structure: Structure<'_>,
    from: &str,
    to: &str,
) -> zbus::zvariant::Result<Structure<'static>> {
    let mut builder = StructureBuilder::new();
    for mut field in structure.into_fields() {
        rewrite_value(&mut field, from, to)?;
        builder = builder.append_field(Value::from(field.try_to_owned()?));
    }
    builder.build()
}

/// Every portal handle in `value` owned by `sender`: the tokens, so the owner can be found
/// again when a signal arrives for them.
pub fn tokens_owned_by<'v>(value: &'v Value<'v>, sender: &str, out: &mut Vec<String>) {
    let mut visit = |s: &str| {
        if let Some((_, owner, token)) = handle_parts(s) {
            if owner == sender {
                out.push(token.to_owned());
            }
        }
    };
    match value {
        Value::Str(s) => visit(s.as_str()),
        Value::ObjectPath(p) => visit(p.as_str()),
        Value::Value(inner) => tokens_owned_by(inner, sender, out),
        Value::Dict(dict) => {
            for (_, v) in dict.iter() {
                tokens_owned_by(v, sender, out);
            }
        }
        Value::Array(array) => {
            for v in array.inner() {
                tokens_owned_by(v, sender, out);
            }
        }
        Value::Structure(structure) => {
            for v in structure.fields() {
                tokens_owned_by(v, sender, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zbus::zvariant::OwnedValue;

    use super::*;

    #[test]
    fn handles_are_renamed_by_owner_only() {
        assert_eq!(sender_component(":1.7"), "1_7");
        assert_eq!(unique_from_component("1_7"), ":1.7");
        assert_eq!(
            rewrite_handle(
                "/org/freedesktop/portal/desktop/request/1_7/t1",
                "1_7",
                "1_42"
            )
            .as_deref(),
            Some("/org/freedesktop/portal/desktop/request/1_42/t1")
        );
        assert_eq!(
            rewrite_handle(
                "/org/freedesktop/portal/desktop/session/1_7/s",
                "1_8",
                "1_42"
            ),
            None
        );
        assert_eq!(
            rewrite_handle("/org/freedesktop/portal/desktop", "1_7", "1_42"),
            None
        );
    }

    #[test]
    fn bodies_are_rewritten_deep() {
        let mut options: HashMap<&str, Value<'_>> = HashMap::new();
        options.insert(
            "session_handle",
            Value::from("/org/freedesktop/portal/desktop/session/1_7/s"),
        );
        let body = StructureBuilder::new()
            .add_field(
                ObjectPath::try_from("/org/freedesktop/portal/desktop/request/1_7/r").unwrap(),
            )
            .add_field(options)
            .add_field(7u32)
            .build()
            .unwrap();
        let mut tokens = Vec::new();
        tokens_owned_by(
            &Value::Structure(body.try_clone().unwrap()),
            "1_7",
            &mut tokens,
        );
        assert_eq!(tokens, ["r", "s"]);

        let out = rewrite_structure(body, "1_7", "1_42").unwrap();
        let fields = out.fields();
        assert_eq!(
            ObjectPath::try_from(&fields[0]).unwrap().as_str(),
            "/org/freedesktop/portal/desktop/request/1_42/r"
        );
        let dict: HashMap<String, OwnedValue> = fields[1].try_clone().unwrap().try_into().unwrap();
        assert_eq!(
            <&str>::try_from(&*dict["session_handle"]).unwrap(),
            "/org/freedesktop/portal/desktop/session/1_42/s"
        );
        assert_eq!(u32::try_from(&fields[2]).unwrap(), 7);
    }
}
