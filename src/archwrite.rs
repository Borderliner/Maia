//! Writing archives. Compressing runs as a job whose tool is watched: what it has read
//! so far (/proc/PID/io) moves the card, Pause stops it where it is, Cancel kills it,
//! and nothing it was writing is left behind.

use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::archive;
use crate::ops::{Gate, Progress};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    TarGz,
    TarXz,
    SevenZ,
}

impl Format {
    pub const ALL: [Format; 4] = [Format::Zip, Format::TarGz, Format::TarXz, Format::SevenZ];

    pub fn ext(self) -> &'static str {
        match self {
            Format::Zip => ".zip",
            Format::TarGz => ".tar.gz",
            Format::TarXz => ".tar.xz",
            Format::SevenZ => ".7z",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Zip => "Zip",
            Format::TarGz => "Tar.gz",
            Format::TarXz => "Tar.xz",
            Format::SevenZ => "7-Zip",
        }
    }

    /// The format a name asks for by its extension.
    pub fn of(name: &str) -> Option<Format> {
        let n = name.to_lowercase();
        Format::ALL.into_iter().find(|f| n.ends_with(f.ext()) && n.len() > f.ext().len())
    }

    /// Zip and 7z take a password; a tarball has none to take.
    pub fn lockable(self) -> bool {
        matches!(self, Format::Zip | Format::SevenZ)
    }
}

/// How hard the compressor works: three steps, as a number field would only ask what
/// the numbers mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Level {
    Fast,
    #[default]
    Normal,
    Smallest,
}

impl Level {
    pub const ALL: [Level; 3] = [Level::Fast, Level::Normal, Level::Smallest];

    pub fn label(self) -> &'static str {
        match self {
            Level::Fast => "Fast",
            Level::Normal => "Normal",
            Level::Smallest => "Smallest",
        }
    }

    /// The tools' own level: zstd counts to 19 and starts its default at 3; the rest count
    /// to 9.
    fn n(self, zstd: bool) -> u8 {
        match (self, zstd) {
            (Level::Fast, _) => 1,
            (Level::Normal, true) => 3,
            (Level::Normal, false) => 6,
            (Level::Smallest, true) => 19,
            (Level::Smallest, false) => 9,
        }
    }
}

/// The level last chosen, offered again for the rest of the session.
pub static LAST_LEVEL: Mutex<Level> = Mutex::new(Level::Normal);

pub fn last_level() -> Level {
    *LAST_LEVEL.lock().unwrap_or_else(|e| e.into_inner())
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}


/// How an archive is locked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lock {
    Open,
    ZipAes,
    /// A 7z with its contents locked, and with its names too when `names`.
    Seven { names: bool },
}

/// A tool's complaints without its password prompts, which name the password whatever
/// went wrong.
fn unprompted(s: &str) -> String {
    let mut s = s.to_string();
    for p in ["Enter passphrase:", "Enter password (will not be echoed):", "Verify password (will not be echoed) :", "Enter password:"] {
        s = s.replace(p, "");
    }
    s
}

pub const CANCELLED: &str = "Cancelled";

/// Bytes a process has read so far, all its threads together.
fn read_so_far(pid: libc::pid_t) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/io")).ok()?;
    s.lines().find_map(|l| l.strip_prefix("rchar: "))?.trim().parse().ok()
}

