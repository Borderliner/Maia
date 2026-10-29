//! View Calendar: a folder's pictures by the day they were taken, a month at a time,
//! Monday first. A day with pictures shows its first one and how many; a click (or the
//! arrows) selects that day's pictures in the listing and shows them in a strip below,
//! Shift takes in a run of days, Enter opens them full window.
//!
//! The dates come from facts.rs (when a photo was taken; else when the file changed),
//! read and grouped on a worker; the frame draws one month, and asks the thumbnail cache
//! only for the days on show and the strip's pictures in view.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, channel};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use egui::{Align2, Color32, CornerRadius, FontId, Key, Modifiers, Rect, Sense, Stroke, StrokeKind, Vec2, pos2, vec2};

use crate::app::App;
use crate::popups as pp;

const MONTHS: [&str; 12] = ["January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December"];
const WEEKDAYS: [&str; 7] = ["M", "T", "W", "T", "F", "S", "S"];

/// Year, month (1 to 12) and day of a day counted from 1970-01-01.
pub fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// The day counted from 1970-01-01 of a date.
pub fn day_of(y: i64, m: u32, d: u32) -> i64 {
    let (y, m) = if m <= 2 { (y - 1, m as i64 + 9) } else { (y, m as i64 - 3) };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * m + 2) / 5 + d as i64 - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// Monday 0 to Sunday 6.
pub fn weekday(day: i64) -> usize {
    (day + 3).rem_euclid(7) as usize
}

fn days_in(y: i64, m: u32) -> u32 {
    let (ny, nm) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
    (day_of(ny, nm, 1) - day_of(y, m, 1)) as u32
}

#[derive(Clone)]
pub struct Pic {
    pub path: PathBuf,
    pub modified: Option<SystemTime>,
    /// Its place among the listing's entries when the calendar opened.
    idx: u32,
}

/// The pictures by day, each day's in the order they were taken.
#[derive(Default)]
struct Grouped {
    days: BTreeMap<i64, Vec<u32>>,
    /// All dates are known (taken, or the file's own where none).
    done: bool,
}

pub struct State {
    pics: Arc<Vec<Pic>>,
    days: BTreeMap<i64, Vec<u32>>,
    rx: Receiver<Grouped>,
    stop: Arc<AtomicBool>,
    reading: bool,
    /// The month shown: year and month.
    month: Option<(i64, u32)>,
    /// The day the keys are on, and where a Shift run started.
    cursor: Option<i64>,
    anchor: Option<i64>,
    /// The days picked: their pictures are selected and in the strip.
    sel: BTreeSet<i64>,
    /// Moved by hand: the month stays put as more dates come in.
    moved: bool,
}

