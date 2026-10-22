//! Show Space Used: a folder's contents biggest first, each with its share, and a ring
//! beside them in the theme's own colours. The folder is read on a thread; folder sizes
//! come from the folder-size workers (stats.rs) and land as they are counted. The list is
//! sorted again only when sizes arrive, at most a few times a second, never on every
//! frame, and the ring eases to the new shares instead of jumping.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use egui::{Color32, CornerRadius, FontId, Key, Modifiers, Pos2, Rect, Sense, Vec2, pos2, vec2};

use crate::app::{Action, App};
use crate::fsx::Entry;
use crate::popups::{self as pp, Popup};

pub struct Item {
    pub entry: Entry,
    /// Files have theirs at once; a folder's is None until it is counted.
    pub size: Option<u64>,
}

/// A slice of the ring: an item, or everything too small for a slice of its own.
struct Slice {
    /// An index into `items`, or `OTHER`.
    key: u32,
    /// Where it starts and ends as shares of the circle, easing towards the counted ones.
    a: f32,
    b: f32,
    /// How far it stands out while pointed at, 0 to 1.
    lift: f32,
}

/// The key of the slice that holds the small items.
const OTHER: u32 = u32::MAX;
/// Items with a slice of their own, at most: past eight the colours stop telling apart.
const SLICES: usize = 8;
/// The dialog fills the window (F). Kept for the next time it opens.
static FULL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A smaller share than this goes into the shared slice; it would be a sliver.
const MIN_SHARE: f32 = 0.015;

pub struct State {
    /// Where it was opened: Backspace goes no higher.
    pub root: PathBuf,
    pub dir: PathBuf,
    listing: Option<mpsc::Receiver<(Vec<Entry>, Option<u64>)>>,
    /// Free space on the folder's filesystem, read with the listing.
    free: Option<u64>,
    slices: Vec<Slice>,
    /// When the listing landed: the ring sweeps round from it.
    shown_at: Option<f64>,
    /// Others fade back while one slice is pointed at, 0 to 1.
    dim: f32,
    /// The slice whose row the pointer was over last frame.
    row_hover: Option<u32>,
    /// The dialog's height besides the list, as last drawn: the list takes what is left
    /// of the window when the dialog fills it.
    chrome_h: f32,
    /// The buttons' size as last drawn: the footer sets its keys beside them.
    buttons: Vec2,
    pub items: Vec<Item>,
    /// Items in the order shown.
    pub order: Vec<u32>,
    /// Folders whose size hasn't come yet.
    waiting: Vec<u32>,
    /// A size came since the last sort.
    unsorted: bool,
    sorted_at: Instant,
    /// The row with the cursor (a position in `order`).
    pub sel: usize,
    pub checked: HashSet<PathBuf>,
    /// The folder to put the cursor on once a listing lands (the one just left).
    select: Option<PathBuf>,
    /// Said under the list in place of the totals, until the next key.
    pub note: Option<String>,
    /// Asking whether to move the checked items to the trash.
    pub confirm: bool,
    reveal: bool,
    /// The cursor was put somewhere: it follows its item as sizes reorder the list;
    /// until then it stays on the first row, the biggest.
    moved: bool,
}

/// Show Space Used for the folder the command targets.
pub fn open(app: &mut App) {
    match crate::devtools::target_dir(app) {
        Some(d) => {
            let ctx = app.ctx.clone();
            app.popup = Popup::SpaceUsed(Box::new(State::new(d, &ctx)));
        }
        None => app.toast("Show Space Used needs a folder"),
    }
}

impl State {
    pub fn new(root: PathBuf, ctx: &egui::Context) -> State {
        let mut st = State { root: root.clone(), dir: root.clone(), listing: None, free: None, slices: vec![], shown_at: None, dim: 0.0, row_hover: None, chrome_h: 230.0, buttons: vec2(330.0, 28.0), items: vec![], order: vec![], waiting: vec![], unsorted: false, sorted_at: Instant::now(), sel: 0, checked: HashSet::new(), select: None, note: None, confirm: false, reveal: false, moved: false };
        st.go(root, None, ctx);
        st
    }

