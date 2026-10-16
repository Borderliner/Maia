//! Search by meaning: files in the chosen folders are cut into passages, each turned
//! into a fingerprint of its meaning by a local model (through Ollama), and a search
//! compares the question's fingerprint with all of them. Off unless switched on in
//! Options; nothing runs, listens or is stored until then.
//!
//! The index lives in the cache (`maia/meaning.idx`): per file its path, mtime and size,
//! per passage where it sits in the file and its fingerprint as DIMS signed bytes. Only
//! files that changed since the last run are read again. The indexer runs at the lowest
//! CPU priority there is, so it never competes with anything else.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

use crate::ollama::{self, DIMS};

/// Passages are about this many characters, overlapping a little so a sentence cut at
/// one edge is whole in the next.
const PASSAGE: usize = 1200;
const OVERLAP: usize = 200;
/// Enough for a long document; a log or data dump past this is not what one searches.
const MAX_PASSAGES: usize = 120;
const MAX_TEXT: u64 = 4 << 20;
const MAX_PDF: u64 = 60 << 20;
const MAX_FILES: usize = 200_000;
/// Passages per request to Ollama.
const BATCH: usize = 16;

/// Pictures, read by the model as they look (with "Include pictures" on).
const IMAGE_EXTS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "bmp"];
const MAX_IMAGE: u64 = 40 << 20;
/// Pictures go to the model at most this many pixels on a side. The model's work grows
/// with the pixels (a 2000-pixel screenshot takes it some 40 s on a laptop CPU, the same
/// shrunk to 512 under 2 s) and what it makes of a picture is as good at this size.
const IMAGE_SIDE: u32 = 512;
/// Smaller than this on its longest side: an icon, cursor or button, not a picture.
const MIN_PICTURE: u32 = 200;
/// A folder with this many pictures, nearly all numbered, holds frames cut from a video
/// or an animation: thousands of near-copies, skipped.
const FRAME_RUN: usize = 40;

pub fn is_picture(p: &Path) -> bool {
    p.extension().is_some_and(|e| IMAGE_EXTS.contains(&e.to_string_lossy().to_ascii_lowercase().as_str()))
}

const TEXT_EXTS: &[&str] = &[
    "txt", "md", "markdown", "rst", "org", "tex", "adoc", "rtf", "csv", "tsv", "log", "rs", "py", "js", "ts", "tsx", "jsx", "go", "c", "h", "cc", "cpp", "hpp", "java", "kt", "swift", "rb",
    "php", "cs", "lua", "sh", "fish", "zsh", "toml", "yaml", "yml", "ini", "conf", "html", "htm", "css", "scss", "sql", "json", "xml", "el", "vim", "nix", "zig", "dart", "scala", "r", "jl",
];

/// Writing meant to be read, searched through to its end. Everything else (code, data,
/// settings, logs) by its opening only: what the file is, which is what one asks for,
/// at a fraction of the work.
const PROSE_EXTS: &[&str] = &["txt", "md", "markdown", "rst", "org", "tex", "adoc", "rtf", "html", "htm", "pdf"];

fn prose(p: &Path) -> bool {
    p.extension().is_some_and(|e| PROSE_EXTS.contains(&e.to_string_lossy().to_ascii_lowercase().as_str()))
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    /// Asking Ollama whether it is there and has the model.
    Checking,
    NotRunning,
    NoModel,
    /// Fetching the model: bytes done, bytes in all.
    Downloading(u64, u64),
    /// Files looked at, files to look at.
    Indexing(usize, usize),
    Ready,
    Failed(String),
}

struct FileRec {
    path: PathBuf,
    mtime: i64,
    size: u64,
    /// Passages: byte where each starts in the text, and its length.
    passages: Vec<(u32, u32)>,
    /// Each passage's opening words, shown with a match (no file read to show it).
    previews: Vec<String>,
    /// First passage's fingerprint index in `Index::vecs` (DIMS bytes each).
    first: usize,
}

#[derive(Default)]
pub struct Index {
    files: Vec<FileRec>,
    vecs: Vec<i8>,
}

