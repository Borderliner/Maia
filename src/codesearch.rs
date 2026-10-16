//! Searching the text of a project's files (Ctrl+Alt+F): plain text, whole words, match
//! case or a regular expression, limited to file types and globs, skipping what the
//! project's `.gitignore`s leave out, hidden files and binary ones unless asked. The walk
//! runs on one thread and the files are read on a few more; matches stream back grouped
//! by file, in path order, with the lines around each. A search stops at `MAX_MATCHES`.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{Arc, Mutex, mpsc};

/// A search stops once it has this many matching lines.
pub const MAX_MATCHES: usize = 2000;
/// Lines shown per file; the rest are counted.
pub const PER_FILE: usize = 100;
/// Bigger files are passed over: generated bundles and data dumps, not code.
pub const MAX_BYTES: u64 = 8 << 20;
/// Bytes of a matching line kept to show.
const LINE_MAX: usize = 240;

/// The comment tags the TODO finder lists.
pub const TODO_TAGS: [&str; 4] = ["TODO", "FIXME", "HACK", "XXX"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Query {
    pub text: String,
    pub case: bool,
    pub word: bool,
    pub regex: bool,
    /// List TODO, FIXME, HACK and XXX comments; the text, when given, narrows them.
    pub todo: bool,
    /// File types and globs, comma or space separated ("rust, src/**, !*.min.js").
    pub files: String,
    /// Ignored, hidden and binary files too.
    pub all: bool,
}

/// One matching line.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    /// From 1.
    pub line: u32,
    /// The line without its indentation, cut around the match when long.
    pub text: String,
    /// Where the matches are in `text` (byte ranges).
    pub spans: Vec<(usize, usize)>,
    /// Text was cut off before and after.
    pub cut: (bool, bool),
    /// The lines on either side, when they hold anything.
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileHits {
    /// Place in the walk: results are kept in this order.
    pub seq: u64,
    pub path: PathBuf,
    /// The path from the search's root.
    pub rel: String,
    pub hits: Vec<Hit>,
    /// Matching lines in the file, shown or not.
    pub total: usize,
    pub binary: bool,
    /// TODO mode: how many of each tag.
    pub tags: [u32; 4],
}

// ------------------------------------------------------------------ matching

pub enum Matcher {
    /// Literal text found with memmem; `fold` searches an ASCII-lowercased copy.
    Bytes { needles: Vec<Vec<u8>>, fold: bool, word: bool },
    /// An expression, and literal text every match contains (one of them), when there is
    /// some: a file without any is passed over without running the expression.
    Re(regex_lite::Regex, Option<Prefilter>),
}

pub struct Prefilter {
    needles: Vec<Vec<u8>>,
    /// The needles are lowercase and the text is searched lowercased.
    fold: bool,
}

/// Literal text that every match of the expression `pat` holds: for each branch of a
/// top-level `a|b`, its longest run of plain characters that no quantifier makes
/// optional. None when a branch has no run of three or more.
pub fn required_literals(pat: &str) -> Option<Vec<String>> {
    let b: Vec<char> = pat.chars().collect();
    // Branches at the top level.
    let mut branches: Vec<(usize, usize)> = vec![];
    let (mut depth, mut start, mut i) = (0i32, 0usize, 0usize);
    while i < b.len() {
        match b[i] {
            '\\' => i += 1,
            '[' => i = class_end(&b, i),
            '(' => depth += 1,
            ')' => depth -= 1,
            '|' if depth == 0 => {
                branches.push((start, i));
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    branches.push((start, b.len()));
    let mut out = vec![];
    for (s, e) in branches {
        let mut runs: Vec<String> = vec![];
        let mut cur = String::new();
        let mut j = s;
        let end_run = |cur: &mut String, runs: &mut Vec<String>| {
            if !cur.is_empty() {
                runs.push(std::mem::take(cur));
            }
        };
        while j < e {
            let c = b[j];
            match c {
                '\\' if j + 1 < e => {
                    let n = b[j + 1];
                    j += 2;
                    if n.is_alphanumeric() {
                        end_run(&mut cur, &mut runs);
                    } else if matches!(b.get(j), Some('*' | '?' | '{')) {
                        end_run(&mut cur, &mut runs);
                    } else {
                        cur.push(n);
                    }
                    continue;
                }
                '[' => {
                    end_run(&mut cur, &mut runs);
                    j = class_end(&b, j) + 1;
                    continue;
                }
                '(' => {
                    end_run(&mut cur, &mut runs);
                    let mut d = 0;
                    while j < e {
                        match b[j] {
                            '\\' => j += 1,
                            '[' => j = class_end(&b, j),
                            '(' => d += 1,
                            ')' => {
                                d -= 1;
                                if d == 0 {
                                    break;
                                }
                            }
                            _ => {}
                        }
                        j += 1;
                    }
                }
                '*' | '?' | '{' => {
                    // The character before is optional: it leaves the run.
                    cur.pop();
                    end_run(&mut cur, &mut runs);
                    if c == '{' {
                        while j < e && b[j] != '}' {
                            j += 1;
                        }
                    }
                }
                '+' => end_run(&mut cur, &mut runs),
                '.' | '^' | '$' | '|' | ')' => end_run(&mut cur, &mut runs),
                _ => {
                    // A character a quantifier follows may be left out.
                    if matches!(b.get(j + 1), Some('*' | '?' | '{')) {
                        end_run(&mut cur, &mut runs);
                    } else {
                        cur.push(c);
                    }
                }
            }
            j += 1;
        }
        end_run(&mut cur, &mut runs);
        let best = runs.into_iter().max_by_key(|r| r.len())?;
        if best.chars().count() < 3 {
            return None;
        }
        out.push(best);
    }
    Some(out)
}

/// Where the class opening at `b[i]` closes (its `]`), or the end.
fn class_end(b: &[char], i: usize) -> usize {
    let mut j = i + 1;
    if b.get(j) == Some(&'^') {
        j += 1;
    }
    if b.get(j) == Some(&']') {
        j += 1;
    }
    while j < b.len() && b[j] != ']' {
        if b[j] == '\\' {
            j += 1;
        }
        j += 1;
    }
    j.min(b.len())
}

fn is_word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

/// Every place `needle` is in `hay`.
fn memmem_all(hay: &[u8], needle: &[u8], out: &mut Vec<(usize, usize)>) {
    if needle.is_empty() || needle.len() > hay.len() {
        return;
    }
    let mut at = 0;
    while at + needle.len() <= hay.len() {
        let rest = &hay[at..];
        // SAFETY: both slices are valid for their lengths; the result is inside `rest`.
        let p = unsafe { libc::memmem(rest.as_ptr().cast(), rest.len(), needle.as_ptr().cast(), needle.len()) };
        if p.is_null() {
            return;
        }
        let i = at + (p as usize - rest.as_ptr() as usize);
        out.push((i, i + needle.len()));
        at = i + needle.len();
    }
}

/// `needle` is somewhere in `hay`.
fn memmem_all_first(hay: &[u8], needle: &[u8]) -> bool {
    if needle.len() > hay.len() {
        return false;
    }
    // SAFETY: both slices are valid for their lengths.
    !unsafe { libc::memmem(hay.as_ptr().cast(), hay.len(), needle.as_ptr().cast(), needle.len()) }.is_null()
}

impl Matcher {
    /// The matcher for `text`: an error says what is wrong with a regular expression.
    pub fn new(text: &str, case: bool, word: bool, regex: bool) -> Result<Matcher, String> {
        if text.is_empty() {
            return Err(String::new());
        }
        let build = |pat: String, pre: Option<Prefilter>| {
            let flags = if case { "(?m)" } else { "(?mi)" };
            let full = if word { format!(r"{flags}\b(?:{pat})\b") } else { format!("{flags}{pat}") };
            match regex_lite::Regex::new(&full) {
                Ok(re) => Ok(Matcher::Re(re, pre)),
                Err(e) => Err(e.to_string().lines().last().unwrap_or("Not a valid expression").trim().to_string()),
            }
        };
        if regex {
            // Flags written in the expression may change its case: then no prefilter.
            let fold = !case;
            let pre = (!text.contains("(?")).then(|| required_literals(text)).flatten().filter(|l| !fold || l.iter().all(|x| x.is_ascii())).map(|l| Prefilter { needles: l.into_iter().map(|x| if fold { x.to_ascii_lowercase().into_bytes() } else { x.into_bytes() }).collect(), fold });
            return build(text.to_string(), pre);
        }
        if !case && !text.is_ascii() {
            // The regex engine folds ASCII only: each other letter becomes the class of its
            // cases ("é" -> [éÉ]).
            let mut pat = String::new();
            for c in text.chars() {
                let mut forms: Vec<char> = vec![c];
                forms.extend(c.to_lowercase().chain(c.to_uppercase()).filter(|_| c.to_lowercase().count() == 1 && c.to_uppercase().count() == 1));
                forms.dedup();
                forms.sort();
                forms.dedup();
                if forms.len() > 1 {
                    pat.push('[');
                    forms.iter().for_each(|f| pat.push_str(&regex_lite::escape(&f.to_string())));
                    pat.push(']');
                } else {
                    pat.push_str(&regex_lite::escape(&c.to_string()));
                }
            }
            return build(pat, None);
        }
        let needle = if case { text.as_bytes().to_vec() } else { text.to_ascii_lowercase().into_bytes() };
        Ok(Matcher::Bytes { needles: vec![needle], fold: !case, word })
    }

    /// TODO, FIXME, HACK and XXX as whole words, in capitals.
    pub fn todo() -> Matcher {
        Matcher::Bytes { needles: TODO_TAGS.iter().map(|t| t.as_bytes().to_vec()).collect(), fold: false, word: true }
    }

    /// The matches in `text`, in order, none overlapping. `low` is scratch room.
    pub fn find(&self, text: &str, low: &mut Vec<u8>, out: &mut Vec<(usize, usize)>) {
        out.clear();
        match self {
            Matcher::Re(re, pre) => {
                if let Some(p) = pre {
                    let hay = if p.fold {
                        low.clear();
                        low.extend(text.bytes().map(|b| b.to_ascii_lowercase()));
                        &low[..]
                    } else {
                        text.as_bytes()
                    };
                    if !p.needles.iter().any(|n| memmem_all_first(hay, n)) {
                        return;
                    }
                }
                out.extend(re.find_iter(text).filter(|m| m.end() > m.start()).map(|m| (m.start(), m.end())));
            }
            Matcher::Bytes { needles, fold, word } => {
                let hay = text.as_bytes();
                let hay = if *fold {
                    low.clear();
                    low.extend(hay.iter().map(u8::to_ascii_lowercase));
                    &low[..]
                } else {
                    hay
                };
                for n in needles {
                    memmem_all(hay, n, out);
                }
                if *word {
                    let b = text.as_bytes();
                    out.retain(|&(s, e)| (s == 0 || !is_word(b[s - 1])) && (e == b.len() || !is_word(b[e])));
                }
                if needles.len() > 1 {
                    out.sort_unstable();
                    let mut end = 0;
                    out.retain(|&(s, e)| {
                        let keep = s >= end;
                        if keep {
                            end = e;
                        }
                        keep
                    });
                }
            }
        }
    }
}

/// The tag a TODO line carries and what it says after it ("TODO(ana): fix" -> TODO, "fix").
pub fn todo_tag(line: &str) -> Option<(usize, &str)> {
    let b = line.as_bytes();
    let mut best: Option<(usize, usize)> = None;
    for (k, t) in TODO_TAGS.iter().enumerate() {
        let mut from = 0;
        while let Some(i) = line[from..].find(t).map(|i| i + from) {
            let e = i + t.len();
            if (i == 0 || !is_word(b[i - 1])) && (e == b.len() || !is_word(b[e])) {
                if best.is_none_or(|(_, at)| i < at) {
                    best = Some((k, i));
                }
                break;
            }
            from = e;
        }
    }
    let (k, i) = best?;
    let mut rest = &line[i + TODO_TAGS[k].len()..];
    // An owner or issue in brackets, then the colon.
    if rest.starts_with('(') {
        rest = rest.find(')').map_or(rest, |j| &rest[j + 1..]);
    }
    let rest = rest.trim_start_matches([':', '-', ' ', '\t', '!']).trim_end();
    let rest = rest.strip_suffix("*/").or_else(|| rest.strip_suffix("-->")).unwrap_or(rest).trim_end();
    Some((k, rest))
}

// ------------------------------------------------------------------ file filter

/// Names a file filter takes for a set of extensions.
const TYPES: &[(&str, &[&str])] = &[
    ("rust", &["rs"]),
    ("js", &["js", "mjs", "cjs", "jsx"]),
    ("ts", &["ts", "tsx", "mts", "cts"]),
    ("python", &["py", "pyi"]),
    ("py", &["py", "pyi"]),
    ("c", &["c", "h"]),
    ("cpp", &["cpp", "cc", "cxx", "hpp", "hh", "hxx", "h"]),
    ("go", &["go"]),
    ("java", &["java"]),
    ("kotlin", &["kt", "kts"]),
    ("ruby", &["rb"]),
    ("shell", &["sh", "bash", "zsh"]),
    ("sh", &["sh", "bash", "zsh"]),
    ("markdown", &["md", "markdown"]),
    ("md", &["md", "markdown"]),
    ("yaml", &["yaml", "yml"]),
    ("html", &["html", "htm"]),
    ("css", &["css", "scss", "sass", "less"]),
    ("csharp", &["cs"]),
    ("web", &["html", "htm", "css", "scss", "js", "jsx", "ts", "tsx", "vue", "svelte"]),
];

#[derive(Clone, Debug, PartialEq)]
enum Pat {
    Ext(String),
    /// Matched against the file name.
    Name(Vec<u8>),
    /// Matched against the path from the root.
    Path(Vec<u8>),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileFilter {
    include: Vec<Pat>,
    exclude: Vec<Pat>,
}

impl FileFilter {
    /// "rust, *.toml, src/**, !tests/": types and extensions, name globs, path globs (a
    /// slash in them; a trailing one means everything inside), each excluded with `!`.
    pub fn parse(s: &str) -> FileFilter {
        let mut f = FileFilter::default();
        for tok in s.split(|c: char| c == ',' || c.is_whitespace()).filter(|t| !t.is_empty()) {
            let (neg, tok) = match tok.strip_prefix('!') {
                Some(t) => (true, t),
                None => (false, tok),
            };
            if tok.is_empty() {
                continue;
            }
            let mut pats = vec![];
            if tok.contains('/') {
                let t = tok.trim_start_matches("./").trim_start_matches('/');
                let t = if t.ends_with('/') { format!("{t}**") } else { t.to_string() };
                pats.push(Pat::Path(t.into_bytes()));
            } else if tok.contains(['*', '?', '[']) {
                pats.push(Pat::Name(tok.as_bytes().to_vec()));
            } else if let Some(e) = tok.strip_prefix('.') {
                pats.push(Pat::Ext(e.to_ascii_lowercase()));
            } else if let Some((_, exts)) = TYPES.iter().find(|(n, _)| n.eq_ignore_ascii_case(tok)) {
                pats.extend(exts.iter().map(|e| Pat::Ext(e.to_string())));
            } else {
                pats.push(Pat::Ext(tok.to_ascii_lowercase()));
            }
            if neg { &mut f.exclude } else { &mut f.include }.extend(pats);
        }
        f
    }

    pub fn accepts(&self, rel: &str, name: &str) -> bool {
        let hit = |p: &Pat| match p {
            Pat::Ext(e) => name.len() > e.len() && name.as_bytes()[name.len() - e.len() - 1] == b'.' && name[name.len() - e.len()..].eq_ignore_ascii_case(e),
            Pat::Name(g) => wildmatch(g, name.as_bytes()),
            Pat::Path(g) => wildmatch(g, rel.as_bytes()),
        };
        (self.include.is_empty() || self.include.iter().any(hit)) && !self.exclude.iter().any(hit)
    }
}

// ------------------------------------------------------------------ gitignore

/// Glob matching as git does it for paths: `*` and `?` stop at a slash, `**` between
/// slashes (or at either end) crosses any number of folders, `[a-z]` and `[!x]` are
/// classes and a backslash takes the next character as it is.
pub fn wildmatch(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0, 0);
    while pi < p.len() {
        match p[pi] {
            b'*' => {
                let mut pj = pi;
                while pj < p.len() && p[pj] == b'*' {
                    pj += 1;
                }
                let double = pj - pi >= 2 && (pi == 0 || p[pi - 1] == b'/') && (pj == p.len() || p[pj] == b'/');
                if double {
                    if pj == p.len() {
                        return true;
                    }
                    let rest = &p[pj + 1..];
                    if wildmatch(rest, &s[si..]) {
                        return true;
                    }
                    return (si..s.len()).any(|k| s[k] == b'/' && wildmatch(rest, &s[k + 1..]));
                }
                let rest = &p[pj..];
                let mut k = si;
                loop {
                    if wildmatch(rest, &s[k..]) {
                        return true;
                    }
                    if k == s.len() || s[k] == b'/' {
                        return false;
                    }
                    k += 1;
                }
            }
            b'?' => {
                if si >= s.len() || s[si] == b'/' {
                    return false;
                }
                pi += 1;
                si += 1;
            }
            b'[' if class(p, pi, 0).is_some() => {
                let Some(&c) = s.get(si) else { return false };
                let Some((ok, next)) = class(p, pi, c) else { return false };
                if !ok || c == b'/' {
                    return false;
                }
                pi = next;
                si += 1;
            }
            c => {
                let (c, step) = if c == b'\\' && pi + 1 < p.len() { (p[pi + 1], 2) } else { (c, 1) };
                if s.get(si) != Some(&c) {
                    return false;
                }
                pi += step;
                si += 1;
            }
        }
    }
    si == s.len()
}

/// A bracket class at `p[i]`: whether `c` is in it and where the pattern goes on; None
/// when the bracket never closes (it is then an ordinary character).
fn class(p: &[u8], i: usize, c: u8) -> Option<(bool, usize)> {
    let mut j = i + 1;
    let neg = matches!(p.get(j), Some(b'!' | b'^'));
    if neg {
        j += 1;
    }
    let (mut ok, mut first) = (false, true);
    while j < p.len() && (p[j] != b']' || first) {
        first = false;
        let mut lo = p[j];
        if lo == b'\\' && j + 1 < p.len() {
            j += 1;
            lo = p[j];
        }
        if p.get(j + 1) == Some(&b'-') && p.get(j + 2).is_some_and(|&x| x != b']') {
            ok |= lo <= c && c <= p[j + 2];
            j += 3;
        } else {
            ok |= lo == c;
            j += 1;
        }
    }
    (j < p.len()).then_some((ok != neg, j + 1))
}

#[derive(Clone, Debug, PartialEq)]
pub struct Rule {
    pat: Vec<u8>,
    neg: bool,
    dir_only: bool,
    /// Has a slash: matched against the path from the file's folder, not the name.
    anchored: bool,
}

/// The rules of a `.gitignore`.
pub fn parse_gitignore(text: &str) -> Vec<Rule> {
    let mut out = vec![];
    for line in text.lines() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        // Trailing spaces go unless escaped.
        let mut l = line.to_string();
        while l.ends_with(' ') && !l.ends_with("\\ ") {
            l.pop();
        }
        if l.ends_with("\\ ") {
            l.truncate(l.len() - 2);
            l.push(' ');
        }
        let (neg, mut l) = match l.strip_prefix('!') {
            Some(r) => (true, r.to_string()),
            None => (false, l.strip_prefix('\\').filter(|r| r.starts_with(['#', '!'])).map(String::from).unwrap_or(l)),
        };
        let dir_only = l.ends_with('/');
        while l.ends_with('/') {
            l.pop();
        }
        if l.is_empty() {
            continue;
        }
        let anchored = l.contains('/');
        let l = l.strip_prefix('/').unwrap_or(&l).to_string();
        out.push(Rule { pat: l.into_bytes(), neg, dir_only, anchored });
    }
    out
}

/// What the last rule matching a path says: Some(true) ignored, Some(false) kept again
/// (`!`), None when no rule speaks of it.
pub fn decide(rules: &[Rule], rel: &str, is_dir: bool) -> Option<bool> {
    let name = rel.rsplit('/').next().unwrap_or(rel);
    rules.iter().rev().find(|r| (is_dir || !r.dir_only) && wildmatch(&r.pat, if r.anchored { rel } else { name }.as_bytes())).map(|r| !r.neg)
}

/// The ignore files that apply in a folder: its own, then those of the folders above it,
/// up to the repository's exclude file and the user's global one.
pub struct Ignore {
    parent: Option<Arc<Ignore>>,
    base: PathBuf,
    rules: Vec<Rule>,
}

impl Ignore {
    pub fn new(parent: Option<Arc<Ignore>>, base: &Path, rules: Vec<Rule>) -> Arc<Ignore> {
        Arc::new(Ignore { parent, base: base.to_path_buf(), rules })
    }

    /// The deepest file with a rule for `path` decides.
    pub fn ignored(&self, path: &Path, is_dir: bool) -> bool {
        let mut at = Some(self);
        while let Some(n) = at {
            if let Ok(rel) = path.strip_prefix(&n.base) {
                if let Some(v) = decide(&n.rules, &rel.to_string_lossy(), is_dir) {
                    return v;
                }
            }
            at = n.parent.as_deref();
        }
        false
    }
}

fn read_rules(p: &Path) -> Option<Vec<Rule>> {
    let m = std::fs::metadata(p).ok()?;
    if !m.is_file() || m.len() > 1 << 20 {
        return None;
    }
    std::fs::read_to_string(p).ok().map(|t| parse_gitignore(&t))
}

/// The ignore files above `root` that apply under it: the user's global one, the
/// repository's exclude file, and the `.gitignore`s from the repository's top down to
/// `root`'s parent (`root`'s own is read when the walk lists it).
pub fn ignores_above(root: &Path, repo: Option<&Path>) -> Option<Arc<Ignore>> {
    let repo = repo?;
    let mut chain = None;
    // The user's own excludes, where git looks for them by default (never in tests: the
    // results must not depend on the machine).
    let global = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).filter(|p| p.is_absolute()).unwrap_or_else(|| crate::fsx::home().join(".config")).join("git/ignore");
    let global = (!cfg!(test)).then_some(global);
    for p in global.into_iter().chain([repo.join(".git/info/exclude")]) {
        if let Some(r) = read_rules(&p) {
            chain = Some(Ignore::new(chain, repo, r));
        }
    }
    let mut dirs: Vec<&Path> = root.ancestors().skip(1).take_while(|d| d.starts_with(repo)).collect();
    dirs.reverse();
    for d in dirs {
        if let Some(r) = read_rules(&d.join(".gitignore")) {
            chain = Some(Ignore::new(chain, d, r));
        }
    }
    chain
}

// ------------------------------------------------------------------ one file

/// The matches in one file's text, as lines with their neighbours. `also`, when given,
/// must match on the line too (TODO mode's narrowing text). Returns the shown hits and
/// the number of matching lines.
pub fn hits_in(text: &str, m: &Matcher, also: Option<&Matcher>, limit: usize, low: &mut Vec<u8>, spans: &mut Vec<(usize, usize)>) -> (Vec<Hit>, usize) {
    m.find(text, low, spans);
    let b = text.as_bytes();
    let (mut out, mut total) = (vec![], 0);
    let (mut line_no, mut counted) = (1u32, 0usize);
    let mut k = 0;
    let mut extra: Vec<(usize, usize)> = vec![];
    while k < spans.len() {
        let s = spans[k].0;
        line_no += b[counted..s].iter().filter(|&&c| c == b'\n').count() as u32;
        counted = s;
        let ls = b[..s].iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1);
        let le = b[s..].iter().position(|&c| c == b'\n').map_or(b.len(), |i| s + i);
        let mut on_line: Vec<(usize, usize)> = vec![];
        while k < spans.len() && spans[k].0 < le.max(s + 1) {
            on_line.push((spans[k].0, spans[k].1.min(le)));
            k += 1;
        }
        if let Some(a) = also {
            a.find(&text[ls..le], low, &mut extra);
            if extra.is_empty() {
                continue;
            }
            on_line.extend(extra.iter().map(|&(x, y)| (ls + x, ls + y)));
            on_line.sort_unstable();
        }
        total += 1;
        if out.len() < limit {
            out.push(make_hit(text, line_no, ls, le, &on_line));
        }
    }
    (out, total)
}

/// A line of `text` (bytes `ls..le`) made ready to show: indentation off, cut to
/// `LINE_MAX` around its first match, spans moved to match.
fn make_hit(text: &str, line: u32, ls: usize, le: usize, spans: &[(usize, usize)]) -> Hit {
    let b = text.as_bytes();
    let le = if le > ls && b[le - 1] == b'\r' { le - 1 } else { le };
    let mut start = ls + b[ls..le].iter().take_while(|c| **c == b' ' || **c == b'\t').count();
    let first = spans.first().map_or(start, |s| s.0).max(start);
    let mut cut_left = false;
    if le - start > LINE_MAX && first - start > 40 {
        start = text.floor_char_boundary(first - 40);
        cut_left = true;
    }
    let end = if le - start > LINE_MAX { text.floor_char_boundary(start + LINE_MAX) } else { le };
    let spans = spans.iter().filter(|s| s.1 > start && s.0 < end).map(|&(s, e)| (s.max(start) - start, e.min(end) - start)).collect();
    let neighbour = |from: usize, to: usize| -> Option<String> {
        let l = text[from..to].trim_end_matches('\r').trim();
        (!l.is_empty()).then(|| l[..l.floor_char_boundary(LINE_MAX)].to_string())
    };
    let before = (ls > 0).then(|| {
        let pe = ls - 1;
        let ps = b[..pe].iter().rposition(|&c| c == b'\n').map_or(0, |i| i + 1);
        neighbour(ps, pe)
    }).flatten();
    let after = (le < b.len()).then(|| {
        let ns = b[le..].iter().position(|&c| c == b'\n').map_or(b.len(), |i| le + i + 1).min(b.len());
        let ne = b[ns..].iter().position(|&c| c == b'\n').map_or(b.len(), |i| ns + i);
        neighbour(ns, ne)
    }).flatten();
    Hit { line, text: text[start..end].to_string(), spans, cut: (cut_left, end < le), before, after }
}

/// Read and search one file. None: no match, unreadable, too big, or binary when only
/// text is wanted.
#[allow(clippy::too_many_arguments)]
fn search_file(path: &Path, m: &Matcher, also: Option<&Matcher>, todo: bool, all: bool, limit: usize, buf: &mut Vec<u8>, low: &mut Vec<u8>, spans: &mut Vec<(usize, usize)>) -> Option<(Vec<Hit>, usize, bool, [u32; 4])> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    // Non-blocking open: a FIFO that slipped through must not hang a worker.
    let f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).ok()?;
    let md = f.metadata().ok()?;
    if !md.is_file() || md.len() > MAX_BYTES || md.len() == 0 {
        return None;
    }
    buf.clear();
    f.take(MAX_BYTES).read_to_end(buf).ok()?;
    let binary = buf[..buf.len().min(8192)].contains(&0);
    if binary && !all {
        return None;
    }
    let text: Cow<str> = String::from_utf8_lossy(buf);
    let (hits, total) = hits_in(&text, m, also, if binary { 0 } else { limit }, low, spans);
    if total == 0 {
        return None;
    }
    let mut tags = [0u32; 4];
    if todo {
        for h in &hits {
            if let Some((k, _)) = todo_tag(&h.text) {
                tags[k] += 1;
            }
        }
    }
    Some((hits, total, binary, tags))
}

