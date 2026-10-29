//! File checksums for Properties, by coreutils' md5sum / sha1sum / sha256sum /
//! sha512sum (fast, and nothing to carry in the binary). A pasted checksum picks the
//! algorithm by its length, so comparing with a download page is one paste.

use std::io::Read;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{Arc, mpsc};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Algo {
    Md5,
    Sha1,
    Sha256,
    Sha512,
}

impl Algo {
    pub fn label(self) -> &'static str {
        match self {
            Algo::Md5 => "MD5",
            Algo::Sha1 => "SHA-1",
            Algo::Sha256 => "SHA-256",
            Algo::Sha512 => "SHA-512",
        }
    }

    fn program(self) -> &'static str {
        match self {
            Algo::Md5 => "md5sum",
            Algo::Sha1 => "sha1sum",
            Algo::Sha256 => "sha256sum",
            Algo::Sha512 => "sha512sum",
        }
    }

    /// The algorithm a hex checksum of this many digits comes from.
    pub fn for_hex(s: &str) -> Option<Algo> {
        if !s.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        match s.len() {
            32 => Some(Algo::Md5),
            40 => Some(Algo::Sha1),
            64 => Some(Algo::Sha256),
            128 => Some(Algo::Sha512),
            _ => None,
        }
    }
}

/// A pasted checksum as bare lowercase hex: "SHA256 (x.iso) = AB..", "ab..  x.iso" and
/// spaced groups all come down to the digits.
pub fn clean(s: &str) -> String {
    let s = s.trim();
    let s = s.rsplit_once(" = ").map_or(s, |(_, h)| h);
    let first = s.split_whitespace().next().unwrap_or("");
    // A tool's "hash  filename" line keeps only the hash; spaced groups join up.
    let hex = if Algo::for_hex(first).is_some() { first.to_string() } else { s.split_whitespace().collect() };
    hex.to_ascii_lowercase()
}

/// A checksum being worked out. Dropping it stops the tool.
pub struct Job {
    pub algo: Algo,
    rx: mpsc::Receiver<Result<String, String>>,
    pid: Arc<AtomicU32>,
    done: Arc<AtomicBool>,
}

impl Job {
    pub fn start(path: &Path, algo: Algo, ctx: &egui::Context) -> Job {
        let (tx, rx) = mpsc::channel();
        let (pid, done) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicBool::new(false)));
        let (p, d, path, ctx) = (pid.clone(), done.clone(), path.to_path_buf(), ctx.clone());
        std::thread::spawn(move || {
            let r = run(&path, algo, &p);
            d.store(true, Relaxed);
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        Job { algo, rx, pid, done }
    }

    /// The result once it is there.
    pub fn poll(&self) -> Option<Result<String, String>> {
        self.rx.try_recv().ok()
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        let pid = self.pid.load(Relaxed);
        if pid != 0 && !self.done.load(Relaxed) {
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
    }
}

fn run(path: &Path, algo: Algo, pid: &AtomicU32) -> Result<String, String> {
    let mut c = crate::media::tool(algo.program());
    c.arg("--").arg(path).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().map_err(|_| format!("This checksum needs {}", algo.program()))?;
    pid.store(child.id(), Relaxed);
    let mut out = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        let _ = e.read_to_string(&mut err);
    }
    let ok = child.wait().map(|s| s.success()).unwrap_or(false);
    // A name with a backslash or newline makes the tool start its line with "\".
    let hex = out.trim_start_matches('\\').split_whitespace().next().unwrap_or("").to_string();
    if ok && Algo::for_hex(&hex) == Some(algo) {
        Ok(hex)
    } else {
        Err(err.lines().last().and_then(|l| l.rsplit(": ").next()).filter(|l| !l.is_empty()).unwrap_or("Couldn't read the file").to_string())
    }
}