    /// Show `dir`, the cursor going to `select` once it is read.
    fn go(&mut self, dir: PathBuf, select: Option<PathBuf>, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let (d, c) = (dir.clone(), ctx.clone());
        let _ = std::thread::Builder::new().name("space used".into()).spawn(move || {
            let mut v = vec![];
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    if let Some(en) = Entry::from_path(&e.path()) {
                        v.push(en);
                    }
                }
            }
            // statvfs waits on a dead mount like read_dir does: here, never in the frame.
            let free = crate::fsx::disk_space(&d).map(|s| s.1);
            let _ = tx.send((v, free));
            c.request_repaint();
        });
        self.dir = dir;
        self.listing = Some(rx);
        self.items.clear();
        self.order.clear();
        self.waiting.clear();
        self.checked.clear();
        self.sel = 0;
        self.select = select;
        self.moved = self.select.is_some();
        self.confirm = false;
        self.slices.clear();
        self.shown_at = None;
        self.row_hover = None;
    }

    /// Everything counted so far.
    pub fn total(&self) -> u64 {
        self.items.iter().filter_map(|i| i.size).sum()
    }

    pub fn counting(&self) -> bool {
        self.listing.is_some() || !self.waiting.is_empty()
    }

    fn current(&self) -> Option<&Item> {
        self.items.get(*self.order.get(self.sel)? as usize)
    }

    /// Biggest first; folders not counted yet after them, by name.
    fn sort(&mut self) {
        let items = &self.items;
        let at = self.current().filter(|_| self.moved).map(|i| i.entry.path.clone());
        let mut order: Vec<u32> = (0..items.len() as u32).collect();
        crate::tab::merge_sort(&mut order, &|a: &u32, b: &u32| {
            let (x, y) = (&items[*a as usize], &items[*b as usize]);
            match (x.size, y.size) {
                (Some(p), Some(q)) => q.cmp(&p).then_with(|| crate::tab::natural_cmp(&x.entry.name.to_lowercase(), &y.entry.name.to_lowercase())),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => crate::tab::natural_cmp(&x.entry.name.to_lowercase(), &y.entry.name.to_lowercase()),
            }
        });
        self.order = order;
        // The cursor stays on its item while others move past it.
        if let Some(p) = at {
            if let Some(i) = self.order.iter().position(|&k| self.items[k as usize].entry.path == p) {
                self.sel = i;
            }
        }
        self.unsorted = false;
        self.sorted_at = Instant::now();
    }

    /// Take in a listing that landed and sizes that were counted.
    pub fn pump(&mut self, stats: &mut crate::stats::Stats, ctx: &egui::Context) {
        if let Some(rx) = &self.listing {
            if let Ok((v, free)) = rx.try_recv() {
                self.listing = None;
                self.free = free;
                self.shown_at = Some(ctx.input(|i| i.time));
                self.items = v.into_iter().map(|e| Item { size: (!e.is_dir || e.is_link).then_some(e.size), entry: e }).collect();
                self.waiting = (0..self.items.len() as u32).filter(|&i| self.items[i as usize].size.is_none()).collect();
                for &i in &self.waiting {
                    stats.request(&self.items[i as usize].entry.path);
                }
                self.sort();
                if let Some(p) = self.select.take() {
                    self.sel = self.order.iter().position(|&k| self.items[k as usize].entry.path == p).unwrap_or(0);
                    self.reveal = true;
                }
            }
        }
        if !self.waiting.is_empty() {
            let items = &mut self.items;
            let before = self.waiting.len();
            self.waiting.retain(|&i| {
                let it = &mut items[i as usize];
                match stats.map.get(&it.entry.path) {
                    Some(s) => {
                        it.size = Some(s.size);
                        false
                    }
                    None => {
                        // Forgotten since it was asked for (its contents changed): again.
                        stats.request(&it.entry.path);
                        true
                    }
                }
            });
            self.unsorted |= self.waiting.len() != before;
        }
        if self.unsorted {
            let wait = Duration::from_millis(250).saturating_sub(self.sorted_at.elapsed());
            if wait.is_zero() || self.waiting.is_empty() {
                self.sort();
            } else {
                ctx.request_repaint_after(wait);
            }
        }
    }

    fn flip(&mut self, row: usize) {
        if let Some(p) = self.order.get(row).map(|&k| self.items[k as usize].entry.path.clone()) {
            if !self.checked.remove(&p) {
                self.checked.insert(p);
            }
        }
    }

    fn checked_size(&self) -> u64 {
        self.items.iter().filter(|i| self.checked.contains(&i.entry.path)).filter_map(|i| i.size).sum()
    }

    /// The ring as counted so far: (key, share) clockwise from the top, biggest first,
    /// and the size and count of what went into the shared slice.
    fn goals(&self) -> (Vec<(u32, f32)>, u64, usize) {
        let total = self.total();
        let (mut v, mut rest, mut rest_n) = (Vec::with_capacity(SLICES + 1), 0u64, 0usize);
        if total == 0 {
            return (v, 0, 0);
        }
        for &k in &self.order {
            let Some(s) = self.items[k as usize].size.filter(|&s| s > 0) else { continue };
            let share = s as f32 / total as f32;
            if v.len() < SLICES && share >= MIN_SHARE {
                v.push((k, share));
            } else {
                rest += s;
                rest_n += 1;
            }
        }
        if rest > 0 {
            v.push((OTHER, rest as f32 / total as f32));
        }
        (v, rest, rest_n)
    }

    /// Ease the slices a step (`k` of the way; 1 snaps) towards `goals`: a slice that is
    /// new grows from nothing where it belongs, unless the ring is still sweeping round,
    /// and one that went shrinks into its middle. Returns whether anything still moves.
    fn settle(&mut self, goals: &[(u32, f32)], k: f32, focus: Option<u32>, sweeping: bool) -> bool {
        let ease = |x: &mut f32, to: f32| {
            let d = to - *x;
            if d.abs() < 1e-4 {
                *x = to;
                false
            } else {
                *x += d * k;
                true
            }
        };
        let mut moving = false;
        let mut at = 0.0;
        for &(key, share) in goals {
            let (a, b) = (at, at + share);
            at = b;
            match self.slices.iter_mut().find(|s| s.key == key) {
                Some(s) => {
                    moving |= ease(&mut s.a, a);
                    moving |= ease(&mut s.b, b);
                }
                None => {
                    let grow = k < 1.0 && !sweeping;
                    self.slices.push(Slice { key, a, b: if grow { a } else { b }, lift: 0.0 });
                    moving |= grow;
                }
            }
        }
        for s in &mut self.slices {
            if !goals.iter().any(|g| g.0 == s.key) {
                let m = (s.a + s.b) / 2.0;
                moving |= ease(&mut s.a, m);
                moving |= ease(&mut s.b, m);
            }
            moving |= ease(&mut s.lift, if focus == Some(s.key) { 1.0 } else { 0.0 });
        }
        self.slices.retain(|s| s.b - s.a > 1e-4 || goals.iter().any(|g| g.0 == s.key));
        moving |= ease(&mut self.dim, if focus.is_some() { 1.0 } else { 0.0 });
        moving
    }
}

