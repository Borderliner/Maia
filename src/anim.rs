//! Animated pictures (GIF, APNG, animated WebP) played in the inspector and the viewer.
//! A worker composites the frames, scales them to the size shown and hands them over a
//! few ahead of when they are due; they reach the screen through one texture. While the
//! whole animation fits a few dozen megabytes it is kept and loops from memory; a longer
//! one is decoded again each time round, holding only the next couple of frames. Nothing
//! ticks while it is paused or out of sight: the worker waits on its full queue and the
//! screen repaints only when the next frame is due.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::time::{Duration, SystemTime};

use egui::{ColorImage, TextureHandle, TextureOptions};

use crate::imgdec::Rgba;

/// Pictures that may move.
pub fn may_animate(ext: &str) -> bool {
    matches!(ext, "gif" | "png" | "webp")
}

/// How long a frame stays: the very short ones (often 0, "as fast as you can") a tenth of
/// a second, as browsers show them.
pub fn delay_ms(ms: u32) -> u32 {
    if ms < 20 { 100 } else { ms }
}

/// Largest canvas played (pixels): beyond it the still first frame shows.
const MAX_PIXELS: u64 = 32 << 20;
/// Frames are scaled to fit the view, but never past this many pixels a side.
const MAX_SIDE: u32 = 2048;
/// Frames decoded ahead of the one shown.
const AHEAD: usize = 2;

/// The memory an animation's frames may keep, out of the asset cache's.
pub fn budget(asset_bytes: usize) -> usize {
    (asset_bytes / 4).min(64 << 20)
}

#[derive(Clone, Copy)]
struct Area {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

/// What becomes of a frame's area before the next is drawn.
#[derive(Clone, Copy, PartialEq)]
enum Undo {
    Keep,
    Clear,
    Restore,
}

enum Source {
    Gif(gif::Decoder<BufReader<File>>),
    Png(png::Reader<BufReader<File>>, Vec<u8>),
    Webp(image_webp::WebPDecoder<BufReader<File>>, Vec<u8>),
}

/// An animation's frames, composited one after another as they are read from the file.
pub struct Frames {
    /// The picture as the last frame left it.
    pub canvas: Rgba,
    /// Times it plays through (None: forever).
    pub plays: Option<u32>,
    src: Source,
    /// The last frame's area and what becomes of it, and what lay under it.
    undo: Option<(Area, Undo)>,
    under: Vec<u8>,
    drawn: usize,
}

impl Frames {
    /// The animation in `path`; None for a still PNG or WebP, or anything else.
    pub fn open(path: &Path) -> Option<Frames> {
        // Regular files only: a FIFO named "*.gif" would block forever.
        if !std::fs::metadata(path).ok()?.is_file() {
            return None;
        }
        let mut f = BufReader::new(File::open(path).ok()?);
        let head = f.fill_buf().ok()?.get(..12)?.to_vec();
        let fits = |w: u32, h: u32| w > 0 && h > 0 && w as u64 * h as u64 <= MAX_PIXELS;
        let (w, h, plays, src) = if head.starts_with(b"GIF8") {
            let mut o = gif::DecodeOptions::new();
            o.set_color_output(gif::ColorOutput::RGBA);
            let d = o.read_info(f).ok()?;
            // The loop count comes with the first frame.
            (d.width() as u32, d.height() as u32, None, Source::Gif(d))
        } else if head.starts_with(b"\x89PNG") {
            let mut d = png::Decoder::new(f);
            d.set_transformations(png::Transformations::normalize_to_color8());
            let mut r = d.read_info().ok()?;
            let (w, h) = (r.info().width, r.info().height);
            let ac = r.info().animation_control?;
            if !fits(w, h) {
                return None;
            }
            let mut buf = vec![0; r.output_buffer_size()];
            // Without a frame control ahead of it the default image is not one of the frames.
            if r.info().frame_control.is_none() {
                r.next_frame(&mut buf).ok()?;
            }
            (w, h, (ac.num_plays > 0).then_some(ac.num_plays), Source::Png(r, buf))
        } else if &head[..4] == b"RIFF" && &head[8..] == b"WEBP" {
            let d = image_webp::WebPDecoder::new(f).ok()?;
            let (w, h) = d.dimensions();
            if !d.is_animated() || !fits(w, h) {
                return None;
            }
            let plays = match d.loop_count() {
                image_webp::LoopCount::Forever => None,
                image_webp::LoopCount::Times(n) => Some(n.get() as u32),
            };
            let buf = vec![0; d.output_buffer_size()?];
            (w, h, plays, Source::Webp(d, buf))
        } else {
            return None;
        };
        if !fits(w, h) {
            return None;
        }
        Some(Frames { canvas: Rgba { w, h, px: vec![0; w as usize * h as usize * 4] }, plays, src, undo: None, under: vec![], drawn: 0 })
    }

