//! Click by letters: every control on screen gets a short tag of letters; typing a tag
//! clicks it (a capital last letter right-clicks). The controls are the ones egui laid
//! out in the frame before, so whatever can be clicked can be reached, rows, tabs,
//! buttons and menus alike; with a dialog open, only the dialog's.

use egui::{Color32, CornerRadius, Event, FontId, Id, Order, Pos2, Rect, Stroke, StrokeKind, pos2, vec2};

use crate::app::App;
use crate::popups::Popup;

/// Letters in the order they are easiest to type, the home row first.
const KEYS: &str = "asdfjklghqweruiotyzxcvbnmp";

pub struct Hints {
    pub targets: Vec<(Rect, String)>,
    pub typed: String,
}

/// `n` tags, none the start of another: single letters while they last, else two.
pub fn tags(n: usize) -> Vec<String> {
    let keys: Vec<char> = KEYS.chars().collect();
    if n <= keys.len() {
        return keys.iter().take(n).map(|c| c.to_string()).collect();
    }
    let mut v = Vec::with_capacity(n);
    'outer: for a in &keys {
        for b in &keys {
            if v.len() == n {
                break 'outer;
            }
            v.push(format!("{a}{b}"));
        }
    }
    v
}

/// What can be clicked now: the frame before's widgets that take clicks, inside the
/// window, not the size of a whole list's background; with a dialog open only the
/// dialog's layer (it is drawn over the rest). In reading order.
pub fn targets(ctx: &egui::Context, popup: bool) -> Vec<Rect> {
    let screen = ctx.content_rect();
    let mut v: Vec<Rect> = ctx.viewport(|vp| {
        let mut v = vec![];
        for (layer, ws) in vp.prev_pass.widgets.layers() {
            if popup != (layer.order == Order::Foreground) || layer.order == Order::Tooltip {
                continue;
            }
            for w in ws {
                if !w.enabled || !w.sense.senses_click() {
                    continue;
                }
                let r = w.interact_rect.intersect(screen);
                if r.width() < 4.0 || r.height() < 4.0 || r.area() > screen.area() * 0.2 {
                    continue;
                }
                v.push(r);
            }
        }
        v
    });
    // One tag for one place: a button laid over the same rectangle twice is one target,
    // and a bar that holds other targets (the path bar round its parts) is theirs.
    v.dedup_by(|a, b| (a.min - b.min).length() < 1.0 && (a.max - b.max).length() < 1.0);
    let holds = |a: &Rect, b: &Rect| a.area() > b.area() * 1.5 && a.expand(1.0).contains_rect(*b);
    let v: Vec<Rect> = v.iter().filter(|a| !v.iter().any(|b| holds(a, b))).copied().collect();
    let key = |r: &Rect| ((r.top() / 6.0).round() as i32, r.left() as i32);
    let mut order: Vec<u32> = (0..v.len() as u32).collect();
    crate::tab::merge_sort(&mut order, &|a: &u32, b: &u32| key(&v[*a as usize]).cmp(&key(&v[*b as usize])));
    order.into_iter().map(|i| v[i as usize]).collect()
}

pub fn start(app: &mut App, ctx: &egui::Context) {
    let rects = targets(ctx, !matches!(app.popup, Popup::None));
    if rects.is_empty() {
        app.toast("Nothing to click here");
        return;
    }
    let names = tags(rects.len());
    app.hints = Some(Hints { targets: rects.into_iter().zip(names).collect(), typed: String::new() });
    ctx.request_repaint();
}

/// Take the keyboard while tags show: letters narrow, Backspace takes one back, Escape
/// (or anything that matches nothing) gives up; a whole tag clicks its control.
pub fn input(app: &mut App, ctx: &egui::Context) {
    let Some(h) = app.hints.as_mut() else { return };
    let events = ctx.input_mut(|i| {
        let mine: Vec<Event> = i.events.iter().filter(|e| matches!(e, Event::Text(_) | Event::Key { .. })).cloned().collect();
        i.events.retain(|e| !matches!(e, Event::Text(_) | Event::Key { .. }));
        mine
    });
    let mut done = false;
    let mut click: Option<(Pos2, bool)> = None;
    for e in events {
        match e {
            Event::Key { key: egui::Key::Escape, pressed: true, .. } => done = true,
            Event::Key { key: egui::Key::Backspace, pressed: true, .. } => {
                h.typed.pop();
            }
            Event::Text(t) => {
                for c in t.chars() {
                    let right = c.is_uppercase();
                    h.typed.push(c.to_ascii_lowercase());
                    let left: Vec<&(Rect, String)> = h.targets.iter().filter(|(_, n)| n.starts_with(&h.typed)).collect();
                    match left.as_slice() {
                        [] => done = true,
                        [(r, n)] if *n == h.typed => {
                            click = Some((r.center(), right));
                            done = true;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    if done {
        app.hints = None;
    }
    if let Some((p, right)) = click {
        let button = if right { egui::PointerButton::Secondary } else { egui::PointerButton::Primary };
        let modifiers = egui::Modifiers::NONE;
        app.inject.push(Event::PointerMoved(p));
        app.inject.push(Event::PointerButton { pos: p, button, pressed: true, modifiers });
        app.inject.push(Event::PointerButton { pos: p, button, pressed: false, modifiers });
    }
    ctx.request_repaint();
}

/// The tags, at each control's top left, over everything else.
pub fn draw(app: &App, ctx: &egui::Context) {
    let Some(h) = &app.hints else { return };
    let pal = app.pal;
    let p = ctx.layer_painter(egui::LayerId::new(Order::Tooltip, Id::new("hints")));
    let screen = ctx.content_rect();
    let font = FontId::monospace(11.5);
    let mut placed: Vec<Rect> = Vec::with_capacity(h.targets.len());
    for (r, name) in &h.targets {
        if !name.starts_with(&h.typed) {
            continue;
        }
        let g = p.layout_no_wrap(name.to_uppercase(), font.clone(), Color32::WHITE);
        let size = g.size() + vec2(8.0, 4.0);
        // Inside the control at its left, so neighbours' tags don't cover each other.
        let at = pos2((r.left() + 1.0).clamp(screen.left(), screen.right() - size.x), (r.center().y - size.y / 2.0).clamp(screen.top(), screen.bottom() - size.y));
        let mut tag = Rect::from_min_size(at, size);
        // Small neighbours (a path's parts, the chevrons between them): a tag that would
        // land on one already shown moves along past it.
        while let Some(o) = placed.iter().find(|o: &&Rect| o.intersects(tag)) {
            tag = tag.translate(vec2(o.right() + 1.0 - tag.left(), 0.0));
        }
        placed.push(tag);
        p.rect(tag, CornerRadius::same(4), pal.accent, Stroke::new(1.0, pal.bg), StrokeKind::Outside);
        // The letters typed so far fade, the ones left to type stay bright.
        let typed = h.typed.len();
        let job = {
            let mut j = egui::text::LayoutJob::default();
            let up = name.to_uppercase();
            let (a, b) = up.split_at(typed.min(up.len()));
            j.append(a, 0.0, egui::TextFormat { font_id: font.clone(), color: Color32::WHITE.gamma_multiply(0.5), ..Default::default() });
            j.append(b, 0.0, egui::TextFormat { font_id: font.clone(), color: Color32::WHITE, ..Default::default() });
            j
        };
        let g = p.layout_job(job);
        p.galley(tag.center() - g.size() / 2.0, g, Color32::WHITE);
    }
}