/// The ring's colours: the accent, then the theme's code colours, which every theme
/// picks to sit together on its background; one too close to an earlier one is passed
/// over, so neighbouring slices never look alike.
fn series(pal: &crate::theme::Palette) -> Vec<Color32> {
    let s = &pal.syntax;
    let mut out: Vec<Color32> = Vec::with_capacity(8);
    for c in [pal.accent, s.function, s.string, s.number, s.keyword, s.kind, s.property, s.meta] {
        let near = |o: &Color32| (c.r() as i32 - o.r() as i32).abs() + (c.g() as i32 - o.g() as i32).abs() + (c.b() as i32 - o.b() as i32).abs() < 60;
        if !out.iter().any(near) {
            out.push(c);
        }
    }
    out
}

/// The shared slice's grey: the dim text, quietened towards the dialog.
fn other_color(pal: &crate::theme::Palette) -> Color32 {
    pal.text_dim.lerp_to_gamma(pal.raised, 0.4)
}

/// A share as a percentage: whole numbers, "<1%" for a sliver.
fn percent(share: f32) -> String {
    if share <= 0.0 {
        String::new()
    } else if share < 0.01 {
        "<1%".into()
    } else {
        format!("{:.0}%", share * 100.0)
    }
}

/// One slice of a ring from angle `a0` to `a1` (radians; the screen's y runs down, so
/// they go clockwise) between radii `r0` and `r1`. Its sides stand `gap` / 2 back from
/// the angles as parallel lines, so neighbours keep the same gap all the way out, and its
/// corners are rounded by `corner`. A mesh of its own with a one-pixel feather round the
/// edge: egui fills only convex paths, and a slice is not one.
fn paint_slice(p: &egui::Painter, c: Pos2, r0: f32, r1: f32, a0: f32, a1: f32, gap: f32, corner: f32, col: Color32) {
    let h = gap / 2.0;
    let across = r0 * (a1 - a0) - gap;
    if across < 0.75 || r0 <= h + 1.0 {
        return;
    }
    let cr = corner.min(across / 2.0 - 0.25).min((r1 - r0) / 2.0 - 0.5).max(0.0);
    let steps = if cr >= 0.5 { 5 } else { 0 };
    // A corner from where it meets the side to where it meets the circle; `sign` is 1 at
    // the start side, -1 at the end.
    let corner_pts = |out: &mut Vec<Vec2>, a: f32, sign: f32, outer: bool| -> f32 {
        let n = vec2(-a.sin(), a.cos()) * sign;
        let rc = if outer { r1 - cr } else { r0 + cr };
        let d = h + cr;
        let k = vec2(a.cos(), a.sin()) * (rc * rc - d * d).max(0.0).sqrt() + n * d;
        let to = if outer { k / rc } else { -k / rc };
        let f = (-n.y).atan2(-n.x);
        let mut sweep = to.y.atan2(to.x) - f;
        while sweep > std::f32::consts::PI {
            sweep -= std::f32::consts::TAU;
        }
        while sweep < -std::f32::consts::PI {
            sweep += std::f32::consts::TAU;
        }
        let start = out.len();
        for i in 0..=steps {
            let s = f + sweep * i as f32 / steps.max(1) as f32;
            out.push(k + vec2(s.cos(), s.sin()) * cr);
        }
        if sign < 0.0 {
            out[start..].reverse();
        }
        k.y.atan2(k.x)
    };
    // Both edges walk from the start side to the end side with as many points each, so
    // the fill pairs them into quads.
    let edge = |outer: bool| -> Option<Vec<Vec2>> {
        let r = if outer { r1 } else { r0 };
        let mut v = Vec::with_capacity(64);
        let s = corner_pts(&mut v, a0, 1.0, outer);
        let mut tail = Vec::with_capacity(steps + 1);
        let mut e = corner_pts(&mut tail, a1, -1.0, outer);
        // atan2 wraps at the left; the end is always past the start.
        while e < s {
            e += std::f32::consts::TAU;
        }
        if e - s > std::f32::consts::TAU - 1e-3 {
            return None;
        }
        let m = (((a1 - a0) * r1 / 4.0).ceil() as usize).clamp(2, 200);
        for i in 1..m {
            let t = s + (e - s) * i as f32 / m as f32;
            v.push(vec2(t.cos(), t.sin()) * r);
        }
        v.extend(tail);
        Some(v)
    };
    let (Some(o), Some(i)) = (edge(true), edge(false)) else { return };
    if o.len() != i.len() {
        return;
    }
    let outline: Vec<Vec2> = o.iter().copied().chain(i.iter().rev().copied()).collect();
    let l = outline.len();
    let area: f32 = (0..l).map(|j| outline[j].x * outline[(j + 1) % l].y - outline[(j + 1) % l].x * outline[j].y).sum();
    let out_normal = |e: Vec2| {
        let len = e.length();
        if len < 1e-4 { Vec2::ZERO } else if area > 0.0 { vec2(e.y, -e.x) / len } else { vec2(-e.y, e.x) / len }
    };
    let hf = 0.5 / p.ctx().pixels_per_point();
    let mut mesh = egui::Mesh::default();
    mesh.reserve_vertices(2 * l);
    mesh.reserve_triangles(2 * l + 2 * o.len());
    for j in 0..l {
        let (prev, cur, next) = (outline[(j + l - 1) % l], outline[j], outline[(j + 1) % l]);
        let (np, nn) = (out_normal(cur - prev), out_normal(next - cur));
        let sum = np + nn;
        let n = if sum.length() > 1e-3 { sum.normalized() } else if nn != Vec2::ZERO { nn } else { np };
        mesh.colored_vertex(c + cur - n * hf, col);
        mesh.colored_vertex(c + cur + n * hf, Color32::TRANSPARENT);
    }
    let v = |j: usize| 2 * j as u32;
    for k in 0..o.len() - 1 {
        let (o0, o1, i1, i0) = (k, k + 1, l - 2 - k, l - 1 - k);
        mesh.add_triangle(v(o0), v(o1), v(i1));
        mesh.add_triangle(v(o0), v(i1), v(i0));
    }
    for j in 0..l {
        let n = (j + 1) % l;
        mesh.add_triangle(v(j), v(j) + 1, v(n) + 1);
        mesh.add_triangle(v(j), v(n) + 1, v(n));
    }
    p.add(egui::Shape::mesh(mesh));
}

