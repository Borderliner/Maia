//! Updates from the project's releases on GitHub, only when asked. "Check for updates"
//! looks at the latest release; "Update" downloads its build, checks it against the
//! release's checksum and puts it in place of the installed one, which the next start
//! runs. Nothing is sent but the requests themselves; curl does them, never with the
//! user's own settings or credentials.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Where the releases are.
pub const REPO: &str = "yappologistic/Maia";
/// A release's files: the program, and its SHA-256 as `sha256sum` writes it.
pub const ASSET: &str = "maia-linux-x86_64";
pub const SUMS: &str = "maia-linux-x86_64.sha256";
/// A build is a few megabytes; anything near this is not one.
const MAX_BIN: u64 = 64 << 20;

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    /// What's new, as the release says it (first lines only are shown).
    pub notes: String,
    pub bin: String,
    pub sum: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum State {
    Idle,
    Checking,
    /// This is the newest.
    Latest,
    Available(Release),
    Downloading(String),
    /// Installed; the next start runs it.
    Ready(String),
    Failed(String),
    /// The program's folder can't be written (installed by a package manager, say).
    CantWrite(PathBuf),
}

/// "v1.2.3", "1.2", "1.2.3-beta" as (major, minor, patch); None for anything else.
pub fn version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches(['v', 'V']).split(['-', '+']).next()?;
    let mut n = core.split('.').map(|p| p.parse::<u64>());
    let major = n.next()?.ok()?;
    let minor = match n.next() {
        Some(p) => p.ok()?,
        None => 0,
    };
    let patch = match n.next() {
        Some(p) => p.ok()?,
        None => 0,
    };
    n.next().is_none().then_some((major, minor, patch))
}

/// Whether `candidate` is a later version than `current`.
pub fn newer(current: &str, candidate: &str) -> bool {
    matches!((version(current), version(candidate)), (Some(a), Some(b)) if b > a)
}

/// The release in GitHub's answer for the latest one, if it carries both files and
/// they are this project's downloads.
pub fn parse_release(body: &[u8]) -> Option<Release> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let tag = v["tag_name"].as_str()?.trim().to_string();
    version(&tag)?;
    let prefix = format!("https://github.com/{REPO}/releases/download/");
    let url = |name: &str| {
        v["assets"].as_array()?.iter().find(|a| a["name"].as_str() == Some(name)).and_then(|a| a["browser_download_url"].as_str()).filter(|u| u.starts_with(&prefix)).map(str::to_string)
    };
    let notes = v["body"].as_str().unwrap_or("").trim().to_string();
    Some(Release { version: tag, notes, bin: url(ASSET)?, sum: url(SUMS)? })
}

