//! Commit history of a work tree as a vertical graph in the inspector: newest at the top,
//! one row per commit, each line of work in a lane of its own. A single `git log` runs
//! per history and is held back by its own pipe between pages, so a repository of any
//! size costs only the pages scrolled through. Nothing here writes to the repository.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use egui::epaint::CubicBezierShape;
use egui::text::{LayoutJob, TextFormat, TextWrapping};
use egui::{Align, Align2, Color32, CornerRadius, FontId, Key, Layout, Pos2, Rect, ScrollArea, Sense, Stroke, Ui, UiBuilder, Vec2, pos2, vec2};

use crate::listing::Env;
use crate::theme::{Glyph, Palette};

/// Commits read per page; the next page is read when the view nears the end.
pub const PAGE: usize = 300;
/// Lanes drawn side by side. Lanes past these share one faint column, so a history with
/// dozens of open branches stays readable.
pub const LANES: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefKind {
    /// The branch HEAD is on, or HEAD itself when detached.
    Head,
    Branch,
    Remote,
    Tag,
}

pub struct Commit {
    pub hash: String,
    pub parents: Vec<String>,
    pub author: String,
    pub time: i64,
    pub refs: Vec<(RefKind, String)>,
    pub subject: String,
}

/// A commit's place in the graph: its lane, whether a child above continues into it, the
/// other lanes that end in it (children on other lanes), the lanes its parents continue
/// in (first parent first) and the lanes passing by.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub col: u16,
    pub up: bool,
    pub ins: Vec<u16>,
    pub outs: Vec<u16>,
    pub pass: Vec<u16>,
}

/// Which commit each lane is heading for, carried from one page to the next.
#[derive(Default)]
pub struct Lanes(Vec<Option<String>>);

impl Lanes {
    pub fn place(&mut self, c: &Commit) -> Row {
        let l = &mut self.0;
        let hits: Vec<u16> = (0..l.len()).filter(|&i| l[i].as_deref() == Some(&c.hash)).map(|i| i as u16).collect();
        let free = |l: &mut Vec<Option<String>>| {
            let i = l.iter().position(Option::is_none).unwrap_or(l.len());
            if i == l.len() {
                l.push(None);
            }
            i
        };
        let col = match hits.first() {
            Some(&h) => h as usize,
            None => free(l),
        };
        for &h in &hits {
            l[h as usize] = None;
        }
        let pass = (0..l.len()).filter(|&i| l[i].is_some()).map(|i| i as u16).collect();
        let mut outs = Vec::with_capacity(c.parents.len());
        for (k, p) in c.parents.iter().enumerate() {
            // The first parent always continues straight down, even when another lane
            // already heads for it: the two meet at the parent, where the branch began,
            // instead of the trunk stepping sideways here.
            let j = match l.iter().position(|x| x.as_deref() == Some(p.as_str())) {
                Some(j) if k > 0 => j,
                _ => {
                    let j = if l[col].is_none() { col } else { free(l) };
                    l[j] = Some(p.clone());
                    j
                }
            };
            outs.push(j as u16);
        }
        while l.last().is_some_and(Option::is_none) {
            l.pop();
        }
        Row { col: col as u16, up: !hits.is_empty(), ins: hits.get(1..).unwrap_or_default().to_vec(), outs, pass }
    }
}

/// One `git log -z --format=…` record (see `FORMAT`).
pub fn parse(rec: &[u8]) -> Option<Commit> {
    let s = String::from_utf8_lossy(rec);
    let mut f = s.trim_start_matches('\n').split('\x1f');
    let hash = f.next()?.to_string();
    let parents = f.next()?.split_whitespace().map(String::from).collect();
    let author = f.next()?.to_string();
    let time = f.next()?.parse().unwrap_or(0);
    let mut refs = vec![];
    for d in f.next()?.split(", ") {
        let r = match d.strip_prefix("HEAD -> ") {
            Some(b) => (RefKind::Head, b.trim_start_matches("refs/heads/")),
            None if d == "HEAD" => (RefKind::Head, d),
            None => match d.strip_prefix("tag: refs/tags/") {
                Some(t) => (RefKind::Tag, t),
                None => match (d.strip_prefix("refs/heads/"), d.strip_prefix("refs/remotes/")) {
                    (Some(b), _) => (RefKind::Branch, b),
                    (_, Some(r)) if !r.ends_with("/HEAD") => (RefKind::Remote, r),
                    // Stash, notes and a remote's HEAD pointer.
                    _ => continue,
                },
            },
        };
        refs.push((r.0, r.1.to_string()));
    }
    let subject = f.next()?.to_string();
    (hash.len() >= 7).then_some(Commit { hash, parents, author, time, refs, subject })
}