    /// Draw the next frame onto the canvas; how long it shows (ms, as the file says).
    pub fn next(&mut self) -> Option<u32> {
        if let Some((a, u)) = self.undo.take() {
            let stride = self.canvas.w as usize * 4;
            for (i, row) in (a.y..a.y + a.h).enumerate() {
                let dst = &mut self.canvas.px[row * stride + a.x * 4..][..a.w * 4];
                match u {
                    Undo::Clear => dst.fill(0),
                    Undo::Restore => dst.copy_from_slice(&self.under[i * a.w * 4..][..a.w * 4]),
                    Undo::Keep => {}
                }
            }
        }
        let (c, under) = (&mut self.canvas, &mut self.under);
        let clip = |x: u32, y: u32, w: u32, h: u32| {
            let (x, y) = (x.min(c.w) as usize, y.min(c.h) as usize);
            Area { x, y, w: (w as usize).min(c.w as usize - x), h: (h as usize).min(c.h as usize - y) }
        };
        let (area, undo, ms) = match &mut self.src {
            Source::Gif(d) => {
                let f = d.read_next_frame().ok()??;
                let area = clip(f.left as u32, f.top as u32, f.width as u32, f.height as u32);
                let undo = match f.dispose {
                    gif::DisposalMethod::Background => Undo::Clear,
                    gif::DisposalMethod::Previous => Undo::Restore,
                    _ => Undo::Keep,
                };
                draw(c, under, &f.buffer, f.width as usize, area, true, undo);
                let ms = f.delay as u32 * 10;
                // A loop count of n means n more times round after the first.
                self.plays = match d.repeat() {
                    gif::Repeat::Infinite => None,
                    gif::Repeat::Finite(n) => Some(n as u32 + 1),
                };
                (area, undo, ms)
            }
            Source::Png(r, buf) => {
                let info = r.next_frame(buf).ok()?;
                let fc = r.info().frame_control?;
                let px = crate::imgdec::rgba_from(buf[..info.buffer_size()].to_vec(), info.color_type.samples())?;
                let area = clip(fc.x_offset, fc.y_offset, info.width, info.height);
                let undo = match fc.dispose_op {
                    png::DisposeOp::None => Undo::Keep,
                    // The first frame has nothing before it to go back to.
                    png::DisposeOp::Previous if self.drawn > 0 => Undo::Restore,
                    _ => Undo::Clear,
                };
                draw(c, under, &px, info.width as usize, area, fc.blend_op == png::BlendOp::Over, undo);
                (area, undo, fc.delay_num as u32 * 1000 / if fc.delay_den == 0 { 100 } else { fc.delay_den as u32 })
            }
            // The decoder composites WebP frames itself.
            Source::Webp(d, buf) => {
                let ms = d.read_frame(buf).ok()?;
                c.px = crate::imgdec::rgba_from(buf.clone(), if d.has_alpha() { 4 } else { 3 })?;
                (clip(0, 0, 0, 0), Undo::Keep, ms)
            }
        };
        self.undo = Some((area, undo));
        self.drawn += 1;
        Some(ms)
    }
}

/// A frame's RGBA pixels (`fw` wide) drawn into `area` of the canvas, over what is there
/// or in its place; what they cover kept in `under` when it is to be put back.
fn draw(c: &mut Rgba, under: &mut Vec<u8>, px: &[u8], fw: usize, area: Area, over: bool, undo: Undo) {
    let stride = c.w as usize * 4;
    if undo == Undo::Restore {
        under.clear();
        for row in area.y..area.y + area.h {
            under.extend_from_slice(&c.px[row * stride + area.x * 4..][..area.w * 4]);
        }
    }
    for y in 0..area.h {
        let Some(src) = px.get(y * fw * 4..y * fw * 4 + area.w * 4) else { break };
        let dst = &mut c.px[(area.y + y) * stride + area.x * 4..][..area.w * 4];
        if !over {
            dst.copy_from_slice(src);
            continue;
        }
        for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
            over_pixel(d, s);
        }
    }
}

