//! The desktop's own colours, for the System theme, read where GTK and Qt programs read
//! them: a GTK user stylesheet's named colours (libadwaita's, as Noctalia, matugen,
//! Gradience and Stylix write them, and GTK 3's theme_* ones), KDE's kdeglobals, and
//! otherwise Adwaita's light or dark with the portal's accent colour. They are followed
//! as they change: the files through inotify, the portal through `appearance`.

use std::collections::HashMap;
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};

use egui::Color32;

/// What the System theme is built from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Desktop {
    pub dark: bool,
    /// Windows' background and text.
    pub window: Color32,
    pub text: Color32,
    /// Lists' and text views' background.
    pub view: Color32,
    pub sidebar: Option<Color32>,
    /// Menus and popovers.
    pub popover: Option<Color32>,
    pub accent: Color32,
    pub danger: Option<Color32>,
    /// Where they came from, as the theme's line in the picker says it.
    pub from: &'static str,
}

/// GNOME's default accent.
const BLUE: Color32 = Color32::from_rgb(0x35, 0x84, 0xe4);

/// Adwaita's own (libadwaita 1.6), light or dark, with `accent`.
pub fn adwaita(dark: bool, accent: Option<Color32>) -> Desktop {
    let c = Color32::from_rgb;
    let from = match (dark, accent.is_some()) {
        (true, true) => "Adwaita dark, your accent",
        (true, false) => "Adwaita dark",
        (false, true) => "Adwaita light, your accent",
        (false, false) => "Adwaita light",
    };
    let accent = accent.unwrap_or(BLUE);
    if dark {
        Desktop { dark, window: c(0x22, 0x22, 0x26), text: c(0xff, 0xff, 0xff), view: c(0x1d, 0x1d, 0x20), sidebar: Some(c(0x2e, 0x2e, 0x32)), popover: Some(c(0x36, 0x36, 0x3a)), accent, danger: Some(c(0xff, 0x7b, 0x63)), from }
    } else {
        Desktop { dark, window: c(0xfa, 0xfa, 0xfb), text: c(0x33, 0x33, 0x36), view: c(0xff, 0xff, 0xff), sidebar: Some(c(0xeb, 0xeb, 0xed)), popover: Some(c(0xff, 0xff, 0xff)), accent, danger: Some(c(0xc0, 0x1c, 0x28)), from }
    }
}

/// The desktop's colours from the config folder `config`: KDE's first on KDE, GTK's
/// first elsewhere (kdeglobals there only when it names a colour scheme, not one a KDE
/// program left behind), else Adwaita as the portal's light or dark has it.
pub fn read(config: &Path, kde_desktop: bool, portal_dark: Option<bool>, portal_accent: Option<Color32>) -> Desktop {
    let found = if kde_desktop { kde(config, true, portal_accent).or_else(|| gtk(config, portal_accent)) } else { gtk(config, portal_accent).or_else(|| kde(config, false, portal_accent)) };
    found.unwrap_or_else(|| adwaita(portal_dark.unwrap_or(true), portal_accent))
}

/// Relative luminance (WCAG): 0 for black, 1 for white.
pub fn luminance(c: Color32) -> f32 {
    let lin = |v: u8| {
        let v = v as f32 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c.r()) + 0.7152 * lin(c.g()) + 0.0722 * lin(c.b())
}

// ---------------------------------------------------------------- GTK

/// From GTK 4's user stylesheet, else GTK 3's: they need a window background and text.
fn gtk(config: &Path, portal_accent: Option<Color32>) -> Option<Desktop> {
    ["gtk-4.0", "gtk-3.0"].iter().find_map(|dir| {
        let mut defs = HashMap::new();
        defines(&config.join(dir).join("gtk.css"), &mut defs, 0);
        let get = |names: &[&str]| names.iter().find_map(|n| css_color(defs.get(*n)?, &defs, 0));
        let window = get(&["window_bg_color", "theme_bg_color"])?;
        let text = get(&["window_fg_color", "theme_fg_color"])?;
        Some(Desktop {
            dark: luminance(window) < luminance(text),
            window,
            text,
            view: get(&["view_bg_color", "theme_base_color"]).unwrap_or(window),
            sidebar: get(&["sidebar_bg_color"]),
            popover: get(&["popover_bg_color", "card_bg_color"]),
            accent: get(&["accent_bg_color", "accent_color", "theme_selected_bg_color"]).or(portal_accent).unwrap_or(BLUE),
            danger: get(&["destructive_color", "destructive_bg_color", "error_color"]),
            from: "Your desktop's GTK colours",
        })
    })
}

