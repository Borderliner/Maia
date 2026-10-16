//! File and folder icons from the desktop's freedesktop icon theme, so files look the
//! way they do everywhere else on the desktop. Lookup follows the Icon Theme spec: the
//! configured theme, its `Inherits` chain, then hicolor. SVGs are rasterized off the UI
//! thread at the exact pixel size and cached as textures; until an icon is ready (or when
//! the theme has none) Maia's built-in drawn icons are used.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, OnceLock};

use egui::{Color32, ColorImage, Painter, Rect, TextureHandle, TextureOptions};

use crate::fsx::Entry;

#[derive(Clone, Copy, PartialEq)]
enum DirKind {
    Fixed,
    Scalable,
    Threshold,
}

struct Dir {
    sub: String,
    size: u32,
    scale: u32,
    kind: DirKind,
    min: u32,
    max: u32,
    threshold: u32,
}

struct Theme {
    bases: Vec<PathBuf>,
    dirs: Vec<Dir>,
}

struct Lookup {
    chain: Vec<Theme>,
    found: HashMap<(String, u32), Option<PathBuf>>,
}

fn base_dirs() -> Vec<PathBuf> {
    let mut v = vec![];
    if let Some(d) = dirs::data_dir() {
        v.push(d.join("icons"));
    }
    v.push(crate::fsx::home().join(".icons"));
    for d in std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into()).split(':').filter(|s| !s.is_empty()) {
        v.push(Path::new(d).join("icons"));
    }
    v
}

/// The desktop's icon theme name (GNOME/GTK settings), if any.
fn theme_name() -> Option<String> {
    std::process::Command::new("gsettings")
        .args(["get", "org.gnome.desktop.interface", "icon-theme"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let cfg = dirs::config_dir()?;
            ["gtk-4.0/settings.ini", "gtk-3.0/settings.ini"].iter().find_map(|f| {
                std::fs::read_to_string(cfg.join(f)).ok()?.lines().find_map(|l| l.trim().strip_prefix("gtk-icon-theme-name=").map(|v| v.trim().trim_matches('"').to_string()))
            })
        })
        .or_else(|| {
            // KDE
            let s = std::fs::read_to_string(dirs::config_dir()?.join("kdeglobals")).ok()?;
            let mut in_icons = false;
            for l in s.lines() {
                let l = l.trim();
                if l.starts_with('[') {
                    in_icons = l == "[Icons]";
                } else if in_icons {
                    if let Some(v) = l.strip_prefix("Theme=") {
                        return Some(v.to_string());
                    }
                }
            }
            None
        })
}

fn load_theme(name: &str, bases: &[PathBuf]) -> Option<(Theme, Vec<String>)> {
    let roots: Vec<PathBuf> = bases.iter().map(|b| b.join(name)).filter(|p| p.is_dir()).collect();
    let index = roots.iter().find_map(|r| std::fs::read_to_string(r.join("index.theme")).ok())?;
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut cur = String::new();
    for l in index.lines() {
        let l = l.trim();
        if let Some(s) = l.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            cur = s.to_string();
        } else if let Some((k, v)) = l.split_once('=') {
            sections.entry(cur.clone()).or_default().insert(k.trim().to_string(), v.trim().to_string());
        }
    }
    let main = sections.get("Icon Theme")?;
    let list = |k: &str| -> Vec<String> { main.get(k).map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default() };
    let mut dirs = vec![];
    for sub in list("Directories").into_iter().chain(list("ScaledDirectories")) {
        let Some(sec) = sections.get(&sub) else { continue };
        let num = |k: &str, d: u32| sec.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
        let size = num("Size", 0);
        if size == 0 {
            continue;
        }
        let kind = match sec.get("Type").map(|s| s.as_str()) {
            Some("Fixed") => DirKind::Fixed,
            Some("Scalable") => DirKind::Scalable,
            _ => DirKind::Threshold,
        };
        dirs.push(Dir { sub, size, scale: num("Scale", 1), kind, min: num("MinSize", size), max: num("MaxSize", size), threshold: num("Threshold", 2) });
    }
    Some((Theme { bases: roots, dirs }, list("Inherits")))
}

