//! The desktop's D-Bus names, for the shim and the probes that exercise it.

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
}