/// A stylesheet's `@define-color`s into `out`, its `@import`s read where they stand, so
/// later ones win as in CSS.
fn defines(file: &Path, out: &mut HashMap<String, String>, depth: usize) {
    let Some(css) = (depth < 5).then(|| std::fs::read_to_string(file).ok()).flatten() else { return };
    let dir = file.parent().unwrap_or(Path::new("/"));
    let mut text = String::with_capacity(css.len());
    let mut rest = css.as_str();
    while let Some(i) = rest.find("/*") {
        text.push_str(&rest[..i]);
        rest = rest[i..].find("*/").map_or("", |j| &rest[i + j + 2..]);
    }
    text.push_str(rest);
    for stmt in text.split(';') {
        // What follows the last brace: a statement at the top level.
        let s = stmt.rsplit(['{', '}']).next().unwrap_or("").trim();
        if let Some(target) = s.strip_prefix("@import") {
            let t = target.trim();
            let t = t.strip_prefix("url(").and_then(|t| t.strip_suffix(')')).unwrap_or(t).trim().trim_matches(['"', '\'']);
            let t = t.strip_prefix("file://").unwrap_or(t);
            if !t.is_empty() {
                defines(&dir.join(t), out, depth + 1);
            }
        } else if let Some((name, value)) = s.strip_prefix("@define-color").and_then(|d| d.trim().split_once(char::is_whitespace)) {
            out.insert(name.to_string(), value.trim().to_string());
        }
    }
}

