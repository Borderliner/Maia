//! Authors' GitHub pictures in the history, only while "Show GitHub pictures" is on.
//! Nothing here runs, reads or asks anything while it is off.
//!
//! A picture is found the cheapest and most private way there is: a noreply address
//! names the account, so its picture is asked for directly; any other address is looked
//! up once, through one of its commits, in GitHub's public API, and only when the work
//! tree's origin is on github.com. Answers are kept in the user's cache folder for a
//! week, pictures and "no picture" alike, so an author is asked about once a week at
//! most. Requests go through curl, never with the user's own settings or credentials.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use crate::imgdec::Rgba;

/// Pictures are asked for at this size: twice the largest drawn at the usual scales.
pub const SIZE: u32 = 96;
/// How long an answer, a picture or none, is kept before asking again.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 86_400);
/// Requests at once.
const JOBS: usize = 3;
/// A larger answer is cut off and dropped.
const BODY_CAP: u64 = 1 << 20;
/// Unauthenticated, GitHub's API allows about 60 requests an hour; once it says no,
/// it is left alone for this long (noreply pictures still come).
const BACKOFF: Duration = Duration::from_secs(20 * 60);

// ------------------------------------------------------------------ which picture

fn username(s: &str) -> bool {
    (1..=39).contains(&s.len()) && !s.starts_with('-') && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The picture a GitHub noreply address names: `ID+name@users.noreply.github.com` by
/// the account's number, `name@users.noreply.github.com` by its name.
pub fn noreply_url(email: &str) -> Option<String> {
    let (local, domain) = email.trim().rsplit_once('@')?;
    if !domain.eq_ignore_ascii_case("users.noreply.github.com") {
        return None;
    }
    match local.split_once('+') {
        Some((id, _)) => (!id.is_empty() && id.len() <= 12 && id.bytes().all(|b| b.is_ascii_digit())).then(|| format!("https://avatars.githubusercontent.com/u/{id}?s={SIZE}")),
        None => username(local).then(|| format!("https://avatars.githubusercontent.com/{local}?s={SIZE}")),
    }
}

/// OWNER and REPO of a remote on github.com, in any of git's forms (https, ssh, the
/// scp-like `git@github.com:o/r.git`). Any other host, a GitHub Enterprise one
/// included, is None: its API is not github.com's.
pub fn github_repo(url: &str) -> Option<(String, String)> {
    let u = url.trim();
    let (auth, path) = match u.split_once("://") {
        Some((scheme, rest)) => {
            if !matches!(scheme.to_ascii_lowercase().as_str(), "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git") {
                return None;
            }
            rest.split_once('/')?
        }
        None => {
            let (auth, path) = u.split_once(':')?;
            if auth.contains('/') || auth.is_empty() {
                return None;
            }
            (auth, path)
        }
    };
    // Whatever is before an @ (a user, or a token someone left in the address) is ignored.
    let host = auth.rsplit_once('@').map_or(auth, |a| a.1);
    let host = host.rsplit_once(':').filter(|(_, port)| port.bytes().all(|b| b.is_ascii_digit())).map_or(host, |h| h.0);
    if !["github.com", "www.github.com", "ssh.github.com"].iter().any(|h| host.eq_ignore_ascii_case(h)) {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    let ok = |s: &str| !s.is_empty() && s != "." && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    (ok(owner) && ok(repo)).then(|| (owner.to_string(), repo.to_string()))
}

/// GitHub's answer about one commit. The list form starting at the commit is asked for:
/// it carries the same author without the changed files, which on a large commit run
/// past the size cap.
pub fn api_url(owner: &str, repo: &str, sha: &str) -> Option<String> {
    (sha.len() >= 7 && sha.bytes().all(|b| b.is_ascii_hexdigit())).then(|| format!("https://api.github.com/repos/{owner}/{repo}/commits?sha={sha}&per_page=1"))
}

/// An avatar address from GitHub at the size drawn. Only GitHub's own picture host is
/// followed, whatever the answer says.
pub fn sized_avatar(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://avatars.githubusercontent.com/")?;
    if rest.is_empty() || rest.contains(|c: char| c.is_whitespace() || c.is_control() || matches!(c, '#' | '\\' | '@')) {
        return None;
    }
    Some(format!("{url}{}s={SIZE}", if url.contains('?') { '&' } else { '?' }))
}

/// The author's picture in GitHub's answer about a commit (the commit itself, or a list
/// starting with it). `Some(None)`: the author has no GitHub account. None: not an
/// answer about a commit.
pub fn avatar_from_api(body: &[u8]) -> Option<Option<String>> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let c = match v.as_array() {
        Some(a) => a.first()?,
        None => &v,
    };
    c.get("sha")?;
    match c.get("author") {
        None | Some(serde_json::Value::Null) => Some(None),
        Some(a) => Some(a.get("avatar_url").and_then(|u| u.as_str()).and_then(sized_avatar)),
    }
}

/// Where an author's picture comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Source {
    Picture(String),
    Api(String),
}

