//! Right-to-left text (Arabic, Persian, Hebrew…) for egui, which lays text out left to
//! right, one glyph per character, and joins nothing. Arabic letters are swapped for their
//! joined presentation forms, then each row is put in display order by the Unicode
//! Bidirectional Algorithm (UAX #9). A paragraph's direction is its first strong letter's,
//! as in GTK and Qt, so a Persian name reads from the right.
//!
//! Wrapping and eliding happen in reading order, before the reordering: a long name's
//! first row holds its first words and the "…" lands where the reading stops.
//!
//! egui shapes text itself (harfrust) and lays a run of a right-to-left script out from
//! the right, run by run: text already in display order would turn around again. So each
//! right-to-left letter goes to egui as a section of its own (`append_shown`), which it
//! draws as it is.

use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use egui::epaint::text::ByteIndex;
use egui::text::{CCursor, CCursorRange, LayoutJob, LayoutSection, TextFormat, TextWrapping};
use egui::text_edit::TextEditOutput;
use egui::{Color32, Event, FontId, Galley, Id, Key, Rect, TextEdit, Ui, pos2};
use unicode_bidi::{BidiClass, Level, ParagraphBidiInfo, bidi_class};

use crate::arabic_forms::{FORMS, JOIN_CAUSING, LAM_ALEF, TRANSPARENT};

const LAM: char = '\u{644}';

/// Whether `text` has right-to-left letters, the only text anything here changes. Quick
/// for the rest: nothing below U+0590 is.
pub fn has_rtl(text: &str) -> bool {
    text.chars().any(|c| c >= '\u{590}' && matches!(bidi_class(c), BidiClass::R | BidiClass::AL))
}

/// `text` as drawn: each line joined and in display order. For a line laid out whole;
/// one that may wrap or be cut short goes through [`layout_job`].
pub fn display(text: &str) -> Cow<'_, str> {
    if !has_rtl(text) {
        return Cow::Borrowed(text);
    }
    Cow::Owned(text.split('\n').map(display_line).collect::<Vec<_>>().join("\n"))
}

/// A path as drawn: each name between the slashes in display order, the names themselves
/// kept in the path's order, as its breadcrumbs show them.
pub fn display_path(path: &str) -> Cow<'_, str> {
    if !has_rtl(path) {
        return Cow::Borrowed(path);
    }
    Cow::Owned(path.split('/').map(display_line).collect::<Vec<_>>().join("/"))
}

/// `text` as drawn, laid out on one line (`display`'s, in sections egui keeps in order);
/// None when it has no right-to-left letters, for the caller's own way.
pub fn shown_job(text: &str, font: FontId, color: Color32) -> Option<LayoutJob> {
    if !has_rtl(text) {
        return None;
    }
    let mut job = LayoutJob::default();
    append_shown(&mut job, &display(text), 0.0, &TextFormat::simple(font, color));
    Some(job)
}

/// A path laid out on one line as `display_path` has it; None when it has no
/// right-to-left letters.
pub fn shown_path_job(path: &str, font: FontId, color: Color32) -> Option<LayoutJob> {
    if !has_rtl(path) {
        return None;
    }
    let mut job = LayoutJob::default();
    append_shown(&mut job, &display_path(path), 0.0, &TextFormat::simple(font, color));
    Some(job)
}

/// A one-line label for `text`, drawn as `display` has it when it has right-to-left
/// letters, else as egui draws a label.
pub fn label(ui: &mut Ui, text: &str, font: FontId, color: Color32) -> egui::Response {
    match shown_job(text, font.clone(), color) {
        Some(job) => ui.label(job),
        None => ui.label(egui::RichText::new(text).font(font).color(color)),
    }
}

/// Whether egui shapes `c` from right to left: a run takes its direction from its script,
/// and these are the right-to-left scripts' blocks (their digits too).
fn rtl_script(c: char) -> bool {
    matches!(c, '\u{590}'..='\u{8ff}' | '\u{fb1d}'..='\u{fdff}' | '\u{fe70}'..='\u{feff}' | '\u{10800}'..='\u{10fff}' | '\u{1e800}'..='\u{1efff}')
}

