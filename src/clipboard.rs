//! System clipboard: text for egui, plus file lists that interoperate with other
//! file managers (text/uri-list, x-special/gnome-copied-files, KDE cut marker).
//! Wayland uses our own wl_data_device client; X11 uses arboard (text only).

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

pub fn path_to_uri(p: &std::path::Path) -> String {
    let mut s = String::from("file://");
    for b in p.as_os_str().as_encoded_bytes() {
        let c = *b;
        if c.is_ascii_alphanumeric() || b"/-_.~".contains(&c) {
            s.push(c as char);
        } else {
            s.push_str(&format!("%{c:02X}"));
        }
    }
    s
}

pub fn uri_to_path(u: &str) -> Option<PathBuf> {
    let rest = u.trim().strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let bytes = rest.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    Some(PathBuf::from(unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(out) }))
}

/// A text/uri-list body (CRLF-terminated lines).
pub fn uri_list(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| format!("{}\r\n", path_to_uri(p))).collect()
}

/// Local files in a text/uri-list body (comments and other schemes skipped).
pub fn parse_uri_list(s: &str) -> Vec<PathBuf> {
    s.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).filter_map(uri_to_path).collect()
}

pub use crate::wlclip::Dnd;

/// Drags from other apps over the window (Wayland; X11 drops come through winit).
pub fn take_dnd() -> Vec<Dnd> {
    crate::wlclip::take_events()
}

/// Called when drag events arrive (wakes the event loop).
pub fn set_dnd_waker(f: impl Fn() + Send + 'static) {
    crate::wlclip::set_waker(f);
}

/// Files can be carried out of the window to other apps.
pub fn can_drag_out() -> bool {
    #[cfg(test)]
    return true;
    #[cfg(not(test))]
    with(|c| matches!(c, Clip::Wayland(_)).then_some(())).is_some()
}

#[cfg(test)]
pub static DRAGGED_OUT: Mutex<Vec<Vec<PathBuf>>> = Mutex::new(Vec::new());

/// Hand an in-window file drag over to the system (the button is still held).
pub fn start_drag(paths: Vec<PathBuf>, picture: Option<crate::wlclip::Picture>) {
    #[cfg(test)]
    DRAGGED_OUT.lock().unwrap_or_else(|e| e.into_inner()).push(paths.clone());
    with(|c| {
        if let Clip::Wayland(w) = c {
            w.start_drag(paths, picture);
        }
        Some(())
    });
}

enum Clip {
    Wayland(crate::wlclip::WlClip),
    X11(arboard::Clipboard),
}

static CLIP: OnceLock<Mutex<Option<Clip>>> = OnceLock::new();

const TEXT_MIMES: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];

/// Connect to the window's display server (call once, after the window exists).
pub fn init(window: &winit::window::Window) {
    use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
    let clip = match window.display_handle().map(|h| h.as_raw()) {
        Ok(RawDisplayHandle::Wayland(w)) => Some(Clip::Wayland(unsafe { crate::wlclip::WlClip::new(w.display.as_ptr()) })),
        Ok(RawDisplayHandle::Xlib(_)) | Ok(RawDisplayHandle::Xcb(_)) => arboard::Clipboard::new().ok().map(Clip::X11),
        _ => None,
    };
    let _ = CLIP.set(Mutex::new(clip));
}

fn with<R>(f: impl FnOnce(&mut Clip) -> Option<R>) -> Option<R> {
    let mut g = CLIP.get()?.lock().ok()?;
    f(g.as_mut()?)
}

pub fn set_text(s: &str) {
    with(|c| {
        match c {
            Clip::Wayland(w) => w.store(TEXT_MIMES.iter().map(|m| (m.to_string(), s.as_bytes().to_vec())).collect()),
            Clip::X11(x) => {
                let _ = x.set_text(s.to_string());
            }
        }
        Some(())
    });
}

/// Clipboard text; a copied file list is returned as one path per line.
pub fn get_text() -> Option<String> {
    with(|c| match c {
        Clip::Wayland(w) => {
            let (mime, bytes) = w.load(&["text/plain;charset=utf-8", "UTF8_STRING", "text/plain", "text/uri-list"]).ok()?;
            let s = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
            if mime == "text/uri-list" {
                Some(s.lines().filter_map(uri_to_path).map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n"))
            } else {
                Some(s)
            }
        }
        Clip::X11(x) => x.get_text().ok(),
    })
}

/// Publish files to the system clipboard. Returns false if only the internal clipboard is used.
pub fn set_files(paths: &[PathBuf], cut: bool) -> bool {
    with(|c| {
        let uris: Vec<String> = paths.iter().map(|p| path_to_uri(p)).collect();
        let plain = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n");
        match c {
            Clip::Wayland(w) => {
                let mut items: Vec<(String, Vec<u8>)> = vec![
                    ("text/uri-list".into(), uri_list(paths).into_bytes()),
                    ("x-special/gnome-copied-files".into(), format!("{}\n{}", if cut { "cut" } else { "copy" }, uris.join("\n")).into_bytes()),
                ];
                items.extend(TEXT_MIMES.iter().map(|m| (m.to_string(), plain.clone().into_bytes())));
                if cut {
                    items.push(("application/x-kde-cutselection".into(), b"1".to_vec()));
                }
                w.store(items);
                Some(true)
            }
            Clip::X11(x) => {
                let _ = x.set_text(plain);
                Some(false)
            }
        }
    })
    .unwrap_or(false)
}

/// Read files from the system clipboard: (paths, is_cut).
pub fn get_files() -> Option<(Vec<PathBuf>, bool)> {
    let from_text = |s: &str| -> Vec<PathBuf> {
        s.lines().map(|l| l.trim().trim_matches('"').trim_matches('\'')).filter(|l| l.starts_with('/')).map(PathBuf::from).filter(|p| p.exists()).collect()
    };
    with(|c| match c {
        Clip::Wayland(w) => {
            let (mime, bytes) = w.load(&["x-special/gnome-copied-files", "text/uri-list", "text/plain;charset=utf-8", "text/plain"]).ok()?;
            let s = String::from_utf8_lossy(&bytes).into_owned();
            let (paths, cut) = match mime.as_str() {
                "x-special/gnome-copied-files" => {
                    let mut lines = s.lines();
                    let cut = lines.next().map(|l| l.trim() == "cut").unwrap_or(false);
                    (lines.filter_map(uri_to_path).collect(), cut)
                }
                "text/uri-list" => {
                    let cut = w.load(&["application/x-kde-cutselection"]).map(|(_, b)| b.first() == Some(&b'1')).unwrap_or(false);
                    (parse_uri_list(&s), cut)
                }
                _ => (from_text(&s), false),
            };
            (!paths.is_empty()).then_some((paths, cut))
        }
        Clip::X11(x) => {
            let paths = from_text(&x.get_text().ok()?);
            (!paths.is_empty()).then_some((paths, false))
        }
    })
}

/// Quote a path for shells only when needed.
pub fn quote_if_needed(s: &str) -> String {
    if s.chars().any(|c| c.is_whitespace() || "'\"$`\\!*?&;|<>()[]{}#~".contains(c)) {
        format!("'{}'", s.replace('\'', "'\\''"))
    } else {
        s.to_string()
    }
}