/// `s` drawn over `d` (neither premultiplied).
fn over_pixel(d: &mut [u8], s: &[u8]) {
    let sa = s[3] as u32;
    if sa == 255 {
        d.copy_from_slice(s);
    } else if sa > 0 {
        let da = d[3] as u32 * (255 - sa) / 255;
        let a = sa + da;
        for i in 0..3 {
            d[i] = ((s[i] as u32 * sa + d[i] as u32 * da) / a) as u8;
        }
        d[3] = a as u8;
    }
}

/// The canvas as shown: within `side` pixels a side.
pub fn shown(c: &Rgba, side: u32) -> ColorImage {
    let small;
    let img = if c.w > side || c.h > side {
        small = crate::imgdec::downscale(c, side);
        &small
    } else {
        c
    };
    ColorImage::from_rgba_unmultiplied([img.w as usize, img.h as usize], &img.px)
}

enum Msg {
    Size([u32; 2]),
    Frame(Arc<ColorImage>, u32),
    /// Every frame has come, and how many times they play.
    Whole(Option<u32>),
}

/// The worker: the frames once through, kept on the other side if they fit `budget`;
/// otherwise again and again, as many times as the animation plays.
fn decode(path: &Path, side: u32, budget: usize, tx: &SyncSender<Msg>, wake: &AtomicBool, ctx: &egui::Context) -> Option<()> {
    let send = |m: Msg| {
        tx.send(m).ok()?;
        if wake.swap(false, Ordering::AcqRel) {
            ctx.request_repaint();
        }
        Some(())
    };
    let (mut kept, mut pass) = (0, 0);
    loop {
        let mut f = Frames::open(path)?;
        if pass == 0 {
            send(Msg::Size([f.canvas.w, f.canvas.h]))?;
        }
        while let Some(ms) = f.next() {
            let img = shown(&f.canvas, side);
            if pass == 0 {
                kept += img.pixels.len() * 4;
            }
            send(Msg::Frame(Arc::new(img), delay_ms(ms)))?;
        }
        pass += 1;
        if kept <= budget {
            return send(Msg::Whole(f.plays));
        }
        if f.plays.is_some_and(|p| pass >= p) {
            return Some(());
        }
    }
}

/// One animation playing.
pub struct Player {
    path: PathBuf,
    mtime: Option<SystemTime>,
    side: u32,
    budget: usize,
    ctx: egui::Context,
    rx: Option<Receiver<Msg>>,
    /// Set while the screen waits for the worker's next frame.
    wake: Arc<AtomicBool>,
    dims: [u32; 2],
    frames: VecDeque<(Arc<ColorImage>, u32)>,
    bytes: usize,
    /// Too long to keep: each frame goes once shown, the head of `frames` is the one on screen.
    streaming: bool,
    /// Every frame is in `frames`.
    whole: bool,
    plays: Option<u32>,
    played: u32,
    at: usize,
    tex: Option<TextureHandle>,
    /// The texture is behind the frame at `at`.
    stale: bool,
    /// When the frame shown is due to move on (None: a whole delay from the next draw).
    due: Option<f64>,
    pub playing: bool,
    /// Played as many times as it says.
    ended: bool,
}

impl Player {
    /// Start decoding `path` for a view `want` pixels a side; `asset_bytes` is the asset
    /// cache's budget.
    pub fn start(path: &Path, mtime: Option<SystemTime>, want: f32, asset_bytes: usize, ctx: &egui::Context) -> Player {
        Player::spawn(path.to_path_buf(), mtime, crate::thumbs::bucket(want).min(MAX_SIDE), budget(asset_bytes), ctx.clone())
    }

    fn spawn(path: PathBuf, mtime: Option<SystemTime>, side: u32, budget: usize, ctx: egui::Context) -> Player {
        let (tx, rx) = sync_channel(AHEAD);
        let wake = Arc::new(AtomicBool::new(true));
        let (p, w, c) = (path.clone(), wake.clone(), ctx.clone());
        std::thread::spawn(move || decode(&p, side, budget, &tx, &w, &c));
        Player { path, mtime, side, budget, ctx, rx: Some(rx), wake, dims: [0, 0], frames: VecDeque::new(), bytes: 0, streaming: false, whole: false, plays: None, played: 0, at: 0, tex: None, stale: true, due: None, playing: true, ended: false }
    }

    /// Whether this plays `path` well enough for a view `want` pixels a side (a bigger
    /// view than it was scaled for wants it decoded again).
    pub fn is_for(&self, path: &Path, mtime: Option<SystemTime>, want: f32) -> bool {
        self.path == path && self.mtime == mtime && (crate::thumbs::bucket(want).min(MAX_SIDE) <= self.side || self.dims[0].max(self.dims[1]) <= self.side)
    }