impl Index {
    pub fn passages(&self) -> usize {
        self.vecs.len() / DIMS
    }
    pub fn files(&self) -> usize {
        self.files.len()
    }
}

#[derive(Clone, Debug)]
pub struct Hit {
    pub path: PathBuf,
    pub score: f32,
    /// The passage that matched, on one line.
    pub passage: String,
}

struct Shared {
    phase: Phase,
    index: Arc<Index>,
    /// Pictures left for when the laptop is on its charger.
    waiting: usize,
    /// The user asked to fetch the model.
    pull: bool,
}

/// The running engine; dropping it stops the indexer.
pub struct Engine {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    wake: Arc<(Mutex<bool>, std::sync::Condvar)>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        self.poke();
    }
}

/// "~/Documents, ~/Notes" as folders.
pub fn folders(list: &str) -> Vec<PathBuf> {
    list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(|s| match s.strip_prefix('~') {
        Some(rest) => crate::fsx::home().join(rest.trim_start_matches('/')),
        None => PathBuf::from(s),
    }).collect()
}

fn index_path() -> PathBuf {
    let base = if cfg!(test) { None } else { dirs::cache_dir() };
    base.unwrap_or_else(std::env::temp_dir).join("maia/meaning.idx")
}

impl Engine {
    pub fn start(folders: Vec<PathBuf>, pictures: bool, ctx: &egui::Context) -> Engine {
        let shared = Arc::new(Mutex::new(Shared { phase: Phase::Checking, index: Arc::new(Index::default()), waiting: 0, pull: false }));
        let stop = Arc::new(AtomicBool::new(false));
        let wake = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let (s, st, w, ctx) = (shared.clone(), stop.clone(), wake.clone(), ctx.clone());
        std::thread::Builder::new().name("meaning".into()).spawn(move || run(folders, pictures, s, st, w, ctx)).ok();
        Engine { shared, stop, wake }
    }

    pub fn phase(&self) -> Phase {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).phase.clone()
    }

    pub fn index(&self) -> Arc<Index> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).index.clone()
    }

    /// Pictures not read yet because the laptop runs on its battery.
    pub fn waiting(&self) -> usize {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).waiting
    }

    /// Fetch the model now (the user said so), then index.
    pub fn download(&self) {
        self.shared.lock().unwrap_or_else(|e| e.into_inner()).pull = true;
        self.poke();
    }

    /// Look again: Ollama may have been started, or files changed.
    pub fn poke(&self) {
        let (m, cv) = &*self.wake;
        *m.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_one();
    }
}

fn set(shared: &Mutex<Shared>, ctx: &egui::Context, phase: Phase) {
    shared.lock().unwrap_or_else(|e| e.into_inner()).phase = phase;
    ctx.request_repaint();
}

/// The indexer: check Ollama, fetch the model when asked, bring the index up to date,
/// then sleep until poked (or a few minutes pass) and look again.
fn run(folders: Vec<PathBuf>, pictures: bool, shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>, wake: Arc<(Mutex<bool>, std::sync::Condvar)>, ctx: egui::Context) {
    // The lowest priority Linux has: only runs when nothing else wants the CPU.
    unsafe {
        let p = libc::sched_param { sched_priority: 0 };
        libc::sched_setscheduler(0, libc::SCHED_IDLE, &p);
    }
    // Last run's index, read here rather than on the window's thread.
    shared.lock().unwrap_or_else(|e| e.into_inner()).index = Arc::new(load(&index_path()));
    ctx.request_repaint();
    while !stop.load(Relaxed) {
        set(&shared, &ctx, Phase::Checking);
        let st = ollama::status();
        let want_pull = std::mem::take(&mut shared.lock().unwrap_or_else(|e| e.into_inner()).pull);
        match st {
            ollama::Status::NotRunning => set(&shared, &ctx, Phase::NotRunning),
            ollama::Status::NoModel if want_pull => {
                let r = ollama::pull(|done, all| set(&shared, &ctx, Phase::Downloading(done, all)), &stop);
                match r {
                    Ok(()) => continue,
                    Err(e) => set(&shared, &ctx, Phase::Failed(e)),
                }
            }
            ollama::Status::NoModel => set(&shared, &ctx, Phase::NoModel),
            ollama::Status::Ready => match update(&folders, pictures, &shared, &stop, &ctx) {
                Ok(()) => set(&shared, &ctx, Phase::Ready),
                Err(e) => set(&shared, &ctx, Phase::Failed(e)),
            },
        }
        // Sleep until poked; look again every few minutes for changed files.
        let (m, cv) = &*wake;
        let mut poked = m.lock().unwrap_or_else(|e| e.into_inner());
        if !*poked {
            poked = cv.wait_timeout(poked, std::time::Duration::from_secs(300)).map(|r| r.0).unwrap_or_else(|e| e.into_inner().0);
        }
        *poked = false;
    }
}

