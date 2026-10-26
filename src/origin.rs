//! Where a download came from: browsers write the address it was fetched from in the
//! `user.xdg.origin.url` extended attribute, and the page that linked to it in
//! `user.xdg.referrer.url`. As with tags, reading an attribute costs several times a
//! stat, so rows ask when drawn (and every row when the list groups by origin), a worker
//! reads them, and the cache keeps only files that have one.

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock, mpsc};

const URL: &[u8] = b"user.xdg.origin.url\0";
const PAGE: &[u8] = b"user.xdg.referrer.url\0";

/// The group for files no browser marked.
pub const UNKNOWN: &str = "Unknown";
/// The group folders go in, ahead of the sites.
pub const FOLDERS: &str = "Folders";

#[derive(Debug, PartialEq, Eq)]
pub struct Origin {
    /// The address it was fetched from, when written.
    pub url: Option<String>,
    /// The page that linked to it, when written.
    pub page: Option<String>,
}

impl Origin {
    /// The site it is grouped under: the address's host, else the page's.
    pub fn host(&self) -> Option<String> {
        self.url.as_deref().and_then(host).or_else(|| self.page.as_deref().and_then(host))
    }
}

fn attr(c: &std::ffi::CStr, name: &[u8]) -> Option<String> {
    let mut buf = vec![0u8; 4096];
    let n = unsafe { libc::getxattr(c.as_ptr(), name.as_ptr().cast(), buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    let s = String::from_utf8_lossy(&buf).trim_end_matches('\0').trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// What the browser wrote on `p`, if anything.
pub fn read(p: &Path) -> Option<Origin> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(p.as_os_str().as_bytes()).ok()?;
    let (url, page) = (attr(&c, URL), attr(&c, PAGE));
    (url.is_some() || page.is_some()).then_some(Origin { url, page })
}

/// The host of an address, without "www.": "https://www.example.com:8080/a" is
/// "example.com". Addresses a page made itself ("blob:https://…") count as the page's.
pub fn host(url: &str) -> Option<String> {
    let u = url.strip_prefix("blob:").unwrap_or(url);
    let (_, rest) = u.split_once("://")?;
    let auth = rest.split(['/', '?', '#']).next()?;
    let auth = auth.rsplit_once('@').map_or(auth, |x| x.1);
    let h = if auth.starts_with('[') { auth.split(']').next().map(|x| format!("{x}]"))? } else { auth.split(':').next()?.to_string() };
    let h = h.to_lowercase();
    let h = h.strip_prefix("www.").map(str::to_string).unwrap_or(h);
    (!h.is_empty()).then_some(h)
}

/// An address to read at a glance: host and path, without the scheme, the query or the
/// fragment ("example.com/files/report.pdf").
pub fn short(url: &str) -> String {
    let u = url.strip_prefix("blob:").unwrap_or(url);
    let rest = u.split_once("://").map_or(u, |x| x.1);
    let rest = rest.split(['?', '#']).next().unwrap_or(rest);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    rest.trim_end_matches('/').to_string()
}

/// Whether the address may be handed to the browser: a web address, not a file or a
/// script someone wrote into the attribute.
pub fn openable(url: &str) -> bool {
    let l = url.to_ascii_lowercase();
    l.starts_with("https://") || l.starts_with("http://")
}

// ------------------------------------------------------------------ cache

#[derive(Default)]
struct Cache {
    marked: HashMap<PathBuf, Arc<Origin>>,
    /// Read and found unmarked (hashes: big folders are mostly these).
    bare: HashSet<u64>,
    pending: HashSet<u64>,
    generation: u64,
}

static CACHE: RwLock<Option<Cache>> = RwLock::new(None);
static QUEUE: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static WORKER: Mutex<Option<mpsc::Sender<Vec<PathBuf>>>> = Mutex::new(None);
/// Past this many marked files the cache starts again.
const MOST: usize = 100_000;

fn key(p: &Path) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    p.hash(&mut h);
    h.finish()
}

/// What is known of `p`: Some (None inside when unmarked) once read, None while it is
/// being read (it is then queued, for `flush`).
pub fn get(p: &Path) -> Option<Option<Arc<Origin>>> {
    let k = key(p);
    {
        let g = CACHE.read().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = g.as_ref() {
            if let Some(o) = c.marked.get(p) {
                return Some(Some(o.clone()));
            }
            if c.bare.contains(&k) {
                return Some(None);
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

fn land(p: PathBuf, o: Option<Origin>) {
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    let c = g.get_or_insert_with(Cache::default);
    let k = key(&p);
    c.pending.remove(&k);
    match o {
        Some(o) => {
            if c.marked.len() >= MOST {
                c.marked.clear();
            }
            c.bare.remove(&k);
            c.marked.insert(p, Arc::new(o));
        }
        None => {
            c.marked.remove(&p);
            c.bare.insert(k);
        }
    }
    c.generation += 1;
}

/// Forget `p` (it changed on disk): read again when next asked.
pub fn invalidate(p: &Path) {
    let mut g = CACHE.write().unwrap_or_else(|e| e.into_inner());
    if let Some(c) = g.as_mut() {
        if c.marked.remove(p).is_some() | c.bare.remove(&key(p)) {
            c.generation += 1;
        }
    }
}

pub fn generation() -> u64 {
    CACHE.read().unwrap_or_else(|e| e.into_inner()).as_ref().map_or(0, |c| c.generation)
}

/// Send what rows asked for this frame to the worker (started on first use).
pub fn flush(ctx: &egui::Context) {
    let batch = std::mem::take(&mut *QUEUE.lock().unwrap_or_else(|e| e.into_inner()));
    if batch.is_empty() {
        return;
    }
    let mut w = WORKER.lock().unwrap_or_else(|e| e.into_inner());
    let tx = w.get_or_insert_with(|| {
        let (tx, rx) = mpsc::channel::<Vec<PathBuf>>();
        let ctx = ctx.clone();
        let _ = std::thread::Builder::new().name("origins".into()).spawn(move || {
            while let Ok(batch) = rx.recv() {
                for chunk in batch.chunks(2000) {
                    let got: Vec<(PathBuf, Option<Origin>)> = chunk.iter().map(|p| (p.clone(), read(p))).collect();
                    for (p, o) in got {
                        land(p, o);
                    }
                    ctx.request_repaint();
                }
            }
        });
        tx
    });
    let _ = tx.send(batch);
}

/// The group an item goes in under Group By Origin: folders together, then each site,
/// then what no browser marked (or what isn't read yet).
pub fn group(e: &crate::fsx::Entry) -> String {
    if e.is_dir {
        return FOLDERS.into();
    }
    get(&e.path).flatten().and_then(|o| o.host()).unwrap_or_else(|| UNKNOWN.into())
}

pub fn rank(label: &str) -> usize {
    match label {
        FOLDERS => 0,
        UNKNOWN => 99,
        _ => 1,
    }
}