/// Run a tool for a job. `feed` goes to its input (passwords); what it reads counts from
/// `base` into the job's done bytes, up to `span` of them; the name it says it is at
/// (bsdtar -v: "a docs/notes.txt") shows under the bar. Paused, its process group is
/// stopped; cancelled, killed.
pub(crate) fn watch(mut c: Command, feed: Option<String>, p: &Mutex<Progress>, gate: &Gate, ctx: &egui::Context, base: u64, span: u64) -> Result<(), String> {
    let lock = || p.lock().unwrap_or_else(|e| e.into_inner());
    c.env("LC_ALL", "C.UTF-8").stdin(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|e| format!("{}: {e}", c.get_program().to_string_lossy()))?;
    if let Some(mut i) = child.stdin.take() {
        if let Some(f) = &feed {
            let _ = i.write_all(f.as_bytes());
        }
    }
    let pid = child.id() as libc::pid_t;
    let err = child.stderr.take();
    std::thread::scope(|sc| {
        let said = sc.spawn(|| {
            let (mut errs, mut line) = (String::new(), Vec::<u8>::new());
            let Some(mut e) = err else { return errs };
            let mut buf = [0u8; 8192];
            let at = |l: &[u8]| (l.starts_with(b"a ") || l.starts_with(b"x ")).then(|| String::from_utf8_lossy(&l[2..]).into_owned());
            while let Ok(n) = e.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut last = None;
                for &b in &buf[..n] {
                    if b != b'\n' {
                        line.push(b);
                        continue;
                    }
                    // A member that failed ends its line with why: "x notes.txt: Incorrect
                    // passphrase". Kept, without the "x ", for the error said at the end.
                    let name = at(&line);
                    if (name.is_none() || name.as_ref().is_some_and(|n| n.contains(": "))) && errs.len() < 64 << 10 {
                        errs.push_str(&String::from_utf8_lossy(if name.is_some() { &line[2..] } else { &line }));
                        errs.push('\n');
                    }
                    if name.is_some() {
                        last = name;
                    }
                    line.clear();
                }
                // The line begun and not yet ended is the item being written now.
                if let Some(name) = at(&line).or(last) {
                    lock().current = name;
                }
            }
            errs.push_str(&String::from_utf8_lossy(&line));
            errs
        });
        let (mut stopped, mut shown) = (false, Instant::now());
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break Ok(st),
                Ok(None) => {}
                Err(e) => break Err(e.to_string()),
            }
            if gate.cancelled() {
                unsafe { libc::kill(-pid, libc::SIGKILL) };
                let _ = child.kill();
                let _ = child.wait();
                break Err(CANCELLED.to_string());
            }
            if gate.paused() != stopped {
                stopped = !stopped;
                unsafe { libc::kill(-pid, if stopped { libc::SIGSTOP } else { libc::SIGCONT }) };
            }
            if let Some(n) = read_so_far(pid) {
                let mut g = lock();
                g.done_bytes = g.done_bytes.max(base + n.min(span));
            }
            if shown.elapsed() > Duration::from_millis(200) {
                ctx.request_repaint();
                shown = Instant::now();
            }
            std::thread::sleep(Duration::from_millis(30));
        };
        let errs = said.join().unwrap_or_default();
        let st = status?;
        if st.success() {
            let mut g = lock();
            g.done_bytes = g.done_bytes.max(base + span);
            return Ok(());
        }
        Err(archive::said(&unprompted(&errs), feed.as_deref()))
    })
}

/// A hidden name beside `p` for work in progress: ".photos.zip.part".
fn beside(p: &Path, what: &str) -> PathBuf {
    let dir = p.parent().unwrap_or(Path::new("/"));
    crate::fsx::unique_dest(dir, &format!(".{}.{what}", name_of(p)))
}

/// `src` laid out at `dst` without copying where the disk allows: folders made, files
/// hard-linked (copied where a link is refused), links made again, special files left
/// out. With `merge`, into what is at `dst` already, replacing files.
fn link_tree(src: &Path, dst: &Path, merge: bool) -> std::io::Result<()> {
    let m = std::fs::symlink_metadata(src)?;
    let there = std::fs::symlink_metadata(dst).ok();
    if merge && there.as_ref().is_some_and(|t| !(t.is_dir() && m.is_dir())) {
        crate::ops::remove_tree(dst)?;
    }
    if m.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)
    } else if m.is_dir() {
        if !(merge && dst.is_dir()) {
            std::fs::create_dir(dst)?;
        }
        for e in std::fs::read_dir(src)? {
            let e = e?;
            link_tree(&e.path(), &dst.join(e.file_name()), merge)?;
        }
        let _ = std::fs::set_permissions(dst, m.permissions());
        if let (Ok(t), Ok(d)) = (m.modified(), std::fs::File::open(dst)) {
            let _ = d.set_modified(t);
        }
        Ok(())
    } else if m.is_file() {
        std::fs::hard_link(src, dst).or_else(|_| std::fs::copy(src, dst).map(|_| ()))
    } else {
        Ok(())
    }
}

/// A staging folder removed, read-only folders inside it (from a tarball) included.
fn clear(dir: &Path) {
    if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let _ = std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700));
        if let Ok(rd) = std::fs::read_dir(&d) {
            stack.extend(rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()));
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