/// Bring the index up to date with the folders: read new and changed files, drop gone
/// ones. Saved as it goes, so an interrupted run keeps what it did.
fn update(folders: &[PathBuf], pictures: bool, shared: &Mutex<Shared>, stop: &AtomicBool, ctx: &egui::Context) -> Result<(), String> {
    let mut found = vec![];
    for f in folders {
        walk(f, pictures, &mut found, stop);
    }
    found.sort();
    found.dedup();
    // Text first (quick), then pictures newest first (slow, and the recent ones are
    // likelier to be looked for).
    found.sort_by_key(|(p, mtime, _)| if is_picture(p) { (1, -*mtime) } else { (0, 0) });
    let old = shared.lock().unwrap_or_else(|e| e.into_inner()).index.clone();
    let known: std::collections::HashMap<&Path, &FileRec> = old.files.iter().map(|f| (f.path.as_path(), f)).collect();
    let mut next = Index::default();
    let total = found.len();
    let mut saved = std::time::Instant::now();
    let mut changed = false;
    let mut copies: std::collections::HashMap<(std::ffi::OsString, u64), usize> = Default::default();
    let mut waiting = 0;
    for (k, (path, mtime, size)) in found.into_iter().enumerate() {
        if stop.load(Relaxed) {
            return Ok(());
        }
        if k % 20 == 0 {
            set(shared, ctx, Phase::Indexing(k, total));
        }
        // Unchanged since the last run: keep its passages as they are.
        if let Some(f) = known.get(path.as_path()).filter(|f| f.mtime == mtime && f.size == size) {
            let first = next.vecs.len() / DIMS;
            if is_picture(&path) {
                copies.entry((path.file_name().map(|n| n.to_os_string()).unwrap_or_default(), size)).or_insert(first);
            }
            next.vecs.extend_from_slice(&old.vecs[f.first * DIMS..(f.first + f.passages.len()) * DIMS]);
            next.files.push(FileRec { path, mtime, size, passages: f.passages.clone(), previews: f.previews.clone(), first });
            continue;
        }
        // A picture: one fingerprint for how it looks.
        if is_picture(&path) {
            // A copy of one already read (same name and size, as in a package unpacked
            // twice) shares its fingerprint.
            let key = (path.file_name().map(|n| n.to_os_string()).unwrap_or_default(), size);
            let e = match copies.get(&key) {
                Some(&at) => next.vecs[at * DIMS..(at + 1) * DIMS].to_vec(),
                // Pictures keep the CPU busy for seconds each: on battery they wait for
                // the charger (the next look, every few minutes, takes them up).
                None if on_battery() => {
                    waiting += 1;
                    continue;
                }
                None => {
                    let Some(bytes) = picture_bytes(&path) else { continue };
                    let Some(e) = ollama::embed_images(&[bytes])?.pop() else { continue };
                    e.iter().map(|x| (x * 127.0).round().clamp(-127.0, 127.0) as i8).collect()
                }
            };
            let first = next.vecs.len() / DIMS;
            copies.entry(key).or_insert(first);
            next.vecs.extend(e);
            next.files.push(FileRec { path, mtime, size, passages: vec![(0, 0)], previews: vec![String::new()], first });
            changed = true;
            if saved.elapsed().as_secs() >= 5 {
                saved = std::time::Instant::now();
                publish(shared, &next, ctx);
            }
            continue;
        }
        let Some(text) = read(&path, size) else { continue };
        // Number dumps, logs of values and tables of figures say nothing a meaning can find.
        let whole = prose(&path);
        let mut passages: Vec<(u32, u32)> = cut(if whole { &text } else { &text[..text.floor_char_boundary(PASSAGE * 3)] }).into_iter().filter(|&(a, n)| wordy(&text[a as usize..(a + n) as usize])).collect();
        if !whole {
            passages.truncate(1);
        }
        if passages.is_empty() {
            continue;
        }
        let title = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut vecs = Vec::with_capacity(passages.len() * DIMS);
        for batch in passages.chunks(BATCH) {
            let inputs: Vec<String> = batch.iter().map(|&(a, n)| format!("title: {title} | text: {}", &text[a as usize..(a + n) as usize])).collect();
            for e in ollama::embed(&inputs)? {
                vecs.extend(e.iter().map(|x| (x * 127.0).round().clamp(-127.0, 127.0) as i8));
            }
            if stop.load(Relaxed) {
                return Ok(());
            }
        }
        let previews = passages.iter().map(|&(a, n)| preview(&text[a as usize..(a + n) as usize])).collect();
        let first = next.vecs.len() / DIMS;
        next.vecs.extend(vecs);
        next.files.push(FileRec { path, mtime, size, passages, previews, first });
        changed = true;
        // Searchable (and saved) as it grows, a few times a minute at most.
        if saved.elapsed().as_secs() >= 5 {
            saved = std::time::Instant::now();
            publish(shared, &next, ctx);
        }
    }
    shared.lock().unwrap_or_else(|e| e.into_inner()).waiting = waiting;
    // Nothing new and nothing gone: the index stands as it is.
    if changed || next.files.len() != old.files.len() {
        publish(shared, &next, ctx);
    }
    Ok(())
}

