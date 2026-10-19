//! Diffs side by side: the old lines on the left, the new on the right, a changed line
//! level with what replaced it and the part of it that changed marked. Made from the
//! unified diff the other views show, so both come from one git run.

use egui::{Align2, CornerRadius, FontId, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2};

use crate::theme::Palette;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    Same,
    Changed,
    /// A hunk's header, or a file's (in a commit of several files): across both sides.
    Head,
}

/// One row: the old line and the new beside it, each as (line number, index in the diff).
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Row {
    pub old: Option<(u32, u32)>,
    pub new: Option<(u32, u32)>,
    pub kind: Kind,
}

/// The rows of a unified diff.
pub fn rows(lines: &[String]) -> Vec<Row> {
    let mut out = Vec::with_capacity(lines.len());
    let (mut o, mut n) = (0u32, 0u32);
    let (mut gone, mut came): (Vec<u32>, Vec<u32>) = (vec![], vec![]);
    let flush = |out: &mut Vec<Row>, gone: &mut Vec<u32>, came: &mut Vec<u32>, o: &mut u32, n: &mut u32| {
        for k in 0..gone.len().max(came.len()) {
            let old = gone.get(k).map(|&i| (*o + k as u32, i));
            let new = came.get(k).map(|&i| (*n + k as u32, i));
            out.push(Row { old, new, kind: Kind::Changed });
        }
        *o += gone.len() as u32;
        *n += came.len() as u32;
        gone.clear();
        came.clear();
    };
    for (i, l) in lines.iter().enumerate() {
        let i = i as u32;
        // A file's "---" line comes with its "+++" line: names, not a line taken out.
        let names = (l.starts_with("--- ") && lines.get(i as usize + 1).is_some_and(|x| x.starts_with("+++ "))) || (l.starts_with("+++ ") && i > 0 && lines[i as usize - 1].starts_with("--- "));
        match l.as_bytes().first() {
            _ if names || l.starts_with("diff ") || l.starts_with("index ") => {
                flush(&mut out, &mut gone, &mut came, &mut o, &mut n);
                out.push(Row { old: Some((0, i)), new: None, kind: Kind::Head });
            }
            Some(b'@') => {
                flush(&mut out, &mut gone, &mut came, &mut o, &mut n);
                // "@@ -12,5 +12,7 @@": where each side starts.
                let start = |sign: char| l.split_whitespace().find_map(|w| w.strip_prefix(sign)).and_then(|w| w.split(',').next()?.parse::<u32>().ok()).unwrap_or(1);
                (o, n) = (start('-'), start('+'));
                out.push(Row { old: Some((0, i)), new: None, kind: Kind::Head });
            }
            Some(b'-') => {
                // A removal after additions starts a new change.
                if !came.is_empty() {
                    flush(&mut out, &mut gone, &mut came, &mut o, &mut n);
                }
                gone.push(i);
            }
            Some(b'+') => came.push(i),
            Some(b'\\') => {}
            _ => {
                flush(&mut out, &mut gone, &mut came, &mut o, &mut n);
                out.push(Row { old: Some((o, i)), new: Some((n, i)), kind: Kind::Same });
                o += 1;
                n += 1;
            }
        }
    }
    flush(&mut out, &mut gone, &mut came, &mut o, &mut n);
    out
}

/// Where two versions of a line part: the bytes before which they are the same, and
/// after which (counted from each end).
pub fn changed_span(a: &str, b: &str) -> ((usize, usize), (usize, usize)) {
    let pre = a.char_indices().zip(b.chars()).take_while(|((_, x), y)| x == y).last().map_or(0, |((i, c), _)| i + c.len_utf8());
    let (ra, rb) = (&a[pre..], &b[pre..]);
    let suf = ra.chars().rev().zip(rb.chars().rev()).take_while(|(x, y)| x == y).map(|(c, _)| c.len_utf8()).sum::<usize>();
    ((pre, a.len() - suf.min(ra.len())), (pre, b.len() - suf.min(rb.len())))
}

/// The rows worked out for a diff, kept while it is shown, and the sideways scroll.
#[derive(Default)]
pub struct View {
    key: (usize, usize),
    rows: Vec<Row>,
    pub x: f32,
}