/// What the ring reports back for the frame.
#[derive(Default)]
struct RingOut {
    /// The slice under the pointer.
    hover: Option<u32>,
    clicked: Option<u32>,
}

/// The ring in `r`, with what is pointed at (or else the folder) in its middle.
#[allow(clippy::too_many_arguments)]
fn ring(ui: &mut egui::Ui, r: Rect, st: &mut State, goals: &[(u32, f32)], other: (u64, usize), colors: &[Color32], anim: bool, round: bool, fs: f32, pal: &crate::theme::Palette) -> RingOut {
    use std::f32::consts::{FRAC_PI_2, TAU};
    // The caller has laid out the space; claiming it again would move the cursor back up
    // to the ring's foot, and what follows would start there.
    let resp = ui.interact(r, egui::Id::new("space ring"), Sense::click());
    crate::ui::note_control(ui, r, "space ring");
    let c = r.center();
    let r1 = (r.width().min(r.height()) / 2.0 - 6.0).floor();
    let thick = (r1 * 0.2).round().min(44.0);
    let r0 = r1 - thick;
    let mut out = RingOut::default();
    if let Some(at) = resp.hover_pos() {
        let d = at - c;
        let dist = d.length();
        if dist >= r0 - 2.0 && dist <= r1 + 6.0 {
            let f = ((d.y.atan2(d.x) + FRAC_PI_2).rem_euclid(TAU)) / TAU;
            out.hover = st.slices.iter().find(|s| f >= s.a && f < s.b && goals.iter().any(|g| g.0 == s.key)).map(|s| s.key);
        }
    }
    if out.hover.is_some_and(|k| k != OTHER && st.items.get(k as usize).is_some_and(|i| i.entry.is_dir && !i.entry.is_link)) {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if resp.clicked() {
        out.clicked = out.hover;
    }
    let cursor = st.current().filter(|_| st.moved).map(|_| st.order[st.sel]).map(|k| if goals.iter().any(|g| g.0 == k) { k } else { OTHER });
    let focus = out.hover.or(st.row_hover).or(cursor);
    let sweep = match st.shown_at {
        Some(t) if anim => {
            let x = ((ui.ctx().input(|i| i.time) - t) as f32 / 0.7).clamp(0.0, 1.0);
            1.0 - (1.0 - x).powi(3)
        }
        Some(_) => 1.0,
        None => 0.0,
    };
    let k = if anim { 1.0 - (-ui.ctx().input(|i| i.stable_dt).min(0.1) * 12.0).exp() } else { 1.0 };
    let moving = st.settle(goals, k, focus, sweep < 1.0);
    if moving || sweep < 1.0 {
        ui.ctx().request_repaint();
    }
    let p = ui.painter();
    let track = pal.raised.lerp_to_gamma(pal.text, 0.07);
    if goals.is_empty() {
        p.circle_stroke(c, (r0 + r1) / 2.0, egui::Stroke::new(thick, track));
    }
    let corner = if round { 4.0 } else { 0.0 };
    for s in &st.slices {
        let b = s.b.min(sweep);
        if b <= s.a {
            continue;
        }
        let i = goals.iter().position(|g| g.0 == s.key);
        let base = match i {
            Some(_) if s.key == OTHER => other_color(pal),
            Some(i) => colors[i % colors.len()],
            // Leaving: it keeps the colour it shrinks in.
            None => track,
        };
        let col = if focus == Some(s.key) { base } else { base.lerp_to_gamma(pal.raised, 0.6 * st.dim) };
        let a = -FRAC_PI_2 + s.a * TAU;
        paint_slice(p, c, r0, r1 + 4.0 * s.lift, a, -FRAC_PI_2 + b * TAU, 3.0, corner, col);
    }
    // The middle: the slice pointed at, or the folder and its total.
    let room = (r0 * 2.0 * 0.78).max(20.0);
    let total = st.total();
    let (label, value, sub) = match focus {
        Some(OTHER) if other.1 == 1 => {
            let it = st.order.iter().find(|&&k| st.items[k as usize].size.is_some_and(|s| s > 0) && !goals.iter().any(|g| g.0 == k)).map(|&k| &st.items[k as usize]);
            (it.map_or_else(|| "Other".to_string(), |i| i.entry.name.clone()), crate::fsx::fmt_size(other.0), percent(other.0 as f32 / total.max(1) as f32))
        }
        Some(OTHER) => (format!("{} smaller items", crate::table::fmt_count(other.1)), crate::fsx::fmt_size(other.0), percent(other.0 as f32 / total.max(1) as f32)),
        Some(k) => match st.items.get(k as usize) {
            Some(it) => (it.entry.name.clone(), it.size.map(crate::fsx::fmt_size).unwrap_or_else(|| "…".into()), it.size.map(|s| percent(s as f32 / total.max(1) as f32)).unwrap_or_default()),
            None => (String::new(), String::new(), String::new()),
        },
        None => {
            let name = st.dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| st.dir.display().to_string());
            (name, if st.listing.is_some() { String::new() } else { crate::fsx::fmt_size(total) }, String::new())
        }
    };
    let lg = crate::listing::elided(ui, &label, FontId::proportional(fs - 1.0), room, pal.text_dim);
    let vg = ui.painter().layout_no_wrap(value, FontId::proportional((r0 * 0.2).clamp(fs * 1.85, fs * 3.0).round()), pal.text);
    let sg = ui.painter().layout_no_wrap(sub, FontId::proportional(fs - 1.0), pal.text_dim);
    let gap = 2.0;
    let mut h = lg.size().y + gap + vg.size().y;
    if !sg.is_empty() {
        h += gap + sg.size().y;
    }
    let mut y = c.y - h / 2.0;
    let p = ui.painter();
    p.galley(pos2(c.x - lg.size().x / 2.0, y), lg.clone(), pal.text_dim);
    y += lg.size().y + gap;
    if st.listing.is_some() {
        crate::ui::spinner(p, pos2(c.x, y + vg.size().y.max(fs) / 2.0), 6.0, pal.text_dim, ui.ctx().input(|i| i.time));
        ui.ctx().request_repaint();
    }
    p.galley(pos2(c.x - vg.size().x / 2.0, y), vg.clone(), pal.text);
    y += vg.size().y + gap;
    if !sg.is_empty() {
        p.galley(pos2(c.x - sg.size().x / 2.0, y), sg, pal.text_dim);
    }
    out
}