/// Make `next` the index searches use, and save it.
fn publish(shared: &Mutex<Shared>, next: &Index, ctx: &egui::Context) {
    let copy = Index { files: next.files.iter().map(|f| FileRec { path: f.path.clone(), mtime: f.mtime, size: f.size, passages: f.passages.clone(), previews: f.previews.clone(), first: f.first }).collect(), vecs: next.vecs.clone() };
    let _ = save(&index_path(), &copy);
    shared.lock().unwrap_or_else(|e| e.into_inner()).index = Arc::new(copy);
    ctx.request_repaint();
}

/// Folders whose pictures belong to software (icons, cursors, app resources), not to
/// the person: their text is still read, their pictures not.
const SOFTWARE: &[&str] = &["icons", "cursors", "squashfs-root", "app.asar.unpacked", "site-packages", "emoji", "flags", "sprites", "pkg"];

/// Files worth reading under `dir`: text and PDFs, outside hidden and build folders, and
/// with `pictures` the pictures that are someone's (not icons, not video frames).
fn walk(dir: &Path, pictures: bool, out: &mut Vec<(PathBuf, i64, u64)>, stop: &AtomicBool) {
    let mut todo = vec![(dir.to_path_buf(), pictures)];
    while let Some((d, pictures)) = todo.pop() {
        if stop.load(Relaxed) || out.len() >= MAX_FILES {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let start = out.len();
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_dir() {
                if !matches!(name.as_ref(), "node_modules" | "target" | "__pycache__" | "venv" | "build" | "dist") {
                    todo.push((e.path(), pictures && !SOFTWARE.contains(&name.to_ascii_lowercase().as_str())));
                }
            } else if ft.is_file() {
                // Generated: lock files and minified bundles.
                if matches!(name.as_ref(), "package-lock.json" | "pnpm-lock.yaml" | "composer.lock") || name.contains(".min.") {
                    continue;
                }
                let ext = name.rsplit_once('.').map(|(_, x)| x.to_ascii_lowercase()).unwrap_or_default();
                let pdf = ext == "pdf";
                let picture = pictures && IMAGE_EXTS.contains(&ext.as_str());
                if !pdf && !picture && !TEXT_EXTS.contains(&ext.as_str()) {
                    continue;
                }
                let Ok(m) = e.metadata() else { continue };
                if m.len() == 0 || m.len() > if pdf { MAX_PDF } else if picture { MAX_IMAGE } else { MAX_TEXT } {
                    continue;
                }
                // A few kilobytes is an icon or a blank.
                if picture && m.len() < 6 << 10 {
                    continue;
                }
                use std::os::unix::fs::MetadataExt;
                out.push((e.path(), m.mtime(), m.len()));
            }
        }
        // Frames of a video: many pictures here, nearly all numbered.
        let here = &out[start..];
        let pics = here.iter().filter(|(p, ..)| is_picture(p)).count();
        if pics >= FRAME_RUN && here.iter().filter(|(p, ..)| is_picture(p) && numbered(p)).count() * 5 >= pics * 4 {
            let mut k = start;
            while k < out.len() {
                if is_picture(&out[k].0) { out.swap_remove(k); } else { k += 1; }
            }
        }
    }
}