// ------------------------------------------------------------------ the search

#[derive(Default)]
struct Shared {
    found: Mutex<Vec<FileHits>>,
    root: Mutex<Option<PathBuf>>,
    cancel: AtomicBool,
    done: AtomicBool,
    searched: AtomicUsize,
    matched: AtomicUsize,
    capped: AtomicBool,
}

/// A search in progress; dropping it stops it.
pub struct Search {
    shared: Arc<Shared>,
}

impl Drop for Search {
    fn drop(&mut self) {
        self.shared.cancel.store(true, Relaxed);
    }
}

impl Search {
    /// Search from `start` (`project`: from the top of its project) for `q`. The
    /// matcher is made here, so a bad expression is an error at once.
    pub fn start(start: PathBuf, project: bool, q: &Query, ctx: &egui::Context) -> Result<Search, String> {
        let text = q.text.trim();
        let (m, also) = if q.todo {
            (Matcher::todo(), if text.is_empty() { None } else { Some(Matcher::new(text, q.case, q.word, q.regex)?) })
        } else {
            (Matcher::new(text, q.case, q.word, q.regex)?, None)
        };
        let shared = Arc::new(Shared::default());
        let (sh, ctx, filter, todo, all) = (shared.clone(), ctx.clone(), FileFilter::parse(&q.files), q.todo, q.all);
        std::thread::Builder::new()
            .name("codesearch".into())
            .spawn(move || {
                let root = if project { crate::project::root(&start) } else { start };
                if let Ok(mut r) = sh.root.lock() {
                    *r = Some(root.clone());
                }
                ctx.request_repaint();
                if !crate::fsx::is_virtual_fs(&root) {
                    run(&root, &m, also.as_ref(), &filter, todo, all, &sh, &ctx);
                }
                sh.done.store(true, Relaxed);
                ctx.request_repaint();
            })
            .map_err(|e| e.to_string())?;
        Ok(Search { shared })
    }

