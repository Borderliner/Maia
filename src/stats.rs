//! Background recursive folder statistics (size, file and folder counts).

use std::collections::{HashMap, HashSet, VecDeque};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};


#[derive(Clone, Copy, Debug, Default)]
pub struct DirStats {
    pub size: u64,
    pub files: u64,
    pub folders: u64,
}

pub struct Stats {
    pub map: HashMap<PathBuf, DirStats>,
    pending: HashSet<PathBuf>,
    queue: Arc<Mutex<VecDeque<PathBuf>>>,
    rx: Receiver<(PathBuf, DirStats)>,
    wake: Sender<()>,
    /// Bumped to cancel in-flight work (e.g. after many invalidations).
    generation: Arc<AtomicU64>,
}

impl Stats {
    pub fn new(ctx: egui::Context) -> Stats {
        let (tx, rx) = channel();
        let (wake_tx, wake_rx) = channel::<()>();
        let wake_rx = Arc::new(Mutex::new(wake_rx));
        let queue: Arc<Mutex<VecDeque<PathBuf>>> = Arc::new(Mutex::new(VecDeque::new()));
        let generation = Arc::new(AtomicU64::new(0));
        for _ in 0..2 {
            let (q, tx, ctx, wake_rx, gen_) = (queue.clone(), tx.clone(), ctx.clone(), wake_rx.clone(), generation.clone());
            std::thread::spawn(move || {
                loop {
                    let job = q.lock().unwrap().pop_front();
                    match job {
                        Some(p) => {
                            let g = gen_.load(Ordering::Relaxed);
                            let s = compute(&p, &gen_, g);
                            if gen_.load(Ordering::Relaxed) != g {
                                continue;
                            }
                            if tx.send((p, s)).is_err() {
                                return;
                            }
                            ctx.request_repaint();
                        }
                        None => {
                            if wake_rx.lock().unwrap().recv().is_err() {
                                return;
                            }
                        }
                    }
                }
            });
        }
        Stats { map: HashMap::new(), pending: HashSet::new(), queue, rx, wake: wake_tx, generation }
    }

    pub fn allowed(&mut self, cfg: &crate::config::Config, path: &Path, ctx: &egui::Context) -> bool {
        cfg.folder_sizes && cfg.sizes_on(crate::fsx::drive_kind_cached(path, ctx))
    }

    /// No folder scans queued or running.
    pub fn idle(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn request(&mut self, p: &Path) {
        if self.map.contains_key(p) || self.pending.contains(p) {
            return;
        }
        self.pending.insert(p.to_path_buf());
        self.queue.lock().unwrap().push_back(p.to_path_buf());
        let _ = self.wake.send(());
    }

    pub fn pump(&mut self) -> bool {
        let mut changed = false;
        while let Ok((p, s)) = self.rx.try_recv() {
            self.pending.remove(&p);
            self.map.insert(p, s);
            changed = true;
        }
        changed
    }

    /// Forget stats for `p` and its ancestors (contents changed).
    pub fn invalidate(&mut self, p: &Path) {
        let mut cur = Some(p);
        while let Some(c) = cur {
            self.map.remove(c);
            cur = c.parent();
        }
    }
}

pub(crate) fn compute(root: &Path, gen_: &AtomicU64, g: u64) -> DirStats {
    let mut s = DirStats::default();
    if crate::fsx::is_virtual_fs(root) {
        return s;
    }
    let Ok(rm) = std::fs::symlink_metadata(root) else { return s };
    let dev = rm.dev();
    let mut seen_inodes: HashSet<u64> = HashSet::new();
    let mut stack = vec![root.to_path_buf()];
    let mut n = 0u32;
    while let Some(d) = stack.pop() {
        n += 1;
        if n % 256 == 0 && gen_.load(Ordering::Relaxed) != g {
            return s;
        }
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for de in rd.flatten() {
            let Ok(m) = de.metadata() else { continue }; // does not follow symlinks
            if m.is_dir() {
                if m.dev() == dev {
                    s.folders += 1;
                    stack.push(de.path());
                }
            } else {
                s.files += 1;
                if m.nlink() > 1 && !seen_inodes.insert(m.ino()) {
                    continue;
                }
                s.size += m.len();
            }
        }
    }
    s
}