#[cfg(test)]
pub static TEST_BATTERY: AtomicBool = AtomicBool::new(false);

/// Running on a battery that is draining (a desktop, or a laptop on its charger, isn't).
fn on_battery() -> bool {
    #[cfg(test)]
    return TEST_BATTERY.load(Relaxed);
    #[cfg(not(test))]
    std::fs::read_dir("/sys/class/power_supply").into_iter().flatten().flatten().any(|e| {
        let read = |f: &str| std::fs::read_to_string(e.path().join(f)).unwrap_or_default();
        read("type").trim() == "Battery" && read("status").trim() == "Discharging"
    })
}

/// "IMG_0042.png", "frame-0001.jpg", "17.png": a number of two or more digits in the name.
fn numbered(p: &Path) -> bool {
    let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    stem.as_bytes().windows(2).any(|w| w[0].is_ascii_digit() && w[1].is_ascii_digit())
}

/// A picture as the model takes it: decoded, shrunk to IMAGE_SIDE and written as PNG.
/// None for one too small to be more than an icon.
fn picture_bytes(path: &Path) -> Option<Vec<u8>> {
    let (img, [w, h]) = crate::imgdec::load(path, IMAGE_SIDE)?;
    if w.max(h) < MIN_PICTURE {
        return None;
    }
    crate::thumbcache::encode_png(&img, &[])
}

/// A file's text: as it is, or a PDF's through pdftotext when that is installed.
fn read(path: &Path, size: u64) -> Option<String> {
    if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("pdf")) {
        if !crate::media::have("pdftotext") {
            return None;
        }
        let mut c = crate::media::tool("pdftotext");
        c.args(["-q", "-enc", "UTF-8", "-l", "60"]).arg(path).arg("-");
        let out = crate::media::output(c, std::time::Duration::from_secs(60))?;
        return Some(String::from_utf8_lossy(&out).into_owned());
    }
    let _ = size;
    let bytes = std::fs::read(path).ok()?;
    let head = &bytes[..bytes.len().min(8192)];
    if bytes.len() as u64 > MAX_TEXT || head.contains(&0) {
        return None;
    }
    // Minified or generated code: lines thousands of characters long.
    if !prose(path) && head.split(|&b| b == b'\n').any(|l| l.len() > 1000) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// Passages of `text`: (start, length) in bytes, ending at a paragraph or line break
/// where one is near.
pub fn cut(text: &str) -> Vec<(u32, u32)> {
    let mut out = vec![];
    let mut at = 0;
    while at < text.len() && out.len() < MAX_PASSAGES {
        let mut end = text.floor_char_boundary((at + PASSAGE).min(text.len()));
        let w0 = text.floor_char_boundary(at + PASSAGE * 2 / 3);
        if end < text.len() && w0 > at && w0 < end {
            // Back to a paragraph break, else a line break, in the passage's last third.
            let window = &text[w0..end];
            if let Some(i) = window.rfind("\n\n").or_else(|| window.rfind('\n')).or_else(|| window.rfind(". ")) {
                end = w0 + i + 1;
            }
        }
        if text[at..end].trim().len() > 20 {
            out.push((at as u32, (end - at) as u32));
        }
        if end >= text.len() {
            break;
        }
        at = text.ceil_char_boundary(end.saturating_sub(OVERLAP).max(at + 1));
    }
    out
}