/// Appends `text`, already in display order, to `job` so egui draws it in that order:
/// each right-to-left letter, with the marks on it, a section of its own (pushed, as
/// `append` joins sections of one format).
fn append_shown(job: &mut LayoutJob, text: &str, leading_space: f32, format: &TextFormat) {
    let mut leading = leading_space;
    let mut push = |job: &mut LayoutJob, piece: &str| {
        let start = job.text.len();
        job.text += piece;
        job.sections.push(LayoutSection { leading_space: std::mem::take(&mut leading), byte_range: ByteIndex(start)..ByteIndex(job.text.len()), format: format.clone() });
    };
    if text.is_empty() {
        push(job, "");
        return;
    }
    let mut plain = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if !rtl_script(c) {
            continue;
        }
        if plain < i {
            push(job, &text[plain..i]);
        }
        let mut end = i + c.len_utf8();
        while let Some(&(j, m)) = chars.peek() {
            if bidi_class(m) != BidiClass::NSM {
                break;
            }
            end = j + m.len_utf8();
            chars.next();
        }
        push(job, &text[i..end]);
        plain = end;
    }
    if plain < text.len() {
        push(job, &text[plain..]);
    }
}

fn display_line(line: &str) -> String {
    let shaped = shape(line);
    let info = ParagraphBidiInfo::new(&shaped.text, None);
    let mut out = String::with_capacity(shaped.text.len());
    for (i, mirror) in visual(&info, 0..shaped.text.len()) {
        out.push(drawn(&shaped.text, i, mirror));
    }
    out
}

/// A run of text laid out by [`layout_job`]: a paragraph of its own.
pub struct Span<'a> {
    pub text: &'a str,
    pub leading_space: f32,
    pub format: TextFormat,
    /// Characters (indexes into `text`) drawn in another colour: what a search matched.
    pub hits: Option<(&'a HashSet<usize>, Color32)>,
}