pub const FORMAT: &str = "--format=%H%x1f%P%x1f%an%x1f%at%x1f%D%x1f%s";

/// Up to `n` records from a running `git log -z`.
pub fn read_page(r: &mut impl BufRead, n: usize) -> Vec<Commit> {
    let (mut v, mut buf) = (vec![], vec![]);
    while v.len() < n {
        buf.clear();
        if r.read_until(0, &mut buf).unwrap_or(0) == 0 {
            break;
        }
        v.extend(parse(buf.strip_suffix(&[0]).unwrap_or(&buf)));
    }
    v
}

fn git(root: &Path) -> std::process::Command {
    let mut c = crate::media::tool("git");
    c.arg("--no-optional-locks").arg("--literal-pathspecs").arg("-C").arg(root).args(["-c", "log.showSignature=false", "-c", "core.quotePath=false"]);
    c
}

/// `git log` over every branch, remote and tag. Topological order keeps each line of work
/// together, so lanes stay short and few; with a commit-graph file (written by `git gc`)
/// git streams it as it walks, and without one the first page waits for the walk, on
/// this thread, never the UI's.
pub fn log_args() -> [&'static str; 11] {
    ["log", "--topo-order", "-z", "--no-color", "--decorate=full", FORMAT, "--branches", "--remotes", "--tags", "HEAD", "--"]
}

/// Output of a short git command, at most `limit` bytes (the rest is cut, and the
/// process killed). The flag says whether it was cut.
fn run(root: &Path, args: &[&str], limit: u64) -> (Vec<u8>, bool) {
    let mut v = vec![];
    let Ok(mut child) = git(root).args(args).stdout(Stdio::piped()).spawn() else { return (v, false) };
    if let Some(out) = child.stdout.take() {
        let _ = out.take(limit + 1).read_to_end(&mut v);
    }
    let cut = v.len() as u64 > limit;
    v.truncate(limit as usize);
    let _ = child.kill();
    let _ = child.wait();
    (v, cut)
}

#[derive(Clone)]
pub struct Change {
    /// `A`dded, `M`odified, `D`eleted, `R`enamed, `C`opied or `T`ype changed.
    pub status: u8,
    pub path: String,
    pub from: Option<String>,
}

/// `git diff-tree -z --name-status` output.
pub fn parse_changes(out: &[u8]) -> Vec<Change> {
    let mut v = vec![];
    let mut it = out.split(|&b| b == 0).map(|s| String::from_utf8_lossy(s).into_owned());
    while let Some(st) = it.next() {
        let Some(&k) = st.as_bytes().first() else { continue };
        let Some(a) = it.next() else { break };
        let (path, from) = if matches!(k, b'R' | b'C') { (it.next().unwrap_or_default(), Some(a)) } else { (a, None) };
        v.push(Change { status: k, path, from });
    }
    v
}

/// The commit against its first parent (a merge shows what it brought in), or against
/// nothing for a root commit.
fn base<'a>(hash: &'a str, parents: &'a [String]) -> [&'a str; 2] {
    match parents.first() {
        Some(p) => [p, hash],
        None => ["--root", hash],
    }
}

/// The changed files of a commit, and its message body.
pub fn changes(root: &Path, hash: &str, parents: &[String]) -> (String, Vec<Change>) {
    let mut a = vec!["diff-tree", "--no-commit-id", "-r", "-z", "-M", "--name-status"];
    a.extend(base(hash, parents));
    let body = run(root, &["show", "-s", "--format=%b", hash], 64 << 10).0;
    (String::from_utf8_lossy(&body).trim().to_string(), parse_changes(&run(root, &a, 4 << 20).0))
}

pub type Spans = Vec<Vec<(usize, usize, crate::syntax::Kind)>>;

/// One file's change in a commit, from its first hunk on, with code highlighted past
/// each line's `+`, `-` or space. The flag says the diff was cut at 1 MB.
pub fn diff(root: &Path, hash: &str, parents: &[String], ch: &Change) -> (Vec<String>, Option<Spans>, bool) {
    let mut a = vec!["diff-tree", "-p", "-M", "--no-color", "--no-ext-diff"];
    a.extend(base(hash, parents));
    a.extend(["--", &ch.path]);
    a.extend(ch.from.as_deref());
    let (out, cut) = run(root, &a, 1 << 20);
    let lines = diff_lines(&out);
    let spans = crate::syntax::lang_for(&ch.path).map(|lang| {
        let code: Vec<String> = lines.iter().map(|l| if l.starts_with("@@") { String::new() } else { l.get(1..).unwrap_or("").to_string() }).collect();
        crate::syntax::highlight(lang, &code, 20_000).into_iter().map(|s| s.into_iter().map(|(a, b, k)| (a + 1, b + 1, k)).collect()).collect()
    });
    (lines, spans, cut)
}

