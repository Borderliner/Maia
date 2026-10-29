//! How much of a project is code, and in which languages: lines that are neither blank
//! nor only a comment, counted in the files git keeps (or, outside a repository, every
//! file but hidden folders and build output). One count runs at a time on its own
//! thread, the newest folder asked for next; the inspector shows what it found.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::Color32;

/// Files read at most, and bytes in all: a huge repository shows the part counted.
const MAX_FILES: usize = 30_000;
const MAX_BYTES: u64 = 256 << 20;
/// A file bigger than this is generated or minified, not written.
const MAX_FILE: u64 = 1 << 20;
/// A count this old is made again when the folder is shown again: while it stays shown
/// it is left as it is.
const STALE: Duration = Duration::from_secs(120);

/// The languages counted: their extensions, name and the colour they are known by.
const LANGS: &[(&[&str], &str, [u8; 3])] = &[
    (&["rs"], "Rust", [0xde, 0xa5, 0x84]),
    (&["py", "pyi"], "Python", [0x35, 0x72, 0xa5]),
    (&["js", "mjs", "cjs", "jsx"], "JavaScript", [0xf1, 0xe0, 0x5a]),
    (&["ts", "tsx", "mts", "cts"], "TypeScript", [0x31, 0x78, 0xc6]),
    (&["go"], "Go", [0x00, 0xad, 0xd8]),
    (&["c", "h"], "C", [0x8f, 0x8f, 0x99]),
    (&["cc", "cpp", "cxx", "hpp", "hh", "hxx"], "C++", [0xf3, 0x4b, 0x7d]),
    (&["cs"], "C#", [0x17, 0x86, 0x00]),
    (&["java"], "Java", [0xb0, 0x72, 0x19]),
    (&["kt", "kts"], "Kotlin", [0xa9, 0x7b, 0xff]),
    (&["swift"], "Swift", [0xf0, 0x51, 0x38]),
    (&["rb"], "Ruby", [0xcc, 0x34, 0x2d]),
    (&["php"], "PHP", [0x4f, 0x5d, 0x95]),
    (&["sh", "bash", "zsh", "fish"], "Shell", [0x89, 0xe0, 0x51]),
    (&["lua"], "Lua", [0x5b, 0x6c, 0xd8]),
    (&["hs"], "Haskell", [0x5e, 0x50, 0x86]),
    (&["zig"], "Zig", [0xec, 0x91, 0x5c]),
    (&["dart"], "Dart", [0x00, 0xb4, 0xab]),
    (&["scala"], "Scala", [0xc2, 0x2d, 0x40]),
    (&["ex", "exs"], "Elixir", [0x6e, 0x4a, 0x7e]),
    (&["vue"], "Vue", [0x41, 0xb8, 0x83]),
    (&["svelte"], "Svelte", [0xff, 0x3e, 0x00]),
    (&["html", "htm"], "HTML", [0xe3, 0x4c, 0x26]),
    (&["css", "scss", "sass", "less"], "CSS", [0x66, 0x33, 0x99]),
    (&["sql"], "SQL", [0xe3, 0x8c, 0x00]),
    (&["nix"], "Nix", [0x7e, 0x7e, 0xff]),
    (&["qml"], "QML", [0x44, 0xa5, 0x1c]),
    (&["glsl", "vert", "frag", "wgsl"], "Shaders", [0x5f, 0x86, 0xc4]),
    (&["toml"], "TOML", [0x9c, 0x42, 0x21]),
    (&["yml", "yaml"], "YAML", [0xcb, 0x17, 0x1e]),
    (&["md", "markdown"], "Markdown", [0x6a, 0x8f, 0xd0]),
];

pub struct Lang {
    pub name: &'static str,
    pub color: Color32,
    pub lines: u64,
    pub files: u64,
}

pub struct Count {
    /// Most lines first.
    pub langs: Vec<Lang>,
    pub lines: u64,
    /// Only part of the project was read.
    pub cut: bool,
}

enum Slot {
    Counting,
    /// When it was made, the last frame (egui's pass) it was asked for in, and the count:
    /// None when not a project or nothing in it is code.
    Done(Instant, u64, Option<Arc<Count>>),
}

#[derive(Default)]
struct Shared {
    slots: HashMap<PathBuf, Slot>,
    /// The folder to count next, and whether a count is running.
    next: Option<(PathBuf, egui::Context)>,
    running: bool,
}

fn shared() -> &'static Mutex<Shared> {
    static S: std::sync::OnceLock<Mutex<Shared>> = std::sync::OnceLock::new();
    S.get_or_init(Default::default)
}