/// Lays `spans` out the way `wrap` asks, in reading order, then puts each row in display
/// order: the job to draw. Each span is reordered on its own, so a name and the dimmed
/// folder after it stay in that order.
pub fn layout_job(ui: &Ui, spans: &[Span], wrap: TextWrapping) -> LayoutJob {
    let shaped: Vec<Shaped> = spans.iter().map(|s| shape(s.text)).collect();
    let mut reading = LayoutJob::default();
    for (s, sh) in spans.iter().zip(&shaped) {
        append_shown(&mut reading, &sh.text, s.leading_space, &s.format);
    }
    reading.wrap = wrap;
    let galley = ui.fonts_mut(|f| f.layout_job(reading));
    // Each row's pieces: (span, the span's characters it holds). Every character has its
    // glyph, in order, so the n-th glyph is the n-th character of the spans one after
    // another. An elided galley ends in the overflow character, no character of the text,
    // which egui draws in the last span laid out; it goes after that span's characters.
    let ends: Vec<usize> = shaped.iter().scan(0, |n, sh| { *n += sh.src.len(); Some(*n) }).collect();
    let mut laid = vec![0usize; spans.len()];
    let (mut n, mut s) = (0, 0);
    let mut overflow = None;
    let mut rows: Vec<Vec<(usize, Range<usize>)>> = Vec::with_capacity(galley.rows.len());
    let last_row = galley.rows.len().saturating_sub(1);
    for (r, row) in galley.rows.iter().enumerate() {
        let mut glyphs = &row.glyphs[..];
        if galley.elided && r == last_row {
            if let Some((last, rest)) = glyphs.split_last() {
                overflow = Some(last.chr);
                glyphs = rest;
            }
        }
        let mut pieces: Vec<(usize, Range<usize>)> = vec![];
        let mut put = |s: usize, at: usize| match pieces.last_mut() {
            Some((ls, range)) if *ls == s => range.end += 1,
            _ => pieces.push((s, at..at + 1)),
        };
        for _ in glyphs {
            while s + 1 < ends.len() && n >= ends[s] {
                s += 1;
            }
            put(s, laid[s]);
            laid[s] += 1;
            n += 1;
        }
        if overflow.is_some() {
            put(s, laid[s]);
        }
        rows.push(pieces);
    }
    let overflow = overflow.map(|c| (s, c));
    // Each span's paragraph as laid out: what fit, then the "…" if it is the span's.
    let paras: Vec<(String, Vec<usize>)> = shaped
        .iter()
        .enumerate()
        .map(|(s, sh)| {
            let mut text: String = sh.text.chars().take(laid[s]).collect();
            if let Some((_, c)) = overflow.filter(|o| o.0 == s) {
                text.push(c);
            }
            let offsets = text.char_indices().map(|(i, _)| i).chain([text.len()]).collect();
            (text, offsets)
        })
        .collect();
    let infos: Vec<ParagraphBidiInfo> = paras.iter().map(|(t, _)| ParagraphBidiInfo::new(t, None)).collect();
    let mut job = LayoutJob::default();
    for (r, pieces) in rows.iter().enumerate() {
        if r > 0 {
            job.append("\n", 0.0, spans[pieces.first().map_or(0, |p| p.0)].format.clone());
        }
        for (s, range) in pieces {
            let (span, sh, (text, offsets)) = (&spans[*s], &shaped[*s], &paras[*s]);
            let mut leading = if range.start == 0 { span.leading_space } else { 0.0 };
            let mut run = String::new();
            let mut run_color = None;
            for (i, mirror) in visual(&infos[*s], offsets[range.start]..offsets[range.end]) {
                let ci = offsets.partition_point(|&o| o < i);
                let hit = span.hits.filter(|(h, _)| ci < laid[*s] && h.contains(&sh.src[ci]));
                let color = hit.map_or(span.format.color, |(_, c)| c);
                if run_color.is_some_and(|c| c != color) {
                    append_shown(&mut job, &run, leading, &TextFormat { color: run_color.unwrap(), ..span.format.clone() });
                    run.clear();
                    leading = 0.0;
                }
                run_color = Some(color);
                run.push(drawn(text, i, mirror));
            }
            if let Some(color) = run_color {
                append_shown(&mut job, &run, leading, &TextFormat { color, ..span.format.clone() });
            }
        }
    }
    job.wrap = TextWrapping::no_max_width();
    job
}

/// Whether `text` reads from the right: its first strong letter is a right-to-left one.
pub fn reads_rtl(text: &str) -> bool {
    text.chars()
        .find_map(|c| match bidi_class(c) {
            BidiClass::L => Some(false),
            BidiClass::R | BidiClass::AL => Some(true),
            _ => None,
        })
        .unwrap_or(false)
}

/// A paragraph laid out by [`paragraph`].
pub struct Para {
    /// Its rows in display order, one after another.
    pub job: LayoutJob,
    /// For each character of the job's text, the paragraph's characters it shows (by
    /// `chars()` index): a letter itself, a lam-alef ligature two, a row's wrap none.
    pub shows: Vec<Range<usize>>,
    /// Whether it reads from the right ([`reads_rtl`]): its rows go against the right edge.
    pub rtl: bool,
}