pub fn diff_lines(out: &[u8]) -> Vec<String> {
    let s = String::from_utf8_lossy(out);
    s.lines().skip_while(|l| !l.starts_with("@@")).map(|l| if l.len() > 2000 { format!("{}…", &l[..l.floor_char_boundary(2000)]) } else { l.replace('\t', "    ") }).collect()
}

/// "3 days ago" for a commit time (seconds since the epoch).
pub fn ago(t: i64, now: i64) -> String {
    let d = (now - t).max(0);
    let (n, unit) = match d {
        0..60 => return "just now".into(),
        60..3600 => (d / 60, "minute"),
        3600..86_400 => (d / 3600, "hour"),
        86_400..604_800 => (d / 86_400, "day"),
        604_800..2_592_000 => (d / 604_800, "week"),
        2_592_000..31_536_000 => (d / 2_592_000, "month"),
        _ => (d / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// A worker's answer, once it has one.
type Slot<T> = Arc<Mutex<Option<T>>>;

fn spawn<T: Send + 'static>(ctx: &egui::Context, f: impl FnOnce() -> T + Send + 'static) -> Slot<T> {
    let (slot, ctx) = (Slot::default(), ctx.clone());
    let theirs = Arc::clone(&slot);
    std::thread::spawn(move || {
        let v = f();
        if let Ok(mut s) = theirs.lock() {
            *s = Some(v);
        }
        ctx.request_repaint();
    });
    slot
}

fn take<T>(slot: &Option<Slot<T>>) -> Option<T> {
    slot.as_ref()?.lock().ok()?.take()
}

/// Between the view and the reader of the running `git log`.
#[derive(Default)]
struct Pages {
    /// The view wants another page.
    want: bool,
    got: Vec<Commit>,
    /// git has nothing more (or could not run).
    end: bool,
    /// The view is gone: stop, and kill git.
    closed: bool,
    /// git's pid while the reader has not yet reaped it.
    pid: u32,
}

struct Feed(Arc<(Mutex<Pages>, Condvar)>);

fn lock(m: &Mutex<Pages>) -> MutexGuard<'_, Pages> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Feed {
    fn lock(&self) -> MutexGuard<'_, Pages> {
        lock(&self.0.0)
    }
    fn ask(&self) {
        self.lock().want = true;
        self.0.1.notify_one();
    }
}

impl Drop for Feed {
    /// A long walk (a huge repository without a commit-graph) stops with its history.
    fn drop(&mut self) {
        let mut p = self.lock();
        p.closed = true;
        if p.pid != 0 {
            unsafe { libc::kill(p.pid as i32, libc::SIGKILL) };
        }
        drop(p);
        self.0.1.notify_one();
    }
}

fn feed(root: PathBuf, ctx: &egui::Context) -> Feed {
    let f = Feed(Arc::default());
    let (sh, ctx) = (Arc::clone(&f.0), ctx.clone());
    std::thread::spawn(move || {
        let (m, cv) = &*sh;
        let Ok(mut child) = git(&root).args(log_args()).stdout(Stdio::piped()).spawn() else {
            lock(m).end = true;
            ctx.request_repaint();
            return;
        };
        lock(m).pid = child.id();
        if let Some(out) = child.stdout.take() {
            let mut r = BufReader::new(out);
            loop {
                let mut p = lock(m);
                while !p.want && !p.closed {
                    p = cv.wait(p).unwrap_or_else(|e| e.into_inner());
                }
                if p.closed {
                    break;
                }
                p.want = false;
                drop(p);
                let page = read_page(&mut r, PAGE);
                let mut p = lock(m);
                p.end = page.len() < PAGE;
                p.got.extend(page);
                ctx.request_repaint();
                if p.end || p.closed {
                    break;
                }
            }
        }
        // Reaped only after the pid is cleared, so the view never signals a reused one.
        let mut p = lock(m);
        p.pid = 0;
        p.end = true;
        drop(p);
        let _ = child.kill();
        let _ = child.wait();
    });
    f
}

struct Open {
    at: usize,
    sel: usize,
    info: Option<(String, Vec<Change>)>,
    job: Option<Slot<(String, Vec<Change>)>>,
    diff: Option<Diff>,
}

struct Diff {
    file: usize,
    text: Option<(Vec<String>, Option<Spans>, bool)>,
    job: Option<Slot<(Vec<String>, Option<Spans>, bool)>>,
}

