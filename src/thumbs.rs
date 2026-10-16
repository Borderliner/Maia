//! Thumbnail & preview image loading on worker threads, with a bounded texture cache.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime};

use egui::{self, ColorImage, TextureHandle, TextureOptions};

pub fn is_image_ext(ext: &str) -> bool {
    matches!(ext, "png" | "jpg" | "jpeg" | "gif" | "bmp" | "webp" | "ico")
}

/// Whether a thumbnail can be made for this item: built-in image formats, SVG (own
/// renderer) or anything an installed desktop thumbnailer handles (video, PDF, HEIF,
/// JPEG XL, office documents, audio covers…).
pub fn can_thumb(e: &crate::fsx::Entry) -> bool {
    if e.is_dir || e.special {
        return false;
    }
    let ext = e.ext();
    is_image_ext(ext) || ext == "svg" || crate::thumbcache::thumbnailer_for(&crate::mime::mime_db().for_name(&e.name, false, false)).is_some()
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
struct Key {
    path: PathBuf,
    size: u32,
    mtime: Option<SystemTime>,
}

pub struct Loaded {
    pub tex: TextureHandle,
    pub at: Instant,
    /// Original pixel dimensions.
    pub dims: [u32; 2],
}

enum Slot {
    Pending,
    Failed,
    Ready(Loaded),
}

struct Job {
    key: Key,
}

struct Done {
    key: Key,
    result: Option<(ColorImage, [u32; 2])>,
}

pub struct Thumbs {
    slots: HashMap<Key, Slot>,
    /// Latest ready key per path, for showing a different size while loading.
    ready_by_path: HashMap<PathBuf, Key>,
    failed: std::collections::HashSet<PathBuf>,
    order: VecDeque<Key>,
    queue: Arc<Mutex<VecDeque<Job>>>,
    rx: Receiver<Done>,
    wake: Sender<()>,
    pub budget_bytes: usize,
    used_bytes: usize,
}

impl Thumbs {
    pub fn new(ctx: egui::Context, workers: usize) -> Thumbs {
        let (tx, rx) = channel::<Done>();
        let queue: Arc<Mutex<VecDeque<Job>>> = Arc::new(Mutex::new(VecDeque::new()));
        let (wake_tx, wake_rx) = channel::<()>();
        let wake_rx = Arc::new(Mutex::new(wake_rx));
        for _ in 0..workers.max(1) {
            let q = queue.clone();
            let tx = tx.clone();
            let ctx = ctx.clone();
            let wake_rx = wake_rx.clone();
            std::thread::spawn(move || {
                loop {
                    // LIFO: the most recently requested (visible) items first.
                    let job = q.lock().unwrap().pop_back();
                    match job {
                        Some(j) => {
                            let result = decode(&j.key.path, j.key.size, j.key.mtime);
                            if tx.send(Done { key: j.key, result }).is_err() {
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
        Thumbs { slots: HashMap::new(), ready_by_path: HashMap::new(), failed: Default::default(), order: VecDeque::new(), queue, rx, wake: wake_tx, budget_bytes: 256 << 20, used_bytes: 0 }
    }

    /// Request an image at a size bucket. Returns it when ready.
    pub fn get(&mut self, path: &Path, mtime: Option<SystemTime>, want: f32) -> Option<&Loaded> {
        let size = bucket(want);
        let key = Key { path: path.to_path_buf(), size, mtime };
        if !self.slots.contains_key(&key) {
            self.slots.insert(key.clone(), Slot::Pending);
            let mut q = self.queue.lock().unwrap();
            q.push_back(Job { key: key.clone() });
            // Don't let stale requests pile up while scrolling fast.
            while q.len() > 512 {
                if let Some(old) = q.pop_front() {
                    self.slots.remove(&old.key);
                }
            }
            drop(q);
            let _ = self.wake.send(());
        }
        if let Some(Slot::Ready(_)) = self.slots.get(&key) {
            if let Some(Slot::Ready(l)) = self.slots.get(&key) {
                return Some(l);
            }
        }
        // Fall back to another ready size of the same file while loading.
        let alt = self.ready_by_path.get(path).filter(|k| k.mtime == mtime)?;
        match self.slots.get(alt) {
            Some(Slot::Ready(l)) => Some(l),
            _ => None,
        }
    }

    pub fn failed(&self, path: &Path) -> bool {
        self.failed.contains(path)
    }

    /// Upload finished decodes (a few per frame to keep frames smooth). True when any landed.
    pub fn pump(&mut self, ctx: &egui::Context) -> bool {
        let mut n = 0;
        while let Ok(d) = self.rx.try_recv() {
            match d.result {
                Some((img, dims)) => {
                    let bytes = img.width() * img.height() * 4;
                    let tex = ctx.load_texture(d.key.path.to_string_lossy(), img, TextureOptions::LINEAR);
                    self.used_bytes += bytes;
                    self.order.push_back(d.key.clone());
                    self.ready_by_path.insert(d.key.path.clone(), d.key.clone());
                    self.slots.insert(d.key, Slot::Ready(Loaded { tex, at: Instant::now(), dims }));
                }
                None => {
                    self.failed.insert(d.key.path.clone());
                    self.slots.insert(d.key, Slot::Failed);
                }
            }
            n += 1;
            if n >= 24 {
                ctx.request_repaint();
                break;
            }
        }
        while self.used_bytes > self.budget_bytes {
            let Some(k) = self.order.pop_front() else { break };
            if self.ready_by_path.get(&k.path) == Some(&k) {
                self.ready_by_path.remove(&k.path);
            }
            if let Some(Slot::Ready(l)) = self.slots.remove(&k) {
                let [w, h] = l.tex.size();
                self.used_bytes = self.used_bytes.saturating_sub(w * h * 4);
            }
        }
        n > 0
    }

    pub fn invalidate(&mut self, path: &Path) {
        self.slots.retain(|k, _| k.path != path);
        self.ready_by_path.remove(path);
        self.failed.remove(path);
    }
}

fn bucket(want: f32) -> u32 {
    for b in [64, 128, 256, 512, 1024, 2048, 4096] {
        if want <= b as f32 {
            return b;
        }
    }
    4096
}

/// Thumbnail pipeline for one request: the shared cache, then the built-in decoders (and
/// SVG renderer), then an installed thumbnailer. New thumbnails up to 1024 px go back to
/// the shared cache; failures are remembered (until the file changes) so nothing is
/// retried every frame.
fn decode(path: &Path, max: u32, mtime: Option<SystemTime>) -> Option<(ColorImage, [u32; 2])> {
    use crate::thumbcache as tc;
    let cacheable = max <= 1024;
    if cacheable {
        if let Some((img, dims)) = tc::lookup(path, mtime, max) {
            let dims = dims.unwrap_or([img.w, img.h]);
            return Some((to_color(img), dims));
        }
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let flavor_px = tc::flavor(max).1;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let made: Option<(crate::imgdec::Rgba, [u32; 2])> = if is_image_ext(&ext) {
        crate::imgdec::load(path, if cacheable { flavor_px } else { max })
    } else if ext == "svg" {
        svg_thumb(path, if cacheable { flavor_px } else { max })
    } else if tc::has_failed(path, mtime) {
        None
    } else {
        let mime = crate::mime::mime_db().for_name(&name, false, false);
        let t = tc::thumbnailer_for(&mime)?;
        let img = tc::run_external(t, path, flavor_px.min(1024), std::time::Duration::from_secs(12));
        if img.is_none() {
            tc::mark_failed(path, mtime);
        }
        img.map(|i| {
            let i = if i.w > flavor_px || i.h > flavor_px { crate::imgdec::downscale(&i, flavor_px) } else { i };
            let d = [i.w, i.h];
            (i, d)
        })
    };
    let (img, dims) = made?;
    if cacheable {
        tc::store(path, mtime, max, &img, meta.len(), Some(dims));
    }
    // The cache flavour can be larger than this request: scale to what was asked.
    let img = if img.w > max || img.h > max { crate::imgdec::downscale(&img, max) } else { img };
    Some((to_color(img), dims))
}

fn to_color(img: crate::imgdec::Rgba) -> ColorImage {
    ColorImage::from_rgba_unmultiplied([img.w as usize, img.h as usize], &img.px)
}

/// SVG through Maia's own renderer (premultiplied output, unpremultiplied here).
fn svg_thumb(path: &Path, px: u32) -> Option<(crate::imgdec::Rgba, [u32; 2])> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > 16 << 20 {
        return None;
    }
    let mut data = std::fs::read(path).ok()?;
    data.truncate(16 << 20);
    let (w, h, mut px_data) = crate::svg::render(&data, px)?;
    for p in px_data.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a > 0 && a < 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    // Its dimensions are the document's own (64 × 48), not the drawing's (2048 × 1536).
    let dims = crate::svg::size(&data).unwrap_or([w, h]);
    Some((crate::imgdec::Rgba { w, h, px: px_data }, dims))
}