    /// What was found since the last call.
    pub fn take(&self) -> Vec<FileHits> {
        self.shared.found.lock().map(|mut f| std::mem::take(&mut *f)).unwrap_or_default()
    }

    pub fn done(&self) -> bool {
        self.shared.done.load(Relaxed)
    }

    /// Where it searches from, once worked out.
    pub fn root(&self) -> Option<PathBuf> {
        self.shared.root.lock().ok().and_then(|r| r.clone())
    }

    pub fn searched(&self) -> usize {
        self.shared.searched.load(Relaxed)
    }

    /// Stopped at `MAX_MATCHES`.
    pub fn capped(&self) -> bool {
        self.shared.capped.load(Relaxed)
    }

    /// Run to the end on this thread and hand back everything, in order (tests).
    #[cfg(test)]
    pub fn wait(self) -> Vec<FileHits> {
        while !self.done() {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let mut v = self.take();
        v.sort_by_key(|f| f.seq);
        v
    }
}

/// The walk (on this thread) and the readers (a few more), until both are through.
#[allow(clippy::too_many_arguments)]
fn run(root: &Path, m: &Matcher, also: Option<&Matcher>, filter: &FileFilter, todo: bool, all: bool, sh: &Shared, ctx: &egui::Context) {
    let workers = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).clamp(1, 6);
    let (tx, rx) = mpsc::sync_channel::<(u64, PathBuf, String)>(4096);
    let rx = Mutex::new(rx);
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                let (mut buf, mut low, mut spans) = (Vec::new(), Vec::new(), Vec::new());
                loop {
                    let job = rx.lock().ok().and_then(|r| r.recv().ok());
                    let Some((seq, path, rel)) = job else { return };
                    if sh.cancel.load(Relaxed) {
                        continue;
                    }
                    let got = search_file(&path, m, also, todo, all, PER_FILE, &mut buf, &mut low, &mut spans);
                    sh.searched.fetch_add(1, Relaxed);
                    let Some((mut hits, total, binary, tags)) = got else { continue };
                    let n = hits.len().max(1);
                    let before = sh.matched.fetch_add(n, Relaxed);
                    if before >= MAX_MATCHES {
                        sh.capped.store(true, Relaxed);
                        sh.cancel.store(true, Relaxed);
                        continue;
                    }
                    if before + n > MAX_MATCHES {
                        hits.truncate(MAX_MATCHES - before);
                        sh.capped.store(true, Relaxed);
                        sh.cancel.store(true, Relaxed);
                    }
                    if let Ok(mut f) = sh.found.lock() {
                        f.push(FileHits { seq, path, rel, hits, total, binary, tags });
                    }
                    ctx.request_repaint();
                }
            });
        }
        walk(root, filter, all, &tx, sh);
        drop(tx);
    });
}