/// `spans` as one paragraph (a sentence with words in bold, a link, some code): laid out
/// the way `wrap` asks in reading order, then each row put in display order, the spans in
/// it too. A line break in a span starts a row, in the paragraph's direction still.
pub fn paragraph(ui: &Ui, spans: &[Span], wrap: TextWrapping) -> Para {
    let text: String = spans.iter().map(|s| s.text).collect();
    // Each character's span, and the character each span starts at.
    let mut span_of = Vec::with_capacity(text.len());
    let mut starts = Vec::with_capacity(spans.len());
    for (k, s) in spans.iter().enumerate() {
        starts.push(span_of.len());
        span_of.extend(std::iter::repeat_n(k, s.text.chars().count()));
    }
    let rtl = reads_rtl(&text);
    let level = if rtl { Level::rtl() } else { Level::ltr() };
    let mut out = Para { job: LayoutJob::default(), shows: vec![], rtl };
    // The line's first character in the paragraph.
    let mut base = 0;
    for (l, line) in text.split('\n').enumerate() {
        let n = line.chars().count();
        let sh = shape(line);
        let len = sh.src.len();
        let offsets: Vec<usize> = sh.text.char_indices().map(|(b, _)| b).chain([sh.text.len()]).collect();
        let span = |i: usize| span_of[base + sh.src[i]];
        // A span's room before it goes before its first character, wherever that lands.
        let lead = |i: usize| if starts[span(i)] == base + sh.src[i] { spans[span(i)].leading_space } else { 0.0 };
        let mut reading = LayoutJob::default();
        let mut i = 0;
        while i < len {
            let j = (i + 1..len).find(|&j| span(j) != span(i)).unwrap_or(len);
            append_shown(&mut reading, &sh.text[offsets[i]..offsets[j]], lead(i), &spans[span(i)].format);
            i = j;
        }
        reading.wrap = wrap.clone();
        let galley = ui.fonts_mut(|f| f.layout_job(reading));
        let info = ParagraphBidiInfo::new(&sh.text, Some(level));
        // Every character has its glyph, in order: a row's glyphs are its characters.
        let mut at = 0;
        for (r, row) in galley.rows.iter().enumerate() {
            let end = (at + row.glyphs.len()).min(len);
            if r > 0 && at < len {
                append_shown(&mut out.job, "\n", 0.0, &spans[span(at)].format);
                out.shows.push(base + sh.src[at]..base + sh.src[at]);
            } else if r == 0 && l > 0 {
                // The line break itself.
                append_shown(&mut out.job, "\n", 0.0, &spans[span_of[base - 1]].format);
                out.shows.push(base - 1..base);
            }
            let mut run = String::new();
            let mut run_at: Option<usize> = None;
            for (b, mirror) in visual(&info, offsets[at]..offsets[end]) {
                let ci = offsets.partition_point(|&o| o < b);
                if let Some(first) = run_at.filter(|&f| span(f) != span(ci)) {
                    append_shown(&mut out.job, &run, lead(first), &spans[span(first)].format);
                    run.clear();
                    run_at = None;
                }
                run_at.get_or_insert(ci);
                run.push(drawn(&sh.text, b, mirror));
                out.shows.push(base + sh.src[ci]..base + sh.src.get(ci + 1).copied().unwrap_or(n));
            }
            if let Some(first) = run_at {
                append_shown(&mut out.job, &run, lead(first), &spans[span(first)].format);
            }
            at = end;
        }
        base += n + 1;
    }
    out.job.wrap = TextWrapping::no_max_width();
    out
}