impl Drop for State {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

fn secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// Group on a worker as the dates come in: what facts.rs knows already at once, the rest
/// as its readers get to them, sent again every quarter second until all are known.
fn group(entries: Vec<crate::fsx::Entry>, stop: Arc<AtomicBool>, ctx: egui::Context) -> Receiver<Grouped> {
    let (tx, rx) = channel();
    let _ = std::thread::Builder::new().name("calendar".into()).spawn(move || {
        let n = entries.len();
        let mut when: Vec<Option<i64>> = vec![None; n];
        let mut known = vec![false; n];
        let start = Instant::now();
        loop {
            let mut pending = 0;
            for i in 0..n {
                if known[i] {
                    continue;
                }
                match crate::facts::get(&entries[i]) {
                    Some(f) => {
                        known[i] = true;
                        when[i] = f.taken.map(secs);
                    }
                    None => pending += 1,
                }
            }
            // A photo that never says when it was taken goes by when the file changed;
            // after half a minute those still being read do too.
            let done = pending == 0 || start.elapsed() > Duration::from_secs(30);
            let mut days: BTreeMap<i64, Vec<(i64, u32)>> = BTreeMap::new();
            for (i, e) in entries.iter().enumerate() {
                let t = when[i].or_else(|| e.modified.map(secs)).unwrap_or(0);
                let day = crate::timefmt::local_day(UNIX_EPOCH + Duration::from_secs(t.max(0) as u64));
                days.entry(day).or_default().push((t, i as u32));
            }
            let days = days
                .into_iter()
                .map(|(d, mut v)| {
                    crate::tab::merge_sort(&mut v, &|a: &(i64, u32), b: &(i64, u32)| a.cmp(b));
                    (d, v.into_iter().map(|x| x.1).collect())
                })
                .collect();
            if tx.send(Grouped { days, done }).is_err() || done || stop.load(Relaxed) {
                ctx.request_repaint();
                return;
            }
            ctx.request_repaint();
            std::thread::sleep(Duration::from_millis(250));
        }
    });
    rx
}

/// View Calendar: the pictures of the active panel's listing.
pub fn open(app: &mut App) {
    let t = app.tab();
    let mut pics = vec![];
    let mut entries = vec![];
    for &i in &t.view {
        let Some(e) = t.entries.get(i as usize) else { continue };
        if !e.is_dir && crate::viewer::is_picture(e.ext()) {
            pics.push(Pic { path: e.path.clone(), modified: e.modified, idx: i });
            entries.push(e.clone());
        }
    }
    if pics.is_empty() {
        app.toast("No pictures here");
        return;
    }
    let stop = Arc::new(AtomicBool::new(false));
    let rx = group(entries, stop.clone(), app.ctx.clone());
    app.popup = pp::Popup::Calendar(Box::new(State { pics: Arc::new(pics), days: BTreeMap::new(), rx, stop, reading: true, month: None, cursor: None, anchor: None, sel: BTreeSet::new(), moved: false }));
}

impl State {
    fn take(&mut self) {
        while let Ok(g) = self.rx.try_recv() {
            self.days = g.days;
            self.reading = !g.done;
            // On the newest month with pictures, following the dates as they come in until
            // the month is changed by hand.
            if !self.moved {
                if let Some(&last) = self.days.keys().next_back() {
                    let (y, m, _) = civil(last);
                    self.month = Some((y, m));
                    self.cursor = Some(last);
                }
            }
        }
    }

    fn shift_month(&mut self, by: i32) {
        self.moved = true;
        if let Some((y, m)) = self.month {
            let k = y * 12 + m as i64 - 1 + by as i64;
            self.month = Some((k.div_euclid(12), (k.rem_euclid(12) + 1) as u32));
        }
    }

    /// The pictures of the days picked, in day order.
    fn picked(&self) -> Vec<u32> {
        self.sel.iter().filter_map(|d| self.days.get(d)).flatten().copied().collect()
    }

