//! Office documents (Word, Excel, PowerPoint, OpenDocument and RTF) previewed as PDF.
//! LibreOffice, when installed, converts each version of a file once, headless, into
//! Maia's cache; the PDF preview shows the result. Without LibreOffice nothing here runs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use crate::drives::Tools;

/// Extensions LibreOffice is asked to convert.
pub const EXTS: [&str; 10] = ["docx", "doc", "odt", "rtf", "xlsx", "xls", "ods", "pptx", "ppt", "odp"];

/// A big spreadsheet or deck can take a while; past this the preview gives up.
pub const TIMEOUT: Duration = Duration::from_secs(90);

/// Converted PDFs kept; the oldest go past it.
const KEEP: usize = 200;

pub fn is_office(ext: &str) -> bool {
    EXTS.contains(&ext)
}

/// LibreOffice's command, under either name distributions install it as.
pub fn program(t: &Tools) -> Option<&'static str> {
    ["soffice", "libreoffice"].into_iter().find(|n| t.has(n))
}

pub fn cache_dir() -> PathBuf {
    // Tests run outside the sandbox too: never the real cache.
    let base = if cfg!(test) { None } else { dirs::cache_dir() };
    base.unwrap_or_else(std::env::temp_dir).join("maia/office")
}

/// Where this version of `path` is kept as PDF: a new size or time is a new file.
pub fn cached(path: &Path, size: u64, mtime: Option<SystemTime>) -> PathBuf {
    let t = mtime.and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    let key = crate::thumbcache::md5_hex(format!("{}\0{size}\0{t}", path.display()).as_bytes());
    cache_dir().join(format!("{key}.pdf"))
}

/// A conversion running for a preview. Dropping it (the preview went away) kills
/// LibreOffice if it is still at work.
pub struct Conversion {
    pub rx: Receiver<Result<PathBuf, String>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Conversion {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Conversion {
    pub fn start(tools: Tools, prog: &'static str, src: PathBuf, size: u64, mtime: Option<SystemTime>, ctx: &egui::Context) -> Conversion {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let (st, ctx) = (stop.clone(), ctx.clone());
        std::thread::spawn(move || {
            let r = convert(&tools, prog, &src, size, mtime, TIMEOUT, &|| st.load(Ordering::Relaxed));
            if !st.load(Ordering::Relaxed) {
                let _ = tx.send(r);
                ctx.request_repaint();
            }
        });
        Conversion { rx, stop }
    }
}

/// The PDF for this version of `src`, converted now unless it already was. Blocks;
/// `stop` ends it early, LibreOffice killed.
pub fn convert(tools: &Tools, prog: &str, src: &Path, size: u64, mtime: Option<SystemTime>, timeout: Duration, stop: &dyn Fn() -> bool) -> Result<PathBuf, String> {
    let out = cached(src, size, mtime);
    if out.is_file() {
        return Ok(out);
    }
    // One at a time: a second LibreOffice on the same profile hands its work to the
    // first and exits before the file is written.
    static ONE: Mutex<()> = Mutex::new(());
    let _one = loop {
        if stop() {
            return Err(String::new());
        }
        match ONE.try_lock() {
            Ok(g) => break g,
            Err(std::sync::TryLockError::Poisoned(p)) => break p.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(30)),
        }
    };
    // Converted while this one waited.
    if out.is_file() {
        return Ok(out);
    }
    let dir = cache_dir();
    let stage = dir.join(format!(".part-{}", out.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
    // A profile of Maia's own, so a LibreOffice the user has open is never asked to do
    // the work (or locked out of its settings). A killed conversion leaves its lock.
    let profile = dir.join("profile");
    let _ = std::fs::remove_file(profile.join(".lock"));
    let mut c = crate::media::tool_cmd(tools.cmd(prog));
    c.arg(format!("-env:UserInstallation={}", crate::clipboard::path_to_uri(&profile)));
    c.args(["--headless", "--norestore", "--nolockcheck", "--convert-to", "pdf", "--outdir"]).arg(&stage).arg(src);
    c.stdout(std::process::Stdio::null());
    let r = run_group(c, timeout, stop).and_then(|()| {
        let mut name = src.file_stem().map(|s| s.to_os_string()).unwrap_or_default();
        name.push(".pdf");
        let made = stage.join(name);
        std::fs::rename(&made, &out).map_err(|_| "Can't preview this document".to_string())
    });
    let _ = std::fs::remove_dir_all(&stage);
    r?;
    prune(&dir);
    Ok(out)
}

/// Run `c` in a process group of its own and wait for it. LibreOffice starts its real
/// worker as a child of the program run, so a timeout or `stop` kills the whole group.
fn run_group(mut c: std::process::Command, timeout: Duration, stop: &dyn Fn() -> bool) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    c.process_group(0);
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(st)) if st.success() => return Ok(()),
            Ok(Some(_)) => return Err("Can't preview this document".into()),
            Ok(None) if t0.elapsed() < timeout && !stop() => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                let _ = child.wait();
                return Err(if stop() { String::new() } else { "LibreOffice took too long to read this document".into() });
            }
        }
    }
}

/// Keep the newest `KEEP` PDFs.
fn prune(dir: &Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut pdfs: Vec<(SystemTime, PathBuf)> = rd.flatten().filter(|d| d.path().extension().is_some_and(|x| x == "pdf")).filter_map(|d| Some((d.metadata().ok()?.modified().ok()?, d.path()))).collect();
    if pdfs.len() <= KEEP {
        return;
    }
    pdfs.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, p) in &pdfs[KEEP..] {
        let _ = std::fs::remove_file(p);
    }
}
