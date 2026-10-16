//! Conditions in the filter: `size>100M`, `size<1k`, `modified<7d`, `modified>2026-01`,
//! `created:today`. Each recognised word becomes a condition; the other words stay the
//! name search. Ages count back from now; dates are local days, months or years.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::fsx::Entry;

#[derive(Clone, Copy, Debug, PartialEq)]
enum Field {
    Size,
    Modified,
    Created,
}

/// A field kept inside [lo, hi): bytes, or seconds since the epoch.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Cond {
    field: Field,
    lo: i64,
    hi: i64,
}

#[derive(Debug, Default)]
pub struct Filter {
    /// The words that are not conditions, for the name search.
    pub name: String,
    conds: Vec<Cond>,
    /// Byte ranges of the recognised words in the filter text (drawn in the accent).
    pub spans: Vec<(usize, usize)>,
}

impl Filter {
    pub fn has_conds(&self) -> bool {
        !self.conds.is_empty()
    }

    /// Whether `e` meets every condition. A folder's size is known only once worked out.
    pub fn ok(&self, e: &Entry, stats: &HashMap<PathBuf, crate::stats::DirStats>) -> bool {
        self.conds.iter().all(|c| {
            let v = match c.field {
                Field::Size if e.is_dir => stats.get(&e.path).map(|s| s.size as i64),
                Field::Size => Some(e.size as i64),
                Field::Modified => e.modified.map(secs),
                Field::Created => e.created.map(secs),
            };
            v.is_some_and(|v| c.lo <= v && v < c.hi)
        })
    }
}

fn secs(t: SystemTime) -> i64 {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => d.as_secs() as i64,
        Err(e) => -(e.duration().as_secs() as i64),
    }
}

/// The name-search part of the filter, without allocating when there are no conditions.
pub fn name_part(text: &str) -> std::borrow::Cow<'_, str> {
    if text.contains(['>', '<', '=', ':']) {
        let f = parse(text, SystemTime::now());
        if f.has_conds() {
            return std::borrow::Cow::Owned(f.name);
        }
    }
    std::borrow::Cow::Borrowed(text)
}

pub fn parse(text: &str, now: SystemTime) -> Filter {
    let mut f = Filter::default();
    let mut words = vec![];
    let mut at = 0;
    for w in text.split_whitespace() {
        let start = at + text[at..].find(w).unwrap_or(0);
        at = start + w.len();
        match cond(w, secs(now)) {
            Some(c) => {
                f.conds.push(c);
                f.spans.push((start, at));
            }
            None => words.push(w),
        }
    }
    f.name = words.join(" ");
    f
}

fn cond(w: &str, now: i64) -> Option<Cond> {
    let split = w.find(['>', '<', '=', ':'])?;
    let field = match w[..split].to_ascii_lowercase().as_str() {
        "size" => Field::Size,
        "modified" | "mod" | "date" => Field::Modified,
        "created" => Field::Created,
        _ => return None,
    };
    let rest = &w[split..];
    let op = ["<=", ">=", "<", ">", "=", ":"].into_iter().find(|o| rest.starts_with(o))?;
    let v = &rest[op.len()..];
    let (lo, hi) = match field {
        Field::Size => {
            let n = size(v)?;
            match op {
                ">" => (n + 1, i64::MAX),
                ">=" => (n, i64::MAX),
                "<" => (0, n),
                "<=" => (0, n + 1),
                _ => (n, n + 1),
            }
        }
        _ => match age(v) {
            // An age: newer than it with "<" (and ":" or "="), older with ">".
            Some(a) => match op {
                ">" | ">=" => (i64::MIN, now - a),
                _ => (now - a, i64::MAX),
            },
            None => {
                let (a, b) = when(v, now)?;
                match op {
                    ">" => (b, i64::MAX),
                    ">=" => (a, i64::MAX),
                    "<" => (i64::MIN, a),
                    "<=" => (i64::MIN, b),
                    _ => (a, b),
                }
            }
        },
    };
    Some(Cond { field, lo, hi })
}

/// "100M", "1.5g", "20kb", "0": binary units, like the sizes shown in the list.
fn size(v: &str) -> Option<i64> {
    let v = v.to_ascii_lowercase();
    let digits = v.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(v.len());
    let n: f64 = v[..digits].parse().ok()?;
    let unit = match v[digits..].trim_end_matches(['b', 'i']) {
        "" => 1u64,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        _ => return None,
    };
    Some((n * unit as f64).round() as i64)
}

/// "30min", "12h", "7d", "2w", "3mo", "1y" in seconds.
fn age(v: &str) -> Option<i64> {
    let digits = v.find(|c: char| !c.is_ascii_digit()).filter(|&d| d > 0)?;
    let n: i64 = v[..digits].parse().ok()?;
    let unit = match v[digits..].to_ascii_lowercase().as_str() {
        "min" => 60,
        "h" => 3600,
        "d" => 86_400,
        "w" => 7 * 86_400,
        "mo" => 30 * 86_400,
        "y" => 365 * 86_400,
        _ => return None,
    };
    Some(n * unit)
}

/// A local day, month or year ("2026-10-14", "2026-10", "2026", "today",
/// "yesterday") as the seconds it spans.
fn when(v: &str, now: i64) -> Option<(i64, i64)> {
    let start_of = |y: i32, m: i32, d: i32| -> Option<i64> {
        crate::timefmt::parse_local(&format!("{y:04}-{m:02}-{d:02}T00:00:00")).map(secs)
    };
    let day = |off: i64| -> Option<(i64, i64)> {
        let tm = crate::timefmt::local(UNIX_EPOCH + std::time::Duration::from_secs((now - off * 86_400).max(0) as u64));
        let a = start_of(tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)?;
        // The next local midnight (a day is 23 or 25 hours across a clock change).
        let next = crate::timefmt::local(UNIX_EPOCH + std::time::Duration::from_secs((a + 30 * 3600) as u64));
        Some((a, start_of(next.tm_year + 1900, next.tm_mon + 1, next.tm_mday)?))
    };
    match v.to_ascii_lowercase().as_str() {
        "today" => return day(0),
        "yesterday" => return day(1),
        _ => {}
    }
    let n: Vec<i32> = v.split('-').map(|p| p.parse().ok()).collect::<Option<_>>()?;
    match n[..] {
        [y] if v.len() == 4 => Some((start_of(y, 1, 1)?, start_of(y + 1, 1, 1)?)),
        [y, m] if (1..=12).contains(&m) => Some((start_of(y, m, 1)?, if m == 12 { start_of(y + 1, 1, 1)? } else { start_of(y, m + 1, 1)? })),
        [y, m, d] if (1..=12).contains(&m) && (1..=31).contains(&d) => {
            let a = start_of(y, m, d)?;
            let next = crate::timefmt::local(UNIX_EPOCH + std::time::Duration::from_secs((a + 30 * 3600).max(0) as u64));
            Some((a, start_of(next.tm_year + 1900, next.tm_mon + 1, next.tm_mday)?))
        }
        _ => None,
    }
}
