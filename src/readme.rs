//! A folder's README above its listing: the start of it, drawn by the Markdown preview,
//! the rest a click away, and gone for that folder when closed. It is looked for among
//! the folder's names once per listing, then read and parsed on a thread.

use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Instant;

use egui::{CornerRadius, FontId, Rect, Sense, Stroke, StrokeKind, Ui, Vec2, pos2, vec2};

use crate::theme::Palette;

/// The README read, at most: the card shows its start.
const MAX: u64 = 64 << 10;

/// Names taken for a README, best first.
const NAMES: [&str; 5] = ["readme.md", "readme.markdown", "readme", "readme.txt", "readme.mdown"];

#[derive(Default)]
pub struct Card {
    /// The folder it belongs to.
    dir: PathBuf,
    /// The listing it was looked for in.
    seen: Option<Instant>,
    pub file: Option<PathBuf>,
    /// The file's size and time when it was read: changed, it is read again.
    stamp: (u64, Option<std::time::SystemTime>),
    view: Option<Box<crate::markdown::View>>,
    rx: Option<mpsc::Receiver<crate::markdown::Doc>>,
    /// Shown whole rather than its start.
    pub open: bool,
}

pub enum Act {
    /// Closed: not shown for this folder again.
    Hide(PathBuf),
    Follow(crate::markdown::Follow),
}

/// The README among `names`, best first.
fn pick<'a>(entries: impl Iterator<Item = &'a crate::fsx::Entry>) -> Option<&'a crate::fsx::Entry> {
    let mut best: Option<(usize, &crate::fsx::Entry)> = None;
    for e in entries {
        if e.is_dir || e.name.len() > 15 {
            continue;
        }
        let l = e.name.to_ascii_lowercase();
        if let Some(k) = NAMES.iter().position(|x| *x == l) {
            if best.as_ref().is_none_or(|b| k < b.0) {
                best = Some((k, e));
            }
        }
    }
    best.map(|b| b.1)
}

/// Keep the tab's card in step with its folder: looked for again after each listing,
/// read again when the README changed.
pub fn sync(t: &mut crate::tab::Tab, ctx: &egui::Context) {
    let card = t.readme.get_or_insert_with(Box::default);
    if card.dir != t.path {
        **card = Card { dir: t.path.clone(), ..Default::default() };
    }
    if t.loading.is_some() || card.seen == t.loaded_at {
        return;
    }
    card.seen = t.loaded_at;
    let found = pick(t.entries.iter().filter(|e| e.depth == 0));
    let (file, stamp) = (found.map(|e| e.path.clone()), found.map_or((0, None), |e| (e.size, e.modified)));
    if file == card.file && stamp == card.stamp && card.view.is_some() {
        return;
    }
    card.file = file.clone();
    card.stamp = stamp;
    card.view = None;
    card.rx = None;
    let Some(f) = file else { return };
    let (tx, rx) = mpsc::channel();
    let ctx = ctx.clone();
    let _ = std::thread::Builder::new().name("readme".into()).spawn(move || {
        use std::io::Read;
        let mut b = vec![];
        if let Some(h) = crate::fsx::open_plain(&f) {
            let _ = h.take(MAX).read_to_end(&mut b);
        }
        let text = String::from_utf8_lossy(&b);
        let lines: Vec<String> = text.lines().map(String::from).collect();
        let _ = tx.send(crate::markdown::parse(&lines, f.parent()));
        ctx.request_repaint();
    });
    card.rx = Some(rx);
}

/// The README's text as parsed, for the tests.
#[cfg(test)]
impl Card {
    pub fn text(&self) -> Option<String> {
        self.view.as_ref().map(|v| format!("{:?}", v.doc.blocks))
    }

    /// The file is still being read.
    pub fn reading(&self) -> bool {
        self.rx.is_some()
    }
}

/// How tall the card wants to be in a list `room` high; 0 for none. `chosen` is the
/// height its edge was dragged to (0: never).
pub fn height(card: &mut Card, size: f32, room: f32, chosen: f32) -> f32 {
    if let Some(doc) = card.rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
        card.rx = None;
        // An empty README is no card.
        card.view = (!doc.blocks.is_empty()).then(|| Box::new(crate::markdown::View::new(doc)));
    }
    let Some(v) = &card.view else { return 0.0 };
    // The list keeps most of the panel: no card where there is too little room.
    if room < size * 22.0 {
        return 0.0;
    }
    let (h, _) = v.height(size);
    let shut = shut_height(size, chosen, room);
    // The first frames lay out nothing yet: the start's height until they have.
    let want = if h <= 0.0 { shut } else { h };
    (if card.open { want.min((room * 0.6).max(shut)) } else { want.min(shut) }).round()
}

/// The start shown: the height the card was dragged to, else a heading and a few lines.
pub fn shut_height(size: f32, chosen: f32, room: f32) -> f32 {
    if chosen > 0.0 { chosen.clamp(min_height(size), max_height(room)) } else { (size * 9.0 + 24.0).round() }
}

/// The least it can be dragged to: a line or two.
pub fn min_height(size: f32) -> f32 {
    (size * 4.0 + 24.0).round()
}

