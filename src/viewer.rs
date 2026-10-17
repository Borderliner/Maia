//! The picture viewer: the pictures of a folder full window over Maia, in the listing's
//! order. Pictures come through the thumbnail cache (decoded on its workers, the right
//! way up, within the asset cache's budget), the neighbours asked for ahead. Nothing
//! repaints while it is still: the slideshow's timer runs only while it plays, and the
//! name fades away once the pointer rests.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use egui::{Align2, Color32, CornerRadius, FontId, Id, Key, Modifiers, Pos2, Rect, Sense, Stroke, Vec2, pos2, vec2};

use crate::app::App;
use crate::popups::Popup;
use crate::theme::Glyph;

/// How long each picture stays in a slideshow.
pub const SLIDE: f64 = 4.0;
/// The fade from one picture to the next.
const FADE: f64 = 0.35;
/// The name and position stay this long after the pointer last moved, then fade out.
const SHOW: f64 = 2.0;
const HIDE: f64 = 0.4;

pub fn is_picture(ext: &str) -> bool {
    crate::thumbs::is_image_ext(ext) || ext == "svg"
}

pub struct Pic {
    pub path: PathBuf,
    pub name: String,
    pub modified: Option<SystemTime>,
}

pub struct State {
    pub panel: u64,
    pub pics: Vec<Pic>,
    pub at: usize,
    /// Scale relative to fitting the window (1: fit) and the offset from the middle.
    pub zoom: f32,
    pub pan: Vec2,
    pub playing: bool,
    /// When the slideshow moves on.
    next_at: f64,
    /// The picture fading out, and when it started to.
    from: Option<(usize, f64)>,
    /// When the pointer last moved or a key was pressed (the name shows meanwhile).
    stirred: f64,
    last_pointer: Option<Pos2>,
}

impl State {
    pub fn new(panel: u64, pics: Vec<Pic>, at: usize, now: f64) -> State {
        State { panel, pics, at, zoom: 1.0, pan: Vec2::ZERO, playing: false, next_at: 0.0, from: None, stirred: now, last_pointer: None }
    }

    /// The picture fading out, while one is.
    #[cfg(test)]
    pub fn fading_from(&self) -> Option<usize> {
        self.from.map(|f| f.0)
    }

    pub fn current(&self) -> Option<&Pic> {
        self.pics.get(self.at)
    }

    /// The name and position's opacity at `now`.
    pub fn label_alpha(&self, now: f64, animations: bool) -> f32 {
        let idle = now - self.stirred;
        if idle < SHOW {
            1.0
        } else if animations && idle < SHOW + HIDE {
            (1.0 - (idle - SHOW) / HIDE) as f32
        } else {
            0.0
        }
    }

    pub fn go(&mut self, to: usize, now: f64, animations: bool) {
        if to == self.at || to >= self.pics.len() {
            return;
        }
        self.from = animations.then_some((self.at, now));
        self.at = to;
        self.zoom = 1.0;
        self.pan = Vec2::ZERO;
    }
}

/// The pictures in the active panel's listing, starting from `start` (or the one under
/// the cursor). None when there are none.
pub fn open_at(app: &mut App, start: Option<PathBuf>) -> bool {
    let panel = app.active;
    let t = app.tab();
    let pics: Vec<Pic> = t.view.iter().map(|&i| &t.entries[i as usize]).filter(|e| !e.is_dir && is_picture(e.ext())).map(|e| Pic { path: e.path.clone(), name: e.name.clone(), modified: e.modified }).collect();
    if pics.is_empty() {
        return false;
    }
    let want = start.or_else(|| t.cursor.and_then(|c| t.entries.get(c as usize)).map(|e| e.path.clone()));
    let at = want.and_then(|w| pics.iter().position(|p| p.path == w)).unwrap_or(0);
    let now = app.ctx.input(|i| i.time);
    app.popup = Popup::Viewer(Box::new(State::new(panel, pics, at, now)));
    true
}