pub struct History {
    pub root: PathBuf,
    branch: String,
    /// The inspector was opened for the history: closing one closes both.
    pub opened: bool,
    commits: Vec<Commit>,
    rows: Vec<Row>,
    lanes: Lanes,
    feed: Option<Feed>,
    asked: bool,
    done: bool,
    sel: usize,
    /// The keyboard has been used: the selected row is marked from then on.
    keyed: bool,
    reveal: bool,
    open: Option<Open>,
}

#[cfg(test)]
impl History {
    /// (commits loaded, all read, commit open, its files, diff lines shown).
    pub fn state(&self) -> (usize, bool, Option<usize>, Option<usize>, Option<usize>) {
        let o = self.open.as_ref();
        (self.commits.len(), self.done, o.map(|o| o.at), o.and_then(|o| o.info.as_ref()).map(|i| i.1.len()), o.and_then(|o| o.diff.as_ref()?.text.as_ref()).map(|t| t.0.len()))
    }
    /// The running git (0 when none).
    pub fn git_pid(&self) -> u32 {
        self.feed.as_ref().map_or(0, |f| f.lock().pid)
    }
    pub fn more(&mut self, ctx: &egui::Context) {
        self.poll(ctx);
        if let Some(f) = &self.feed {
            f.ask();
        }
    }
}

impl History {
    pub fn new(root: PathBuf, branch: String, opened: bool) -> History {
        History { root, branch, opened, commits: vec![], rows: vec![], lanes: Lanes::default(), feed: None, asked: false, done: false, sel: 0, keyed: false, reveal: false, open: None }
    }

    /// Back to the graph (the palette command run while the history is open).
    pub fn top(&mut self) {
        self.open = None;
        self.reveal = true;
    }

    /// Take in what the workers finished, and start what is wanted.
    pub(crate) fn poll(&mut self, ctx: &egui::Context) {
        // The first page is asked for here, the next ones as the graph nears its end.
        let f = self.feed.get_or_insert_with(|| feed(self.root.clone(), ctx));
        if self.commits.is_empty() && !self.asked && !self.done {
            f.ask();
            self.asked = true;
        }
        let mut p = f.lock();
        let page = std::mem::take(&mut p.got);
        self.done = p.end;
        drop(p);
        if !page.is_empty() || self.done {
            self.asked = false;
        }
        for c in page {
            self.rows.push(self.lanes.place(&c));
            self.commits.push(c);
        }
        if let Some(o) = &mut self.open {
            let (c, root) = (&self.commits[o.at], self.root.clone());
            if o.info.is_none() && o.job.is_none() {
                let (root, hash, parents) = (root.clone(), c.hash.clone(), c.parents.clone());
                o.job = Some(spawn(ctx, move || changes(&root, &hash, &parents)));
            }
            if let Some(r) = take(&o.job) {
                o.info = Some(r);
                o.job = None;
            }
            if let (Some(d), Some((_, files))) = (&mut o.diff, &o.info) {
                if d.text.is_none() && d.job.is_none() {
                    let (hash, parents, ch) = (c.hash.clone(), c.parents.clone(), files[d.file].clone());
                    d.job = Some(spawn(ctx, move || diff(&root, &hash, &parents, &ch)));
                }
                if let Some(r) = take(&d.job) {
                    d.text = Some(r);
                    d.job = None;
                }
            }
        }
    }

    fn open_commit(&mut self, i: usize) {
        if i < self.commits.len() {
            self.sel = i;
            self.open = Some(Open { at: i, sel: 0, info: None, job: None, diff: None });
        }
    }

    fn back(&mut self) {
        match &mut self.open {
            Some(o) if o.diff.is_some() => o.diff = None,
            _ => self.top(),
        }
    }

    /// A key while the history has the keyboard; false when it should close.
    pub fn key(&mut self, k: Key) -> bool {
        let step = |sel: &mut usize, n: usize| {
            let d: isize = match k {
                Key::ArrowDown => 1,
                Key::ArrowUp => -1,
                Key::PageDown => 10,
                Key::PageUp => -10,
                Key::Home => -(*sel as isize),
                Key::End => n as isize,
                _ => return false,
            };
            *sel = sel.saturating_add_signed(d).min(n.saturating_sub(1));
            true
        };
        self.keyed = true;
        match &mut self.open {
            _ if k == Key::Escape => {
                if self.open.is_none() {
                    return false;
                }
                self.back();
            }
            None if k == Key::Enter => self.open_commit(self.sel),
            None => self.reveal |= step(&mut self.sel, self.commits.len()),
            Some(o) if o.diff.is_none() => {
                let n = o.info.as_ref().map_or(0, |i| i.1.len());
                if k == Key::Enter && o.sel < n {
                    o.diff = Some(Diff { file: o.sel, text: None, job: None });
                } else {
                    step(&mut o.sel, n);
                }
            }
            Some(_) => {}
        }
        true
    }
}

