use unicode_bidi::{BidiInfo, Level};

use crate::arabic_forms::{FORMS, JOIN_CAUSING, LAM_ALEF, TRANSPARENT};

const LAM: char = '\u{644}';

/// Check if string contains any Arabic / Persian script characters
pub fn has_rtl(text: &str) -> bool {
    text.chars().any(|c| {
        ('\u{0600}'..='\u{06FF}').contains(&c)
            || ('\u{0750}'..='\u{077F}').contains(&c)
            || ('\u{08A0}'..='\u{08FF}').contains(&c)
            || ('\u{FB50}'..='\u{FDFF}').contains(&c)
            || ('\u{FE70}'..='\u{FEFF}').contains(&c)
    })
}

/// Reshape Arabic/Persian cursive joining and reorder for LTR rendering
pub fn fix_rtl(text: &str) -> String {
    if !has_rtl(text) {
        return text.to_string();
    }
    if text.contains('\n') {
        text.lines().map(fix_rtl_single_line).collect::<Vec<_>>().join("\n")
    } else {
        fix_rtl_single_line(text)
    }
}

fn fix_rtl_single_line(text: &str) -> String {
    if !has_rtl(text) {
        return text.to_string();
    }
    let reshaped = shape(text);
    let bidi_info = BidiInfo::new(&reshaped, Some(Level::ltr()));
    if let Some(para) = bidi_info.paragraphs.first() {
        bidi_info.reorder_line(para, para.range.clone()).into_owned()
    } else {
        reshaped
    }
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

/// `text` with its Arabic letters joined: each swapped for the presentation form its
/// neighbours call for (egui draws one glyph per character and knows no shaping), lam
/// then alef as one ligature. Still in reading order.
fn shape(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let kinds: Vec<Joining> = chars.iter().map(|&c| joining(c)).collect();
    // The nearest letter before and after each character, looking through marks.
    let neighbour = |i: usize, step: isize| {
        let mut j = i as isize + step;
        while j >= 0 && (j as usize) < kinds.len() && kinds[j as usize] == Joining::Transparent {
            j += step;
        }
        (j >= 0 && (j as usize) < kinds.len()).then(|| kinds[j as usize])
    };
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let joins_before = matches!(neighbour(i, -1), Some(Joining::Dual(_) | Joining::Causing));
        let drawn = match kinds[i] {
            Joining::Dual(forms) => {
                let alef = (c == LAM).then(|| chars.get(i + 1)).flatten().and_then(|&a| LAM_ALEF.iter().find(|l| l.0 == a));
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
        out.push(if drawn == '\0' { c } else { drawn });
        i += 1;
    }
    out
}