    /// Pick `day` (with Shift, the run from the anchor to it), and select its pictures in
    /// the listing.
    fn pick(&mut self, app: &mut App, day: i64, run: bool) {
        self.moved = true;
        self.cursor = Some(day);
        let (y, m, _) = civil(day);
        self.month = Some((y, m));
        let from = if run { self.anchor.unwrap_or(day) } else { day };
        if !run {
            self.anchor = Some(day);
        }
        let (a, b) = (from.min(day), from.max(day));
        self.sel = self.days.range(a..=b).map(|(d, _)| *d).collect();
        let pics = self.pics.clone();
        let picked = self.picked();
        let t = app.tab();
        t.selected.clear();
        for k in &picked {
            if let Some(p) = pics.get(*k as usize) {
                // The listing may have changed under it: only what is still in its place.
                if t.entries.get(p.idx as usize).is_some_and(|e| e.path == p.path) {
                    t.selected.insert(p.idx);
                }
            }
        }
        if let Some(first) = picked.first().and_then(|k| pics.get(*k as usize)) {
            if t.selected.contains(&first.idx) {
                t.cursor = Some(first.idx);
                t.scroll_to_cursor = true;
            }
        }
    }
}

/// The part of a picture that fills a square, from its middle.
fn cover_uv(dims: [u32; 2]) -> Rect {
    let (w, h) = (dims[0].max(1) as f32, dims[1].max(1) as f32);
    if w > h {
        let f = h / w / 2.0;
        Rect::from_min_max(pos2(0.5 - f, 0.0), pos2(0.5 + f, 1.0))
    } else {
        let f = w / h / 2.0;
        Rect::from_min_max(pos2(0.0, 0.5 - f), pos2(1.0, 0.5 + f))
    }
}

/// The popup. Returns false when it closes.
pub fn popup(app: &mut App, ctx: &egui::Context, st: &mut State) -> bool {
    let pal = app.pal;
    st.take();
    let take = |m: Modifiers, k: Key| crate::devtools::take(ctx, m, k);
    if take(Modifiers::NONE, Key::Escape) {
        return false;
    }
    // The keys: a day at a time sideways, a week up and down, Shift for a run of days,
    // Page Up and Down a month, Enter the pictures full window.
    let mut step = 0i64;
    let mut run = false;
    // Shift first: a plain key's check would take Shift and the key as well.
    for (k, d) in [(Key::ArrowLeft, -1), (Key::ArrowRight, 1), (Key::ArrowUp, -7), (Key::ArrowDown, 7)] {
        if take(Modifiers::SHIFT, k) {
            step = d;
            run = true;
        }
        if take(Modifiers::NONE, k) {
            step = d;
        }
    }
    let (prev_m, next_m, enter) = (take(Modifiers::NONE, Key::PageUp), take(Modifiers::NONE, Key::PageDown), take(Modifiers::NONE, Key::Enter));
    if prev_m {
        st.shift_month(-1);
    }
    if next_m {
        st.shift_month(1);
    }
    if step != 0 {
        let from = st.cursor.or_else(|| st.days.keys().next_back().copied());
        if let Some(c) = from {
            // Moving off the edge of the dates still lands on a date with pictures.
            let to = c + step;
            let (lo, hi) = (st.days.keys().next().copied().unwrap_or(to), st.days.keys().next_back().copied().unwrap_or(to));
            st.pick(app, to.clamp(lo.min(c), hi.max(c)), run);
        }
    }
    let mut open_at: Option<PathBuf> = None;
    if enter {
        open_at = st.picked().first().or_else(|| st.cursor.and_then(|c| st.days.get(&c)).and_then(|v| v.first())).and_then(|&k| st.pics.get(k as usize)).map(|p| p.path.clone());
    }
    let fs = app.cfg.font_size;
    let small = FontId::proportional(fs - 1.5);
    let screen = ctx.content_rect();
    let top = pp::popup_top(screen);
    let width = 760.0f32.min(screen.width() - 24.0);
    let inner = width - 2.0 * pp::DIALOG_PAD;
    let room = screen.height() - top - 12.0 - 2.0 * pp::DIALOG_PAD;
    let head_h = 30.0;
    let wk_h = fs + 6.0;
    let strip_h = 72.0f32;
    let hints_h = 34.0;
    let gap = 4.0;
    let cell = ((inner - 6.0 * gap) / 7.0).min((room - head_h - wk_h - strip_h - hints_h - 24.0 - 5.0 * gap) / 6.0).floor().max(36.0);
    let grid_w = cell * 7.0 + gap * 6.0;
    let ppp = ctx.pixels_per_point();
    let mut clicked: Option<(i64, bool)> = None;
    let mut flip: i32 = 0;
    let shift = ctx.input(|i| i.modifiers.shift);
    let (_, rect) = pp::centered_top(ctx, "calendar", width, top, &pal, |ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        let (head, _) = ui.allocate_exact_size(vec2(inner, head_h), Sense::hover());
        let x0 = head.left() + ((inner - grid_w) / 2.0).max(0.0);
        let Some((y, m)) = st.month else {
            crate::ui::spinner(ui.painter(), head.center(), 7.0, pal.text_dim, ui.input(|i| i.time));
            ui.ctx().request_repaint();
            return;
        };
        // ‹ October 2026 ›, and how many pictures the month has at the right.
        let first = day_of(y, m, 1);
        let n_days = days_in(y, m);
        let in_month: usize = st.days.range(first..first + n_days as i64).map(|(_, v)| v.len()).sum();
        let title = format!("{} {y}", MONTHS[(m - 1) as usize]);
        let tg = ui.painter().layout_no_wrap(title, crate::theme::semibold(fs + 1.5), pal.text);
        let back = Rect::from_center_size(pos2(x0 + 12.0, head.center().y), Vec2::splat(24.0));
        let fwd = Rect::from_center_size(pos2(back.right() + 16.0 + tg.size().x + 16.0, head.center().y), Vec2::splat(24.0));
        ui.painter().galley(pos2(back.right() + 10.0, head.center().y - tg.size().y / 2.0), tg, pal.text);
        for (r, icon, by, what) in [(back, "chevron-left", -1, "calendar previous month"), (fwd, "chevron-right", 1, "calendar next month")] {
            let resp = ui.interact(r, egui::Id::unique(what), Sense::click());
            crate::ui::note_control(ui, r, what);
            if resp.hovered() {
                ui.painter().rect_filled(r, CornerRadius::same(5), pal.hover);
            }
            crate::theme::paint_symbol(ui.painter(), r.shrink(5.0), icon, if resp.hovered() { pal.text } else { pal.text_dim });
            if resp.on_hover_text(if by < 0 { "Previous month (Page Up)" } else { "Next month (Page Down)" }).clicked() {
                flip = by;
            }
        }
        let count = if st.reading { format!("{in_month} pictures · reading dates…") } else { format!("{in_month} picture{}", if in_month == 1 { "" } else { "s" }) };
        // Only where it clears the month's arrows: a narrow calendar keeps the month.
        let cg = ui.painter().layout_no_wrap(count, small.clone(), pal.text_dim);
        if x0 + grid_w - cg.size().x >= fwd.right() + 12.0 {
            ui.painter().galley(pos2(x0 + grid_w - cg.size().x, head.center().y - cg.size().y / 2.0), cg, pal.text_dim);
        }
        if st.reading {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
        }
        // M T W T F S S over the columns.
        let (wk, _) = ui.allocate_exact_size(vec2(inner, wk_h), Sense::hover());
        for (i, l) in WEEKDAYS.iter().enumerate() {
            let cx = x0 + i as f32 * (cell + gap) + cell / 2.0;
            ui.painter().text(pos2(cx, wk.center().y), Align2::CENTER_CENTER, *l, small.clone(), pal.text_dim);
        }
        let lead = weekday(first);
        let (grid, _) = ui.allocate_exact_size(vec2(inner, 6.0 * cell + 5.0 * gap), Sense::hover());
        for k in 0..n_days as usize {
            let day = first + k as i64;
            let slot = lead + k;
            let (col, row) = (slot % 7, slot / 7);
            let r = Rect::from_min_size(pos2(x0 + col as f32 * (cell + gap), grid.top() + row as f32 * (cell + gap)), Vec2::splat(cell));
            let pics = st.days.get(&day);
            let resp = ui.interact(r, egui::Id::unique(("calendar day", day)), if pics.is_some() { Sense::click() } else { Sense::hover() });
            let p = ui.painter();
            let num = format!("{}", k + 1);
            match pics.and_then(|v| v.first()).and_then(|&i| st.pics.get(i as usize)) {
                Some(pic) => {
                    crate::ui::note_control(ui, r, "calendar day");
                    match app.thumbs.get(&pic.path, pic.modified, cell * ppp) {
                        Some(t) => {
                            // Rounded as the empty days are.
                            egui::Image::from_texture(egui::load::SizedTexture::new(t.tex.id(), Vec2::splat(cell))).uv(cover_uv(t.dims)).corner_radius(6).paint_at(ui, r);
                        }
                        None => {
                            p.rect_filled(r, CornerRadius::same(6), pal.card());
                        }
                    }
                    // Over the picture, light on a dark veil, whatever the theme.
                    let ng = p.layout_no_wrap(num, FontId::proportional(fs - 2.0), Color32::WHITE);
                    let nr = Rect::from_min_size(r.min + vec2(4.0, 4.0), ng.size() + vec2(8.0, 2.0));
                    p.rect_filled(nr, CornerRadius::same(4), Color32::from_black_alpha(120));
                    p.galley(nr.min + vec2(4.0, 1.0), ng, Color32::WHITE);
                    let n = pics.map_or(0, |v| v.len());
                    if n > 1 {
                        let cg = p.layout_no_wrap(n.to_string(), FontId::proportional(fs - 2.5), Color32::WHITE);
                        let cr = Rect::from_min_size(r.max - cg.size() - vec2(12.0, 6.0), cg.size() + vec2(8.0, 2.0));
                        // In a small cell the count would sit on the day: the strip says it.
                        if !cr.expand(2.0).intersects(nr) {
                            p.rect_filled(cr, CornerRadius::same(7), Color32::from_black_alpha(150));
                            p.galley(cr.min + vec2(4.0, 1.0), cg, Color32::WHITE);
                        }
                    }
                    if resp.hovered() {
                        p.rect_stroke(r, CornerRadius::same(6), Stroke::new(1.0, pal.text_dim), StrokeKind::Inside);
                    }
                    if resp.clicked() {
                        clicked = Some((day, shift));
                    }
                    if resp.double_clicked() {
                        open_at = Some(pic.path.clone());
                    }
                }
                None => {
                    p.rect_filled(r, CornerRadius::same(6), pal.card());
                    p.text(r.min + vec2(7.0, 5.0), Align2::LEFT_TOP, num, FontId::proportional(fs - 2.0), pal.text_dim.gamma_multiply(0.7));
                }
            }
            if st.sel.contains(&day) {
                p.rect_stroke(r, CornerRadius::same(6), Stroke::new(2.0, pal.accent), StrokeKind::Inside);
            }
            if st.cursor == Some(day) && st.moved {
                p.rect_stroke(r.expand(2.0), CornerRadius::same(8), Stroke::new(1.0, pal.accent.gamma_multiply(0.7)), StrokeKind::Inside);
            }
        }
        // The strip: the pictures of the days picked, the ones in view asked for.
        ui.add_space(12.0);
        let picked = st.picked();
        let (strip, _) = ui.allocate_exact_size(vec2(inner, strip_h), Sense::hover());
        if !picked.is_empty() {
            let side = strip_h - 8.0;
            let mut sui = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(pos2(x0, strip.top()), vec2(grid_w, strip_h))).layout(egui::Layout::left_to_right(egui::Align::Center)));
            egui::ScrollArea::horizontal().id_salt("calendar strip").auto_shrink([false, false]).show_viewport(&mut sui, |ui, view| {
                let full = picked.len() as f32 * (side + 6.0);
                let (area, _) = ui.allocate_exact_size(vec2(full, side), Sense::hover());
                let from = ((view.left() / (side + 6.0)).floor().max(0.0)) as usize;
                let to = (((view.right() / (side + 6.0)).ceil()) as usize + 1).min(picked.len());
                for j in from..to {
                    let Some(pic) = st.pics.get(picked[j] as usize) else { continue };
                    let r = Rect::from_min_size(pos2(area.left() + j as f32 * (side + 6.0), area.top()), Vec2::splat(side));
                    let resp = ui.interact(r, egui::Id::unique(("calendar strip", j)), Sense::click());
                    crate::ui::note_control(ui, r, "calendar strip picture");
                    match app.thumbs.get(&pic.path, pic.modified, side * ppp) {
                        Some(t) => {
                            egui::Image::from_texture(egui::load::SizedTexture::new(t.tex.id(), Vec2::splat(side))).uv(cover_uv(t.dims)).corner_radius(4).paint_at(ui, r);
                        }
                        None => {
                            ui.painter().rect_filled(r, CornerRadius::same(4), pal.card());
                        }
                    }
                    if resp.hovered() {
                        ui.painter().rect_stroke(r, CornerRadius::same(4), Stroke::new(1.0, pal.text_dim), StrokeKind::Inside);
                    }
                    if resp.on_hover_text(pic.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).clicked() {
                        open_at = Some(pic.path.clone());
                    }
                }
            });
        }
        ui.add_space(8.0);
        pp::hints(ui, &[("←→↑↓", "day"), ("Shift", "more days"), ("PgUp PgDn", "month"), ("Enter", "view"), ("Esc", "close")], &pal);
    });
    if flip != 0 {
        st.shift_month(flip);
    }
    if let Some((day, run)) = clicked {
        st.pick(app, day, run);
    }
    if let Some(p) = open_at {
        if crate::viewer::open_at(app, Some(p)) {
            return false;
        }
    }
    !pp::clicked_outside(ctx, rect)
}