/// Lane colours: the theme's accent for the first lane, then quiet hues toned toward the
/// theme's own text colour so they sit on dark and light backgrounds alike. Hues close
/// to the accent are skipped, so the first two lanes never look alike.
pub fn lane_color(pal: &Palette, lane: usize) -> Color32 {
    const HUES: [u32; 8] = [0x7aa2f7, 0xd9a05b, 0xb58ee0, 0x62b58f, 0xe0849a, 0x58b4c9, 0xc9b36a, 0x9aa5b8];
    if lane == 0 {
        return pal.accent;
    }
    let hue = |c: Color32| egui::ecolor::Hsva::from(c).h;
    let a = hue(pal.accent);
    let hues: Vec<Color32> = HUES.iter().map(|&h| Color32::from_rgb((h >> 16) as u8, (h >> 8) as u8, h as u8)).filter(|&c| ((hue(c) - a + 1.5).rem_euclid(1.0) - 0.5).abs() > 0.07).collect();
    let c = hues[(lane - 1) % hues.len()];
    let (to, t) = if pal.dark { (pal.text_dim, 0.18) } else { (pal.text, 0.25) };
    c.lerp_to_gamma(to, t)
}

fn status_color(s: u8, pal: &Palette) -> Color32 {
    match s {
        b'A' => crate::gitstatus::GREEN,
        b'D' => pal.danger,
        b'R' | b'C' => crate::gitstatus::BLUE,
        _ => crate::gitstatus::AMBER,
    }
}

/// A small rounded label, at most `max` wide; returns its width.
#[allow(clippy::too_many_arguments)]
fn pill(ui: &Ui, p: &egui::Painter, at: Pos2, text: &str, font: &FontId, kind: RefKind, col: Color32, pal: &Palette, max: f32) -> f32 {
    let g = crate::listing::elided(ui, text, font.clone(), max - 12.0, if kind == RefKind::Remote { pal.text_dim } else { pal.text });
    let r = Rect::from_min_size(pos2(at.x, at.y - g.size().y / 2.0 - 1.0), g.size() + vec2(12.0, 2.0));
    let rad = CornerRadius::same((r.height() / 2.0) as u8);
    match kind {
        RefKind::Tag => {
            p.rect_stroke(r, rad, Stroke::new(1.0, col.gamma_multiply(0.55)), egui::StrokeKind::Inside);
        }
        RefKind::Head => {
            p.rect_filled(r, rad, col.gamma_multiply(0.42));
        }
        RefKind::Branch => {
            p.rect_filled(r, rad, col.gamma_multiply(0.22));
        }
        RefKind::Remote => {
            p.rect_filled(r, rad, pal.text_dim.gamma_multiply(0.14));
        }
    }
    let c = g.size();
    p.galley(pos2(r.left() + 6.0, r.center().y - c.y / 2.0), g, pal.text);
    r.width()
}