/// A GTK CSS colour: #hex, rgb() and rgba(), white and black, another `@name`, and
/// alpha(), mix(), shade(), lighter() and darker() of those. Transparency is dropped:
/// the window is what shows through.
fn css_color(v: &str, defs: &HashMap<String, String>, depth: usize) -> Option<Color32> {
    let v = v.trim();
    if depth > 8 {
        return None;
    }
    if let Some(name) = v.strip_prefix('@') {
        return css_color(defs.get(name)?, defs, depth + 1);
    }
    if let Some(hex) = v.strip_prefix('#') {
        let n = |i: usize, w: usize| u8::from_str_radix(hex.get(i..i + w)?, 16).ok().map(|x| if w == 1 { x * 17 } else { x });
        return match hex.len() {
            3 | 4 => Some(Color32::from_rgb(n(0, 1)?, n(1, 1)?, n(2, 1)?)),
            6 | 8 => Some(Color32::from_rgb(n(0, 2)?, n(2, 2)?, n(4, 2)?)),
            _ => None,
        };
    }
    match v {
        "white" => return Some(Color32::WHITE),
        "black" => return Some(Color32::BLACK),
        _ => {}
    }
    let (func, args) = v.split_once('(')?;
    let args = args.strip_suffix(')')?;
    // The arguments, split at the commas not inside another call.
    let mut parts = vec![];
    let (mut level, mut start) = (0, 0);
    for (i, ch) in args.char_indices() {
        match ch {
            '(' => level += 1,
            ')' => level -= 1,
            ',' if level == 0 => {
                parts.push(args[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(args[start..].trim());
    let num = |s: &str| s.strip_suffix('%').map_or_else(|| s.parse::<f32>().ok(), |p| p.parse::<f32>().ok().map(|p| p / 100.0));
    let mix = |a: Color32, b: Color32, t: f32| {
        let t = t.clamp(0.0, 1.0);
        let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
        Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
    };
    let shade = |c: Color32, f: f32| if f < 1.0 { mix(c, Color32::BLACK, 1.0 - f) } else { mix(c, Color32::WHITE, f - 1.0) };
    let arg = |i: usize| css_color(parts.get(i)?, defs, depth + 1);
    match func.trim() {
        "rgb" | "rgba" => {
            let ch = |i: usize| {
                let s = *parts.get(i)?;
                if s.ends_with('%') { num(s).map(|f| (f * 255.0).round() as u8) } else { s.parse::<f32>().ok().map(|x| x.round().clamp(0.0, 255.0) as u8) }
            };
            Some(Color32::from_rgb(ch(0)?, ch(1)?, ch(2)?))
        }
        "alpha" => arg(0),
        "mix" => Some(mix(arg(0)?, arg(1)?, num(parts.get(2)?)?)),
        "shade" => Some(shade(arg(0)?, num(parts.get(1)?)?)),
        "lighter" => Some(shade(arg(0)?, 1.3)),
        "darker" => Some(shade(arg(0)?, 0.7)),
        _ => None,
    }
}

// ---------------------------------------------------------------- KDE

/// From kdeglobals; where KDE is not the desktop, only a colour scheme someone chose.
fn kde(config: &Path, anyway: bool, portal_accent: Option<Color32>) -> Option<Desktop> {
    let ini = std::fs::read_to_string(config.join("kdeglobals")).ok()?;
    let mut keys: HashMap<(String, String), String> = HashMap::new();
    let mut group = String::new();
    for line in ini.lines().map(str::trim) {
        if let Some(g) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            group = g.to_string();
        } else if let Some((k, v)) = line.split_once('=') {
            keys.insert((group.clone(), k.trim().to_string()), v.trim().to_string());
        }
    }
    let get = |g: &str, k: &str| keys.get(&(g.to_string(), k.to_string()));
    if !anyway && get("General", "ColorScheme").is_none_or(|s| s.is_empty()) {
        return None;
    }
    // "r,g,b" or "r,g,b,a"; newer ones "#rrggbb".
    let rgb = |g: &str, k: &str| {
        let v = get(g, k)?;
        if v.starts_with('#') {
            return css_color(v, &HashMap::new(), 0);
        }
        let n: Vec<u8> = v.split(',').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
        (n.len() >= 3).then(|| Color32::from_rgb(n[0], n[1], n[2]))
    };
    let window = rgb("Colors:Window", "BackgroundNormal")?;
    let text = rgb("Colors:Window", "ForegroundNormal")?;
    Some(Desktop {
        dark: luminance(window) < luminance(text),
        window,
        text,
        view: rgb("Colors:View", "BackgroundNormal").unwrap_or(window),
        sidebar: None,
        popover: None,
        accent: rgb("General", "AccentColor").or_else(|| rgb("Colors:Selection", "BackgroundNormal")).or(portal_accent).unwrap_or(BLUE),
        danger: rgb("Colors:View", "ForegroundNegative"),
        from: "Your desktop's KDE colours",
    })
}

// ---------------------------------------------------------------- now

/// The portal's light or dark (`appearance`'s values) and accent (0 for none, else
/// 0x01RRGGBB), as `appearance` last read them.
static SCHEME: AtomicU8 = AtomicU8::new(crate::appearance::UNKNOWN);
static ACCENT: AtomicU32 = AtomicU32::new(0);
static GENERATION: AtomicU64 = AtomicU64::new(0);
static CURRENT: Mutex<Option<Desktop>> = Mutex::new(None);

fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| crate::fsx::home().join(".config"))
}

/// The desktop's colours now (read once, then again after each change).
pub fn current() -> Desktop {
    let mut cur = CURRENT.lock().unwrap_or_else(|e| e.into_inner());
    *cur.get_or_insert_with(|| {
        let kde = std::env::var("XDG_CURRENT_DESKTOP").is_ok_and(|d| d.split(':').any(|d| d.eq_ignore_ascii_case("KDE")));
        let dark = match SCHEME.load(Ordering::Relaxed) {
            crate::appearance::DARK => Some(true),
            crate::appearance::LIGHT => Some(false),
            _ => None,
        };
        let a = ACCENT.load(Ordering::Relaxed);
        let accent = (a != 0).then(|| Color32::from_rgb((a >> 16) as u8, (a >> 8) as u8, a as u8));
        read(&config_dir(), kde, dark, accent)
    })
}

/// Counts the changes, for a theme built from `current` to know when to build again.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::Relaxed)
}

/// Something they come from changed: read them again when next asked.
pub fn changed() {
    *CURRENT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// The portal's light or dark preference.
pub fn set_scheme(scheme: u8) {
    if SCHEME.swap(scheme, Ordering::Relaxed) != scheme {
        changed();
    }
}

/// The portal's accent colour, red, green and blue from 0 to 1; out of that range, or
/// none, is no accent.
pub fn set_accent(rgb: Option<[f64; 3]>) {
    let v = rgb.filter(|c| c.iter().all(|x| (0.0..=1.0).contains(x))).map_or(0, |c| {
        let b = |x: f64| (x * 255.0).round() as u32;
        0x0100_0000 | b(c[0]) << 16 | b(c[1]) << 8 | b(c[2])
    });
    if ACCENT.swap(v, Ordering::Relaxed) != v {
        changed();
    }
}

/// Follow the GTK stylesheets' folders and kdeglobals' for changes (a shell writing
/// new colours with a new wallpaper), waking the window. Asked for while the System
/// theme is in use; one watcher at a time, a new one once the config folder it watched
/// is gone.
pub fn watch(wake: impl Fn() + Send + 'static) {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if RUNNING.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(move || {
        let config = config_dir();
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd >= 0 {
            follow(fd, &config, &wake);
            unsafe { libc::close(fd) };
        }
        RUNNING.store(false, Ordering::Relaxed);
    });
}

