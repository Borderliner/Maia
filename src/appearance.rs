//! The desktop's light or dark preference (the XDG portal's color-scheme): read at start
//! and followed as it changes, on a thread with a session bus connection of its own.

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
        let mut key = vec![];
        put_str(&mut key, "org.freedesktop.appearance");
        put_str(&mut key, "color-scheme");
        // ReadOne where the portal has it; the older Read wraps the value once more.
        let now = bus.call(PORTAL, PATH, SETTINGS, "ReadOne", "ss", &key).or_else(|| bus.call(PORTAL, PATH, SETTINGS, "Read", "ss", &key));
        state.store(scheme(now.and_then(|m| crate::fm1::variant_u32(&m.body, 0))), Ordering::Relaxed);
        wake();
        let mut rule = vec![];
        put_str(&mut rule, "type='signal',interface='org.freedesktop.portal.Settings',member='SettingChanged',arg0='org.freedesktop.appearance',arg1='color-scheme'");
        if bus.call("org.freedesktop.DBus", "/org/freedesktop/DBus", "org.freedesktop.DBus", "AddMatch", "s", &rule).is_none() {
            return;
        }
        while let Some(m) = bus.recv() {
            if m.kind == 4 && m.member == "SettingChanged" {
                state.store(scheme(crate::fm1::variant_u32(&m.body, 2)), Ordering::Relaxed);
                wake();
            }
        }
    });
}