/// The history in place of the inspector's preview. False when it should close.
pub fn view(ui: &mut Ui, rect: Rect, h: &mut History, env: &mut Env) -> bool {
    let pal = *env.pal;
    h.poll(ui.ctx());
    ui.painter().rect_filled(rect, CornerRadius::ZERO, pal.bg);
    let hdr = Rect::from_min_max(rect.min, pos2(rect.right(), rect.top() + 30.0));
    ui.painter().line_segment([hdr.left_bottom(), hdr.right_bottom()], Stroke::new(1.0, pal.border));
    let mut hui = ui.new_child(UiBuilder::new().max_rect(hdr.shrink2(vec2(6.0, 3.0))).layout(Layout::left_to_right(Align::Center)));
    hui.set_clip_rect(hdr.intersect(ui.clip_rect()));
    hui.spacing_mut().item_spacing.x = 2.0;
    let deep = h.open.is_some();
    if crate::ui::glyph_button(&mut hui, Glyph::Close, if deep { "Close history" } else { "Close history (Esc)" }, true, &pal).clicked() {
        return false;
    }
    if deep && crate::ui::glyph_button(&mut hui, Glyph::Back, "Back (Esc)", true, &pal).clicked() {
        h.back();
    }
    // What is shown, as the same tinted pill the preview puts its file name in.
    let (icon, label) = match &h.open {
        None => ("git-branch", h.branch.clone()),
        Some(o) => match (&o.diff, &o.info) {
            (Some(d), Some(i)) => ("file", i.1[d.file].path.rsplit('/').next().unwrap_or("").to_string()),
            _ => ("", h.commits[o.at].hash[..7].to_string()),
        },
    };
    hui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        let iw = if icon.is_empty() { 0.0 } else { 16.0 };
        let room = ui.available_width() - 6.0 - iw - 20.0;
        if room < 24.0 || label.is_empty() {
            return;
        }
        let font = if icon.is_empty() { FontId::monospace(11.5) } else { FontId::proportional(12.0) };
        let g = crate::listing::elided(ui, &label, font, room.min(260.0), pal.text);
        let (r, _) = ui.allocate_exact_size(vec2(g.size().x + iw + 20.0, 22.0), Sense::hover());
        ui.painter().rect_filled(r, CornerRadius::same(11), pal.accent.gamma_multiply(0.22));
        crate::theme::paint_lucide(ui.painter(), Rect::from_center_size(pos2(r.left() + 16.0, r.center().y), Vec2::splat(12.0)), icon, pal.text);
        ui.painter().galley(pos2(r.left() + 10.0 + iw, r.center().y - g.size().y / 2.0), g, pal.text);
    });
    let body = Rect::from_min_max(pos2(rect.left(), hdr.bottom() + 1.0), rect.max);
    let mut bui = ui.new_child(UiBuilder::new().max_rect(body).id_salt(("git", env.panel_id)));
    bui.set_clip_rect(body.intersect(ui.clip_rect()));
    let fs = env.cfg.font_size;
    let dim = |ui: &Ui, msg: &str| {
        ui.painter().text(body.center(), Align2::CENTER_CENTER, msg, FontId::proportional(fs), pal.text_dim);
    };
    match h.open.as_ref().map(|o| o.diff.as_ref().map(|d| &d.text)) {
        None if h.commits.is_empty() => dim(&bui, if h.done { "No commits yet" } else { "Loading…" }),
        None => graph(&mut bui, body, h, env),
        Some(None) => commit(&mut bui, body, h, env),
        Some(Some(None)) => dim(&bui, "Loading…"),
        Some(Some(Some((l, _, _)))) if l.is_empty() => dim(&bui, "No text changes"),
        Some(Some(Some((lines, spans, cut)))) => crate::inspector::text_view(&mut bui, body, lines, *cut, spans.as_deref(), false, None, env.cfg.inspector_font_size, &pal, true),
    }
    true
}