/// View Pictures: from the picture under the cursor, else the folder's first.
pub fn open(app: &mut App) {
    if !open_at(app, None) {
        app.toast("No pictures here");
    }
}

/// The scale that fits `dims` into `area`, never past one point per pixel.
pub fn fit_scale(area: Rect, dims: [u32; 2]) -> f32 {
    (area.width() / dims[0].max(1) as f32).min(area.height() / dims[1].max(1) as f32).min(1.0)
}

/// The viewer. Returns false when it closes.
pub fn draw(app: &mut App, ctx: &egui::Context, st: &mut State) -> bool {
    let animations = app.cfg.animations;
    let (now, ppp, pointer) = ctx.input(|i| (i.time, i.pixels_per_point(), i.pointer.latest_pos()));
    let screen = ctx.content_rect();
    let n = st.pics.len();
    if n == 0 {
        return false;
    }
    st.at = st.at.min(n - 1);
    let key = |k: Key| ctx.input_mut(|i| i.consume_key(Modifiers::NONE, k));
    let shift_key = |k: Key| ctx.input_mut(|i| i.consume_key(Modifiers::SHIFT, k));
    let (esc, left, right, space, home, end) = (key(Key::Escape), key(Key::ArrowLeft), key(Key::ArrowRight), key(Key::Space), key(Key::Home), key(Key::End));
    let zoom_in = key(Key::Plus) || key(Key::Equals) || shift_key(Key::Equals) || shift_key(Key::Plus);
    let (zoom_out, fit, actual) = (key(Key::Minus), key(Key::Num0), key(Key::Num1));
    if esc {
        close(app, st);
        return false;
    }
    if left || right || space || home || end || zoom_in || zoom_out || fit || actual || pointer != st.last_pointer {
        st.stirred = now;
    }
    st.last_pointer = pointer;
    if space {
        st.playing = !st.playing;
        st.next_at = now + SLIDE;
    }
    let mut to = None;
    if right && st.at + 1 < n {
        to = Some(st.at + 1);
    }
    if left && st.at > 0 {
        to = Some(st.at - 1);
    }
    if home {
        to = Some(0);
    }
    if end {
        to = Some(n - 1);
    }
    if st.playing && now >= st.next_at {
        to = Some((st.at + 1) % n);
        st.next_at = now + SLIDE;
    }
    if let Some(i) = to {
        st.go(i, now, animations);
        if st.playing {
            st.next_at = now + SLIDE;
        }
    }
    let area = screen.shrink(12.0);
    // No bigger than the graphics card takes in one texture.
    let most_side = ctx.input(|i| i.max_texture_side).min(4096) as f32;
    let want = |zoom: f32| (area.width().max(area.height()) * ppp * zoom.max(1.0)).min(most_side);
    // Ask for the neighbours first: the cache's workers take the newest request first,
    // so the picture shown is decoded before them.
    for d in [st.at + 1, st.at.wrapping_sub(1)] {
        if let Some(p) = st.pics.get(d) {
            let _ = app.thumbs.get(&p.path, p.modified, want(1.0));
        }
    }
    let cur = &st.pics[st.at];
    let shown = app.thumbs.get(&cur.path, cur.modified, want(st.zoom)).map(|l| (l.tex.id(), l.dims));
    let failed = shown.is_none() && app.thumbs.failed(&cur.path);
    // Zoom by keys, around the middle.
    if let Some((_, dims)) = shown {
        let base = fit_scale(area, dims);
        let most = (16.0 / ppp) / base.max(1e-4);
        if zoom_in {
            st.zoom = (st.zoom * 1.25).min(most.max(1.0));
        }
        if zoom_out {
            st.zoom = (st.zoom / 1.25).max(0.1);
        }
        if actual {
            st.zoom = (1.0 / ppp) / base.max(1e-4);
        }
        if fit {
            st.zoom = 1.0;
            st.pan = Vec2::ZERO;
        }
    }
    let pal = app.pal;
    let mut close_now = false;
    let label_a = st.label_alpha(now, animations);
    egui::Area::new(Id::new("picture viewer")).order(egui::Order::Foreground).fixed_pos(screen.min).show(ctx, |ui| {
        let (r, resp) = ui.allocate_exact_size(screen.size(), Sense::click_and_drag());
        let p = ui.painter();
        p.rect_filled(r, CornerRadius::ZERO, Color32::from_rgb(14, 14, 15));
        if let Some((tex, dims)) = shown {
            let base = fit_scale(area, dims);
            // The wheel zooms around the pointer.
            if resp.hovered() {
                let (scroll, zd) = ui.input(|i| (i.smooth_scroll_delta.y, i.zoom_delta()));
                let factor = if (zd - 1.0).abs() > 1e-4 { zd } else if scroll != 0.0 { (scroll / 300.0).exp() } else { 1.0 };
                if factor != 1.0 {
                    let old = st.zoom;
                    st.zoom = (st.zoom * factor).clamp(0.1, ((16.0 / ppp) / base.max(1e-4)).max(1.0));
                    if let Some(pp) = pointer {
                        let rel = pp - (area.center() + st.pan);
                        st.pan += rel - rel * (st.zoom / old);
                    }
                    st.stirred = now;
                }
            }
            if resp.dragged() {
                st.pan += resp.drag_delta();
            }
            if resp.double_clicked() {
                // Between fitting the window and one pixel for one.
                let one = (1.0 / ppp) / base.max(1e-4);
                st.zoom = if (st.zoom - 1.0).abs() < 1e-3 { one } else { 1.0 };
                st.pan = Vec2::ZERO;
            }
            let size = vec2(dims[0] as f32, dims[1] as f32) * base * st.zoom;
            // Moved only as far as there is picture to see.
            let room = ((size - area.size()) / 2.0).max(Vec2::ZERO);
            st.pan = st.pan.clamp(-room, room);
            let fading = st.from.map(|(_, t0)| ((now - t0) / FADE).clamp(0.0, 1.0) as f32).filter(|t| *t < 1.0);
            if let (Some(t), Some((i, _))) = (fading, st.from) {
                if let Some(old) = st.pics.get(i) {
                    if let Some(l) = app.thumbs.get(&old.path, old.modified, want(1.0)) {
                        let b = fit_scale(area, l.dims);
                        let r0 = Rect::from_center_size(area.center(), vec2(l.dims[0] as f32, l.dims[1] as f32) * b);
                        p.image(l.tex.id(), r0, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE.gamma_multiply(1.0 - t));
                    }
                }
            }
            let img = Rect::from_center_size(area.center() + st.pan, size);
            let p = ui.painter().with_clip_rect(r);
            p.image(tex, img, Rect::from_min_max(Pos2::ZERO, pos2(1.0, 1.0)), Color32::WHITE.gamma_multiply(fading.unwrap_or(1.0)));
            if fading.is_some() {
                ctx.request_repaint();
            } else {
                st.from = None;
            }
        } else if failed {
            p.text(area.center(), Align2::CENTER_CENTER, "Can't show this picture", FontId::proportional(app.cfg.font_size), Color32::from_gray(170));
        } else {
            crate::ui::spinner(p, area.center(), 9.0, pal.accent, now);
            ctx.request_repaint_after(Duration::from_millis(33));
        }
        // The name and where it is in the folder, quietly, while the pointer moves.
        if label_a > 0.0 {
            let fs = app.cfg.font_size;
            let name = crate::listing::elided(ui, &cur.name, FontId::proportional(fs), (r.width() - 160.0).max(60.0), Color32::from_gray(235).gamma_multiply(label_a));
            let pos = ui.painter().layout_no_wrap(format!("{} / {}", st.at + 1, n), FontId::proportional(fs - 1.5), Color32::from_gray(160).gamma_multiply(label_a));
            let w = name.size().x + 16.0 + pos.size().x;
            let h = name.size().y.max(pos.size().y) + 12.0;
            let pill = Rect::from_center_size(pos2(r.center().x, r.bottom() - 18.0 - h / 2.0), vec2(w + 28.0, h));
            let p = ui.painter();
            p.rect_filled(pill, CornerRadius::same((h / 2.0) as u8), Color32::from_black_alpha((150.0 * label_a) as u8));
            let ny = pill.center().y - name.size().y / 2.0;
            let px = pill.left() + 14.0 + name.size().x + 16.0;
            p.galley(pos2(pill.left() + 14.0, ny), name, Color32::WHITE);
            p.galley(pos2(px, pill.center().y - pos.size().y / 2.0), pos, Color32::WHITE);
            if st.playing {
                // A small play mark: the slideshow is on.
                let c = pos2(pill.right() + 14.0, pill.center().y);
                p.add(egui::Shape::convex_polygon(vec![c + vec2(-4.0, -5.0), c + vec2(5.0, 0.0), c + vec2(-4.0, 5.0)], Color32::from_gray(200).gamma_multiply(label_a), Stroke::NONE));
            }
            // Close at the corner, previous and next at the sides.
            let button = |rect: Rect, g: Glyph, what: &'static str, enabled: bool| -> bool {
                let b = ui.interact(rect, Id::new(("viewer button", what)), Sense::click());
                crate::ui::note_control(ui, rect, what);
                let fill = if b.hovered() { 120.0 } else { 70.0 };
                ui.painter().rect_filled(rect, CornerRadius::same((rect.width() / 2.0) as u8), Color32::from_black_alpha((fill * label_a) as u8));
                let col = Color32::from_gray(if enabled { 230 } else { 110 }).gamma_multiply(label_a);
                crate::theme::paint_glyph(ui.painter(), Rect::from_center_size(rect.center(), Vec2::splat(rect.width() * 0.45)), g, col);
                enabled && b.clicked()
            };
            if button(Rect::from_center_size(pos2(r.right() - 30.0, r.top() + 30.0), Vec2::splat(34.0)), Glyph::Close, "viewer close", true) {
                close_now = true;
            }
            if n > 1 && r.width() > 200.0 {
                if button(Rect::from_center_size(pos2(r.left() + 34.0, r.center().y), Vec2::splat(40.0)), Glyph::Back, "viewer previous", st.at > 0) {
                    to = Some(st.at - 1);
                }
                if button(Rect::from_center_size(pos2(r.right() - 34.0, r.center().y), Vec2::splat(40.0)), Glyph::Forward, "viewer next", st.at + 1 < n) {
                    to = Some(st.at + 1);
                }
            }
            if label_a < 1.0 {
                ctx.request_repaint();
            } else {
                ctx.request_repaint_after(Duration::from_secs_f64((SHOW - (now - st.stirred)).max(0.0) + 0.01));
            }
        }
    });
    if let Some(i) = to.filter(|i| *i != st.at) {
        st.go(i, now, animations);
        st.stirred = now;
        if st.playing {
            st.next_at = now + SLIDE;
        }
        ctx.request_repaint();
    }
    if st.playing {
        ctx.request_repaint_after(Duration::from_secs_f64((st.next_at - now).max(0.0)));
    }
    if close_now {
        close(app, st);
        return false;
    }
    true
}

/// Leave the listing's cursor on the picture last shown.
fn close(app: &mut App, st: &State) {
    let Some(p) = st.current().map(|p| p.path.clone()) else { return };
    if let Some(t) = app.panels.get_mut(&st.panel).map(|p| p.tab_mut()) {
        if let Some(i) = t.entries.iter().position(|e| e.path == p) {
            t.select_only(i as u32);
            t.scroll_to_cursor = true;
        }
    }
}
