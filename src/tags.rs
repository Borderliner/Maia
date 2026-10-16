//! Tags and colour labels, kept on the files themselves in the `user.xdg.tags` extended
//! attribute (comma-separated, as KDE's file managers write it), so they travel with the
//! files and other apps see them. A colour label is a tag named after one of seven
//! colours, drawn as a dot.
//!
//! Reading an attribute costs several times a stat, so tags are never read while
//! listing: rows ask for theirs when drawn, and a `tag:` filter asks for every row; a
//! worker reads them and the cache keeps only files that have tags.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock, mpsc};

use egui::Color32;

const ATTR: &[u8] = b"user.xdg.tags\0";

pub const COLORS: [(&str, Color32); 7] = [
    ("Red", Color32::from_rgb(0xe5, 0x48, 0x4d)),
    ("Orange", Color32::from_rgb(0xf0, 0x8a, 0x24)),
    ("Yellow", Color32::from_rgb(0xe8, 0xc1, 0x2f)),
    ("Green", Color32::from_rgb(0x46, 0xb3, 0x5c)),
    ("Blue", Color32::from_rgb(0x3b, 0x8e, 0xea)),
    ("Purple", Color32::from_rgb(0xa1, 0x5f, 0xd6)),
    ("Gray", Color32::from_rgb(0x8e, 0x8e, 0x93)),
];

/// The colour of a colour label ("red", "Red": any case).
pub fn color_of(tag: &str) -> Option<Color32> {
    COLORS.iter().find(|(n, _)| n.eq_ignore_ascii_case(tag)).map(|(_, c)| *c)
}

/// "Red,work, taxes" as tags, in their order, without blanks or repeats.
pub fn parse(v: &str) -> Vec<String> {
    let mut out: Vec<String> = vec![];
    for t in v.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        if !out.iter().any(|o| o == t) {
            out.push(t.to_string());
        }
    }
    out
}

fn cpath(p: &Path) -> Option<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::CString::new(p.as_os_str().as_bytes()).ok()
}

/// A file's tags (none when it has no attribute or its filesystem has none).
pub fn read(p: &Path) -> Vec<String> {
    let Some(c) = cpath(p) else { return vec![] };
    let mut buf = [0u8; 1024];
    let n = unsafe { libc::getxattr(c.as_ptr(), ATTR.as_ptr().cast(), buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 {
        return vec![];
    }
    parse(&String::from_utf8_lossy(&buf[..n as usize]))
}

/// Give `p` exactly these tags (no tags removes the attribute).
pub fn write(p: &Path, tags: &[String]) -> std::io::Result<()> {
    let c = cpath(p).ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let r = if tags.is_empty() {
        let r = unsafe { libc::removexattr(c.as_ptr(), ATTR.as_ptr().cast()) };
        if r != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENODATA) { 0 } else { r }
    } else {
        let v = tags.join(",");
        unsafe { libc::setxattr(c.as_ptr(), ATTR.as_ptr().cast(), v.as_ptr().cast(), v.len(), 0) }
    };
    if r == 0 {
        set_cached(p, tags.to_vec());
        Ok(())
    } else {
        let e = std::io::Error::last_os_error();
        Err(if e.raw_os_error() == Some(libc::ENOTSUP) { std::io::Error::other("This drive can't keep tags") } else { e })
    }
}

/// Add or remove one tag on every file; the first failure is returned.
pub fn toggle(paths: &[PathBuf], tag: &str, on: bool) -> Result<(), String> {
    let mut first = None;
    for p in paths {
        let mut t = read(p);
        let had = t.iter().any(|x| x.eq_ignore_ascii_case(tag));
        if on && !had {
            t.push(tag.to_string());
        } else if !on && had {
            t.retain(|x| !x.eq_ignore_ascii_case(tag));
        } else {
            continue;
        }
        if let Err(e) = write(p, &t) {
            first.get_or_insert(format!("{}: {e}", p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
        }
    }
    first.map_or(Ok(()), Err)
}

// ------------------------------------------------------------------ cache

#[derive(Default)]
struct Cache {
    /// Files known to have tags.
    tagged: HashMap<PathBuf, Vec<String>>,
    /// Files read and found to have none (hashes: 8 bytes each, for big folders).
    bare: HashSet<u64>,
    /// Asked for and not back yet.
    pending: HashSet<u64>,
    /// Bumped when anything lands, so views know to look again.
    generation: u64,
}

static CACHE: RwLock<Option<Cache>> = RwLock::new(None);
static QUEUE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static WORKER: Mutex<Option<mpsc::Sender<Vec<PathBuf>>>> = Mutex::new(None);

/// A path's hash, the same in every thread and call (unseeded).
fn key(p: &Path) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    h.finish()
}

/// What is known of `p`'s tags: Some (maybe empty) once read, None while not yet read
/// (it is then queued, for `flush` to send to the worker).
pub fn get<R>(p: &Path, f: impl FnOnce(&[String]) -> R) -> Option<R> {
    let k = key(p);
    {
        let g = CACHE.read().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = g.as_ref() {
            if let Some(t) = c.tagged.get(p) {
                return Some(f(t));
            }
            if c.bare.contains(&k) {
                return Some(f(&[]));
            }
            if c.pending.contains(&k) {
                return None;
            }
        }
    }
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(Cache::default).pending.insert(k);
    QUEUE.lock().unwrap_or_else(|e| e.into_inner()).push(p.to_path_buf());
    None
}

fn set_cached(p: &Path, tags: Vec<String>) {
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    let c = g.get_or_insert_with(Cache::default);
    let k = key(p);
    c.pending.remove(&k);
    if tags.is_empty() {
        c.tagged.remove(p);
        c.bare.insert(k);
    } else {
        c.bare.remove(&k);
        c.tagged.insert(p.to_path_buf(), tags);
    }
    c.generation += 1;
}

/// Forget what is known of `p` (it changed on disk), so it is read again.
pub fn invalidate(p: &Path) {
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = g.as_mut() {
        let k = key(p);
        if c.tagged.remove(p).is_some() | c.bare.remove(&k) {
            c.generation += 1;
        }
    }
}

pub fn generation() -> u64 {
    CACHE.read().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |c| c.generation)
}

/// Every tag in use among the files read so far (for the menu), sorted.
pub fn known() -> Vec<String> {
    let g = CACHE.read().unwrap_or_else(|e| e.into_inner());
    let mut v: Vec<String> = g.as_ref().map(|c| c.tagged.values().flatten().filter(|t| color_of(t).is_none()).cloned().collect::<HashSet<_>>().into_iter().collect()).unwrap_or_default();
    v.sort_by_key(|t| t.to_lowercase());
    v
}

/// Send what was asked for this frame to the worker (started on first use).
pub fn flush(ctx: &egui::Context) {
    let batch = std::mem::take(&mut *QUEUE.lock().unwrap_or_else(|e| e.into_inner()));
    if batch.is_empty() {
        return;
    }
    let mut w = WORKER.lock().unwrap_or_else(|e| e.into_inner());
    let tx = w.get_or_insert_with(|| {
        let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            while let Ok(batch) = rx.recv() {
                for chunk in batch.chunks(2000) {
                    let got: Vec<(PathBuf, Vec<String>)> = chunk.iter().map(|p| (p.clone(), read(p))).collect();
                    for (p, t) in got {
                        set_cached(&p, t);
                    }
                    ctx.request_repaint();
                }
            }
        });
        tx
    });
    let _ = tx.send(batch);
}