/// The watcher's loop, until reading fails or the config folder is gone.
fn follow(fd: i32, config: &Path, wake: &dyn Fn()) {
    // Watch descriptor -> whether it is a GTK folder (else the config folder).
    let mut wds: HashMap<i32, bool> = HashMap::new();
    let add = |wds: &mut HashMap<i32, bool>, dir: &Path, is_gtk: bool| {
        let Ok(c) = CString::new(dir.as_os_str().as_bytes()) else { return };
        let wd = unsafe { libc::inotify_add_watch(fd, c.as_ptr(), libc::IN_CLOSE_WRITE | libc::IN_MOVED_TO | libc::IN_DELETE | libc::IN_CREATE) };
        if wd >= 0 {
            wds.insert(wd, is_gtk);
        }
    };
    for (dir, is_gtk) in [(config.join("gtk-4.0"), true), (config.join("gtk-3.0"), true), (config.to_path_buf(), false)] {
        add(&mut wds, &dir, is_gtk);
    }
    if !wds.values().any(|g| !g) {
        return;
    }
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        if n <= 0 {
            return;
        }
        let (mut off, mut hit, mut gone) = (0usize, false, false);
        let hdr = std::mem::size_of::<libc::inotify_event>();
        while off + hdr <= n as usize {
            let ev: libc::inotify_event = unsafe { std::ptr::read_unaligned(buf.as_ptr().add(off) as *const libc::inotify_event) };
            let name = buf[off + hdr..off + hdr + ev.len as usize].split(|b| *b == 0).next().unwrap_or(&[]);
            off += hdr + ev.len as usize;
            // A folder deleted: a GTK one comes back through the config folder;
            // the config folder itself, through a new watcher.
            if ev.mask & libc::IN_IGNORED != 0 {
                gone |= wds.remove(&ev.wd) == Some(false);
                hit = true;
                continue;
            }
            hit |= match wds.get(&ev.wd) {
                Some(true) => name.ends_with(b".css"),
                // A GTK folder made after the start is followed from then on.
                Some(false) if name == b"gtk-4.0" || name == b"gtk-3.0" => {
                    add(&mut wds, &config.join(std::ffi::OsStr::from_bytes(name)), true);
                    true
                }
                Some(false) => name == b"kdeglobals",
                None => false,
            };
        }
        if hit {
            // A shell writes several files at once: read them when it is done.
            std::thread::sleep(std::time::Duration::from_millis(150));
            changed();
            wake();
        }
        if gone {
            return;
        }
    }
}
