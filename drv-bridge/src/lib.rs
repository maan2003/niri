//! What crosses from an app's private session bus to the human's session.
//!
//! `drv-bridge serve` runs as its own user. Every connection is keyed on the peer UID
//! (`SO_PEERCRED`) and the identity daemon's answer for it; the app never names itself.
//! `drv-bridge app` runs in the app's UID on its private bus and claims the desktop names
//! apps expect (`org.freedesktop.Notifications`, `org.freedesktop.portal.Desktop`); it is a
//! convenience, never a boundary.
//!
//! D-Bus ends in the shim. What reaches the server is [`wire`]: a few messages of fixed
//! shape, so the trusted side never parses anything an app could shape. The server answers
//! the portals itself (with drv-portal behind the ones that need consent); nothing an app
//! says reaches the human's bus.

pub mod wire;

/// Where apps find the server's socket.
pub const SOCKET_ENV: &str = "DRV_BRIDGE_SOCKET";
pub const PORTAL_NAME: &str = "org.freedesktop.portal.Desktop";
pub const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
pub const NOTIFICATIONS_NAME: &str = "org.freedesktop.Notifications";
pub const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";

/// The part of a unique name that portals put in object paths: `:1.7` becomes `1_7`.
pub fn sender_component(unique: &str) -> String {
    unique.trim_start_matches(':').replace('.', "_")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_names_become_path_components() {
        assert_eq!(sender_component(":1.7"), "1_7");
    }

    #[test]
    fn clip_keeps_char_boundaries() {
        let s = "é".repeat(wire::MAX_TEXT);
        let cut = wire::clip(&s);
        assert!(cut.len() <= wire::MAX_TEXT);
        assert!(cut.chars().all(|c| c == 'é'));
    }
}