    /// More than one frame: it moves (a still GIF is the thumbnail's to show).
    pub fn moving(&self) -> bool {
        self.frames.len() > 1 || self.streaming
    }

    pub fn toggle(&mut self) {
        if self.ended {
            if self.streaming {
                *self = Player::spawn(self.path.clone(), self.mtime, self.side, self.budget, self.ctx.clone());
                return;
            }
            (self.at, self.played, self.stale, self.ended) = (0, 0, true, false);
        }
        self.playing = !self.playing;
        self.due = None;
    }

    /// Take what the worker sent, no more than `AHEAD` frames past the one shown, so it
    /// decodes no further ahead than that.
    fn pull(&mut self) {
        let Some(rx) = &self.rx else { return };
        while self.frames.len() <= self.at + AHEAD {
            match rx.try_recv() {
                Ok(Msg::Size(d)) => self.dims = d,
                Ok(Msg::Frame(img, ms)) => {
                    self.bytes += img.pixels.len() * 4;
                    self.frames.push_back((img, ms));
                    if !self.streaming && self.bytes > self.budget {
                        self.streaming = true;
                        self.frames.drain(..self.at);
                        self.at = 0;
                    }
                }
                Ok(Msg::Whole(plays)) => {
                    (self.whole, self.plays) = (true, plays);
                    // One frame: a still picture after all.
                    if self.frames.len() < 2 {
                        self.frames.clear();
                    }
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.rx = None;
                    return;
                }
            }
        }
    }

    /// On to the next frame: false while it isn't decoded yet, or once the animation is over.
    fn step(&mut self) -> bool {
        if self.streaming {
            if self.frames.len() > 1 {
                self.frames.pop_front();
                self.stale = true;
                return true;
            }
            self.ended = self.rx.is_none();
        } else if self.at + 1 < self.frames.len() {
            self.at += 1;
            self.stale = true;
            return true;
        } else if self.whole {
            self.played += 1;
            self.ended = self.plays.is_some_and(|p| self.played >= p);
            if !self.ended {
                (self.at, self.stale) = (0, true);
                return true;
            }
        }
        self.playing &= !self.ended;
        false
    }

    /// Wait for the worker: it repaints once the next frame lands (asked first, then
    /// looked for once more, so a frame landing in between isn't missed).
    fn wait(&mut self) {
        if self.rx.is_some() {
            self.wake.store(true, Ordering::Release);
            self.pull();
        }
    }

    /// The frame to show now and the picture's size, moving on when its time is up (only
    /// while `live`: on screen and not covered). None until it is known to move.
    pub fn frame(&mut self, live: bool) -> Option<(egui::TextureId, [u32; 2])> {
        self.pull();
        if !self.moving() {
            self.wait();
            if !self.moving() {
                return None;
            }
        }
        let now = self.ctx.input(|i| i.time);
        let delay = |p: &Player| p.frames.get(p.at).map_or(0.1, |f| f.1 as f64 / 1000.0);
        let mut starved = false;
        if live && self.playing && self.due.is_some_and(|d| now >= d) {
            let mut moved = self.step();
            if !moved && self.playing {
                self.wait();
                moved = self.step();
            }
            if moved {
                // On time from when it was due, unless it fell far behind (it was hidden).
                let d = self.due.unwrap_or(now);
                self.due = Some(if now - d > 0.25 { now } else { d } + delay(self));
            }
            starved = !moved;
        }
        let (img, _) = self.frames.get(self.at)?.clone();
        match &mut self.tex {
            Some(t) if self.stale => t.set(img, TextureOptions::LINEAR),
            Some(_) => {}
            None => self.tex = Some(self.ctx.load_texture("animation", img, TextureOptions::LINEAR)),
        }
        self.stale = false;
        if live && self.playing {
            let due = *self.due.get_or_insert(now + delay(self));
            if !starved {
                self.ctx.request_repaint_after(Duration::from_secs_f64((due - now).max(0.0)));
            }
        }
        Some((self.tex.as_ref()?.id(), self.dims))
    }

    /// Frame shown, frames held, bytes they hold, decoded again each time round, and the
    /// shown frame's first pixel.
    #[cfg(test)]
    pub fn state(&self) -> (usize, usize, usize, bool, Option<egui::Color32>) {
        (self.at, self.frames.len(), self.frames.iter().map(|f| f.0.pixels.len() * 4).sum(), self.streaming, self.frames.get(self.at).map(|f| f.0.pixels[0]))
    }
}