/// The most: the list keeps a fifth of its room.
pub fn max_height(room: f32) -> f32 {
    (room * 0.8).round()
}

/// Draw the card in `r`, the margin around it included.
pub fn draw(ui: &mut Ui, r: Rect, card: &mut Card, size: f32, shut: f32, spacing: f32, pal: &Palette, thumbs: &mut crate::thumbs::Thumbs, id: u64) -> Option<Act> {
    let Some(v) = card.view.as_deref_mut() else { return None };
    let box_r = r.shrink2(vec2(8.0, 6.0));
    let p = ui.painter().clone();
    p.rect_filled(box_r, CornerRadius::same(8), pal.alt);
    p.rect_stroke(box_r, CornerRadius::same(8), Stroke::new(1.0, pal.card_line()), StrokeKind::Inside);
    let body = box_r.shrink(1.0);
    let mut act = None;
    // The start fades out above Show all; open, the link reads Show less. Where the fade
    // or the link's bar is solid, the text under it is not drawn: nothing of it shows.
    let (full, done) = v.height(size);
    let more = full > body.height() + 1.0 || !done && full >= shut;
    let fs = (size - 1.0).max(10.0);
    let link_h = fs + 12.0;
    let strip = Rect::from_min_max(pos2(body.left(), body.bottom() - link_h - if card.open { 0.0 } else { 18.0 }), body.max);
    let steps = 12;
    let solid = if card.open { strip.top() } else { egui::lerp(strip.top()..=strip.bottom(), SOLID_FROM as f32 / steps as f32) };
    let shown = if more || card.open { Rect::from_min_max(body.min, pos2(body.right(), solid)) } else { body };
    let mut inner = ui.new_child(egui::UiBuilder::new().max_rect(body));
    inner.set_clip_rect(shown.intersect(ui.clip_rect()));
    if let Some(f) = crate::markdown::view(&mut inner, body, v, size, pal, spacing, thumbs, id ^ 0x5eed) {
        act = Some(Act::Follow(f));
    }
    if more || card.open {
        if !card.open {
            for k in 0..steps {
                let t0 = k as f32 / steps as f32;
                let y0 = egui::lerp(strip.top()..=strip.bottom(), t0);
                let y1 = egui::lerp(strip.top()..=strip.bottom(), (k + 1) as f32 / steps as f32);
                let a = fade_alpha(k, steps);
                p.rect_filled(Rect::from_x_y_ranges(strip.x_range(), y0..=y1), CornerRadius::ZERO, pal.alt.gamma_multiply(a));
            }
        } else {
            p.rect_filled(Rect::from_min_max(pos2(body.left(), body.bottom() - link_h), body.max), CornerRadius { nw: 0, ne: 0, sw: 7, se: 7 }, pal.alt);
        }
        let label = if card.open { "Show less" } else { "Show all" };
        let g = p.layout_no_wrap(label.into(), FontId::proportional(fs), pal.accent);
        let lr = Rect::from_center_size(pos2(body.center().x, body.bottom() - link_h / 2.0), g.size() + vec2(12.0, 4.0));
        let resp = ui.interact(lr, egui::Id::new(("readme more", id)), Sense::click());
        crate::ui::note_control(ui, lr, "readme more");
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        p.galley(lr.center() - g.size() / 2.0, g, pal.accent);
        if resp.clicked() {
            card.open = !card.open;
        }
    }
    // Close: this folder's README stays out of the way from now on. While the card slides
    // in or out it is drawn (clipped) but not yet a button: the edge below is in its way.
    let cr = Rect::from_center_size(pos2(box_r.right() - 14.0, box_r.top() + 14.0), Vec2::splat(18.0));
    if !settled(r.height()) {
        crate::theme::paint_glyph(&p, cr.shrink(4.0), crate::theme::Glyph::Close, pal.text_dim);
        return act;
    }
    let resp = ui.interact(cr, egui::Id::new(("readme close", id)), Sense::click());
    crate::ui::note_control(ui, cr, "readme close");
    if resp.hovered() {
        p.rect_filled(cr, CornerRadius::same(4), pal.hover);
    }
    crate::theme::paint_glyph(&p, cr.shrink(4.0), crate::theme::Glyph::Close, if resp.hovered() { pal.text } else { pal.text_dim });
    if resp.on_hover_text("Hide this folder's README").clicked() {
        act = Some(Act::Hide(card.dir.clone()));
    }
    act
}

/// How solid the fade over the start is at step `k` of `steps`, from the top.
fn fade_alpha(k: usize, steps: usize) -> f32 {
    ((k + 1) as f32 / steps as f32 * 1.6).min(1.0)
}

/// The first of the fade's twelve steps drawn solid.
const SOLID_FROM: usize = 7;

#[cfg(test)]
pub fn fade_solid_from_ok() -> bool {
    fade_alpha(SOLID_FROM, 12) >= 1.0 && fade_alpha(SOLID_FROM - 1, 12) < 1.0
}

/// Tall enough for the close button and the dragged edge not to meet: the card is out,
/// not sliding.
pub fn settled(h: f32) -> bool {
    h >= 40.0
}
