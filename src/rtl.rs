//! Right-to-left text (Arabic, Persian, Hebrew…) for egui, which lays text out left to
//! right, one glyph per character, and joins nothing. Arabic letters are swapped for their
//! joined presentation forms, then each row is put in display order by the Unicode
//! Bidirectional Algorithm (UAX #9). A paragraph's direction is its first strong letter's,
//! as in GTK and Qt, so a Persian name reads from the right.
//!
//! Wrapping and eliding happen in reading order, before the reordering: a long name's
//! first row holds its first words and the "…" lands where the reading stops.

use std::borrow::Cow;
use std::collections::HashSet;
use std::ops::Range;
use std::sync::Arc;

use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Color32, FontId, Galley, Ui};
use unicode_bidi::{BidiClass, ParagraphBidiInfo, bidi_class};

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
        reading.append(&sh.text, s.leading_space, s.format.clone());
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
                    job.append(&run, leading, TextFormat { color: run_color.unwrap(), ..span.format.clone() });
                    run.clear();
                    leading = 0.0;
                }
                run_color = Some(color);
                run.push(drawn(text, i, mirror));
            }
            if let Some(color) = run_color {
                job.append(&run, leading, TextFormat { color, ..span.format.clone() });
            }
        }
    }
    job.wrap = TextWrapping::no_max_width();
    job
}

/// `text` wrapped at `width` for a label, its rows in display order.
pub fn wrapped(ui: &Ui, text: &str, font: FontId, color: Color32, width: f32) -> Arc<Galley> {
    let span = Span { text, leading_space: 0.0, format: TextFormat::simple(font, color), hits: None };
    let job = layout_job(ui, &[span], TextWrapping { max_width: width, ..Default::default() });
    ui.fonts_mut(|f| f.layout_job(job))
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