/// `text` wrapped at `width` for a label, its rows in display order.
pub fn wrapped(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<Galley> {
    let span = Span { text, leading_space: 0.0, format: TextFormat::simple(font, color), hits: None };
    let job = layout_job(ui, &[span], TextWrapping { max_width: width, ..Default::default() });
    ui.fonts_mut(|f| f.layout_job(job))
}

/// A one-line text field for text that may hold right-to-left letters. egui's field
/// types, pastes, composes and undoes, drawn invisibly; this draws the text as `display`
/// does, the caret and selection where those characters are on screen, and puts clicks,
/// drags and Left and Right where they land on screen. Text with no right-to-left
/// letters is egui's field as it is.
pub fn line_edit(ui: &mut Ui, id: Id, text: &mut String, font: FontId, build: impl for<'a> FnOnce(TextEdit<'a>) -> TextEdit<'a>) -> TextEditOutput {
    let ctx = ui.ctx().clone();
    let color = ui.visuals().override_text_color.unwrap_or_else(|| ui.visuals().widgets.inactive.text_color());
    let rtl = has_rtl(text);
    // Left and Right move along the screen, through either direction's text alike.
    if rtl && ctx.memory(|m| m.has_focus(id)) {
        let moves: Vec<(f32, bool)> = ui.input_mut(|i| {
            let mut moves = vec![];
            i.events.retain(|e| match e {
                Event::Key { key: k @ (Key::ArrowLeft | Key::ArrowRight), pressed, modifiers: m, .. } if !m.ctrl && !m.alt && !m.command && !m.mac_cmd => {
                    if *pressed {
                        moves.push((if *k == Key::ArrowLeft { -1.0 } else { 1.0 }, m.shift));
                    }
                    false
                }
                _ => true,
            });
            moves
        });
        if !moves.is_empty() {
            let carets = place(ui, text, &font, color).carets;
            let mut st = TextEdit::load_state(&ctx, id).unwrap_or_default();
            let mut r = st.cursor.char_range().unwrap_or_default();
            for (toward, shift) in moves {
                let (caret, anchor) = (r.primary.index.0.min(carets.len() - 1), r.secondary.index.0.min(carets.len() - 1));
                let to = if !shift && caret != anchor {
                    // A selection collapses to its end on that side.
                    if (carets[caret] - carets[anchor]) * toward >= 0.0 { caret } else { anchor }
                } else {
                    step(&carets, caret, toward)
                };
                r = CCursorRange::two(CCursor::new(if shift { anchor } else { to }), CCursor::new(to));
            }
            st.cursor.set_char_range(Some(r));
            st.store(&ctx, id);
        }
    }
    // egui's own layout of the text, which its keys (End, a word on) go by: a glyph per
    // letter in reading order, as `append_shown` keeps it, where egui alone would give a
    // right-to-left run more glyphs than letters.
    let mut layouter = |ui: &Ui, buf: &dyn egui::TextBuffer, _: f32| {
        let mut job = LayoutJob::default();
        append_shown(&mut job, buf.as_str(), 0.0, &TextFormat::simple(font.clone(), Color32::TRANSPARENT));
        ui.fonts_mut(|f| f.layout_job(job))
    };
    let out = ui
        .scope(|ui| {
            if rtl {
                let v = ui.visuals_mut();
                v.selection.bg_fill = Color32::TRANSPARENT;
                v.selection.stroke.color = Color32::TRANSPARENT;
                v.text_cursor.stroke.color = Color32::TRANSPARENT;
            }
            let edit = build(TextEdit::singleline(&mut *text).id(id).font(font.clone()));
            if rtl { edit.text_color(Color32::TRANSPARENT).layouter(&mut layouter) } else { edit }.show(ui)
        })
        .inner;
    if !rtl || !has_rtl(text) {
        return out;
    }
    let placed = place(ui, text, &font, color);
    let resp = &out.response;
    let mut st = TextEdit::load_state(&ctx, id).unwrap_or_default();
    let mut range = st.cursor.char_range();
    let scroll_id = id.with("rtl scroll");
    let mut scroll: f32 = ctx.data(|d| d.get_temp(scroll_id)).unwrap_or(0.0);
    let left = out.text_clip_rect.left();
    // A press puts the caret at the nearest place between characters, Shift keeping the
    // other end; a drag stretches from the press; a double click takes the word there
    // and a triple click everything.
    if let Some(p) = resp.interact_pointer_pos().filter(|_| resp.is_pointer_button_down_on() || resp.clicked()) {
        let at = nearest(&placed.carets, p.x - left + scroll);
        let anchor_id = id.with("rtl anchor");
        let (pressed, shift) = ui.input(|i| (i.pointer.any_pressed(), i.modifiers.shift));
        let anchor = match (pressed, shift) {
            (true, false) => at,
            (true, true) => range.map_or(at, |r| r.secondary.index.0),
            _ => ctx.data(|d| d.get_temp(anchor_id)).unwrap_or(at),
        };
        if pressed {
            ctx.data_mut(|d| d.insert_temp(anchor_id, anchor));
        }
        let mut r = CCursorRange::two(CCursor::new(anchor), CCursor::new(at));
        if resp.triple_clicked() {
            r = CCursorRange::two(CCursor::new(0), CCursor::new(placed.carets.len() - 1));
        } else if resp.double_clicked() {
            let chars: Vec<char> = text.chars().collect();
            let (mut a, mut b) = (at, at);
            while a > 0 && chars[a - 1].is_alphanumeric() {
                a -= 1;
            }
            while b < chars.len() && chars[b].is_alphanumeric() {
                b += 1;
            }
            r = CCursorRange::two(CCursor::new(a), CCursor::new(b));
        }
        range = Some(r);
        st.cursor.set_char_range(range);
        st.store(&ctx, id);
    }
    // Scrolled to keep the caret in view when the text is wider than the field.
    let width = out.text_clip_rect.width();
    let caret = range.map_or(0, |r| r.primary.index.0).min(placed.carets.len() - 1);
    let cx = placed.carets[caret];
    let full = placed.galley.size().x;
    if full <= width {
        scroll = 0.0;
    } else {
        scroll = scroll.max(cx + 1.0 - width).min(cx).clamp(0.0, full - width + 1.0);
    }
    ctx.data_mut(|d| d.insert_temp(scroll_id, scroll));
    let painter = ui.painter_at(out.text_clip_rect.expand2(egui::vec2(1.0, 0.0)));
    let (x0, top, h) = (left - scroll, out.galley_pos.y, placed.galley.size().y);
    let focused = resp.has_focus();
    let mut galley = placed.galley.clone();
    if let Some(r) = range.filter(|r| focused && r.primary != r.secondary) {
        // The selected characters' places, merged where they touch.
        let sorted = r.as_sorted_char_range();
        let mut spans: Vec<(f32, f32)> = (sorted.start.0..sorted.end.0).filter_map(|i| placed.spans.get(i).copied()).filter(|(a, b)| b > a).collect();
        spans.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut merged: Vec<(f32, f32)> = vec![];
        for (a, b) in spans {
            match merged.last_mut() {
                Some(m) if a <= m.1 + 0.5 => m.1 = m.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        for (a, b) in merged {
            painter.rect_filled(Rect::from_min_max(pos2(x0 + a, top), pos2(x0 + b, top + h)), 0.0, ui.visuals().selection.bg_fill);
        }
        // The selected letters in the selection's text colour, as egui's fields show them.
        let selected = sorted.start.0..sorted.end.0;
        let mut job = LayoutJob::default();
        let mut run = (String::new(), color);
        for (c, &i) in placed.galley.text().chars().zip(&placed.logical) {
            let col = if selected.contains(&i) { ui.visuals().selection.stroke.color } else { color };
            if col != run.1 && !run.0.is_empty() {
                append_shown(&mut job, &std::mem::take(&mut run.0), 0.0, &TextFormat::simple(font.clone(), run.1));
            }
            run.1 = col;
            run.0.push(c);
        }
        append_shown(&mut job, &run.0, 0.0, &TextFormat::simple(font.clone(), run.1));
        galley = ui.fonts_mut(|f| f.layout_job(job));
    }
    painter.galley(pos2(x0, top), galley, color);
    if focused {
        let x = (x0 + cx).round() + 0.5;
        let caret_rect = Rect::from_min_max(pos2(x - 0.5, top), pos2(x + 0.5, top + h));
        painter.line_segment([caret_rect.center_top(), caret_rect.center_bottom()], ui.visuals().text_cursor.stroke);
        // The input method's window beside the caret as drawn.
        let to_global = ctx.layer_transform_to_global(ui.layer_id()).unwrap_or_default();
        ctx.output_mut(|o| {
            if let Some(ime) = o.ime.as_mut() {
                ime.cursor_rect = to_global * caret_rect;
            }
        });
    }
    out
}

/// Where a line of text is drawn: its galley in display order, where each caret position
/// (0 to the number of characters) is in it, and each character's left and right.
struct Placed {
    galley: Arc<Galley>,
    carets: Vec<f32>,
    spans: Vec<(f32, f32)>,
    /// The character of the text each of the galley's stands for.
    logical: Vec<usize>,
}

fn place(ui: &Ui, text: &str, font: &FontId, color: Color32) -> Placed {
    let n = text.chars().count();
    let shaped = shape(text);
    let info = ParagraphBidiInfo::new(&shaped.text, None);
    let offsets: Vec<usize> = shaped.text.char_indices().map(|(i, _)| i).collect();
    let mut shown = String::with_capacity(shaped.text.len());
    let mut from = vec![];
    for (i, rtl) in visual(&info, 0..shaped.text.len()) {
        shown.push(drawn(&shaped.text, i, rtl));
        from.push((shaped.src[offsets.partition_point(|&o| o < i)], rtl));
    }
    let mut job = LayoutJob::default();
    append_shown(&mut job, &shown, 0.0, &TextFormat::simple(font.clone(), color));
    let galley = ui.fonts_mut(|f| f.layout_job(job));
    // Each character's place and direction; one drawn with no glyph of its own (the alef
    // of a lam-alef, a left-out override) is a point where the one before it ends.
    let mut placed: Vec<Option<(f32, f32, bool)>> = vec![None; n];
    if let Some(row) = galley.rows.first() {
        for (g, &(src, rtl)) in row.glyphs.iter().zip(&from) {
            placed[src] = Some((g.pos.x, g.pos.x + g.advance_width, rtl));
        }
    }
    let start = if info.paragraph_level.is_rtl() { galley.size().x } else { 0.0 };
    let mut spans: Vec<(f32, f32)> = Vec::with_capacity(n);
    let mut dirs: Vec<bool> = Vec::with_capacity(n);
    for i in 0..n {
        let (a, b, rtl) = placed[i].unwrap_or_else(|| {
            let end = if i == 0 { start } else if dirs[i - 1] { spans[i - 1].0 } else { spans[i - 1].1 };
            (end, end, i > 0 && dirs[i - 1])
        });
        spans.push((a, b));
        dirs.push(rtl);
    }
    // A caret before a character sits at its leading edge: the left of a left-to-right
    // one, the right of a right-to-left one; after the last, at its trailing edge.
    let mut carets: Vec<f32> = (0..n).map(|i| if dirs[i] { spans[i].1 } else { spans[i].0 }).collect();
    carets.push(match n {
        0 => start,
        _ if dirs[n - 1] => spans[n - 1].0,
        _ => spans[n - 1].1,
    });
    let logical = from.iter().map(|f| f.0).collect();
    Placed { galley, carets, spans, logical }
}

/// The caret position nearest `x`.
fn nearest(carets: &[f32], x: f32) -> usize {
    (0..carets.len()).min_by(|&a, &b| (carets[a] - x).abs().total_cmp(&(carets[b] - x).abs())).unwrap_or(0)
}

/// The caret position next to `from` on the screen, left (`toward` below 0) or right;
/// of two in the same place (inside a lam-alef), the one fewer characters away.
fn step(carets: &[f32], from: usize, toward: f32) -> usize {
    let x = carets[from];
    let key = |i: usize| ((carets[i] - x).abs(), i.abs_diff(from));
    (0..carets.len()).filter(|&i| (carets[i] - x) * toward > 0.5).min_by(|&a, &b| key(a).partial_cmp(&key(b)).unwrap_or(std::cmp::Ordering::Equal)).unwrap_or(from)
}

/// Text with its Arabic letters joined, in reading order, and for each character the
/// index of the character of the original it stands for.
struct Shaped {
    text: String,
    src: Vec<usize>,
}

#[derive(Clone, Copy, PartialEq)]
enum Joining {
    /// Joins on both sides: isolated, final, initial and medial forms.
    Dual([char; 4]),
    /// Joins only to the letter before it: isolated and final forms.
    Right([char; 2]),
    /// Tatweel and ZWJ: joined to on both sides, never changed.
    Causing,
    /// Marks on a letter, which joining looks through.
    Transparent,
    None,
}

fn joining(c: char) -> Joining {
    if !('\u{600}'..='\u{8ff}').contains(&c) && c != '\u{200d}' {
        return Joining::None;
    }
    if let Ok(i) = FORMS.binary_search_by_key(&c, |f| f.0) {
        let (_, dual, f) = FORMS[i];
        return if dual { Joining::Dual(f) } else { Joining::Right([f[0], f[1]]) };
    }
    if JOIN_CAUSING.contains(&c) {
        Joining::Causing
    } else if TRANSPARENT.iter().any(|&(a, b)| (a..=b).contains(&c)) {
        Joining::Transparent
    } else {
        Joining::None
    }
}

/// The overrides, embeddings and isolates: left out, so a name cannot reorder itself to
/// look like another (an "exe" shown as "pdf"). Marks (LRM, RLM, ALM) only steer the
/// characters around them and stay.
fn explicit(c: char) -> bool {
    matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

fn shape(text: &str) -> Shaped {
    // Line and segment separators break a paragraph; a name is drawn as one.
    let chars: Vec<(char, usize)> = text
        .chars()
        .enumerate()
        .filter(|&(_, c)| !explicit(c))
        .map(|(i, c)| (if matches!(bidi_class(c), BidiClass::B | BidiClass::S) { ' ' } else { c }, i))
        .collect();
    let kinds: Vec<Joining> = chars.iter().map(|&(c, _)| joining(c)).collect();
    // The nearest letter before and after each character, looking through marks.
    let neighbour = |i: usize, step: isize| {
        let mut j = i as isize + step;
        while j >= 0 && (j as usize) < kinds.len() && kinds[j as usize] == Joining::Transparent {
            j += step;
        }
        (j >= 0 && (j as usize) < kinds.len()).then(|| kinds[j as usize])
    };
    let mut out = Shaped { text: String::with_capacity(text.len()), src: Vec::with_capacity(chars.len()) };
    let mut i = 0;
    while i < chars.len() {
        let (c, src) = chars[i];
        let joins_before = matches!(neighbour(i, -1), Some(Joining::Dual(_) | Joining::Causing));
        let drawn = match kinds[i] {
            Joining::Dual(forms) => {
                let alef = (c == LAM).then(|| chars.get(i + 1)).flatten().and_then(|&(a, _)| LAM_ALEF.iter().find(|l| l.0 == a));
                if let Some(&(_, isolated, fin)) = alef {
                    i += 1; // the alef is in the ligature
                    if joins_before { fin } else { isolated }
                } else {
                    let joins_after = matches!(neighbour(i, 1), Some(Joining::Dual(_) | Joining::Right(_) | Joining::Causing));
                    forms[match (joins_before, joins_after) {
                        (false, false) => 0,
                        (true, false) => 1,
                        (false, true) => 2,
                        (true, true) => 3,
                    }]
                }
            }
            Joining::Right(forms) => forms[joins_before as usize],
            _ => c,
        };
        out.text.push(if drawn == '\0' { c } else { drawn });
        out.src.push(src);
        i += 1;
    }
    out
}

/// The characters of `line` (a byte range of the paragraph) in display order: each one's
/// byte offset, and whether it is in a right-to-left run (where brackets are mirrored).
fn visual(info: &ParagraphBidiInfo, line: Range<usize>) -> Vec<(usize, bool)> {
    let mut out = Vec::with_capacity(line.len());
    if line.is_empty() {
        return out;
    }
    let (levels, runs) = info.visual_runs(line);
    for run in runs {
        let chars: Vec<(usize, char)> = info.text[run.clone()].char_indices().map(|(i, c)| (run.start + i, c)).collect();
        if !levels[run.start].is_rtl() {
            out.extend(chars.iter().map(|&(i, _)| (i, false)));
            continue;
        }
        // Letters in reverse, each still followed by its marks: drawn after it, a mark
        // lands on it (UAX #9, L3).
        let mut end = chars.len();
        while end > 0 {
            let mut start = end - 1;
            while start > 0 && bidi_class(chars[start].1) == BidiClass::NSM {
                start -= 1;
            }
            out.extend(chars[start..end].iter().map(|&(i, _)| (i, true)));
            end = start;
        }
    }
    out
}

/// The character at byte `i`, mirrored when it is a bracket in a right-to-left run.
fn drawn(text: &str, i: usize, mirror: bool) -> char {
    let c = text[i..].chars().next().unwrap_or(' ');
    if !mirror {
        return c;
    }
    match c {
        '(' => ')',
        ')' => '(',
        '[' => ']',
        ']' => '[',
        '{' => '}',
        '}' => '{',
        '<' => '>',
        '>' => '<',
        '«' => '»',
        '»' => '«',
        '‹' => '›',
        '›' => '‹',
        c => c,
    }
}