fn graph(ui: &mut Ui, body: Rect, h: &mut History, env: &mut Env) {
    let pal = *env.pal;
    let fs = env.cfg.font_size;
    let (font, small, tag) = (FontId::proportional(fs), FontId::proportional(fs - 1.5), FontId::proportional((fs - 2.5).max(8.5)));
    let row_h = ((fs * 1.35 + (fs - 1.5) * 1.35) + 7.0 * env.cfg.spacing).round();
    let step = (fs + 1.0).round();
    let dot = (step * 0.27).max(3.0);
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let n = h.commits.len() + usize::from(!h.done);
    let anim = env.cfg.animations;
    let mut open = None;
    let mut inner = ui.new_child(UiBuilder::new().max_rect(body.shrink2(vec2(4.0, 4.0))));
    ScrollArea::vertical().auto_shrink([false, false]).id_salt("git_graph").show_viewport(&mut inner, |ui, vp| {
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), n as f32 * row_h), Sense::hover());
        if std::mem::take(&mut h.reveal) {
            ui.scroll_to_rect(Rect::from_min_size(pos2(r.left(), r.top() + h.sel as f32 * row_h), vec2(r.width(), row_h)), None);
        }
        let first = (vp.min.y / row_h).floor().max(0.0) as usize;
        let last = ((vp.max.y / row_h).ceil() as usize + 1).min(n);
        if !h.done && last + 100 > h.commits.len() && !h.asked {
            if let Some(f) = &h.feed {
                f.ask();
                h.asked = true;
            }
        }
        let gx = r.left() + 8.0 + step / 2.0;
        let x = |l: u16| gx + (l as usize).min(LANES) as f32 * step;
        // The text starts after the widest row on screen, easing there as rows scroll by,
        // so a busy stretch far down never pushes the quiet ones aside.
        let wide = h.rows[first.min(h.rows.len())..last.min(h.rows.len())].iter().flat_map(|r| r.pass.iter().chain(&r.outs).chain(&r.ins).chain([&r.col])).max().map_or(1, |&l| (l as usize + 1).min(LANES + 1));
        let wide = ui.ctx().animate_value_with_time(ui.id().with("gitw"), wide as f32, if anim { 0.18 } else { 0.0 });
        let tx = gx + (wide - 1.0) * step + step / 2.0 + 8.0;
        let fold = pal.text_dim.gamma_multiply(0.4);
        let colors: Vec<Color32> = (0..=LANES).map(|l| if l < LANES { lane_color(&pal, l) } else { fold }).collect();
        let col = |l: u16| colors[(l as usize).min(LANES)];
        let line = |l: u16| Stroke::new(1.6, col(l));
        let p = ui.painter().clone();
        let curve = |a: Pos2, b: Pos2, s: Stroke| {
            let k = (b.y - a.y) * 0.6;
            p.add(CubicBezierShape::from_points_stroke([a, pos2(a.x, a.y + k), pos2(b.x, b.y - k), b], false, Color32::TRANSPARENT, s));
        };
        for i in first..last {
            let rr = Rect::from_min_size(pos2(r.left(), r.top() + i as f32 * row_h), vec2(r.width(), row_h));
            if i >= h.commits.len() {
                crate::ui::spinner(&p, pos2(tx + 6.0, rr.center().y), 5.0, pal.accent, ui.input(|i| i.time));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(16));
                continue;
            }
            let resp = ui.interact(rr, ui.id().with(("gitrow", i)), Sense::click());
            let under = if i == h.sel && h.keyed { pal.row_hover } else if resp.hovered() { pal.hover } else { pal.bg };
            if under != pal.bg {
                p.rect_filled(rr.shrink2(vec2(2.0, 1.0)), CornerRadius::same(5), under);
            }
            if resp.clicked() {
                open = Some(i);
            }
            let (c, row) = (&h.commits[i], &h.rows[i]);
            let (top, cy, bot) = (rr.top(), rr.center().y, rr.bottom());
            let mut folded = false;
            for &l in &row.pass {
                if (l as usize) < LANES || !std::mem::replace(&mut folded, true) {
                    p.line_segment([pos2(x(l), top), pos2(x(l), bot)], line(l));
                }
            }
            let cx = x(row.col);
            if row.up {
                p.line_segment([pos2(cx, top), pos2(cx, cy)], line(row.col));
            }
            for &l in &row.ins {
                curve(pos2(x(l), top), pos2(cx, cy), line(l));
            }
            for &l in &row.outs {
                if l == row.col {
                    p.line_segment([pos2(cx, cy), pos2(cx, bot)], line(l));
                } else {
                    curve(pos2(cx, cy), pos2(x(l), bot), line(l));
                }
            }
            // The dot sits in a ring of the row's own background, so lines stop short of it.
            let dc = col(row.col);
            let head = c.refs.iter().any(|r| r.0 == RefKind::Head);
            p.circle_filled(pos2(cx, cy), dot + 1.8, under);
            if c.parents.len() > 1 {
                p.circle_stroke(pos2(cx, cy), dot - 0.6, Stroke::new(1.6, dc));
            } else {
                p.circle_filled(pos2(cx, cy), dot, dc);
            }
            if head {
                p.circle_stroke(pos2(cx, cy), dot + 3.2, Stroke::new(1.2, dc.gamma_multiply(0.7)));
            }
            // Subject line: ref pills first (as many as fit in half the row), then the subject.
            let w = rr.right() - 8.0 - tx;
            if w < 16.0 {
                continue;
            }
            let g = crate::listing::elided(ui, &c.subject, font.clone(), w, pal.text);
            let m = small.size * 1.35;
            let y0 = cy - (g.size().y + m) / 2.0;
            let mut px = tx;
            for (k, (kind, name)) in c.refs.iter().enumerate() {
                let room = tx + w * 0.5 - px;
                if k > 0 && name.len() as f32 * tag.size * 0.6 + 16.0 > room || room < 40.0 {
                    let more = format!("+{}", c.refs.len() - k);
                    let mg = p.layout_no_wrap(more, tag.clone(), pal.text_dim);
                    p.galley(pos2(px, y0 + g.size().y / 2.0 - mg.size().y / 2.0), mg.clone(), pal.text_dim);
                    px += mg.size().x + 6.0;
                    break;
                }
                px += pill(ui, &p, pos2(px, y0 + g.size().y / 2.0), name, &tag, *kind, dc, &pal, room) + 4.0;
            }
            let g = if px > tx { crate::listing::elided(ui, &c.subject, font.clone(), tx + w - px - 2.0, pal.text) } else { g };
            p.galley(pos2(px + if px > tx { 2.0 } else { 0.0 }, y0), g.clone(), pal.text);
            // Meta line: short hash, author, when.
            let mut job = LayoutJob::default();
            job.append(&c.hash[..7], 0.0, TextFormat { font_id: FontId::monospace(small.size - 0.5), color: pal.text_dim.gamma_multiply(0.8), valign: Align::Center, ..Default::default() });
            job.append(&format!("  {}  ·  {}", c.author, ago(c.time, now)), 0.0, TextFormat { font_id: small.clone(), color: pal.text_dim, valign: Align::Center, ..Default::default() });
            job.wrap = TextWrapping { max_width: w, max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
            let mg = ui.fonts_mut(|f| f.layout_job(job));
            p.galley(pos2(tx, y0 + g.size().y + 1.0), mg, pal.text_dim);
        }
    });
    if let Some(i) = open {
        h.open_commit(i);
    }
}

