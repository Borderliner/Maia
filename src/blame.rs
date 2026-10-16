//! Who last changed each line of a file in a git work tree, from
//! `git blame --porcelain` run on a worker through `gitstatus::guard`.

use std::collections::HashMap;
use std::path::Path;

use egui::Color32;

use crate::theme::Palette;

#[derive(Clone, Debug, PartialEq)]
pub struct Commit {
    pub hash: String,
    pub author: String,
    pub time: i64,
    pub summary: String,
}

impl Commit {
    /// Lines changed in the work tree and not committed carry git's all-zero hash.
    pub fn committed(&self) -> bool {
        !self.hash.bytes().all(|b| b == b'0')
    }
}

#[derive(Debug, Default, PartialEq)]
pub struct Blame {
    pub commits: Vec<Commit>,
    /// The commit of each line of the file (an index into `commits`).
    pub lines: Vec<u32>,
}

/// Porcelain output: a header per line (hash, original line, final line), the commit's
/// details the first time it appears, then the line itself after a tab.
pub fn parse(out: &[u8]) -> Blame {
    let mut b = Blame::default();
    let mut index: HashMap<String, u32> = HashMap::new();
    let mut cur: Option<u32> = None;
    for raw in out.split(|&c| c == b'\n') {
        if raw.first() == Some(&b'\t') {
            continue;
        }
        let line = String::from_utf8_lossy(raw);
        let mut words = line.split(' ');
        let first = words.next().unwrap_or("");
        if first.len() >= 40 && first.bytes().all(|c| c.is_ascii_hexdigit()) {
            let fin: Option<usize> = words.nth(1).and_then(|w| w.parse().ok());
            let k = *index.entry(first.to_string()).or_insert_with(|| {
                b.commits.push(Commit { hash: first.to_string(), author: String::new(), time: 0, summary: String::new() });
                (b.commits.len() - 1) as u32
            });
            cur = Some(k);
            if let Some(n) = fin.filter(|&n| n >= 1 && n <= 10_000_000) {
                if b.lines.len() < n {
                    b.lines.resize(n, k);
                }
                b.lines[n - 1] = k;
            }
            continue;
        }
        let Some(c) = cur.and_then(|k| b.commits.get_mut(k as usize)) else { continue };
        let rest = line.get(first.len() + 1..).unwrap_or("");
        match first {
            "author" => c.author = rest.to_string(),
            "author-time" => c.time = rest.trim().parse().unwrap_or(0),
            "summary" => c.summary = rest.to_string(),
            _ => {}
        }
    }
    b
}

/// Blame `file` in the work tree at `root`.
pub fn run(root: &Path, file: &Path) -> Result<Blame, String> {
    let rel = file.strip_prefix(root).map_err(|_| "Not in this repository".to_string())?;
    let mut c = crate::media::tool("git");
    c.arg("--no-optional-locks").args(crate::gitstatus::guard(root)).arg("-C").arg(root).args(["blame", "--porcelain", "--"]).arg(rel);
    let out = crate::media::output_capped(c, std::time::Duration::from_secs(30), 256 << 20, &|| false).ok_or_else(|| "Not committed".to_string())?;
    let b = parse(&out);
    if b.lines.is_empty() && !out.is_empty() {
        return Err("Can't read the blame".into());
    }
    Ok(b)
}

/// The author as the gutter has room for: the first name.
pub fn short_author(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or(name)
}

/// How long ago, in the fewest characters: "now", "5m", "3h", "4d", "2w", "6mo", "3y".
pub fn short_ago(t: i64, now: i64) -> String {
    let d = (now - t).max(0);
    match d {
        0..60 => "now".into(),
        60..3600 => format!("{}m", d / 60),
        3600..86_400 => format!("{}h", d / 3600),
        86_400..604_800 => format!("{}d", d / 86_400),
        604_800..2_592_000 => format!("{}w", d / 604_800),
        2_592_000..31_536_000 => format!("{}mo", d / 2_592_000),
        _ => format!("{}y", d / 31_536_000),
    }
}

/// A commit's colour by its age: the accent for this week's work, fading toward the dim
/// text colour over the years.
pub fn age_color(t: i64, now: i64, pal: &Palette) -> Color32 {
    let days = ((now - t).max(0) as f32 / 86_400.0).max(0.0);
    let k = ((1.0 + days).ln() / (1.0f32 + 1825.0).ln()).clamp(0.0, 1.0);
    pal.accent.lerp_to_gamma(pal.text_dim.gamma_multiply(0.75), k)
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}