/// The command that packs `names` (inside `base`) into `tmp`, in the format `out`'s name
/// asks for, at `level`, locked as `lock` says; and what to feed it.
fn pack(out: &Path, tmp: &Path, base: &Path, names: &[OsString], level: Level, lock: Lock, pw: Option<&str>) -> Result<(Command, Option<String>), String> {
    let format = archive::format_for(out);
    if let (Lock::Seven { names: hidden }, Some(pw)) = (lock, pw) {
        let mut c = archive::tool(archive::seven()?);
        // The part file's name says nothing of its kind: -t7z does.
        c.current_dir(base).stdout(Stdio::null()).args(["a", "-bso0", "-bsp0", "-y", "-t7z", "-snl", "-spd", &format!("-mx={}", level.n(false)), "-p"]);
        if hidden {
            c.arg("-mhe=on");
        }
        c.arg("--").arg(tmp).args(names);
        return Ok((c, Some(format!("{pw}\n").repeat(4))));
    }
    let filter = archive::filter_for(out);
    let mut opts = vec![];
    let n = level.n(filter.contains(&"--zstd"));
    match (format, filter.first().copied()) {
        ("zip", _) => opts.push(format!("zip:compression-level={n}")),
        ("7zip", _) => opts.push(format!("7zip:compression-level={n}")),
        (_, Some("-z")) => opts.push(format!("gzip:compression-level={n}")),
        (_, Some("-J")) => opts.push(format!("xz:compression-level={n}")),
        (_, Some("-j")) => opts.push(format!("bzip2:compression-level={n}")),
        (_, Some("--zstd")) => opts.push(format!("zstd:compression-level={n}")),
        _ => {}
    }
    let feed = match (lock, pw) {
        (Lock::ZipAes, Some(pw)) => {
            opts.push("zip:encryption=aes256".into());
            Some(format!("{pw}\n"))
        }
        _ => None,
    };
    let mut c = archive::tool("bsdtar");
    c.stdout(Stdio::null()).arg("-cvf").arg(tmp).arg("--format").arg(format).args(filter);
    if !opts.is_empty() {
        c.arg("--options").arg(opts.join(","));
    }
    if names.is_empty() {
        // Everything deleted: an empty archive, still of its kind.
        c.args(["-T", "/dev/null"]);
    } else {
        c.arg("-C").arg(base).arg("--").args(names);
    }
    Ok((c, feed))
}

/// Compress `srcs` into `out` at `level`, locked with `pw` when given (a zip with
/// AES-256, a 7z with its names locked too). Sources from several folders are gathered
/// under one as links first. Once made, a locked archive opens with its password for
/// the rest of the session.
pub fn compress(out: &Path, srcs: &[PathBuf], level: Level, pw: Option<&str>, p: &Mutex<Progress>, gate: &Gate, ctx: &egui::Context) -> Result<(), String> {
    if pw.is_some_and(|pw| pw.is_empty() || pw.contains(['\n', '\r'])) {
        return Err("Type a password on one line".into());
    }
    let total: u64 = srcs.iter().map(|s| crate::ops::tree_size(s)).sum();
    p.lock().unwrap_or_else(|e| e.into_inner()).total_bytes = total;
    let names: Vec<OsString> = srcs.iter().filter_map(|s| s.file_name().map(|n| n.to_os_string())).collect();
    let mut parents: Vec<&Path> = srcs.iter().filter_map(|s| s.parent()).collect();
    parents.sort();
    parents.dedup();
    let tmp = beside(out, "part");
    let mut stage = None;
    let r = (|| {
        let base = match parents.as_slice() {
            [one] => one.to_path_buf(),
            _ => {
                let st = beside(out, "gather");
                std::fs::create_dir(&st).map_err(|e| e.to_string())?;
                stage = Some(st.clone());
                for s in srcs {
                    link_tree(s, &st.join(name_of(s)), false).map_err(|e| format!("{}: {e}", s.display()))?;
                }
                st
            }
        };
        let lock = match (pw, Format::of(&name_of(out))) {
            (None, _) => Lock::Open,
            (Some(_), Some(Format::SevenZ)) => Lock::Seven { names: true },
            (Some(_), _) => Lock::ZipAes,
        };
        let (c, feed) = pack(out, &tmp, &base, &names, level, lock, pw)?;
        watch(c, feed, p, gate, ctx, 0, total)?;
        std::fs::rename(&tmp, out).map_err(|e| e.to_string())
    })();
    if let Some(st) = stage {
        clear(&st);
    }
    match r {
        Ok(()) => {
            if let Some(pw) = pw {
                archive::set_password(out, pw.to_string());
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Extract `members` (all, when none) of `archive` into `dir` for a job: its reading
/// counted against the archive's size, Pause and Cancel heeded.
pub fn extract(archive: &Path, members: &[PathBuf], dir: &Path, p: &Mutex<Progress>, gate: &Gate, ctx: &egui::Context) -> Result<(), String> {
    let size = std::fs::metadata(archive).map(|m| m.len()).unwrap_or(0);
    {
        let mut g = p.lock().unwrap_or_else(|e| e.into_inner());
        g.total_bytes += size;
    }
    let base = p.lock().unwrap_or_else(|e| e.into_inner()).done_bytes;
    archive::extract_with(archive, members, dir, true, &mut |c, pw| watch(c, pw.map(|p| format!("{p}\n")), p, gate, ctx, base, size))
}
