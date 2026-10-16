//! Background file operations with progress reporting.

use std::fs;
use std::io::{Read, Write};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::fsx::unique_dest;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Conflict {
    /// Keep both: "name (1).ext".
    KeepBoth,
    Overwrite,
    Skip,
    /// Folders only: copy into the existing folder, replacing files.
    Merge,
}

#[derive(Clone, Debug)]
pub enum OpKind {
    Copy { srcs: Vec<PathBuf>, dest: PathBuf, decide: HashMap<PathBuf, Conflict> },
    Move { srcs: Vec<PathBuf>, dest: PathBuf, decide: HashMap<PathBuf, Conflict> },
    Trash { paths: Vec<PathBuf> },
    Delete { paths: Vec<PathBuf> },
    /// Paths inside `archive` (none: all of it, into its own folder when it holds
    /// several things) extracted into `dest`.
    Extract { archive: PathBuf, members: Vec<PathBuf>, dest: PathBuf },
    /// `srcs` (all in one folder) packed into the archive `out`.
    Compress { srcs: Vec<PathBuf>, out: PathBuf },
    /// One command run as administrator (through pkexec).
    Admin { task: crate::admin::Task, tools: crate::drives::Tools },
}

impl OpKind {
    pub fn verb(&self) -> &'static str {
        match self {
            OpKind::Copy { .. } => "Copying",
            OpKind::Move { .. } => "Moving",
            OpKind::Trash { .. } => "Moving to trash",
            OpKind::Delete { .. } => "Deleting",
            OpKind::Extract { .. } => "Extracting",
            OpKind::Compress { .. } => "Compressing",
            OpKind::Admin { .. } => "Working as administrator",
        }
    }
}

#[derive(Default, Debug)]
pub struct Progress {
    pub total_bytes: u64,
    pub done_bytes: u64,
    pub total_items: usize,
    pub done_items: usize,
    pub current: String,
    pub errors: Vec<String>,
    pub finished: bool,
    /// (src, dst) pairs actually produced, for undo.
    pub produced: Vec<(PathBuf, PathBuf)>,
    /// Items refused for lack of permission (offered again as administrator).
    pub denied: Vec<PathBuf>,
    /// An archive that wants a password (or refused the one given): asked for, then the
    /// job runs again.
    pub password: Option<(PathBuf, bool)>,
}

pub struct Job {
    pub id: u64,
    pub label: String,
    pub kind: OpKind,
    pub progress: Arc<Mutex<Progress>>,
    pub cancel: Arc<AtomicBool>,
    pub pause: Arc<AtomicBool>,
    pub started: std::time::Instant,
    pub finished_at: Option<std::time::Instant>,
    pub show_errors: bool,
}

pub fn spawn(id: u64, kind: OpKind, ctx: egui::Context) -> Job {
    let progress = Arc::new(Mutex::new(Progress::default()));
    let cancel = Arc::new(AtomicBool::new(false));
    let pause = Arc::new(AtomicBool::new(false));
    let n = match &kind {
        OpKind::Copy { srcs, .. } | OpKind::Move { srcs, .. } | OpKind::Compress { srcs, .. } => srcs.len(),
        OpKind::Trash { paths } | OpKind::Delete { paths } | OpKind::Extract { members: paths, .. } => paths.len(),
        OpKind::Admin { .. } => 1,
    };
    let label = match &kind {
        OpKind::Extract { archive, members, .. } if members.is_empty() => format!("Extracting {}", name_of(archive)),
        OpKind::Compress { out, .. } => format!("Compressing into {}", name_of(out)),
        OpKind::Admin { task, .. } => task.label(),
        _ => format!("{} {} item{}", kind.verb(), n, if n == 1 { "" } else { "s" }),
    };
    let (p2, c2, k2, pa2) = (progress.clone(), cancel.clone(), kind.clone(), pause.clone());
    std::thread::spawn(move || {
        let gate = Gate { cancel: &c2, pause: &pa2 };
        run(&k2, &p2, &gate, &ctx);
        p2.lock().unwrap().finished = true;
        ctx.request_repaint();
    });
    Job { id, label, kind, progress, cancel, pause, started: std::time::Instant::now(), finished_at: None, show_errors: false }
}

/// Cancellation + pause checkpoint shared by the workers.
pub struct Gate<'a> {
    cancel: &'a AtomicBool,
    pause: &'a AtomicBool,
}