fn commit(ui: &mut Ui, body: Rect, h: &mut History, env: &mut Env) {
    let pal = *env.pal;
    let fs = env.cfg.font_size;
    let keyed = h.keyed;
    let Some(o) = h.open.as_mut() else { return };
    let c = &h.commits[o.at];
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64);
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(c.time.max(0) as u64);
    let mut inner = ui.new_child(UiBuilder::new().max_rect(body.shrink2(vec2(6.0, 4.0))));
    let mut pick = None;
    ScrollArea::vertical().auto_shrink([false, false]).id_salt(("git_commit", o.at)).show(&mut inner, |ui| {
        ui.add_space(6.0);
        egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 0)).show(ui, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.add(egui::Label::new(egui::RichText::new(&c.subject).size(fs + 1.0).color(pal.text)).wrap());
            if !c.refs.is_empty() {
                let tag = FontId::proportional((fs - 2.5).max(8.5));
                let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), tag.size + 8.0), Sense::hover());
                let mut px = r.left();
                for (kind, name) in &c.refs {
                    if r.right() - px < 40.0 {
                        break;
                    }
                    px += pill(ui, ui.painter(), pos2(px, r.center().y), name, &tag, *kind, lane_color(&pal, h.rows[o.at].col as usize), &pal, r.right() - px) + 4.0;
                }
            }
            let meta = format!("{}  ·  {}  ·  {}", c.author, ago(c.time, now), crate::fsx::fmt_time(Some(when)));
            ui.add(egui::Label::new(egui::RichText::new(meta).size(fs - 1.5).color(pal.text_dim)).wrap());
            if let Some((msg, _)) = o.info.as_ref().filter(|i| !i.0.is_empty()) {
                ui.add_space(2.0);
                ui.add(egui::Label::new(egui::RichText::new(msg).size(fs - 0.5).color(pal.text_dim)).wrap());
            }
        });
        ui.add_space(8.0);
        let (r, _) = ui.allocate_exact_size(vec2(ui.available_width(), 1.0), Sense::hover());
        ui.painter().line_segment([r.left_center(), r.right_center()], Stroke::new(1.0, pal.border));
        ui.add_space(4.0);
        let Some((_, files)) = &o.info else {
            ui.label(egui::RichText::new("Loading…").size(fs - 0.5).color(pal.text_dim));
            return;
        };
        let row_h = (fs + 8.0).round();
        let (font, small) = (FontId::proportional(fs - 0.5), FontId::proportional(fs - 2.0));
        for (i, f) in files.iter().enumerate() {
            let (r, resp) = ui.allocate_exact_size(vec2(ui.available_width(), row_h), Sense::click());
            let on = i == o.sel && keyed;
            if on || resp.hovered() {
                ui.painter().rect_filled(r, CornerRadius::same(4), if on { pal.row_hover } else { pal.hover });
            }
            if resp.clicked() {
                pick = Some(i);
            }
            ui.painter().circle_filled(pos2(r.left() + 10.0, r.center().y), 3.5, status_color(f.status, &pal));
            let (dir, name) = f.path.rsplit_once('/').unwrap_or(("", &f.path));
            let g = crate::listing::elided(ui, name, font.clone(), r.width() - 30.0, pal.text);
            let nx = r.left() + 20.0 + g.size().x;
            ui.painter().galley(pos2(r.left() + 20.0, r.center().y - g.size().y / 2.0), g, pal.text);
            let hint = match &f.from {
                Some(from) => format!("from {from}"),
                None => dir.to_string(),
            };
            if r.right() - nx > 40.0 && !hint.is_empty() {
                let g = crate::listing::elided(ui, &hint, small.clone(), r.right() - nx - 14.0, pal.text_dim);
                ui.painter().galley(pos2(nx + 8.0, r.center().y - g.size().y / 2.0), g, pal.text_dim);
            }
        }
    });
    if let Some(i) = pick {
        o.sel = i;
        o.diff = Some(Diff { file: i, text: None, job: None });
    }
}