/// The best passages for `query`, one per file, best first. Meaning ranks; a question's
/// own words found in a file's name or in the passage add to it, so names, codes and
/// rare words still count as typed.
pub fn search(index: &Index, query: &str, limit: usize) -> Result<Vec<Hit>, String> {
    let q = ollama::embed(&[format!("task: search result | query: {query}")])?.pop().unwrap_or_default();
    let q8: Vec<i32> = q.iter().map(|x| (x * 127.0).round() as i32).collect();
    let words = terms(query);
    let cos = |f: &FileRec, p: usize| -> f32 {
        let v = &index.vecs[(f.first + p) * DIMS..(f.first + p + 1) * DIMS];
        v.iter().zip(&q8).map(|(&a, &b)| a as i32 * b).sum::<i32>() as f32 / (127.0 * 127.0)
    };
    let found = |hay: &str| -> usize {
        let hay = hay.to_lowercase();
        words.iter().filter(|w| hay.contains(w.as_str())).count()
    };
    // Every file by its closest passage and its name; then the leaders looked at closely,
    // their passages weighed with the question's words too.
    let mut files: Vec<(f32, usize)> = index
        .files
        .iter()
        .enumerate()
        .filter(|(_, f)| !f.passages.is_empty())
        .map(|(fi, f)| {
            let best = (0..f.passages.len()).map(|p| cos(f, p)).fold(f32::MIN, f32::max);
            let name = f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            (best + 0.08 * found(&name).min(3) as f32, fi)
        })
        .collect();
    files.sort_by(|a, b| b.0.total_cmp(&a.0));
    let mut hits: Vec<(f32, usize, usize)> = files
        .iter()
        .take(limit.max(60))
        .map(|&(_, fi)| {
            let f = &index.files[fi];
            let name = f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let named = 0.08 * found(&name).min(3) as f32;
            let (s, p) = (0..f.passages.len())
                .map(|p| (cos(f, p) + 0.03 * f.previews.get(p).map_or(0, |t| found(t)).min(4) as f32, p))
                .fold((f32::MIN, 0), |a, b| if b.0 > a.0 { b } else { a });
            (s + named, fi, p)
        })
        .collect();
    hits.sort_by(|a, b| b.0.total_cmp(&a.0));
    // The same picture saved in several places (or near copies of it) shown once: the
    // best placed one stands for the rest.
    let look = |fi: usize| &index.vecs[index.files[fi].first * DIMS..(index.files[fi].first + 1) * DIMS];
    let alike = |a: &[i8], b: &[i8]| {
        let dot = |x: &[i8], y: &[i8]| x.iter().zip(y).map(|(&p, &q)| p as i32 * q as i32).sum::<i32>() as f32;
        dot(a, b) >= 0.99 * (dot(a, a) * dot(b, b)).sqrt()
    };
    let mut shown: Vec<usize> = vec![];
    hits.retain(|&(_, fi, _)| {
        if !is_picture(&index.files[fi].path) {
            return true;
        }
        if shown.iter().any(|&o| alike(look(o), look(fi))) {
            return false;
        }
        shown.push(fi);
        true
    });
    Ok(hits.into_iter().take(limit).map(|(score, fi, p)| {
        let f = &index.files[fi];
        Hit { path: f.path.clone(), score, passage: f.previews.get(p).cloned().unwrap_or_default() }
    }).collect())
}