/// Draw `lines` (a unified diff) side by side in `body`.
pub fn view(ui: &mut Ui, body: Rect, lines: &[String], v: &mut View, size: f32, spacing: f32, pal: &Palette) {
    let key = (lines.as_ptr() as usize, lines.len());
    if v.key != key {
        *v = View { key, rows: rows(lines), x: 0.0 };
    }
    let font = FontId::monospace(size);
    let cw = ui.fonts_mut(|f| f.glyph_width(&font, 'M'));
    let row_h = crate::inspector::row_height(ui, size, spacing);
    let lift = ((row_h - ui.fonts_mut(|f| f.row_height(&font))) / 2.0).floor().max(0.0);
    let digits = v.rows.iter().filter_map(|r| r.new.or(r.old)).map(|x| x.0).max().unwrap_or(1).to_string().len();
    let gutter = digits as f32 * cw + 14.0;
    let mut inner = ui.new_child(UiBuilder::new().max_rect(body.shrink2(vec2(4.0, 4.0))));
    let (green, red) = (crate::gitstatus::GREEN, pal.danger);
    let tint = if pal.dark { 0.13 } else { 0.11 };
    egui::ScrollArea::vertical().id_salt("side-diff").auto_shrink([false, false]).show_viewport(&mut inner, |ui, vp| {
        let (r, resp) = ui.allocate_exact_size(vec2(vp.width(), v.rows.len() as f32 * row_h + 8.0), Sense::hover());
        // Sideways: a wheel's sideways turn (or Shift with it) moves both sides together.
        if resp.hovered() {
            let dx = ui.input(|i| i.smooth_scroll_delta.x);
            let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0) as f32 * cw;
            v.x = (v.x - dx).clamp(0.0, (widest - r.width() / 2.0 + gutter + 24.0).max(0.0));
        }
        let half = r.width() / 2.0;
        let p = ui.painter();
        let first = (vp.min.y / row_h).floor().max(0.0) as usize;
        let last = ((vp.max.y / row_h).ceil() as usize + 1).min(v.rows.len());
        for (k, row) in v.rows[first..last].iter().enumerate() {
            let top = r.top() + (first + k) as f32 * row_h;
            let y = top + lift;
            if row.kind == Kind::Head {
                let i = row.old.map_or(0, |x| x.1) as usize;
                let band = Rect::from_min_size(pos2(r.left(), top), vec2(r.width(), row_h));
                p.rect_filled(band, CornerRadius::ZERO, pal.text_dim.gamma_multiply(0.06));
                let g = crate::listing::elided(ui, &lines[i], font.clone(), r.width() - 12.0, pal.text_dim);
                p.galley(pos2(r.left() + 6.0, y), g, pal.text_dim);
                continue;
            }
            // The other side's line, for what changed within this one.
            let text = |s: Option<(u32, u32)>| s.map(|(_, i)| &lines[i as usize][1.min(lines[i as usize].len())..]);
            let (old, new) = (text(row.old), text(row.new));
            let span = match (old, new) {
                (Some(a), Some(b)) if row.kind == Kind::Changed => Some(changed_span(a, b)),
                _ => None,
            };
            for (left, side, col, x0) in [(true, row.old, red, r.left()), (false, row.new, green, r.left() + half)] {
                let cell = Rect::from_min_size(pos2(x0, top), vec2(half, row_h));
                match (side, row.kind) {
                    (Some(_), Kind::Changed) => {
                        p.rect_filled(cell, CornerRadius::ZERO, col.gamma_multiply(tint));
                    }
                    // Nothing on this side of a change: a quiet gap.
                    (None, _) => {
                        p.rect_filled(cell, CornerRadius::ZERO, pal.text_dim.gamma_multiply(0.05));
                    }
                    _ => {}
                }
                let Some((num, _)) = side else { continue };
                p.text(pos2(x0 + gutter - 8.0, y), Align2::RIGHT_TOP, num.to_string(), font.clone(), pal.text_dim.gamma_multiply(0.7));
                let s = if left { old } else { new }.unwrap_or("");
                let clip = Rect::from_min_max(pos2(x0 + gutter, top), pos2(x0 + half - 4.0, top + row_h));
                let pc = p.with_clip_rect(clip.intersect(p.clip_rect()));
                let tx = x0 + gutter - v.x;
                if let Some(((a0, a1), (b0, b1))) = span {
                    let (s0, s1) = if left { (a0, a1) } else { (b0, b1) };
                    let cols = |t: &str| t.chars().map(crate::inspector::char_cols).sum::<usize>() as f32 * cw;
                    if s1 > s0 {
                        let mr = Rect::from_min_size(pos2(tx + cols(&s[..s0]), top + 1.0), vec2(cols(&s[s0..s1]).max(2.0), row_h - 2.0));
                        pc.rect_filled(mr, CornerRadius::same(2), col.gamma_multiply(tint * 2.6));
                    }
                }
                pc.text(pos2(tx, y), Align2::LEFT_TOP, s, font.clone(), pal.text);
            }
        }
        // The seam between the sides.
        let mx = r.left() + half;
        p.line_segment([pos2(mx, r.top() + vp.min.y), pos2(mx, r.top() + vp.max.y)], Stroke::new(1.0, pal.border));
    });
}