impl Gate<'_> {
    /// Blocks while paused; returns true if cancelled.
    fn stop(&self) -> bool {
        while self.pause.load(Ordering::Relaxed) && !self.cancel.load(Ordering::Relaxed) {
            std::thread::sleep(std::time::Duration::from_millis(80));
        }
        self.cancel.load(Ordering::Relaxed)
    }
}

fn run(kind: &OpKind, p: &Mutex<Progress>, cancel: &Gate, ctx: &egui::Context) {
    match kind {
        OpKind::Copy { srcs, dest, decide } => {
            let total: u64 = srcs.iter().map(|s| tree_size(s)).sum();
            {
                let mut g = p.lock().unwrap();
                g.total_bytes = total;
                g.total_items = srcs.len();
            }
            for s in srcs {
                if cancel.stop() {
                    break;
                }
                let policy = decide.get(s).copied().unwrap_or(Conflict::KeepBoth);
                let Some(dst) = resolve_dest(s, dest, policy, p) else { continue };
                if s.is_dir() && inside(&dst, s) {
                    p.lock().unwrap().errors.push(format!("Can't copy {} into itself", s.display()));
                    continue;
                }
                match copy_tree(s, &dst, p, cancel, ctx, policy == Conflict::Merge) {
                    Ok(()) => p.lock().unwrap().produced.push((s.clone(), dst)),
                    Err(e) => {
                        let mut g = p.lock().unwrap();
                        if crate::admin::denied(&e) {
                            g.denied.push(s.clone());
                        }
                        g.errors.push(format!("{}: {e}", s.display()));
                    }
                }
                p.lock().unwrap().done_items += 1;
            }
        }
        OpKind::Move { srcs, dest, decide } => {
            p.lock().unwrap().total_items = srcs.len();
            for s in srcs {
                if cancel.stop() {
                    break;
                }
                if s.parent() == Some(dest.as_path()) {
                    p.lock().unwrap().done_items += 1;
                    continue; // moving onto itself is a no-op
                }
                if inside(dest, s) {
                    p.lock().unwrap().errors.push(format!("Can't move {} into itself", s.display()));
                    continue;
                }
                let policy = decide.get(s).copied().unwrap_or(Conflict::KeepBoth);
                let Some(dst) = resolve_dest(s, dest, policy, p) else { continue };
                p.lock().unwrap().current = name_of(s);
                let merge = policy == Conflict::Merge && dst.is_dir();
                let r = match if merge { Err(std::io::Error::from_raw_os_error(libc::EXDEV)) } else { fs::rename(s, &dst) } {
                    Ok(()) => Ok(()),
                    // Cross-device: copy then delete.
                    Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
                        p.lock().unwrap().total_bytes += tree_size(s);
                        copy_tree(s, &dst, p, cancel, ctx, merge).and_then(|_| remove_tree(s))
                    }
                    Err(e) => Err(e),
                };
                match r {
                    Ok(()) => p.lock().unwrap().produced.push((s.clone(), dst)),
                    Err(e) => {
                        let mut g = p.lock().unwrap();
                        if crate::admin::denied(&e) {
                            g.denied.push(s.clone());
                        }
                        g.errors.push(format!("{}: {e}", s.display()));
                    }
                }
                p.lock().unwrap().done_items += 1;
                ctx.request_repaint();
            }
        }
        OpKind::Trash { paths } => {
            p.lock().unwrap().total_items = paths.len();
            for s in paths {
                if cancel.stop() {
                    break;
                }
                p.lock().unwrap().current = name_of(s);
                if let Err(e) = crate::trashcan::delete(s) {
                    p.lock().unwrap().errors.push(format!("{}: {e}", s.display()));
                }
                p.lock().unwrap().done_items += 1;
                ctx.request_repaint();
            }
        }
        OpKind::Admin { task, tools } => {
            p.lock().unwrap().total_items = 1;
            match crate::admin::run(tools, task) {
                Ok(()) => p.lock().unwrap().done_items = 1,
                Err(e) => p.lock().unwrap().errors.push(e),
            }
        }
        OpKind::Extract { archive, members, dest } => {
            p.lock().unwrap().current = name_of(archive);
            extract(archive, members, dest, p);
        }
        OpKind::Compress { srcs, out } => {
            p.lock().unwrap().total_items = srcs.len();
            p.lock().unwrap().current = name_of(out);
            let names: Vec<std::ffi::OsString> = srcs.iter().filter_map(|s| s.file_name().map(|n| n.to_os_string())).collect();
            match out.parent().map(|parent| crate::archive::create(out, parent, &names)) {
                Some(Ok(())) => {
                    let mut g = p.lock().unwrap();
                    g.produced.push((out.clone(), out.clone()));
                    g.done_items = srcs.len();
                }
                Some(Err(e)) => p.lock().unwrap().errors.push(format!("{}: {e}", name_of(out))),
                None => {}
            }
        }
        OpKind::Delete { paths } => {
            p.lock().unwrap().total_items = paths.len();
            for s in paths {
                if cancel.stop() {
                    break;
                }
                p.lock().unwrap().current = name_of(s);
                if let Err(e) = remove_tree(s) {
                    let mut g = p.lock().unwrap();
                    if crate::admin::denied(&e) {
                        g.denied.push(s.clone());
                    }
                    g.errors.push(format!("{}: {e}", s.display()));
                }
                p.lock().unwrap().done_items += 1;
                ctx.request_repaint();
            }
        }
    }
}

