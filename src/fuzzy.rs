//! Small, fast fuzzy matcher with highlight positions.
//!
//! Scoring prefers (in order): prefix matches, contiguous runs, word-boundary
//! hits, and earlier positions. Case-insensitive.

/// Prepared lowercase query.
pub struct Query {
    chars: Vec<char>,
}

impl Query {
    pub fn new(q: &str) -> Query {
        Query { chars: q.trim().to_lowercase().chars().filter(|c| !c.is_whitespace()).collect() }
    }
    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }
}

fn is_boundary(prev: char, cur: char) -> bool {
    matches!(prev, ' ' | '_' | '-' | '.' | '/' | '\\' | '(' | '[') || (prev.is_lowercase() && cur.is_uppercase())
}

/// Score only (no positions); `None` if not a subsequence. Uses the
/// lowercase candidate for speed.
pub fn score(q: &Query, lower: &str) -> Option<i32> {
    score_chars(q, lower.chars())
}

/// Like `score`, lowercasing `s` on the fly (no allocation per candidate).
pub fn score_ci(q: &Query, s: &str) -> Option<i32> {
    if s.is_ascii() {
        score_chars(q, s.bytes().map(|b| b.to_ascii_lowercase() as char))
    } else {
        score_chars(q, s.chars().flat_map(char::to_lowercase))
    }
}

thread_local! {
    /// Reused buffers: candidate chars, working positions, best positions. Scoring runs
    /// once per file when filtering a flattened folder, so it must not allocate.
    static BUF: std::cell::RefCell<(Vec<char>, Vec<usize>, Vec<usize>)> = const { std::cell::RefCell::new((Vec::new(), Vec::new(), Vec::new())) };
}

fn score_chars(q: &Query, chars: impl Iterator<Item = char> + Clone) -> Option<i32> {
    if q.chars.is_empty() {
        return Some(0);
    }
    // Fast reject: subsequence test.
    let mut qi = 0;
    for c in chars.clone() {
        if c == q.chars[qi] {
            qi += 1;
            if qi == q.chars.len() {
                break;
            }
        }
    }
    if qi < q.chars.len() {
        return None;
    }
    BUF.with(|b| {
        let (cand, pos, best) = &mut *b.borrow_mut();
        cand.clear();
        cand.extend(chars);
        Some(search(q, cand, &[], pos, best))
    })
}

/// Full match with positions (char indices into `original`).
pub fn positions(q: &Query, original: &str) -> Option<(i32, Vec<usize>)> {
    if q.chars.is_empty() {
        return Some((0, vec![]));
    }
    let lower = original.to_lowercase();
    score(q, &lower)?;
    let (s, p) = match_positions(q, &lower, Some(original));
    Some((s, p))
}

fn match_positions(q: &Query, lower: &str, original: Option<&str>) -> (i32, Vec<usize>) {
    let cand: Vec<char> = lower.chars().collect();
    let orig: Vec<char> = original.map(|o| o.chars().collect()).unwrap_or_default();
    let (mut pos, mut best) = (vec![], vec![]);
    let s = search(q, &cand, &orig, &mut pos, &mut best);
    (s, best)
}

/// Best greedy match over (at most 8) start positions of the first query char.
/// Returns its score (`i32::MIN` if none) and leaves its positions in `best`.
fn search(q: &Query, cand: &[char], orig: &[char], pos: &mut Vec<usize>, best: &mut Vec<usize>) -> i32 {
    let n = cand.len();
    let mut best_score = i32::MIN;
    best.clear();
    let mut starts = 0;
    for start in 0..n {
        if cand[start] != q.chars[0] {
            continue;
        }
        starts += 1;
        if starts > 8 {
            break; // bounded work on long candidates
        }
        pos.clear();
        pos.push(start);
        let mut ci = start + 1;
        let mut ok = true;
        for &qc in &q.chars[1..] {
            // Prefer the next char contiguous; else a boundary; else first occurrence.
            if ci < n && cand[ci] == qc {
                pos.push(ci);
                ci += 1;
                continue;
            }
            let mut first = None;
            let mut boundary = None;
            for j in ci..n {
                if cand[j] == qc {
                    if first.is_none() {
                        first = Some(j);
                    }
                    let prev = cand[j - 1];
                    let cur = if orig.len() == n { orig[j] } else { cand[j] };
                    if is_boundary(prev, cur) {
                        boundary = Some(j);
                        break;
                    }
                }
            }
            match boundary.or(first) {
                Some(j) => {
                    pos.push(j);
                    ci = j + 1;
                }
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if !ok {
            continue;
        }
        let s = score_positions(cand, orig, pos);
        if s > best_score {
            best_score = s;
            best.clear();
            best.extend_from_slice(pos);
        }
    }
    best_score
}

fn score_positions(cand: &[char], orig: &[char], pos: &[usize]) -> i32 {
    let mut s = 0i32;
    if pos[0] == 0 {
        s += 60;
    }
    for (i, &p) in pos.iter().enumerate() {
        if i > 0 && p == pos[i - 1] + 1 {
            s += 25;
        } else if i > 0 {
            s -= ((p - pos[i - 1]) as i32).min(20);
        }
        if p > 0 {
            let cur = if orig.len() == cand.len() { orig[p] } else { cand[p] };
            if is_boundary(cand[p - 1], cur) {
                s += 18;
            }
        }
    }
    s -= (pos[0] as i32).min(30);
    s -= (cand.len() as i32 / 8).min(10);
    s
}