/// An author's source: their noreply address, else their commit on the github.com
/// repository the work tree came from, else none.
pub fn source(email: &str, sha: &str, repo: Option<&(String, String)>) -> Option<Source> {
    if let Some(u) = noreply_url(email) {
        return Some(Source::Picture(u));
    }
    let (o, r) = repo?;
    email.contains('@').then(|| api_url(o, r, sha).map(Source::Api)).flatten()
}

// ------------------------------------------------------------------ cache

pub fn cache_dir() -> PathBuf {
    // Tests run outside the sandbox too: never the real cache.
    let base = if cfg!(test) { None } else { dirs::cache_dir() };
    base.unwrap_or_else(std::env::temp_dir).join("maia/avatars")
}

/// The file name an author's answer is kept under: no address is written down.
pub fn key(email: &str) -> String {
    crate::thumbcache::md5_hex(email.trim().to_lowercase().as_bytes())
}

/// An answer written at `written` still holds. One from the future (the clock was set
/// back) holds no longer than it would have from the past.
pub fn fresh(written: SystemTime, now: SystemTime) -> bool {
    match now.duration_since(written) {
        Ok(age) => age < MAX_AGE,
        Err(e) => e.duration() < MAX_AGE,
    }
}

#[derive(Debug, PartialEq)]
pub enum Cached {
    Nothing,
    /// GitHub said there is no picture, this week.
    Miss,
    Hit(Vec<u8>),
    /// A picture older than a week: shown, and asked for again.
    Stale(Vec<u8>),
}