/// The count for `dir` when there is one; asked for when not.
pub fn get(dir: &Path, ctx: &egui::Context) -> Option<Arc<Count>> {
    let mut s = shared().lock().unwrap_or_else(|e| e.into_inner());
    // Asked for in the frame before (a frame may run twice), it is still shown.
    let pass = ctx.cumulative_pass_nr();
    let (have, again) = match s.slots.get_mut(dir) {
        Some(Slot::Done(at, seen, c)) => {
            let back = *seen + 2 < pass;
            *seen = pass;
            (c.clone(), back && at.elapsed() > STALE)
        }
        Some(Slot::Counting) => return None,
        None => (None, true),
    };
    if again && s.next.as_ref().is_none_or(|n| n.0 != dir) {
        s.next = Some((dir.to_path_buf(), ctx.clone()));
        if !s.running {
            s.running = true;
            let _ = std::thread::Builder::new().name("line count".into()).spawn(worker);
        }
    }
    have
}

fn worker() {
    loop {
        let job = {
            let mut s = shared().lock().unwrap_or_else(|e| e.into_inner());
            match s.next.take() {
                Some(j) => {
                    // A count already made stays shown while the new one runs.
                    if !matches!(s.slots.get(&j.0), Some(Slot::Done(..))) {
                        s.slots.insert(j.0.clone(), Slot::Counting);
                    }
                    j
                }
                None => {
                    s.running = false;
                    return;
                }
            }
        };
        let (dir, ctx) = job;
        let c = count(&dir).map(Arc::new);
        shared().lock().unwrap_or_else(|e| e.into_inner()).slots.insert(dir, Slot::Done(Instant::now(), ctx.cumulative_pass_nr(), c));
        ctx.request_repaint();
    }
}

/// Count a project's code; None when `dir` isn't the top of one.
pub fn count(dir: &Path) -> Option<Count> {
    let git = crate::project::git_root(dir).as_deref() == Some(dir);
    if !git && !crate::project::MARKERS.iter().any(|m| dir.join(m).is_file()) {
        return None;
    }
    let stop = AtomicBool::new(false);
    let files = crate::quickopen::list(dir, &stop);
    let mut by: HashMap<usize, (u64, u64)> = HashMap::new();
    let (mut read, mut cut) = (0u64, false);
    let mut buf = Vec::new();
    for (n, rel) in files.iter().enumerate() {
        if n >= MAX_FILES || read >= MAX_BYTES {
            cut = true;
            break;
        }
        let ext = rel.rsplit_once('.').map(|x| x.1.to_ascii_lowercase()).unwrap_or_default();
        let Some(k) = LANGS.iter().position(|l| l.0.contains(&ext.as_str())) else { continue };
        let path = dir.join(rel);
        let Some(f) = crate::fsx::open_plain(&path) else { continue };
        if f.metadata().map_or(true, |m| m.len() > MAX_FILE) {
            continue;
        }
        buf.clear();
        if f.take(MAX_FILE).read_to_end(&mut buf).is_err() || buf.contains(&0) {
            continue;
        }
        read += buf.len() as u64;
        let marks = crate::syntax::lang_for(rel.rsplit('/').next().unwrap_or(rel)).map(|l| l.comments()).unwrap_or((&[], None));
        let e = by.entry(k).or_default();
        e.0 += code_lines(&String::from_utf8_lossy(&buf), marks.0, marks.1);
        e.1 += 1;
    }
    let mut langs: Vec<Lang> = by
        .into_iter()
        .filter(|(_, (l, _))| *l > 0)
        .map(|(k, (lines, files))| {
            let (_, name, [r, g, b]) = LANGS[k];
            Lang { name, color: Color32::from_rgb(r, g, b), lines, files }
        })
        .collect();
    langs.sort_by(|a, b| b.lines.cmp(&a.lines).then(a.name.cmp(b.name)));
    let lines = langs.iter().map(|l| l.lines).sum();
    (lines > 0).then_some(Count { langs, lines, cut })
}

/// Lines with code on them: not blank and not only a comment.
pub fn code_lines(text: &str, line: &[&str], block: Option<(&str, &str)>) -> u64 {
    let mut n = 0;
    let mut inside = false;
    for l in text.lines() {
        let mut t = l.trim();
        if inside {
            match block.and_then(|(_, close)| t.find(close).map(|i| i + close.len())) {
                Some(end) => {
                    inside = false;
                    t = t[end..].trim();
                }
                None => continue,
            }
        }
        // Comments that open and close on the line are stepped over; what is left is code.
        loop {
            if t.is_empty() || line.iter().any(|m| t.starts_with(m)) {
                t = "";
                break;
            }
            match block {
                Some((open, close)) if t.starts_with(open) => match t[open.len()..].find(close) {
                    Some(i) => t = t[open.len() + i + close.len()..].trim(),
                    None => {
                        inside = true;
                        t = "";
                        break;
                    }
                },
                _ => break,
            }
        }
        // A comment opened after code on the line is not followed: the same characters in a
        // string ("src/*") would hide everything below.
        if !t.is_empty() {
            n += 1;
        }
    }
    n
}