/// Extract into a hidden staging folder beside the results, then move each top-level
/// result into place (renamed if the name is taken), so a failure leaves nothing half done.
fn extract(archive: &Path, members: &[PathBuf], dest: &Path, p: &Mutex<Progress>) {
    let fail = |e: String| {
        let mut g = p.lock().unwrap_or_else(|e| e.into_inner());
        if crate::archive::wants_password(&e) {
            g.password = Some((archive.to_path_buf(), e == crate::archive::WRONG_PASSWORD));
        }
        g.errors.push(format!("{}: {e}", name_of(archive)));
    };
    // The folder made for several top-level items, removed again if nothing lands in it.
    let mut made: Option<PathBuf> = None;
    let (members, dest) = if members.is_empty() {
        let tops: Vec<PathBuf> = match crate::archive::list(archive) {
            Ok(all) => all.iter().filter(|i| i.path.components().count() == 1).map(|i| i.path.clone()).collect(),
            Err(e) => return fail(e),
        };
        // Several things at the top go into a folder named after the archive.
        let dest = if tops.len() > 1 {
            let stem = name_of(archive);
            let stem = crate::archive::stem(&stem);
            let d = unique_dest(dest, stem);
            if let Err(e) = fs::create_dir(&d) {
                return fail(e.to_string());
            }
            made = Some(d.clone());
            d
        } else {
            dest.to_path_buf()
        };
        (tops, dest)
    } else {
        (members.to_vec(), dest.to_path_buf())
    };
    p.lock().unwrap_or_else(|e| e.into_inner()).total_items = members.len();
    let stage = dest.join(format!(".maia-extract-{}", std::process::id()));
    if let Err(e) = crate::archive::extract(archive, &members, &stage) {
        let _ = fs::remove_dir_all(&stage);
        if let Some(d) = made {
            let _ = fs::remove_dir(d);
        }
        return fail(e);
    }
    for m in &members {
        let from = stage.join(m);
        let to = unique_dest(&dest, &name_of(m));
        match fs::rename(&from, &to) {
            Ok(()) => p.lock().unwrap_or_else(|e| e.into_inner()).produced.push((archive.join(m), to)),
            Err(e) => fail(format!("{}: {e}", m.display())),
        }
        p.lock().unwrap_or_else(|e| e.into_inner()).done_items += 1;
    }
    let _ = fs::remove_dir_all(&stage);
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn resolve_dest(src: &Path, dest_dir: &Path, conflict: Conflict, p: &Mutex<Progress>) -> Option<PathBuf> {
    let name = name_of(src);
    let target = dest_dir.join(&name);
    let exists = fs::symlink_metadata(&target).is_ok();
    // Copying a file onto itself always duplicates.
    if !exists || target == src {
        return Some(if exists { unique_dest(dest_dir, &name) } else { target });
    }
    match conflict {
        Conflict::KeepBoth => Some(unique_dest(dest_dir, &name)),
        Conflict::Skip => {
            p.lock().unwrap().done_items += 1;
            None
        }
        Conflict::Merge if target.is_dir() && src.is_dir() => Some(target),
        Conflict::Merge | Conflict::Overwrite => {
            if let Err(e) = remove_tree(&target) {
                p.lock().unwrap().errors.push(format!("{}: {e}", target.display()));
                return None;
            }
            Some(target)
        }
    }
}

/// Is `p` the same as or inside `dir`, also through symlinks (compared by real path)?
fn inside(p: &Path, dir: &Path) -> bool {
    if p.starts_with(dir) {
        return true;
    }
    let real = |x: &Path| -> Option<PathBuf> {
        // `p` may not exist yet: resolve its closest existing ancestor.
        let mut base = x.to_path_buf();
        let mut rest = vec![];
        while !base.exists() {
            rest.push(base.file_name()?.to_os_string());
            base = base.parent()?.to_path_buf();
        }
        let mut r = fs::canonicalize(&base).ok()?;
        for c in rest.iter().rev() {
            r.push(c);
        }
        Some(r)
    };
    match (real(p), real(dir)) {
        (Some(a), Some(b)) => a.starts_with(&b),
        _ => false,
    }
}

/// Total size of files below `p` (symlinks not followed). Iterative, so arbitrarily deep
/// trees can't overflow the stack.
pub fn tree_size(p: &Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![p.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(m) = fs::symlink_metadata(&d) else { continue };
        if m.is_dir() {
            if let Ok(rd) = fs::read_dir(&d) {
                stack.extend(rd.flatten().map(|e| e.path()));
            }
        } else {
            total += m.len();
        }
    }
    total
}

fn copy_tree(
    src: &Path,
    dst: &Path,
    p: &Mutex<Progress>,
    cancel: &Gate,
    ctx: &egui::Context,
    merge: bool,
) -> std::io::Result<()> {
    if cancel.stop() {
        return Err(std::io::Error::other("cancelled"));
    }
    let m = fs::symlink_metadata(src)?;
    let ft = m.file_type();
    {
        use std::os::unix::fs::FileTypeExt;
        // Devices, FIFOs and sockets: reading them can block forever or never end
        // (/dev/zero would fill the disk). Skip them with an error instead.
        if ft.is_fifo() || ft.is_socket() || ft.is_char_device() || ft.is_block_device() {
            p.lock().unwrap().errors.push(format!("Skipped special file {}", src.display()));
            return Ok(());
        }
    }
    if ft.is_symlink() {
        let target = fs::read_link(src)?;
        std::os::unix::fs::symlink(target, dst)?;
    } else if ft.is_dir() {
        if !(merge && dst.is_dir()) {
            fs::create_dir(dst)?;
        }
        for de in fs::read_dir(src)?.flatten() {
            let child = dst.join(de.file_name());
            if merge && !child.is_dir() && fs::symlink_metadata(&child).is_ok() {
                let _ = fs::remove_file(&child);
            }
            copy_tree(&de.path(), &child, p, cancel, ctx, merge)?;
        }
        fs::set_permissions(dst, m.permissions())?;
    } else {
        p.lock().unwrap().current = name_of(src);
        let mut r = fs::File::open(src)?;
        let mut w = fs::File::create(dst)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut since_repaint = 0u64;
        loop {
            if cancel.stop() {
                drop(w);
                let _ = fs::remove_file(dst);
                return Err(std::io::Error::other("cancelled"));
            }
            // A failed read or write (disk full, device gone) must not leave a truncated
            // file behind that looks like a finished copy.
            let step = r.read(&mut buf).and_then(|n| w.write_all(&buf[..n]).map(|_| n));
            let n = match step {
                Ok(n) => n,
                Err(e) => {
                    drop(w);
                    let _ = fs::remove_file(dst);
                    return Err(e);
                }
            };
            if n == 0 {
                break;
            }
            p.lock().unwrap().done_bytes += n as u64;
            since_repaint += n as u64;
            if since_repaint > 8 << 20 {
                ctx.request_repaint();
                since_repaint = 0;
            }
        }
        fs::set_permissions(dst, m.permissions())?;
        if let Ok(mt) = m.modified() {
            let _ = w.set_modified(mt);
        }
    }
    Ok(())
}

pub fn remove_tree(p: &Path) -> std::io::Result<()> {
    let m = fs::symlink_metadata(p)?;
    if m.is_dir() { fs::remove_dir_all(p) } else { fs::remove_file(p) }
}
