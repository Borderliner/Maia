//! The desktop's light or dark preference and accent colour (the XDG portal's
//! color-scheme and accent-color): read at start and followed as they change, on a thread
//! with a session bus connection of its own. The accent is for the System theme.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::fm1::{Bus, put_str};

pub const UNKNOWN: u8 = 0;
pub const DARK: u8 = 1;
pub const LIGHT: u8 = 2;

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const SETTINGS: &str = "org.freedesktop.portal.Settings";

/// The portal's 1 (prefer dark) and 2 (prefer light); no preference says nothing.
fn scheme(v: Option<u32>) -> u8 {
    match v {
        Some(1) => DARK,
        Some(2) => LIGHT,
        _ => UNKNOWN,
    }
}

/// Keep `state` at the desktop's preference, waking the window when it changes.
pub fn watch(state: Arc<AtomicU8>, wake: impl Fn() + Send + 'static) {
    std::thread::spawn(move || {
        let Some(mut bus) = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().and_then(|a| Bus::connect(&a)) else { return };
        let mut read = |name: &str| {
            let mut key = vec![];
            put_str(&mut key, "org.freedesktop.appearance");
            put_str(&mut key, name);
            // ReadOne where the portal has it; the older Read wraps the value once more.
            bus.call(PORTAL, PATH, SETTINGS, "ReadOne", "ss", &key).or_else(|| bus.call(PORTAL, PATH, SETTINGS, "Read", "ss", &key))
        };
        let look = scheme(read("color-scheme").and_then(|m| crate::fm1::variant_u32(&m.body, 0)));
        let accent = read("accent-color").and_then(|m| crate::fm1::variant_rgb(&m.body, 0));
        state.store(look, Ordering::Relaxed);
        crate::syscolors::set_scheme(look);
        crate::syscolors::set_accent(accent);
        wake();
        let mut rule = vec![];
        put_str(&mut rule, "type='signal',interface='org.freedesktop.portal.Settings',member='SettingChanged',arg0='org.freedesktop.appearance'");
        if bus.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "AddMatch", "s", &rule).is_none() {
            return;
        }
        while let Some(m) = bus.recv() {
            if m.kind == 4 && m.member == "SettingChanged" {
                match crate::fm1::string_at(&m.body, 1).as_deref() {
                    Some("color-scheme") => {
                        let look = scheme(crate::fm1::variant_u32(&m.body, 2));
                        state.store(look, Ordering::Relaxed);
                        crate::syscolors::set_scheme(look);
                    }
                    Some("accent-color") => crate::syscolors::set_accent(crate::fm1::variant_rgb(&m.body, 2)),
                    _ => continue,
                }
                wake();
            }
        }
    });
}