impl Lookup {
    fn new() -> Lookup {
        let bases = base_dirs();
        let mut chain = vec![];
        let mut seen = HashSet::new();
        let mut queue: Vec<String> = theme_name().into_iter().collect();
        // Common full-color themes as a safety net, then hicolor last.
        queue.extend(["breeze", "Adwaita", "AdwaitaLegacy"].map(String::from));
        let mut i = 0;
        while i < queue.len() && chain.len() < 12 {
            let n = queue[i].clone();
            i += 1;
            if n == "hicolor" || !seen.insert(n.clone()) {
                continue;
            }
            if let Some((t, parents)) = load_theme(&n, &bases) {
                chain.push(t);
                // Parents come right after their child.
                for (k, p) in parents.into_iter().enumerate() {
                    queue.insert(i + k, p);
                }
            }
        }
        if let Some((t, _)) = load_theme("hicolor", &bases) {
            chain.push(t);
        }
        Lookup { chain, found: HashMap::new() }
    }

    fn distance(d: &Dir, size: u32, scale: u32) -> u32 {
        let want = size * scale;
        match d.kind {
            DirKind::Fixed => (d.size * d.scale).abs_diff(want),
            // Full-color scalable art renders crisply at any size; prefer it over the small
            // simplified sizes many themes ship (only an exact fixed size beats it).
            DirKind::Scalable if want >= 16 => 0,
            DirKind::Scalable => {
                if want < d.min * d.scale {
                    d.min * d.scale - want
                } else if want > d.max * d.scale {
                    want - d.max * d.scale
                } else {
                    0
                }
            }
            DirKind::Threshold => {
                let (lo, hi) = ((d.size.saturating_sub(d.threshold)) * d.scale, (d.size + d.threshold) * d.scale);
                if want < lo {
                    lo - want
                } else if want > hi {
                    want - hi
                } else {
                    0
                }
            }
        }
    }

    /// Best file for `name` at `px` physical pixels: the closest size in the first theme
    /// (of the chain) that has the icon at all. Scalable SVGs win ties.
    fn find(&mut self, name: &str, px: u32) -> Option<PathBuf> {
        let key = (name.to_string(), px);
        if let Some(r) = self.found.get(&key) {
            return r.clone();
        }
        let mut result = None;
        'themes: for t in &self.chain {
            let mut best: Option<(u32, bool, PathBuf)> = None;
            let symbolic_name = name.ends_with("-symbolic");
            for d in &t.dirs {
                // Monochrome symbolic sets only serve "-symbolic" names.
                if !symbolic_name && d.sub.contains("symbolic") {
                    continue;
                }
                // Among scalable sets, the one designed at the larger size has the full art.
                // Ties go to scalable art (the larger design first) over small fixed sizes.
                let dist = Self::distance(d, px, 1) * 1000 + if d.kind == DirKind::Scalable { 998u32.saturating_sub(d.size.min(998)) } else { 999 };
                if best.as_ref().map(|b| dist > b.0 || (dist == b.0 && b.1)).unwrap_or(false) {
                    continue;
                }
                for base in &t.bases {
                    for ext in ["svg", "png"] {
                        let p = base.join(&d.sub).join(format!("{name}.{ext}"));
                        if p.exists() {
                            let scalable = ext == "svg";
                            if best.as_ref().map(|b| dist < b.0 || (dist == b.0 && scalable && !b.1)).unwrap_or(true) {
                                best = Some((dist, scalable, p));
                            }
                            break;
                        }
                    }
                }
            }
            if let Some((_, _, p)) = best {
                result = Some(p);
                break 'themes;
            }
        }
        self.found.insert(key, result.clone());
        result
    }
}

static ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// Follow the "System icon theme" option.
pub fn set_enabled(on: bool) {
    ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

fn enabled() -> bool {
    ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Icon names for a folder at `path` (special user folders get their own icons).
pub fn dir_names(path: &Path, link: bool) -> Vec<String> {
    let mut v = vec![];
    let home = crate::fsx::home();
    let special = [
            (home.clone(), "user-home"),
            (dirs::desktop_dir().unwrap_or_default(), "user-desktop"),
            (dirs::download_dir().unwrap_or_default(), "folder-download"),
            (dirs::document_dir().unwrap_or_default(), "folder-documents"),
            (dirs::audio_dir().unwrap_or_default(), "folder-music"),
            (dirs::picture_dir().unwrap_or_default(), "folder-pictures"),
            (dirs::video_dir().unwrap_or_default(), "folder-videos"),
            (dirs::template_dir().unwrap_or_default(), "folder-templates"),
            (dirs::public_dir().unwrap_or_default(), "folder-publicshare"),
            (crate::sidebar::trash_dir(), "user-trash"),
        ];
    if let Some((_, n)) = special.iter().find(|(p, _)| !p.as_os_str().is_empty() && *p == path) {
        v.push(n.to_string());
    }
    if link {
        v.push("folder-link".into());
    }
    v.push("folder".into());
    v
}

/// Icon names to try for an entry, most specific first.
pub fn names_for(e: &Entry) -> Vec<String> {
    if e.is_dir {
        return dir_names(&e.path, e.is_link);
    }
    file_names(&e.name, e.is_executable())
}

/// Icon names to try for a file called `name` (no folder, nothing read from disk).
fn file_names(name: &str, exec: bool) -> Vec<String> {
    let mime = crate::mime::mime_db().for_name(name, false, exec);
    let mut v = vec![mime.replace('/', "-")];
    if let Some(g) = generic_icons().get(&mime) {
        v.push(g.clone());
    }
    let top = mime.split('/').next().unwrap_or("");
    if exec && !matches!(top, "image" | "audio" | "video" | "text") {
        v.push("application-x-executable".into());
    }
    match top {
        "image" | "audio" | "video" | "text" | "font" => v.push(format!("{top}-x-generic")),
        "application" => v.push("application-x-generic".into()),
        _ => {}
    }
    v.push("text-x-generic".into());
    v.push("unknown".into());
    v
}

fn generic_icons() -> &'static HashMap<String, String> {
    static G: OnceLock<HashMap<String, String>> = OnceLock::new();
    G.get_or_init(|| {
        let mut m = HashMap::new();
        for d in std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into()).split(':').filter(|s| !s.is_empty()).map(PathBuf::from).chain(dirs::data_dir()) {
            if let Ok(s) = std::fs::read_to_string(d.join("mime/generic-icons")) {
                for l in s.lines() {
                    if let Some((k, v)) = l.split_once(':') {
                        m.entry(k.to_string()).or_insert_with(|| v.to_string());
                    }
                }
            }
        }
        m
    })
}

// ------------------------------------------------------------------ cache + worker

type Key = (String, u32); // (first name of the list, px)

struct Cache {
    tex: HashMap<Key, Option<TextureHandle>>,
    pending: HashSet<Key>,
    ready: Vec<(Key, Option<ColorImage>)>,
    tx: Sender<(Key, Vec<String>)>,
}

fn cache() -> &'static Mutex<Cache> {
    static C: OnceLock<Mutex<Cache>> = OnceLock::new();
    C.get_or_init(|| {
        let (tx, rx) = channel::<(Key, Vec<String>)>();
        std::thread::Builder::new().name("icons".into()).spawn(move || worker(rx)).ok();
        Mutex::new(Cache { tex: HashMap::new(), pending: HashSet::new(), ready: vec![], tx })
    })
}

static REPAINT: OnceLock<egui::Context> = OnceLock::new();

fn worker(rx: Receiver<(Key, Vec<String>)>) {
    let mut lookup = Lookup::new();
    while let Ok((key, names)) = rx.recv() {
        let px = key.1;
        let img = names
            .iter()
            .find_map(|n| if n.starts_with('/') { Some(PathBuf::from(n)).filter(|p| p.is_file()) } else { lookup.find(n, px) })
            .and_then(|p| rasterize(&p, px));
        if let Ok(mut c) = cache().lock() {
            c.ready.push((key, img));
        }
        if let Some(ctx) = REPAINT.get() {
            ctx.request_repaint();
        }
    }
}

fn rasterize(path: &Path, px: u32) -> Option<ColorImage> {
    let data = std::fs::read(path).ok()?;
    if path.extension().map(|e| e == "png").unwrap_or(false) {
        let (img, _) = crate::imgdec::load(path, px)?;
        return Some(ColorImage::from_rgba_unmultiplied([img.w as usize, img.h as usize], &img.px));
    }
    let (w, h, px) = crate::svg::render(&data, px)?;
    Some(ColorImage::from_rgba_premultiplied([w as usize, h as usize], &px))
}