/// Every file under `root` that the filter takes and the ignore files leave in, handed to
/// the readers in path order. Links are not followed and other file systems not entered.
fn walk(root: &Path, filter: &FileFilter, all: bool, tx: &mpsc::SyncSender<(u64, PathBuf, String)>, sh: &Shared) {
    use std::os::unix::fs::MetadataExt;
    let Ok(dev) = std::fs::metadata(root).map(|m| m.dev()) else { return };
    let repo = crate::project::git_root(root);
    let above = if all { None } else { ignores_above(root, repo.as_deref()) };
    enum Item {
        Dir(PathBuf, Option<Arc<Ignore>>),
        File(PathBuf, String),
    }
    let mut stack = vec![Item::Dir(root.to_path_buf(), above)];
    let mut seq = 0u64;
    while let Some(item) = stack.pop() {
        if sh.cancel.load(Relaxed) {
            return;
        }
        let (dir, ign) = match item {
            Item::File(p, rel) => {
                seq += 1;
                if tx.send((seq, p, rel)).is_err() {
                    return;
                }
                continue;
            }
            Item::Dir(d, i) => (d, i),
        };
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        let mut kids: Vec<(String, std::fs::FileType, PathBuf)> = rd.flatten().filter_map(|e| Some((e.file_name().to_string_lossy().into_owned(), e.file_type().ok()?, e.path()))).collect();
        let ign = if all {
            None
        } else {
            match kids.iter().find(|k| k.0 == ".gitignore" && k.1.is_file()).and_then(|k| read_rules(&k.2)) {
                Some(r) => Some(Ignore::new(ign, &dir, r)),
                None => ign,
            }
        };
        kids.sort_by(|a, b| a.0.to_lowercase().cmp(&b.0.to_lowercase()).then_with(|| a.0.cmp(&b.0)));
        for (name, ft, path) in kids.into_iter().rev() {
            if name == ".git" || ft.is_symlink() || (!all && name.starts_with('.')) {
                continue;
            }
            let is_dir = ft.is_dir();
            if !is_dir && !ft.is_file() {
                continue;
            }
            if ign.as_ref().is_some_and(|i| i.ignored(&path, is_dir)) {
                continue;
            }
            if is_dir {
                if std::fs::symlink_metadata(&path).is_ok_and(|m| m.dev() == dev) {
                    stack.push(Item::Dir(path, ign.clone()));
                }
            } else {
                let rel = crate::project::relative(&path, root);
                if filter.accepts(&rel, &name) {
                    stack.push(Item::File(path, rel));
                }
            }
        }
    }
}