/// A passage's opening words on one line.
fn preview(text: &str) -> String {
    let mut out = String::new();
    for w in text.split_whitespace() {
        if out.len() > 420 {
            out.push('…');
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out
}

/// Mostly words, not numbers or symbols: half its visible characters letters, and a few
/// real words.
pub fn wordy(s: &str) -> bool {
    let (mut letters, mut visible) = (0usize, 0usize);
    for c in s.chars().filter(|c| !c.is_whitespace()) {
        visible += 1;
        if c.is_alphabetic() {
            letters += 1;
        }
    }
    let words = s.split_whitespace().filter(|w| w.chars().filter(|c| c.is_alphabetic()).count() >= 3).count();
    letters * 2 >= visible && words >= 6
}

/// The words of a question that are worth matching as written: lowercase, three letters
/// or more, without the small words every question has.
pub fn terms(query: &str) -> Vec<String> {
    const SKIP: &[&str] = &["the", "and", "for", "with", "from", "that", "this", "what", "how", "are", "was", "about", "into", "where", "when", "which", "file", "files", "some", "any", "find", "about", "there", "have", "has", "had", "its", "our", "your", "their", "was", "were", "who", "why", "can", "not", "but", "all"];
    let mut v: Vec<String> = query.split(|c: char| !c.is_alphanumeric()).filter(|w| w.chars().count() >= 3).map(str::to_lowercase).filter(|w| !SKIP.contains(&w.as_str())).collect();
    v.dedup();
    v
}

const MAGIC: &[u8; 8] = b"MAIAMNG4";

fn save(path: &Path, idx: &Index) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let mut b: Vec<u8> = Vec::with_capacity(idx.vecs.len() + idx.files.len() * 96);
    b.extend_from_slice(MAGIC);
    b.extend_from_slice(&(DIMS as u32).to_le_bytes());
    b.extend_from_slice(&(idx.files.len() as u32).to_le_bytes());
    for f in &idx.files {
        let p = f.path.as_os_str().as_bytes();
        b.extend_from_slice(&(p.len() as u32).to_le_bytes());
        b.extend_from_slice(p);
        b.extend_from_slice(&f.mtime.to_le_bytes());
        b.extend_from_slice(&f.size.to_le_bytes());
        b.extend_from_slice(&(f.passages.len() as u32).to_le_bytes());
        for &(a, n) in &f.passages {
            b.extend_from_slice(&a.to_le_bytes());
            b.extend_from_slice(&n.to_le_bytes());
        }
        b.extend(idx.vecs[f.first * DIMS..(f.first + f.passages.len()) * DIMS].iter().map(|&v| v as u8));
        for s in &f.previews {
            b.extend_from_slice(&(s.len() as u32).to_le_bytes());
            b.extend_from_slice(s.as_bytes());
        }
    }
    std::fs::create_dir_all(path.parent().unwrap_or(Path::new("/")))?;
    let tmp = path.with_extension("new");
    std::fs::write(&tmp, &b)?;
    std::fs::rename(&tmp, path)
}

/// The saved index, or an empty one when there is none (or it is from another version).
pub fn load(path: &Path) -> Index {
    use std::os::unix::ffi::OsStrExt;
    let Ok(b) = std::fs::read(path) else { return Index::default() };
    let mut at = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = b.get(at..at + n)?;
        at += n;
        Some(s)
    };
    let mut parse = || -> Option<Index> {
        if take(8)? != MAGIC || u32::from_le_bytes(take(4)?.try_into().ok()?) as usize != DIMS {
            return None;
        }
        let n = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        let mut idx = Index::default();
        for _ in 0..n {
            let pl = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            let path = PathBuf::from(std::ffi::OsStr::from_bytes(take(pl)?));
            let mtime = i64::from_le_bytes(take(8)?.try_into().ok()?);
            let size = u64::from_le_bytes(take(8)?.try_into().ok()?);
            let np = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
            let mut passages = Vec::with_capacity(np.min(MAX_PASSAGES));
            for _ in 0..np {
                let a = u32::from_le_bytes(take(4)?.try_into().ok()?);
                let l = u32::from_le_bytes(take(4)?.try_into().ok()?);
                passages.push((a, l));
            }
            let first = idx.vecs.len() / DIMS;
            idx.vecs.extend(take(np * DIMS)?.iter().map(|&v| v as i8));
            let mut previews = Vec::with_capacity(np.min(MAX_PASSAGES));
            for _ in 0..np {
                let l = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
                previews.push(String::from_utf8_lossy(take(l)?).into_owned());
            }
            idx.files.push(FileRec { path, mtime, size, passages, previews, first });
        }
        Some(idx)
    };
    parse().unwrap_or_default()
}