/// Draw the theme icon for `e` into `rect`. Returns false when no theme icon is available
/// yet (the caller then draws its built-in icon).
pub fn entry(painter: &Painter, rect: Rect, e: &Entry) -> bool {
    enabled() && paint_names(painter, rect, names_for(e), e.is_link && !e.is_dir)
}

/// Theme icon by name (or an absolute icon file path), e.g. an application's Icon=.
pub fn named(painter: &Painter, rect: Rect, name: &str) -> bool {
    !name.is_empty() && enabled() && paint_names(painter, rect, vec![name.to_string()], false)
}

/// The icon a file called `name` has in the listing: the theme's, or Maia's own drawn
/// one while the theme's loads or when there is none.
pub fn file(painter: &Painter, rect: Rect, name: &str) {
    if !(enabled() && paint_names(painter, rect, file_names(name, false), false)) {
        let ext = name.rsplit_once('.').map(|(_, e)| e.to_lowercase()).unwrap_or_default();
        crate::theme::paint_icon(painter, rect, crate::theme::categorize(&ext, false, false), "", false);
    }
}

/// A folder's icon in the listing, by its path.
pub fn folder(painter: &Painter, rect: Rect, path: &Path) {
    if !dir(painter, rect, path) {
        crate::theme::paint_icon(painter, rect, crate::theme::FileCat::Folder, "", false);
    }
}

/// Theme icon for the folder at `path`.
pub fn dir(painter: &Painter, rect: Rect, path: &Path) -> bool {
    enabled() && paint_names(painter, rect, dir_names(path, false), false)
}

fn paint_names(painter: &Painter, rect: Rect, names: Vec<String>, badge: bool) -> bool {
    let ctx = painter.ctx();
    let _ = REPAINT.set(ctx.clone());
    let side = rect.width().min(rect.height());
    let ppp = ctx.pixels_per_point();
    // Exact pixels for list sizes; coarser steps for big, animating icon views.
    let raw = (side * ppp).round().max(8.0) as u32;
    let px = if raw <= 64 { raw } else { raw.div_ceil(16) * 16 };
    let key = (names.join(","), px);
    let Ok(mut c) = cache().lock() else { return false };
    if !c.ready.is_empty() {
        for (k, img) in std::mem::take(&mut c.ready) {
            c.pending.remove(&k);
            let tex = img.map(|i| ctx.load_texture(format!("icon:{}:{}", k.0, k.1), i, TextureOptions::LINEAR));
            c.tex.insert(k, tex);
        }
    }
    match c.tex.get(&key) {
        Some(Some(t)) => {
            let sz = t.size_vec2() / ppp;
            let r = Rect::from_center_size(rect.center(), sz);
            painter.image(t.id(), r, Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
            if badge {
                link_badge(painter, rect);
            }
            true
        }
        Some(None) => false,
        None => {
            if c.pending.insert(key.clone()) {
                let _ = c.tx.send((key, names));
            }
            false
        }
    }
}

fn link_badge(p: &Painter, r: Rect) {
    let s = (r.width() * 0.38).max(7.0);
    let b = Rect::from_min_size(egui::pos2(r.left(), r.bottom() - s), egui::vec2(s, s));
    p.rect_filled(b, egui::CornerRadius::same(2), Color32::from_rgb(0xf2, 0xf2, 0xf2));
    let st = egui::Stroke::new((s / 7.0).max(1.0), Color32::from_rgb(0x20, 0x20, 0x20));
    let (a, z) = (egui::pos2(b.left() + s * 0.28, b.bottom() - s * 0.28), egui::pos2(b.right() - s * 0.25, b.top() + s * 0.25));
    p.line_segment([a, z], st);
    p.line_segment([z, egui::pos2(z.x - s * 0.32, z.y)], st);
    p.line_segment([z, egui::pos2(z.x, z.y + s * 0.32)], st);
}

/// Forget cached textures (icon theme or scale changed).
pub fn clear() {
    if let Ok(mut c) = cache().lock() {
        c.tex.clear();
    }
}