/// The checksum in a `sha256sum` line ("<64 hex>  name"), lowercase.
pub fn parse_sum(text: &str) -> Option<String> {
    let h = text.split_whitespace().next()?.to_ascii_lowercase();
    (h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

/// A GET: the address, and where to save the body (None: return it).
pub type Fetch = Arc<dyn Fn(&str, Option<&Path>) -> Result<Vec<u8>, String> + Send + Sync>;

/// The command for one GET. `-q` first, so no ~/.curlrc is read.
pub fn curl_args(url: &str, to: Option<&Path>) -> Vec<String> {
    let mut a: Vec<String> = ["-q", "--silent", "--show-error", "--fail", "--location", "--max-redirs", "5", "--proto", "=https", "--proto-redir", "=https", "--connect-timeout", "10", "--max-time", "180", "--user-agent", "Maia"].map(String::from).to_vec();
    a.extend(["--max-filesize".into(), MAX_BIN.to_string()]);
    if url.starts_with("https://api.github.com/") {
        a.extend(["--header".into(), "Accept: application/vnd.github+json".into()]);
    }
    if let Some(p) = to {
        a.extend(["--output".into(), p.display().to_string()]);
    }
    a.push(url.to_string());
    a
}

#[cfg(not(test))]
fn curl(url: &str, to: Option<&Path>) -> Result<Vec<u8>, String> {
    if !url.starts_with("https://") {
        return Err("Not a secure address".into());
    }
    if !crate::media::have("curl") {
        return Err("Needs curl, which isn't installed".into());
    }
    let mut c = crate::media::tool("curl");
    c.args(curl_args(url, to));
    crate::media::output_capped(c, Duration::from_secs(200), MAX_BIN + 1024, &|| false).ok_or_else(|| "Couldn't reach GitHub".to_string())
}

/// Test builds never reach the network: a test hands in its own fetch.
#[cfg(test)]
fn curl(_: &str, _: Option<&Path>) -> Result<Vec<u8>, String> {
    Err("No network in tests".into())
}

/// SHA-256 of a file, by coreutils' sha256sum.
fn sha256(path: &Path) -> Option<String> {
    let mut c = crate::media::tool("sha256sum");
    c.arg(path);
    parse_sum(&String::from_utf8_lossy(&crate::media::output(c, Duration::from_secs(60))?))
}

pub struct Updater {
    state: Arc<Mutex<State>>,
    fetch: Fetch,
    /// The program to replace.
    exe: PathBuf,
}

impl Default for Updater {
    fn default() -> Self {
        Updater { state: Arc::new(Mutex::new(State::Idle)), fetch: Arc::new(curl), exe: std::env::current_exe().unwrap_or_default() }
    }
}

impl Updater {
    pub fn state(&self) -> State {
        self.state.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn busy(&self) -> bool {
        matches!(self.state(), State::Checking | State::Downloading(_))
    }

    /// Ask GitHub for the latest release, on a thread.
    pub fn check(&self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = State::Checking;
        let (state, fetch, ctx) = (self.state.clone(), self.fetch.clone(), ctx.clone());
        std::thread::spawn(move || {
            let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
            let next = match fetch(&url, None) {
                Ok(body) => match parse_release(&body) {
                    Some(r) if newer(env!("CARGO_PKG_VERSION"), &r.version) => State::Available(r),
                    Some(_) => State::Latest,
                    None => State::Failed("The latest release has no build for this system".into()),
                },
                Err(e) => State::Failed(e),
            };
            *state.lock().unwrap_or_else(|e| e.into_inner()) = next;
            ctx.request_repaint();
        });
    }

    /// Download `r`, check it, and put it in place of the installed program, on a thread.
    pub fn install(&self, r: Release, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        let dir = self.exe.parent().map(Path::to_path_buf).unwrap_or_default();
        let probe = dir.join(format!(".maia-write-check-{}", std::process::id()));
        if std::fs::write(&probe, b"").is_err() {
            *self.state.lock().unwrap_or_else(|e| e.into_inner()) = State::CantWrite(dir);
            return;
        }
        let _ = std::fs::remove_file(&probe);
        *self.state.lock().unwrap_or_else(|e| e.into_inner()) = State::Downloading(r.version.clone());
        let (state, fetch, exe, ctx) = (self.state.clone(), self.fetch.clone(), self.exe.clone(), ctx.clone());
        std::thread::spawn(move || {
            let next = match put_in_place(&r, &exe, &fetch) {
                Ok(()) => State::Ready(r.version.clone()),
                Err(e) => State::Failed(e),
            };
            *state.lock().unwrap_or_else(|e| e.into_inner()) = next;
            ctx.request_repaint();
        });
    }

    /// Start the new version in a moment and close this one (its window, and so the app).
    pub fn restart(&self, ctx: &egui::Context) {
        let exe = self.exe.clone();
        let mut c = std::process::Command::new("setsid");
        c.args(["sh", "-c", "sleep 1; exec \"$0\""]).arg(&exe).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        if cfg!(not(test)) && c.spawn().is_ok() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    #[cfg(test)]
    pub fn for_test(fetch: Fetch, exe: PathBuf) -> Updater {
        Updater { state: Arc::new(Mutex::new(State::Idle)), fetch, exe }
    }
}

/// The download, checked, made runnable, and renamed over the program (a rename on the
/// same filesystem: the running program keeps its old file until it exits).
fn put_in_place(r: &Release, exe: &Path, fetch: &Fetch) -> Result<(), String> {
    let want = fetch(&r.sum, None).ok().and_then(|b| parse_sum(&String::from_utf8_lossy(&b))).ok_or("The release's checksum couldn't be read")?;
    let tmp = exe.with_file_name(format!(".maia-update-{}", std::process::id()));
    let done = (|| {
        fetch(&r.bin, Some(&tmp))?;
        let head = std::fs::read(&tmp).map_err(|e| e.to_string())?;
        if !head.starts_with(b"\x7fELF") {
            return Err("The download isn't a program".to_string());
        }
        if sha256(&tmp).as_deref() != Some(want.as_str()) {
            return Err("The download doesn't match the release's checksum".to_string());
        }
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, exe).map_err(|e| e.to_string())
    })();
    if done.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    done
}
