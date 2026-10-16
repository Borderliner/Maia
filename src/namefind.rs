//! Finding files by name anywhere in the home folder (Ctrl+K). The folders are walked on
//! a thread when the search opens and the names held only while it is open; each
//! keystroke then looks through all of them, a few hundred thousand in milliseconds.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};

/// Never walked into: what tools make, not what people name.
const SKIP: &[&str] = &["node_modules", "target", "__pycache__", "venv", "build", "dist"];

struct Item {
    /// Index into `Found::dirs`.
    dir: u32,
    name: Box<str>,
    is_dir: bool,
}

#[derive(Default)]
struct Found {
    dirs: Vec<PathBuf>,
    items: Vec<Item>,
}

/// The names under a folder, gathered on a thread; dropping it stops the walk.
pub struct Names {
    found: Arc<Mutex<Found>>,
    done: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

impl Drop for Names {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

impl Names {
    pub fn start(root: PathBuf, ctx: &egui::Context) -> Names {
        let found = Arc::new(Mutex::new(Found::default()));
        let (done, stop) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (f, d, s, ctx) = (found.clone(), done.clone(), stop.clone(), ctx.clone());
        std::thread::Builder::new().name("names".into()).spawn(move || {
            walk(&root, &f, &s, &ctx);
            d.store(true, Relaxed);
            ctx.request_repaint();
        }).ok();
        Names { found, done, stop }
    }

    /// Still walking: more may turn up.
    pub fn walking(&self) -> bool {
        !self.done.load(Relaxed)
    }

    /// How many names are in so far.
    pub fn len(&self) -> usize {
        self.found.lock().unwrap_or_else(|e| e.into_inner()).items.len()
    }

    /// The best `limit` matches for `query`: every word in the name (or, past the first,
    /// in its folders), the closest and shortest names first. Paths, and whether each
    /// is a folder.
    pub fn find(&self, query: &str, limit: usize) -> Vec<(PathBuf, bool)> {
        let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        if words.is_empty() {
            return vec![];
        }
        let whole = words.join(" ");
        let found = self.found.lock().unwrap_or_else(|e| e.into_inner());
        // Which words each folder's path has, worked out once per folder.
        let mut in_dir: Vec<Option<u64>> = vec![None; found.dirs.len()];
        let mut best: Vec<(i32, usize)> = vec![];
        for (i, it) in found.items.iter().enumerate() {
            let mut score = 0i32;
            let mut named = 0;
            let mut ok = true;
            for (w, word) in words.iter().enumerate() {
                match find_ci(&it.name, word) {
                    Some(at) => {
                        named += 1;
                        score += 40;
                        if at == 0 {
                            score += 120;
                        } else if it.name.as_bytes().get(at - 1).is_some_and(|b| !b.is_ascii_alphanumeric()) {
                            score += 50;
                        }
                    }
                    None => {
                        let mask = *in_dir[it.dir as usize].get_or_insert_with(|| {
                            let d = found.dirs[it.dir as usize].to_string_lossy();
                            words.iter().enumerate().filter(|(_, w)| find_ci(&d, w).is_some()).fold(0u64, |m, (k, _)| m | 1 << k.min(63))
                        });
                        if mask & 1 << w.min(63) == 0 {
                            ok = false;
                            break;
                        }
                    }
                }
            }
            if !ok || named == 0 {
                continue;
            }
            if it.name.len() == whole.len() && find_ci(&it.name, &whole) == Some(0) {
                score += 1000;
            }
            score -= it.name.chars().count().min(200) as i32;
            if best.len() < limit || score > best[best.len() - 1].0 {
                let at = best.partition_point(|b| b.0 >= score);
                best.insert(at, (score, i));
                best.truncate(limit);
            }
        }
        best.into_iter().map(|(_, i)| {
            let it = &found.items[i];
            (found.dirs[it.dir as usize].join(&*it.name), it.is_dir)
        }).collect()
    }
}

/// Where `needle` (lowercase) is in `hay`, ignoring case.
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    if hay.is_ascii() {
        let n = needle.as_bytes();
        return hay.as_bytes().windows(n.len()).position(|w| w.eq_ignore_ascii_case(n));
    }
    hay.to_lowercase().find(needle)
}

/// Every name under `root`, outside hidden and build folders and other drives, handed
/// over a folder at a time so a search can start before the walk ends.
fn walk(root: &Path, found: &Mutex<Found>, stop: &AtomicBool, ctx: &egui::Context) {
    use std::os::unix::fs::MetadataExt;
    let Ok(dev) = std::fs::metadata(root).map(|m| m.dev()) else { return };
    let mut todo = vec![root.to_path_buf()];
    let mut shown = std::time::Instant::now();
    while let Some(d) = todo.pop() {
        if stop.load(Relaxed) {
            return;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        let mut here = vec![];
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = e.file_type() else { continue };
            let is_dir = ft.is_dir();
            if is_dir && !SKIP.contains(&name.as_str()) && e.metadata().is_ok_and(|m| m.dev() == dev) {
                todo.push(e.path());
            }
            here.push((name, is_dir));
        }
        if here.is_empty() {
            continue;
        }
        let mut f = found.lock().unwrap_or_else(|e| e.into_inner());
        let dir = f.dirs.len() as u32;
        f.dirs.push(d);
        f.items.extend(here.into_iter().map(|(name, is_dir)| Item { dir, name: name.into(), is_dir }));
        drop(f);
        if shown.elapsed().as_millis() >= 100 {
            shown = std::time::Instant::now();
            ctx.request_repaint();
        }
    }
}