pub fn read_cache(dir: &Path, key: &str, now: SystemTime) -> Cached {
    let modified = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    let (img, miss) = (dir.join(format!("{key}.img")), dir.join(format!("{key}.none")));
    if modified(&miss).is_some_and(|t| fresh(t, now)) {
        return Cached::Miss;
    }
    match (modified(&img), std::fs::read(&img)) {
        (Some(t), Ok(b)) if fresh(t, now) => Cached::Hit(b),
        (Some(_), Ok(b)) => Cached::Stale(b),
        _ => Cached::Nothing,
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) {
    let Some(dir) = path.parent() else { return };
    let _ = std::fs::create_dir_all(dir);
    let tmp = path.with_extension(format!("part{}", std::process::id()));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

pub fn write_hit(dir: &Path, key: &str, bytes: &[u8]) {
    write_atomic(&dir.join(format!("{key}.img")), bytes);
    let _ = std::fs::remove_file(dir.join(format!("{key}.none")));
}

pub fn write_miss(dir: &Path, key: &str) {
    write_atomic(&dir.join(format!("{key}.none")), b"");
    let _ = std::fs::remove_file(dir.join(format!("{key}.img")));
}

// ------------------------------------------------------------------ fetching

#[derive(Clone, Debug, PartialEq)]
pub enum Http {
    Ok(Vec<u8>),
    Status(u16),
    Failed,
}

/// curl's output: the body, then a line with the last response's status
/// (`--write-out '\n%{http_code}'`).
pub fn parse_curl(out: &[u8]) -> Http {
    let Some(nl) = out.iter().rposition(|&b| b == b'\n') else { return Http::Failed };
    match std::str::from_utf8(&out[nl + 1..]).ok().and_then(|s| s.trim().parse::<u16>().ok()) {
        Some(200) => Http::Ok(out[..nl].to_vec()),
        Some(0) | None => Http::Failed,
        Some(c) => Http::Status(c),
    }
}

/// A GET: the address, and whether to give up now (the setting was switched off).
pub type Get = Arc<dyn Fn(&str, &dyn Fn() -> bool) -> Http + Send + Sync>;

/// The command for one GET. `-q` comes first so no ~/.curlrc is read: nothing of the
/// user's (tokens, netrc, extra headers) goes along, and only https is spoken, redirects
/// included.
pub fn curl_args(url: &str) -> Vec<String> {
    let mut a: Vec<String> = ["-q", "--silent", "--location", "--max-redirs", "5", "--proto", "=https", "--proto-redir", "=https", "--connect-timeout", "5", "--max-time", "10", "--user-agent", "Maia"].map(String::from).to_vec();
    a.extend(["--max-filesize".into(), BODY_CAP.to_string(), "--write-out".into(), "\n%{http_code}".into()]);
    if url.starts_with("https://api.github.com/") {
        a.extend(["--header".into(), "Accept: application/vnd.github+json".into()]);
    }
    a.push(url.to_string());
    a
}

#[cfg(not(test))]
fn curl_get(url: &str, stop: &dyn Fn() -> bool) -> Http {
    if !url.starts_with("https://") || !crate::media::have("curl") {
        return Http::Failed;
    }
    let mut c = crate::media::tool("curl");
    c.args(curl_args(url));
    match crate::media::output_capped(c, Duration::from_secs(12), BODY_CAP + 16, stop) {
        Some(out) => parse_curl(&out),
        None => Http::Failed,
    }
}

/// Test builds never reach the network: a test hands in its own GET.
#[cfg(test)]
fn curl_get(_: &str, _: &dyn Fn() -> bool) -> Http {
    Http::Failed
}

pub enum Found {
    Picture(Rgba),
    /// There is none (GitHub said so, now or this week).
    None,
    /// Not now: offline, a timeout, an error. Asked again next session.
    Failed,
    /// GitHub's API said too many requests.
    Limited,
}

/// One author's picture: from the cache while it holds, else from GitHub, whose answer
/// is kept (a picture only once it decodes).
pub fn find(dir: &Path, key: &str, src: &Source, get: &dyn Fn(&str) -> Http, now: SystemTime) -> Found {
    let stale = match read_cache(dir, key, now) {
        Cached::Hit(b) => return crate::imgdec::decode_bytes(&b).map_or(Found::Failed, Found::Picture),
        Cached::Miss => return Found::None,
        Cached::Stale(b) => crate::imgdec::decode_bytes(&b),
        Cached::Nothing => None,
    };
    // A week-old picture is better than none when GitHub can't be asked.
    let or_stale = |f: Found, stale: Option<Rgba>| stale.map_or(f, Found::Picture);
    let url = match src {
        Source::Picture(u) => u.clone(),
        Source::Api(api) => match get(api) {
            Http::Ok(body) => match avatar_from_api(&body) {
                Some(Some(u)) => u,
                Some(None) => {
                    write_miss(dir, key);
                    return Found::None;
                }
                None => return or_stale(Found::Failed, stale),
            },
            Http::Status(403 | 429) => return or_stale(Found::Limited, stale),
            // The commit isn't on GitHub (never pushed, or the repository is private).
            Http::Status(404 | 409 | 422) => {
                write_miss(dir, key);
                return Found::None;
            }
            _ => return or_stale(Found::Failed, stale),
        },
    };
    match get(&url) {
        Http::Ok(b) => match crate::imgdec::decode_bytes(&b) {
            Some(img) => {
                write_hit(dir, key, &b);
                Found::Picture(img)
            }
            None => or_stale(Found::Failed, stale),
        },
        Http::Status(404) => {
            write_miss(dir, key);
            Found::None
        }
        _ => or_stale(Found::Failed, stale),
    }
}

/// A picture as drawn: the middle square, `px` across, outside the circle transparent
/// with a one-pixel soft edge.
pub fn round(img: &Rgba, px: u32) -> egui::ColorImage {
    let s = img.w.min(img.h).max(1);
    let (x0, y0) = ((img.w - s) / 2, (img.h - s) / 2);
    let mut sq = Rgba { w: s, h: s, px: Vec::with_capacity((s * s * 4) as usize) };
    for y in y0..y0 + s {
        let at = ((y * img.w + x0) * 4) as usize;
        sq.px.extend_from_slice(&img.px[at..at + (s * 4) as usize]);
    }
    let mut d = if s > px { crate::imgdec::downscale(&sq, px.max(1)) } else { sq };
    let r = d.w as f32 / 2.0;
    for y in 0..d.h {
        for x in 0..d.w {
            let dist = ((x as f32 + 0.5 - r).powi(2) + (y as f32 + 0.5 - r).powi(2)).sqrt();
            // Half a pixel inside the circle, so the edge pixels never poke past the ring.
            let k = (r - dist).clamp(0.0, 1.0);
            let a = &mut d.px[((y * d.w + x) * 4 + 3) as usize];
            *a = (*a as f32 * k).round() as u8;
        }
    }
    egui::ColorImage::from_rgba_unmultiplied([d.w as usize, d.h as usize], &d.px)
}

// ------------------------------------------------------------------ the store

/// An author's picture at the two sizes drawn: the commit's header and the graph rows.
pub struct Pic {
    pub big: egui::TextureHandle,
    pub small: egui::TextureHandle,
}

enum Entry {
    Waiting,
    Ready(Pic),
    /// Initials: no picture, or none to be had now.
    None,
}

struct Job {
    email: String,
    src: Source,
    px: [u32; 2],
}

enum Answer {
    Pictures(egui::ColorImage, egui::ColorImage),
    None,
    Failed,
    Limited,
}

type Slot<T> = Arc<Mutex<Option<T>>>;

enum Remote {
    Asking(Slot<Option<(String, String)>>),
    Known(Option<(String, String)>),
}

pub struct Avatars {
    on: bool,
    pub dir: PathBuf,
    get: Get,
    entries: HashMap<String, Entry>,
    queue: VecDeque<Job>,
    running: usize,
    /// Finished jobs, with the generation they were started in.
    done: Arc<Mutex<Vec<(u64, String, Answer)>>>,
    /// Bumped when switched off: older jobs give up, and their answers are dropped.
    epoch: Arc<AtomicU64>,
    /// The API is not asked again before this.
    until: Option<Instant>,
    remotes: HashMap<PathBuf, Remote>,
    curl: Arc<OnceLock<bool>>,
    /// GETs started (a test checks none happen while off).
    asked: Arc<AtomicU64>,
}

impl Default for Avatars {
    fn default() -> Self {
        Avatars::new()
    }
}

impl Avatars {
    pub fn new() -> Avatars {
        let curl = Arc::new(OnceLock::new());
        #[cfg(test)]
        let _ = curl.set(true);
        Avatars { on: false, dir: cache_dir(), get: Arc::new(curl_get), entries: HashMap::new(), queue: VecDeque::new(), running: 0, done: Arc::default(), epoch: Arc::default(), until: None, remotes: HashMap::new(), curl, asked: Arc::default() }
    }

    /// Whether curl is installed; None until known. Looked for off the UI thread, the
    /// first time anyone asks.
    pub fn curl(&self) -> Option<bool> {
        if let Some(v) = self.curl.get() {
            return Some(*v);
        }
        static ASKED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !ASKED.swap(true, Ordering::Relaxed) {
            let c = Arc::clone(&self.curl);
            std::thread::spawn(move || {
                let _ = c.set(crate::media::have("curl"));
            });
        }
        None
    }

    /// Follow the setting. Switched off, everything stops at once: queued authors are
    /// dropped, requests in flight are killed, pictures go and initials come back. The
    /// cache on disk stays, so switching on again asks GitHub nothing new for a week.
    pub fn set_on(&mut self, on: bool) {
        if on == self.on {
            return;
        }
        self.on = on;
        if !on {
            self.epoch.fetch_add(1, Ordering::SeqCst);
            self.entries.clear();
            self.queue.clear();
            self.remotes.clear();
            self.running = 0;
            self.until = None;
        }
    }

    /// Take in finished jobs and start waiting ones.
    pub fn pump(&mut self, ctx: &egui::Context) {
        if !self.on {
            return;
        }
        let epoch = self.epoch.load(Ordering::SeqCst);
        let done = std::mem::take(&mut *self.done.lock().unwrap_or_else(|e| e.into_inner()));
        for (g, email, a) in done {
            if g != epoch {
                continue;
            }
            self.running = self.running.saturating_sub(1);
            let e = match a {
                Answer::Pictures(big, small) => {
                    let opts = egui::TextureOptions::LINEAR;
                    Entry::Ready(Pic { big: ctx.load_texture("avatar", big, opts), small: ctx.load_texture("avatar-small", small, opts) })
                }
                Answer::None | Answer::Failed => Entry::None,
                Answer::Limited => {
                    self.until = Some(Instant::now() + BACKOFF);
                    // Forgotten, so it is asked for again once it is on screen after the wait.
                    self.entries.remove(&email);
                    continue;
                }
            };
            self.entries.insert(email, e);
        }
        let ripe: Vec<PathBuf> = self.remotes.iter().filter_map(|(k, r)| matches!(r, Remote::Asking(s) if s.lock().map_or(true, |s| s.is_some())).then(|| k.clone())).collect();
        for k in ripe {
            if let Some(Remote::Asking(s)) = self.remotes.get(&k) {
                let v = s.lock().ok().and_then(|mut s| s.take()).flatten();
                self.remotes.insert(k, Remote::Known(v));
            }
        }
        self.start(ctx);
    }

    fn start(&mut self, ctx: &egui::Context) {
        let limited = self.until.is_some_and(|t| Instant::now() < t);
        while self.running < JOBS {
            let Some(i) = self.queue.iter().position(|j| !(limited && matches!(j.src, Source::Api(_)))) else { break };
            let Some(job) = self.queue.remove(i) else { break };
            self.running += 1;
            let (dir, get, done, epoch, asked, ctx) = (self.dir.clone(), Arc::clone(&self.get), Arc::clone(&self.done), Arc::clone(&self.epoch), Arc::clone(&self.asked), ctx.clone());
            let mine = epoch.load(Ordering::SeqCst);
            std::thread::spawn(move || {
                let live = || epoch.load(Ordering::SeqCst) == mine;
                let get = |url: &str| {
                    if !live() {
                        return Http::Failed;
                    }
                    asked.fetch_add(1, Ordering::SeqCst);
                    get(url, &|| !live())
                };
                let a = match find(&dir, &key(&job.email), &job.src, &get, SystemTime::now()) {
                    Found::Picture(img) if live() => Answer::Pictures(round(&img, job.px[0]), round(&img, job.px[1])),
                    Found::Picture(_) | Found::Failed => Answer::Failed,
                    Found::None => Answer::None,
                    Found::Limited => Answer::Limited,
                };
                if let Ok(mut d) = done.lock() {
                    d.push((mine, job.email, a));
                }
                ctx.request_repaint();
            });
        }
    }

    /// The github.com repository a work tree's origin points to, read once per tree.
    fn repo(&mut self, ctx: &egui::Context, root: &Path) -> Option<Option<&(String, String)>> {
        if !self.remotes.contains_key(root) {
            let slot: Slot<Option<(String, String)>> = Arc::default();
            let (theirs, root2, ctx) = (Arc::clone(&slot), root.to_path_buf(), ctx.clone());
            std::thread::spawn(move || {
                let v = crate::gitlog::origin_url(&root2).and_then(|u| github_repo(&u));
                if let Ok(mut s) = theirs.lock() {
                    *s = Some(v);
                }
                ctx.request_repaint();
            });
            self.remotes.insert(root.to_path_buf(), Remote::Asking(slot));
        }
        match self.remotes.get(root) {
            Some(Remote::Known(v)) => Some(v.as_ref()),
            _ => None,
        }
    }

    /// An author's picture once there is one; asked for the first time it is wanted.
    /// `px`: the header's and the rows' sizes in pixels.
    pub fn picture(&mut self, ctx: &egui::Context, email: &str, sha: &str, root: &Path, px: [u32; 2]) -> Option<&Pic> {
        if !self.on || email.trim().is_empty() {
            return None;
        }
        if !self.entries.contains_key(email) {
            let src = if noreply_url(email).is_some() {
                source(email, sha, None)
            } else {
                // Asked only for authors whose address doesn't already say who they are.
                match self.repo(ctx, root) {
                    None => return None,
                    Some(repo) => {
                        let repo = repo.cloned();
                        source(email, sha, repo.as_ref())
                    }
                }
            };
            let src = src?;
            self.entries.insert(email.to_string(), Entry::Waiting);
            self.queue.push_back(Job { email: email.to_string(), src, px });
            self.start(ctx);
        }
        match self.entries.get(email) {
            Some(Entry::Ready(p)) => Some(p),
            _ => None,
        }
    }
}

#[cfg(test)]
impl Avatars {
    pub fn on(&self) -> bool {
        self.on
    }
    pub fn set_get(&mut self, get: Get) {
        self.get = get;
    }
    /// As if curl were installed or not.
    pub fn set_curl(&mut self, have: bool) {
        let l = OnceLock::new();
        let _ = l.set(have);
        self.curl = Arc::new(l);
    }
    /// GETs started so far.
    pub fn asked(&self) -> u64 {
        self.asked.load(Ordering::SeqCst)
    }
    /// "waiting", "ready" or "none" for an author asked about, else "".
    pub fn state(&self, email: &str) -> &'static str {
        match self.entries.get(email) {
            Some(Entry::Waiting) => "waiting",
            Some(Entry::Ready(_)) => "ready",
            Some(Entry::None) => "none",
            None => "",
        }
    }
    pub fn busy(&self) -> bool {
        self.running > 0 || !self.queue.is_empty()
    }
}

/// A GET that answers from a table instead of the network: the first entry whose text
/// the address contains gives its answer; anything else fails. Every address asked is
/// written down.
#[cfg(test)]
pub fn fake_get(table: Vec<(String, Http)>, log: Arc<Mutex<Vec<String>>>) -> Get {
    Arc::new(move |url: &str, _stop: &dyn Fn() -> bool| {
        log.lock().unwrap().push(url.to_string());
        table.iter().find(|(k, _)| url.contains(k.as_str())).map_or(Http::Failed, |(_, h)| h.clone())
    })
}
