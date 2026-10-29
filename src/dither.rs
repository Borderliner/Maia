//! The welcome's moving dither: a slow field folded on itself so it flows like ink,
//! drawn in the accent's tones through an ordered (Bayer) dither, each cell a square dot
//! with a hairline of the background around it. The field is worked out on a thread of
//! its own, about two dozen frames a second and only while it is on screen; the window
//! uploads the newest frame, one texel a cell, and draws the gaps over it as lines.

use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, Rect, TextureOptions, Ui, pos2, vec2};

/// A cell's side in physical pixels: five of dot, two of gap.
const CELL: f32 = 7.0;
const GAP: f32 = 2.0;
/// Frames a second while it moves.
const FPS: u64 = 24;

/// What the thread is asked to draw.
#[derive(Clone, Copy, PartialEq)]
struct Ask {
    cols: usize,
    rows: usize,
    tones: [Color32; 3],
    moving: bool,
}

#[derive(Default)]
struct Shared {
    ask: Option<Ask>,
    /// The newest frame, waiting to be uploaded.
    frame: Option<egui::ColorImage>,
}

pub struct Dither {
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    tex: Option<egui::TextureHandle>,
}

impl Drop for Dither {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

impl Dither {
    pub fn new(ctx: &egui::Context) -> Dither {
        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, st2, c2) = (shared.clone(), stop.clone(), ctx.clone());
        let _ = std::thread::Builder::new().name("dither".into()).spawn(move || worker(s2, st2, c2));
        Dither { shared, stop, tex: None }
    }

    /// Draw it over `r`, its corners rounded by `round`, on a ground of `bg`; `fade` from
    /// 0 to 1 as it comes in.
    pub fn paint(&mut self, ui: &Ui, r: Rect, accent: Color32, bg: Color32, moving: bool, round: u8, fade: f32) {
        let ppp = ui.ctx().pixels_per_point();
        // On the pixel grid, whole cells across and down.
        let x0 = (r.left() * ppp).round() / ppp;
        let y0 = (r.top() * ppp).round() / ppp;
        let cols = ((r.right() - x0) * ppp / CELL).floor().max(1.0) as usize;
        let rows = ((r.bottom() - y0) * ppp / CELL).floor().max(1.0) as usize;
        let rect = Rect::from_min_size(pos2(x0, y0), vec2(cols as f32 * CELL / ppp, rows as f32 * CELL / ppp));
        let tones = [bg.lerp_to_gamma(accent, 0.22), bg.lerp_to_gamma(accent, 0.5), bg.lerp_to_gamma(accent, 0.92)];
        let ask = Ask { cols, rows, tones, moving };
        let frame = {
            let mut s = self.shared.lock().unwrap_or_else(|e| e.into_inner());
            if s.ask != Some(ask) {
                s.ask = Some(ask);
            }
            s.frame.take()
        };
        let nearest = TextureOptions::NEAREST;
        if let Some(img) = frame.filter(|i| i.size == [cols, rows]) {
            match &mut self.tex {
                Some(t) => t.set(img, nearest),
                None => self.tex = Some(ui.ctx().load_texture("dither", img, nearest)),
            }
        }
        let Some(tex) = &self.tex else { return };
        let corner = CornerRadius::same(round);
        let tint = Color32::WHITE.gamma_multiply(fade);
        egui::Image::new((tex.id(), rect.size())).corner_radius(corner).tint(tint).paint_at(ui, rect);
        // The gaps: a pixel-wide line of the ground after every cell, across and down.
        let gap = GAP / ppp;
        let painter = ui.painter().with_clip_rect(rect.intersect(ui.clip_rect()));
        let line = bg.gamma_multiply(fade);
        for i in 1..=cols {
            let x = rect.left() + i as f32 * CELL / ppp - gap;
            painter.rect_filled(Rect::from_min_max(pos2(x, rect.top()), pos2(x + gap, rect.bottom())), CornerRadius::ZERO, line);
        }
        for j in 1..=rows {
            let y = rect.top() + j as f32 * CELL / ppp - gap;
            painter.rect_filled(Rect::from_min_max(pos2(rect.left(), y), pos2(rect.right(), y + gap)), CornerRadius::ZERO, line);
        }
    }
}