/// The dialog. Returns false when it closes.
pub fn popup(app: &mut App, ctx: &egui::Context, st: &mut State) -> bool {
    let pal = app.pal;
    st.pump(&mut app.stats, ctx);
    let k = pp::take_keys(ctx);
    let (space, delete) = (crate::devtools::take(ctx, Modifiers::NONE, Key::Space), crate::devtools::take(ctx, Modifiers::NONE, Key::Delete));
    let backspace = k.backspace && !st.confirm;
    if k.esc {
        if st.confirm {
            st.confirm = false;
        } else {
            return false;
        }
    }
    if k.up || k.down || k.page_up || k.page_down || space || backspace || k.enter {
        st.note = None;
    }
    let n = st.order.len();
    if !st.confirm {
        let before = st.sel;
        pp::step(&mut st.sel, n, &k, |_| false);
        if crate::devtools::take(ctx, Modifiers::NONE, Key::Home) {
            st.sel = 0;
        }
        if crate::devtools::take(ctx, Modifiers::NONE, Key::End) {
            st.sel = n.saturating_sub(1);
        }
        st.reveal |= st.sel != before;
        st.moved |= st.sel != before;
    }
    if space && !st.confirm {
        let at = st.sel;
        st.flip(at);
    }
    let fs = app.cfg.font_size;
    let small = fs - 1.5;
    let row_h = (fs * 1.35 + 12.0).round();
    let list_max = pp::fit_list(ctx, 460.0, 220.0);
    let total = st.total();
    let (mut enter, mut show, mut trash, mut cancel, mut close) = (None, false, (delete || k.enter && st.confirm) && !st.checked.is_empty(), false, false);
    if k.enter && !st.confirm {
        enter = Some(st.sel);
    }
    let mut toggle = None;
    let counting = st.counting();
    let now = ctx.input(|i| i.time);
    let (goals, other_size, other_n) = st.goals();
    let colors = series(&pal);
    let (anim, round) = (app.cfg.animations, app.cfg.round_corners);
    // The slice each row belongs to, for its dot and to light it with the ring.
    let slice_of = |k: u32, it: &Item| -> Option<(u32, usize)> {
        match goals.iter().position(|g| g.0 == k) {
            Some(i) => Some((k, i)),
            None if it.size.is_some_and(|s| s > 0) && goals.last().is_some_and(|g| g.0 == OTHER) => Some((OTHER, goals.len() - 1)),
            None => None,
        }
    };
    let mut ring_click = None;
    use std::sync::atomic::Ordering::Relaxed;
    let mut flip_full = !st.confirm && crate::devtools::take(ctx, Modifiers::NONE, Key::F);
    let full = FULL.load(Relaxed);
    // Growing to the window and back, eased at both ends.
    let t = if anim { ctx.animate_bool_with_time(egui::Id::new("space full"), full, 0.2) } else if full { 1.0 } else { 0.0 };
    let e = t * t * (3.0 - 2.0 * t);
    let lerp = |a: f32, b: f32| a + (b - a) * e;
    let screen = ctx.content_rect();
    let top = lerp(pp::popup_top(screen), 12.0).round();
    let width = lerp(780.0, screen.width() - 24.0).round();
    // As `centered` lets it grow before it scrolls.
    let room = (screen.height() - top - 12.0 - 2.0 * pp::DIALOG_PAD).max(60.0);
    // The list's height when the dialog fills the window: all the window leaves it.
    // Filling the window, ten points more round everything than a dialog's own padding.
    let pad = lerp(0.0, 10.0).round();
    let full_h = (room - st.chrome_h - 2.0 * pad - 4.0).max(list_max);
    let list_cap = lerp(list_max, full_h);
    let list_natural = n as f32 * row_h;
    let mut body_h = 0.0;
    pp::centered_top(ctx, "space used", width, top, &pal, |ui| {
        egui::Frame::NONE.inner_margin(egui::Margin::same(pad as i8)).show(ui, |ui| {
        let y0 = ui.cursor().top();
        // Title, ring and list, and the footer share one column and its two edges. Filling
        // the window the column stops growing at 1120 points and stands in the middle: a
        // list stretched across a wide window puts each size a long way from its name.
        let page = ui.available_width();
        let avail = lerp(page, page.min(1120.0)).round();
        let col = ui.max_rect().left() + ((page - avail) / 2.0).round();
        // Past about 400 points a ring stops reading better and only crowds the list.
        let ring_dialog = (avail * 0.3).clamp(200.0, 236.0).min(list_max);
        let ring_full = (full_h - 24.0).min(avail * 0.36).min(400.0);
        let ring_d = lerp(ring_dialog, ring_full).round();
        let gutter = lerp(28.0, 64.0).round();
        // In a narrow window the list keeps the room and the total goes under the title.
        let wide = avail >= 600.0 && ring_d >= 160.0;
        let (ring_d, gutter) = if wide { (ring_d, gutter) } else { (0.0, 0.0) };
        let list_w = avail - ring_d - gutter;
        let inset = col - ui.max_rect().left();
        let list_h = list_natural.min(list_cap);
        let mut what = pp::home_short(&st.dir);
        if !wide {
            what += &format!(" · {}{}", crate::fsx::fmt_size(total), if counting { " so far" } else { "" });
        }
        if let Some(f) = st.free {
            what += &format!(" · {} free", crate::fsx::fmt_size(f));
        }
        let title_h = ui.painter().layout_no_wrap("Space used".into(), FontId::proportional(ui.style().text_styles.get(&egui::TextStyle::Body).map_or(fs, |f| f.size) + 1.5), pal.text).size().y;
        ui.horizontal(|ui| {
            ui.add_space(inset);
            ui.vertical(|ui| {
                ui.set_max_width(avail - 36.0);
                pp::dialog_title(ui, "Space used", Some(&what), &pal);
            });
        });
        // On the title's line, its middle with the title's; its right edge on the column's.
        let br = Rect::from_center_size(pos2(col + avail - 13.0 + 4.0, y0 + title_h / 2.0), vec2(26.0, 24.0));
        let b = ui.interact(br, egui::Id::new("space full button"), Sense::click());
        crate::ui::note_control(ui, br, "space full");
        if b.hovered() {
            ui.painter().rect_filled(br, CornerRadius::same(5), pal.row_hover);
        }
        crate::theme::paint_symbol(ui.painter(), Rect::from_center_size(br.center(), Vec2::splat(15.0)), if full { "minimize-2" } else { "maximize-2" }, if b.hovered() { pal.text } else { pal.text_dim });
        flip_full |= b.on_hover_text(if full { "Back to a dialog (F)" } else { "Fill the window (F)" }).clicked();
        ui.add_space(lerp(18.0, 24.0).round());
        // Ring and list are centred on each other: whichever is shorter stands in the
        // middle of the other's height, and filling the window both stand in its middle.
        let bh = ring_d.max(list_h).max(lerp(0.0, full_h)).round();
        let (body, _) = ui.allocate_exact_size(vec2(page, bh), Sense::hover());
        let mut ring_hover = None;
        if wide {
            // The ring keeps 6 points round itself for a slice standing out; its drawn edge,
            // not that margin, lines up with the title.
            let rr = Rect::from_min_size(pos2(body.left() + inset, (body.center().y - ring_d / 2.0).round()), Vec2::splat(ring_d)).expand(6.0);
            let o = ring(ui, rr, st, &goals, (other_size, other_n), &colors, anim, round, fs, &pal);
            ring_hover = o.hover;
            ring_click = o.clicked;
        }
        let lr = Rect::from_min_size(pos2(body.left() + inset + ring_d + gutter, (body.center().y - list_h / 2.0).round()), vec2(list_w, list_h));
        let mut lui = ui.new_child(egui::UiBuilder::new().max_rect(lr).layout(egui::Layout::top_down(egui::Align::Min)));
        {
            let ui = &mut lui;
            {
                let mut row_hover = None;
                if n > 0 {
                    let mut area = egui::ScrollArea::vertical().id_salt("space list").max_height(list_h).min_scrolled_height(list_h).auto_shrink([false, false]);
                    let list_cap = list_h;
                    if std::mem::take(&mut st.reveal) {
                        let top = st.sel as f32 * row_h;
                        let most = (n as f32 * row_h - list_cap).max(0.0);
                        area = area.vertical_scroll_offset((top - list_cap / 2.0 + row_h).clamp(0.0, most));
                    }
                    ui.spacing_mut().item_spacing.y = 0.0;
                    area.show_rows(ui, row_h, n, |ui, range| {
                        for row in range {
                            let k = st.order[row];
                            let it = &st.items[k as usize];
                            let slice = slice_of(k, it);
                            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
                            crate::ui::note_control(ui, r, "space row");
                            let on = row == st.sel;
                            let lit = ring_hover.is_some() && slice.map(|s| s.0) == ring_hover;
                            if resp.hovered() {
                                row_hover = slice.map(|s| s.0);
                            }
                            if on {
                                ui.painter().rect_filled(r, CornerRadius::same(6), pal.selected);
                            } else if resp.hovered() || lit {
                                ui.painter().rect_filled(r, CornerRadius::same(6), pal.hover);
                            }
                            let (tc, dc) = if on { (pal.on_sel(), pal.on_sel_dim()) } else { (pal.text, pal.text_dim) };
                            // 12 points in from the row's edge, as the share is from the other.
                            let cb = Rect::from_center_size(pos2(r.left() + 19.0, r.center().y), Vec2::splat(14.0));
                            pp::checkbox_glyph(ui.painter(), cb, st.checked.contains(&it.entry.path), &pal);
                            if resp.clicked() {
                                let at_box = resp.interact_pointer_pos().is_some_and(|p| p.x < cb.right() + 6.0);
                                if at_box {
                                    toggle = Some(row);
                                } else if it.entry.is_dir && !it.entry.is_link {
                                    enter = Some(row);
                                } else {
                                    st.sel = row;
                                    st.moved = true;
                                }
                            }
                            // The dot ties the row to its slice; a row with nothing counted has none.
                            // As far from the box as from the icon.
                            let dot = pos2(cb.right() + 10.5, r.center().y);
                            if wide {
                                match slice {
                                    Some((OTHER, _)) => {
                                        ui.painter().circle_filled(dot, 3.5, other_color(&pal));
                                    }
                                    Some((_, i)) => {
                                        ui.painter().circle_filled(dot, 3.5, colors[i % colors.len()]);
                                    }
                                    None => {}
                                }
                            }
                            let ic = Rect::from_center_size(pos2(if wide { dot.x + 18.5 } else { cb.right() + 18.0 }, r.center().y), Vec2::splat(16.0));
                            if !crate::icons::entry(ui.painter(), ic, &it.entry) {
                                crate::theme::paint_icon(ui.painter(), ic, crate::theme::categorize(it.entry.ext(), it.entry.is_dir, it.entry.is_executable()), it.entry.ext(), it.entry.is_link);
                            }
                            // The share at the edge, the size before it, the name in what is left.
                            let pct_w = ui.painter().layout_no_wrap("100%".into(), FontId::proportional(small), dc).size().x;
                            let edge = r.right() - 12.0;
                            let share = match it.size {
                                Some(s) if total > 0 => percent(s as f32 / total as f32),
                                _ => String::new(),
                            };
                            let pg = ui.painter().layout_no_wrap(share, FontId::proportional(small), tc);
                            ui.painter().galley(pos2(edge - pg.size().x, r.center().y - pg.size().y / 2.0), pg, tc);
                            let size_right = edge - pct_w - 14.0;
                            let left = match it.size {
                                Some(s) => {
                                    let sg = ui.painter().layout_no_wrap(crate::fsx::fmt_size(s), FontId::proportional(small), dc);
                                    let x = size_right - sg.size().x;
                                    ui.painter().galley(pos2(x, r.center().y - sg.size().y / 2.0), sg, dc);
                                    x
                                }
                                None => {
                                    crate::ui::spinner(ui.painter(), pos2(size_right - 5.0, r.center().y), 4.5, dc, now);
                                    size_right - 10.0
                                }
                            };
                            let x = ic.right() + 10.0;
                            let room = left - 16.0 - x;
                            let g = crate::listing::elided(ui, &it.entry.name, FontId::proportional(fs), room, tc);
                            ui.painter().galley(pos2(x, r.center().y - g.size().y / 2.0), g, tc);
                        }
                    });
                    if counting {
                        ctx.request_repaint_after(Duration::from_millis(100));
                    }
                }
                st.row_hover = row_hover;
            }
        }
        body_h = bh;
        ui.add_space(lerp(16.0, 24.0).round());
        let chosen = st.checked.len();
        let (status, col_status) = if st.confirm {
            let what = if chosen == 1 { "1 item".to_string() } else { format!("{chosen} items") };
            (format!("Move {what} ({}) to the trash?", crate::fsx::fmt_size(st.checked_size())), pal.danger)
        } else if let Some(t) = &st.note {
            (t.clone(), pal.text_dim)
        } else if st.listing.is_some() {
            ("Reading the folder…".to_string(), pal.text_dim)
        } else if n == 0 {
            ("This folder is empty".to_string(), pal.text_dim)
        } else if counting {
            (format!("Counting {} folders…", crate::table::fmt_count(st.waiting.len())), pal.text_dim)
        } else {
            let items = if n == 1 { "1 item".to_string() } else { format!("{} items", crate::table::fmt_count(n)) };
            (if chosen > 0 { format!("{items}; {chosen} checked, {}", crate::fsx::fmt_size(st.checked_size())) } else { items }, pal.text_dim)
        };
        // The footer: a rule across the column, then what is going on and the keys on the
        // left, the buttons on the right, the two centred on each other. With no room
        // beside the buttons the keys go above them.
        let y = ui.cursor().top();
        ui.painter().line_segment([pos2(col, y), pos2(col + avail, y)], egui::Stroke::new(1.0, pal.border));
        ui.add_space(14.0);
        let ft = ui.cursor().top();
        // The buttons' width from their labels, this frame: last frame's was a frame late
        // when they changed (asking to trash has two), and the keys ran under them.
        let labels: Vec<String> = if st.confirm {
            vec!["Move to Trash".into(), "Cancel".into()]
        } else {
            let mut v = vec![if chosen == 0 { "Move to Trash".to_string() } else { format!("Move {} to Trash", crate::fsx::fmt_size(st.checked_size())) }, "Close".into()];
            if n > 0 {
                v.push("Show in Maia".into());
            }
            v
        };
        let bfont = egui::TextStyle::Button.resolve(ui.style());
        let pad_x = ui.spacing().button_padding.x;
        let bw: f32 = labels.iter().map(|l| (ui.painter().layout_no_wrap(l.clone(), bfont.clone(), pal.text).size().x + 2.0 * pad_x).max(72.0)).sum::<f32>() + 8.0 * (labels.len() as f32 - 1.0);
        st.buttons.x = bw;
        let side = avail - st.buttons.x - 32.0 >= 320.0;
        let left_w = if side { avail - st.buttons.x - 32.0 } else { avail };
        let mut left = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(pos2(col, ft), vec2(left_w, 400.0))).layout(egui::Layout::top_down(egui::Align::Min)));
        left.label(egui::RichText::new(status).size(small).color(col_status));
        left.add_space(6.0);
        pp::hint_chips(&mut left, &[("Enter", "open folder"), ("Backspace", "up"), ("Space", "check"), ("Delete", "trash"), ("F", "full window"), ("Esc", "close")], &pal);
        let lh = left.min_rect().height();
        let (bt, band) = if side { (ft, lh.max(st.buttons.y)) } else { (ft + lh + 12.0, st.buttons.y) };
        let mut bui = ui.new_child(egui::UiBuilder::new().max_rect(Rect::from_min_size(pos2(col, bt), vec2(avail, band))).layout(egui::Layout::right_to_left(egui::Align::Center)));
        bui.spacing_mut().item_spacing.x = 8.0;
        let mut btn_h: f32 = 0.0;
        {
            let ui = &mut bui;
            if st.confirm {
                let b = pp::primary_button(ui, "Move to Trash", true, &pal);
                crate::ui::note_control(ui, b.rect, "space trash confirm");
                btn_h = btn_h.max(b.rect.height());
                trash |= b.clicked();
                let c = pp::plain_button(ui, "Cancel", &pal);
                crate::ui::note_control(ui, c.rect, "space trash cancel");
                cancel = c.clicked();
            } else {
                let label = if chosen == 0 { "Move to Trash".to_string() } else { format!("Move {} to Trash", crate::fsx::fmt_size(st.checked_size())) };
                let b = pp::primary_button(ui, &label, chosen > 0, &pal);
                crate::ui::note_control(ui, b.rect, "space trash");
                btn_h = btn_h.max(b.rect.height());
                trash |= b.clicked();
                let c = pp::plain_button(ui, "Close", &pal);
                crate::ui::note_control(ui, c.rect, "space close");
                close = c.clicked();
                if n > 0 {
                    let s = pp::plain_button(ui, "Show in Maia", &pal);
                    crate::ui::note_control(ui, s.rect, "space show");
                    show = s.clicked();
                }
            }
        }
        // Their height from the buttons themselves: a centred row is as tall as its room.
        st.buttons.y = btn_h.max(16.0);
        ui.allocate_exact_size(vec2(page, bt + band - ft), Sense::hover());
        st.chrome_h = ui.min_rect().height() - body_h;
        });
    });
    if flip_full {
        FULL.store(!full, Relaxed);
    }
    if close {
        return false;
    }
    if cancel {
        st.confirm = false;
    }
    if let Some(k) = ring_click.filter(|&k| k != OTHER) {
        if let Some(row) = st.order.iter().position(|&o| o == k) {
            if st.items[k as usize].entry.is_dir && !st.items[k as usize].entry.is_link {
                enter = Some(row);
            } else {
                st.sel = row;
                st.moved = true;
                st.reveal = true;
            }
        }
    }
    if let Some(row) = toggle {
        st.sel = row;
        st.moved = true;
        st.flip(row);
    }
    if let Some(row) = enter {
        let dir = st.order.get(row).map(|&k| &st.items[k as usize]).filter(|i| i.entry.is_dir && !i.entry.is_link).map(|i| i.entry.path.clone());
        if let Some(d) = dir {
            st.go(d, None, ctx);
            return true;
        }
    }
    if backspace {
        if st.dir == st.root || !st.dir.starts_with(&st.root) {
            st.note = Some(format!("Space used was opened in {}: it goes no higher", st.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| st.root.display().to_string())));
        } else if let Some(up) = st.dir.parent().map(Path::to_path_buf) {
            let from = st.dir.clone();
            st.go(up, Some(from), ctx);
        }
        return true;
    }
    if show {
        if let Some(p) = st.current().map(|i| i.entry.path.clone()) {
            let panel = app.active;
            app.actions.push(Action::Navigate { panel, path: p });
            return false;
        }
    }
    if trash && !st.checked.is_empty() {
        if !st.confirm {
            st.confirm = true;
            return true;
        }
        let paths: Vec<PathBuf> = st.items.iter().filter(|i| st.checked.contains(&i.entry.path)).map(|i| i.entry.path.clone()).collect();
        app.actions.push(Action::StartJob(crate::ops::OpKind::Trash { paths }));
        // Gone from the list now; the job takes them away.
        let checked = std::mem::take(&mut st.checked);
        let at = st.current().map(|i| i.entry.path.clone());
        st.items.retain(|i| !checked.contains(&i.entry.path));
        st.waiting = (0..st.items.len() as u32).filter(|&i| st.items[i as usize].size.is_none()).collect();
        st.sort();
        st.sel = at.and_then(|p| st.order.iter().position(|&k| st.items[k as usize].entry.path == p)).unwrap_or(st.sel).min(st.order.len().saturating_sub(1));
        st.confirm = false;
    }
    true
}
