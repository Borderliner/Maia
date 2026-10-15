//! Archives as folders, through `bsdtar` (libarchive: zip, tar and its compressions, 7z,
//! rar, iso, cpio). A path inside an archive is the archive's path joined with the path
//! inside it, so tabs, breadcrumbs and history need nothing special. Archives are
//! read-only here: what is inside is extracted to be opened or copied out.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

const EXTS: &[&str] = &[".zip", ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tbz", ".tar.xz", ".txz", ".tar.zst", ".tzst", ".tar.lz", ".tar.lzma", ".7z", ".rar", ".iso", ".cpio", ".jar", ".apk", ".cbz", ".cbr", ".xpi"];

pub fn is_archive(name: &str) -> bool {
    let n = name.to_lowercase();
    EXTS.iter().any(|e| n.ends_with(e) && n.len() > e.len())
}

/// A name without its archive extension ("photos.tar.gz" -> "photos").
pub fn stem(name: &str) -> &str {
    let l = name.to_lowercase();
    EXTS.iter().filter(|e| l.ends_with(*e) && l.len() > e.len()).map(|e| &name[..name.len() - e.len()]).min_by_key(|s| s.len()).unwrap_or(name)
}

pub fn available() -> bool {
    crate::media::have("bsdtar")
}

/// (archive, path inside it) when `p` lies inside an archive file. Only called for
/// paths that are not folders on disk.
pub fn split(p: &Path) -> Option<(PathBuf, PathBuf)> {
    let mut cur = p;
    let mut inner = Vec::new();
    loop {
        let name = cur.file_name()?;
        if is_archive(&name.to_string_lossy()) && cur.is_file() {
            let inside: PathBuf = inner.iter().rev().collect();
            return Some((cur.to_path_buf(), inside));
        }
        inner.push(name.to_os_string());
        cur = cur.parent()?;
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    /// Path inside the archive, without "./" or a trailing slash.
    pub path: PathBuf,
    pub is_dir: bool,
    pub link: bool,
    pub size: u64,
    pub modified: Option<SystemTime>,
    /// Stored as "./name" (many zips): extraction must name it that way.
    pub dot: bool,
}

/// `LC_ALL=C bsdtar -tvf` output. Folders that only exist as prefixes of other entries
/// (zip files often have none) are added.
pub fn parse_list(text: &str, now: SystemTime) -> Vec<Item> {
    let mut out: Vec<Item> = vec![];
    for line in text.lines() {
        // perms links owner group size month day time-or-year name
        let mut rest = line;
        let mut f = vec![];
        for _ in 0..8 {
            let t = rest.trim_start();
            let end = t.find(' ').unwrap_or(t.len());
            f.push(&t[..end]);
            rest = &t[end..];
        }
        let Some(name) = rest.strip_prefix(' ') else { continue };
        let (perms, size) = (f[0], f[4].parse::<u64>().unwrap_or(0));
        if perms.len() < 10 {
            continue;
        }
        let link = perms.starts_with('l');
        let name = if link { name.split(" -> ").next().unwrap_or(name) } else { name };
        let name = unescape(name);
        let dot = name.starts_with("./");
        let name = name.trim_start_matches("./").trim_end_matches('/');
        if name.is_empty() || name == "." || name.split('/').any(|c| c == "..") {
            continue;
        }
        out.push(Item { path: PathBuf::from(name), is_dir: perms.starts_with('d'), link, size, modified: when(f[5], f[6], f[7], now), dot });
    }
    let have: std::collections::HashSet<PathBuf> = out.iter().map(|i| i.path.clone()).collect();
    let mut implied = std::collections::BTreeSet::new();
    for i in &out {
        let mut p = i.path.parent();
        while let Some(d) = p.filter(|d| !d.as_os_str().is_empty()) {
            if !have.contains(d) {
                implied.insert(d.to_path_buf());
            }
            p = d.parent();
        }
    }
    let dot = out.iter().any(|i| i.dot);
    out.extend(implied.into_iter().map(|path| Item { path, is_dir: true, link: false, size: 0, modified: None, dot }));
    out
}

/// bsdtar prints unprintable bytes as \ooo and a backslash as \\.
fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 1 < b.len() && b[i + 1] == b'\\' {
            out.push(b'\\');
            i += 2;
        } else if b[i] == b'\\' && i + 3 < b.len() + 0 && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push(((b[i + 1] - b'0') << 6) | ((b[i + 2] - b'0') << 3) | (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// "Oct 14 22:57" (this year, or last if that is in the future) or "Mar 5 2020",
/// in local time.
fn when(month: &str, day: &str, t: &str, now: SystemTime) -> Option<SystemTime> {
    let m = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"].iter().position(|x| *x == month)? as i32;
    let day: i32 = day.parse().ok()?;
    let now_secs = now.duration_since(SystemTime::UNIX_EPOCH).ok()?.as_secs() as i64;
    let local = |year: i32, hh: i32, mm: i32| -> Option<i64> {
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        tm.tm_year = year - 1900;
        tm.tm_mon = m;
        tm.tm_mday = day;
        tm.tm_hour = hh;
        tm.tm_min = mm;
        tm.tm_isdst = -1;
        let s = unsafe { libc::mktime(&mut tm) };
        (s != -1).then_some(s)
    };
    let secs = if let Some((hh, mm)) = t.split_once(':') {
        let (hh, mm) = (hh.parse().ok()?, mm.parse().ok()?);
        let mut tm: libc::tm = unsafe { std::mem::zeroed() };
        let t0 = now_secs as libc::time_t;
        unsafe { libc::localtime_r(&t0, &mut tm) };
        let this = local(tm.tm_year + 1900, hh, mm)?;
        if this > now_secs + 86_400 { local(tm.tm_year + 1899, hh, mm)? } else { this }
    } else {
        local(t.parse().ok()?, 0, 0)?
    };
    (secs >= 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Output of a command, or its error text, with a time limit.
fn run(c: Command, timeout: Duration) -> Result<Vec<u8>, String> {
    let (ok, out, err) = run_full(c, timeout)?;
    if ok { Ok(out) } else { Err(err) }
}

/// (succeeded, stdout, the useful line of stderr).
fn run_full(mut c: Command, timeout: Duration) -> Result<(bool, Vec<u8>, String), String> {
    // C.UTF-8: English month names to parse, and any file name intact.
    c.env("LC_ALL", "C.UTF-8").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("bsdtar: {e}"))?;
    let (mut out, mut err) = (child.stdout.take(), child.stderr.take());
    let ro = std::thread::spawn(move || {
        let mut v = vec![];
        if let Some(o) = out.as_mut() {
            let _ = std::io::Read::read_to_end(o, &mut v);
        }
        v
    });
    let re = std::thread::spawn(move || {
        let mut v = vec![];
        if let Some(e) = err.as_mut() {
            let _ = std::io::Read::read_to_end(e, &mut v);
        }
        v
    });
    let t0 = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if t0.elapsed() < timeout => std::thread::sleep(Duration::from_millis(15)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let (o, e) = (ro.join().unwrap_or_default(), re.join().unwrap_or_default());
    let msg = String::from_utf8_lossy(&e);
    // The first complaint says what went wrong ("Error exit delayed…" comes last).
    let line = msg.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("bsdtar failed").trim_start_matches("bsdtar: ").to_string();
    match status {
        Some(st) => Ok((st.success(), o, line)),
        None => Err("Timed out".into()),
    }
}

type Listing = Arc<Vec<Item>>;

/// Everything in an archive, cached until the file changes.
pub fn list(archive: &Path) -> Result<Listing, String> {
    static CACHE: Mutex<Vec<(PathBuf, Option<SystemTime>, Listing)>> = Mutex::new(Vec::new());
    let mtime = std::fs::metadata(archive).and_then(|m| m.modified()).ok();
    if let Some(hit) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).iter().find(|c| c.0 == archive && c.1 == mtime) {
        return Ok(hit.2.clone());
    }
    let mut c = Command::new("bsdtar");
    c.arg("-tvf").arg(archive);
    // One unreadable name makes bsdtar exit with an error after listing the rest: keep them.
    let (ok, out, err) = run_full(c, Duration::from_secs(60))?;
    if !ok && out.is_empty() {
        return Err(err);
    }
    let items: Listing = Arc::new(parse_list(&String::from_utf8_lossy(&out), SystemTime::now()));
    let mut g = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    g.retain(|c| c.0 != archive);
    if g.len() >= 8 {
        g.remove(0);
    }
    g.push((archive.to_path_buf(), mtime, items.clone()));
    Ok(items)
}

/// The entries directly inside `inner` (a folder in the archive), as listing rows whose
/// paths continue the archive's path.
pub fn entries(archive: &Path, inner: &Path) -> Result<Vec<crate::fsx::Entry>, String> {
    let all = list(archive)?;
    if !inner.as_os_str().is_empty() && !all.iter().any(|i| i.path == inner && i.is_dir) {
        return Err(format!("No folder {} in this archive", inner.display()));
    }
    Ok(all
        .iter()
        .filter(|i| i.path.parent().unwrap_or(Path::new("")) == inner)
        .map(|i| {
            let name = i.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let mut e = crate::fsx::Entry::placeholder(archive.join(&i.path), name, i.is_dir);
            e.size = i.size;
            e.modified = i.modified;
            e.is_link = i.link;
            e.mode = if i.is_dir { 0o40755 } else { 0o100644 };
            e
        })
        .collect())
}

pub fn list_async(path: PathBuf, archive: PathBuf, inner: PathBuf, ctx: egui::Context) -> std::sync::mpsc::Receiver<crate::fsx::Listing> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = entries(&archive, &inner);
        let _ = tx.send(crate::fsx::Listing { path, result });
        ctx.request_repaint();
    });
    rx
}

/// Extract `members` (paths inside the archive, folders with their contents) into `dir`.
pub fn extract(archive: &Path, members: &[PathBuf], dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let all = list(archive)?;
    let mut c = Command::new("bsdtar");
    c.arg("-xf").arg(archive).arg("-C").arg(dir).arg("--");
    for m in members {
        // A folder pattern takes everything under it; bsdtar matches names as stored.
        let dot = all.iter().find(|i| i.path == *m).is_some_and(|i| i.dot);
        c.arg(if dot { Path::new(".").join(m) } else { m.clone() });
    }
    run(c, Duration::from_secs(3600)).map(|_| ())
}

/// Where a member is extracted to be opened (one folder per archive and version).
pub fn open_cache(archive: &Path) -> PathBuf {
    let mtime = std::fs::metadata(archive).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0);
    let key = crate::thumbcache::md5_hex(format!("{}\0{mtime}", archive.display()).as_bytes());
    dirs::cache_dir().unwrap_or_else(std::env::temp_dir).join("maia/archives").join(key)
}

/// Extract one file to the cache and return where it landed.
pub fn extract_for_open(archive: &Path, inner: &Path) -> Result<PathBuf, String> {
    let dir = open_cache(archive);
    let dest = dir.join(inner);
    if !dest.exists() {
        extract(archive, &[inner.to_path_buf()], &dir)?;
    }
    dest.exists().then_some(dest).ok_or_else(|| "Couldn't extract it".into())
}

/// Compress `names` (inside `parent`) into `out`; the format follows its extension.
pub fn create(out: &Path, parent: &Path, names: &[std::ffi::OsString]) -> Result<(), String> {
    let tmp = out.with_file_name(format!(".{}.part", out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()));
    let mut c = Command::new("bsdtar");
    c.arg("-cf").arg(&tmp).arg("--format").arg(format_for(out)).args(filter_for(out)).arg("-C").arg(parent).arg("--").args(names);
    let r = run(c, Duration::from_secs(3600));
    match r {
        Ok(_) => std::fs::rename(&tmp, out).map_err(|e| e.to_string()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

fn format_for(out: &Path) -> &'static str {
    let n = out.to_string_lossy().to_lowercase();
    if n.ends_with(".zip") {
        "zip"
    } else if n.ends_with(".7z") {
        "7zip"
    } else {
        "pax"
    }
}

fn filter_for(out: &Path) -> Vec<&'static str> {
    let n = out.to_string_lossy().to_lowercase();
    if n.ends_with(".gz") || n.ends_with(".tgz") {
        vec!["-z"]
    } else if n.ends_with(".xz") {
        vec!["-J"]
    } else if n.ends_with(".bz2") {
        vec!["-j"]
    } else if n.ends_with(".zst") {
        vec!["--zstd"]
    } else {
        vec![]
    }
}

/// Members grouped as the job needs them: archive -> paths inside it.
pub fn group(paths: &[PathBuf]) -> HashMap<PathBuf, Vec<PathBuf>> {
    let mut m: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
    for p in paths {
        if let Some((a, inner)) = split(p) {
            m.entry(a).or_default().push(inner);
        }
    }
    m
}