fn worker(shared: Arc<Mutex<Shared>>, stop: Arc<AtomicBool>, ctx: egui::Context) {
    let t0 = Instant::now();
    let mut done: Option<Ask> = None;
    let mut field = vec![];
    while !stop.load(Relaxed) {
        let tick = Instant::now();
        let ask = shared.lock().unwrap_or_else(|e| e.into_inner()).ask;
        let Some(a) = ask else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        // Still, once drawn it is drawn.
        if !a.moving && done == Some(a) {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        let t = if a.moving { t0.elapsed().as_secs_f32() } else { 8.0 };
        let img = frame(&a, t, &mut field);
        shared.lock().unwrap_or_else(|e| e.into_inner()).frame = Some(img);
        done = Some(a);
        ctx.request_repaint();
        if let Some(rest) = Duration::from_millis(1000 / FPS).checked_sub(tick.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

/// The 4×4 ordered-dither thresholds, in sixteenths.
const BAYER: [u8; 16] = [0, 8, 2, 10, 12, 4, 14, 6, 3, 11, 1, 9, 15, 7, 13, 5];

fn frame(a: &Ask, t: f32, field: &mut Vec<f32>) -> egui::ColorImage {
    let (cols, rows) = (a.cols, a.rows);
    // The field at every other cell, filled in between: a quarter of the work, and the
    // dither hides the difference.
    let (fc, fr) = (cols / 2 + 2, rows / 2 + 2);
    field.clear();
    field.reserve(fc * fr);
    let scale = 3.2 / cols.max(rows) as f32;
    for y in 0..fr {
        for x in 0..fc {
            let (px, py) = (x as f32 * 2.0 * scale, y as f32 * 2.0 * scale);
            // Folded twice: the field moved by the field itself, as ink spreads in water.
            let qx = fbm(px + 0.03 * t, py - 0.02 * t, 3);
            let qy = fbm(px + 5.2 - 0.025 * t, py + 1.3 + 0.015 * t, 3);
            field.push(fbm(px + 1.9 * qx, py + 1.9 * qy + 0.01 * t, 4));
        }
    }
    let mut px = vec![Color32::TRANSPARENT; cols * rows];
    for y in 0..rows {
        for x in 0..cols {
            let (fx, fy) = (x as f32 / 2.0, y as f32 / 2.0);
            let (ix, iy) = (fx as usize, fy as usize);
            let (ax, ay) = (fx - ix as f32, fy - iy as f32);
            let at = |i: usize, j: usize| field[j.min(fr - 1) * fc + i.min(fc - 1)];
            let v = (at(ix, iy) * (1.0 - ax) + at(ix + 1, iy) * ax) * (1.0 - ay) + (at(ix, iy + 1) * (1.0 - ax) + at(ix + 1, iy + 1) * ax) * ay;
            // Thickest toward the top right, thinning to nothing toward the bottom left.
            let (u, w) = (x as f32 / cols as f32, y as f32 / rows as f32);
            let reach = smooth(0.3, 1.0, 0.55 * u + 0.65 * (1.0 - w));
            let tone = smooth(0.38, 0.72, v) * reach * 3.0;
            // Flat tones with dithered edges between them, as a print of few inks: the
            // dither only where one tone turns into the next.
            let base = tone.floor();
            let edge = smooth(0.3, 0.7, tone - base);
            let level = base as usize + (edge * 16.0 > BAYER[(y % 4) * 4 + x % 4] as f32) as usize;
            if level > 0 {
                px[y * cols + x] = a.tones[(level - 1).min(2)];
            }
        }
    }
    egui::ColorImage::new([cols, rows], px)
}

fn smooth(a: f32, b: f32, x: f32) -> f32 {
    let t = ((x - a) / (b - a)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// A lattice point's value, 0 to 1.
fn hash(x: i32, y: i32) -> f32 {
    let mut h = (x as u32).wrapping_mul(0x8da6_b343) ^ (y as u32).wrapping_mul(0xd816_3841);
    h ^= h >> 13;
    h = h.wrapping_mul(0x85eb_ca6b);
    h ^= h >> 16;
    (h & 0xffff) as f32 / 65535.0
}

/// Smooth noise between lattice values.
fn noise(x: f32, y: f32) -> f32 {
    let (ix, iy) = (x.floor(), y.floor());
    let (fx, fy) = (x - ix, y - iy);
    let (ux, uy) = (fx * fx * (3.0 - 2.0 * fx), fy * fy * (3.0 - 2.0 * fy));
    let (i, j) = (ix as i32, iy as i32);
    let a = hash(i, j) + (hash(i + 1, j) - hash(i, j)) * ux;
    let b = hash(i, j + 1) + (hash(i + 1, j + 1) - hash(i, j + 1)) * ux;
    a + (b - a) * uy
}

/// Octaves of noise, each finer and fainter, turned a little against the last.
fn fbm(mut x: f32, mut y: f32, octaves: u32) -> f32 {
    let (mut sum, mut amp, mut norm) = (0.0, 0.5, 0.0);
    for _ in 0..octaves {
        sum += amp * noise(x, y);
        norm += amp;
        let (nx, ny) = (1.6 * x + 1.2 * y, -1.2 * x + 1.6 * y);
        x = nx + 1.7;
        y = ny + 9.2;
        amp *= 0.5;
    }
    sum / norm
}
