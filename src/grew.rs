//! What grew: the sizes Space used counted in a folder, kept so the next count, on a
//! later day, can say what changed since. Written only when Space used finishes counting
//! a folder (never on a timer), on a thread, as freeup.rs keeps its record of the disks.
//!
//! The file is a block a reading: `@day<TAB>seconds<TAB>folder`, then `size<TAB>name`
//! for each thing in it. A folder keeps its last few days; the least recent folders go
//! once there are many.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Readings kept for each folder: the newest few days.
const DAYS: usize = 4;
/// Folders kept, at most.
const FOLDERS: usize = 400;
/// A folder holding more than this isn't kept: its reading would outweigh all the rest.
pub const MOST_ITEMS: usize = 5000;
/// Names kept in all, the most recently counted folders' first: the file stays a few
/// megabytes, rewritten each time a folder is counted.
const MOST_NAMES: usize = 100_000;

#[derive(Clone, Debug)]
pub struct Reading {
    /// The day it was taken (local), and when exactly (seconds since the epoch).
    pub day: i64,
    pub at: u64,
    pub dir: PathBuf,
    pub sizes: HashMap<String, u64>,
}

impl Reading {
    pub fn total(&self) -> u64 {
        self.sizes.values().sum()
    }

    /// "Oct 18".
    pub fn label(&self) -> String {
        crate::timefmt::format(UNIX_EPOCH + Duration::from_secs(self.at), "%b %-d")
    }
}

pub fn book_path() -> PathBuf {
    crate::config::data_dir().join("space-readings")
}

static BOOK: Mutex<()> = Mutex::new(());

fn read_book(file: &Path) -> Vec<Reading> {
    let text = std::fs::read_to_string(file).unwrap_or_default();
    let mut out: Vec<Reading> = vec![];
    for l in text.lines() {
        if let Some(h) = l.strip_prefix('@') {
            let mut it = h.splitn(3, '\t');
            let (Some(day), Some(at), Some(dir)) = (it.next().and_then(|d| d.parse().ok()), it.next().and_then(|a| a.parse().ok()), it.next()) else { continue };
            out.push(Reading { day, at, dir: PathBuf::from(dir), sizes: HashMap::new() });
        } else if let (Some(r), Some((s, name))) = (out.last_mut(), l.split_once('\t')) {
            if let Ok(s) = s.parse() {
                r.sizes.insert(name.to_string(), s);
            }
        }
    }
    out
}

/// The newest reading of `dir` taken before `today`, from `file`.
pub fn earlier_in(file: &Path, dir: &Path, today: i64) -> Option<Reading> {
    read_book(file).into_iter().filter(|r| r.dir == dir && r.day < today).max_by_key(|r| r.at)
}

/// `earlier_in` the user's own file. Test builds read and write only in the
/// simulations' sandbox.
pub fn earlier(dir: &Path) -> Option<Reading> {
    if cfg!(test) && std::env::var_os("MAIA_SIM_SANDBOX").is_none() {
        return None;
    }
    earlier_in(&book_path(), dir, crate::timefmt::local_day(SystemTime::now()))
}

/// Keep today's reading of `dir` in `file`, in place of one taken earlier today.
pub fn note_in(file: &Path, r: Reading) {
    let _g = BOOK.lock();
    let key = r.dir.to_string_lossy();
    if key.contains(['\n', '\t']) {
        return;
    }
    let mut all = read_book(file);
    all.retain(|o| !(o.dir == r.dir && o.day == r.day));
    all.push(r);
    // The newest few days of each folder, and of the folders the most recently counted.
    let mut newest: HashMap<PathBuf, u64> = HashMap::new();
    for o in &all {
        let e = newest.entry(o.dir.clone()).or_default();
        *e = (*e).max(o.at);
    }
    let mut folders: Vec<(u64, PathBuf)> = newest.into_iter().map(|(d, at)| (at, d)).collect();
    folders.sort_unstable_by(|a, b| b.0.cmp(&a.0));
    folders.truncate(FOLDERS);
    let mut names: HashMap<&Path, Vec<usize>> = HashMap::new();
    for o in &all {
        names.entry(o.dir.as_path()).or_default().push(o.sizes.len());
    }
    let (mut kept, mut used) = (std::collections::HashSet::new(), 0);
    for (_, d) in folders {
        // What is written of a folder, at most: its biggest few days.
        let mut n = names.remove(d.as_path()).unwrap_or_default();
        n.sort_unstable_by(|a, b| b.cmp(a));
        used += n.iter().take(DAYS).sum::<usize>();
        if used > MOST_NAMES && !kept.is_empty() {
            break;
        }
        kept.insert(d);
    }
    drop(names);
    all.retain(|o| kept.contains(&o.dir));
    all.sort_by(|a, b| a.dir.cmp(&b.dir).then(b.at.cmp(&a.at)));
    let mut text = String::new();
    let mut run: (Option<&Path>, usize) = (None, 0);
    for o in &all {
        if run.0 == Some(o.dir.as_path()) {
            run.1 += 1;
        } else {
            run = (Some(o.dir.as_path()), 1);
        }
        if run.1 > DAYS {
            continue;
        }
        text.push_str(&format!("@{}\t{}\t{}\n", o.day, o.at, o.dir.display()));
        for (name, s) in &o.sizes {
            if !name.contains(['\n', '\t']) {
                text.push_str(&format!("{s}\t{name}\n"));
            }
        }
    }
    if let Some(d) = file.parent() {
        let _ = std::fs::create_dir_all(d);
    }
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, file).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// Keep what Space used counted in `dir`, on a thread.
pub fn note_later(dir: PathBuf, sizes: HashMap<String, u64>) {
    if cfg!(test) && std::env::var_os("MAIA_SIM_SANDBOX").is_none() {
        return;
    }
    let _ = std::thread::Builder::new().name("space readings".into()).spawn(move || {
        let now = SystemTime::now();
        let at = now.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        note_in(&book_path(), Reading { day: crate::timefmt::local_day(now), at, dir, sizes });
    });
}

/// A change in size as said beside a row: "+9.1 GB", "−3.2 GB", "same".
pub fn fmt_change(d: i64) -> String {
    if d.unsigned_abs() < 64 << 10 {
        return "same".into();
    }
    format!("{}{}", if d > 0 { "+" } else { "−" }, crate::fsx::fmt_size(d.unsigned_abs()))
}
