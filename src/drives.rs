//! Block devices (lsblk) and mounting, unmounting and ejecting them through udisks
//! (udisksctl): the same service desktop file managers use, so permissions and password
//! prompts behave as the user expects.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

/// Where `lsblk`, `udisksctl` and the mount table come from. Release builds use the
/// system's; test builds only ever use fake tools a simulation sets up (or none), so no
/// test can touch a real disk.
#[derive(Clone, Debug)]
pub struct Tools {
    dir: Option<PathBuf>,
}

#[cfg(test)]
thread_local! {
    /// Folder holding fake `lsblk`, `udisksctl` and `mounts` for this thread's app.
    pub static FAKE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

impl Tools {
    pub fn current() -> Option<Tools> {
        #[cfg(test)]
        return FAKE.with(|f| f.borrow().clone()).map(|d| Tools { dir: Some(d) });
        #[cfg(not(test))]
        Some(Tools { dir: None })
    }

    /// The tool exists (on PATH, or among the fakes).
    pub fn has(&self, name: &str) -> bool {
        match &self.dir {
            Some(d) => d.join(name).is_file(),
            None => crate::media::have(name),
        }
    }

    pub fn cmd(&self, name: &str) -> Command {
        match &self.dir {
            Some(d) => Command::new(d.join(name)),
            None => Command::new(name),
        }
    }
}

/// The mount table to read (a fake one under test).
pub fn mounts_table() -> PathBuf {
    Tools::current().and_then(|t| t.dir).map(|d| d.join("mounts")).unwrap_or_else(|| PathBuf::from("/proc/self/mounts"))
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Drive {
    /// Device node, e.g. /dev/sdb1.
    pub dev: String,
    /// The whole disk it lives on (itself for a partitionless disk), e.g. /dev/sdb.
    pub disk: String,
    pub label: String,
    pub fstype: String,
    pub size: u64,
    pub mounts: Vec<PathBuf>,
    /// Can be powered off: removable media or a hot-pluggable disk that holds no part of
    /// the running system.
    pub removable: bool,
}

/// Mount points that mean "this disk runs the system": never offer to eject it.
fn system_mount(p: &std::path::Path) -> bool {
    let s = p.to_string_lossy();
    // The system's own trees, and anything inside them; /home itself but not folders in
    // it (a stick mounted at ~/usb is still a stick).
    s == "/" || s == "[SWAP]" || s == "/home" || ["/boot", "/efi", "/usr", "/var", "/opt", "/srv", "/root", "/nix", "/gnu"].iter().any(|m| s == *m || s.starts_with(&format!("{m}/")))
}

/// `lsblk -P -b -o PATH,PKNAME,LABEL,FSTYPE,SIZE,RM,HOTPLUG,TYPE,MOUNTPOINTS` output.
pub fn parse_lsblk(text: &str) -> Vec<Drive> {
    struct Row {
        path: String,
        pk: String,
        label: String,
        fstype: String,
        size: u64,
        rm: bool,
        hotplug: bool,
        kind: String,
        mounts: Vec<PathBuf>,
    }
    let mut rows = vec![];
    for line in text.lines() {
        let kv = pairs(line);
        let get = |k: &str| kv.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.clone()).unwrap_or_default();
        let path = get("PATH");
        if path.is_empty() {
            continue;
        }
        let mounts = get("MOUNTPOINTS").split('\n').filter(|m| !m.is_empty()).map(PathBuf::from).collect();
        rows.push(Row { path, pk: get("PKNAME"), label: get("LABEL"), fstype: get("FSTYPE"), size: get("SIZE").parse().unwrap_or(0), rm: get("RM") == "1", hotplug: get("HOTPLUG") == "1", kind: get("TYPE"), mounts });
    }
    let disk_of = |r: &Row| if r.pk.is_empty() { r.path.clone() } else { format!("/dev/{}", r.pk) };
    let mut out = vec![];
    for r in &rows {
        if !matches!(r.kind.as_str(), "part" | "disk" | "crypt" | "loop" | "rom") {
            continue;
        }
        let disk = disk_of(r);
        // A disk carrying any system mount is never "removable", whatever it reports.
        let system = rows.iter().any(|o| (o.path == disk || disk_of(o) == disk) && o.mounts.iter().any(|m| system_mount(m)));
        let disk_row = rows.iter().find(|o| o.path == disk);
        let rm = r.rm || r.hotplug || disk_row.is_some_and(|d| d.rm || d.hotplug);
        out.push(Drive { dev: r.path.clone(), disk, label: r.label.clone(), fstype: r.fstype.clone(), size: r.size, mounts: r.mounts.clone(), removable: rm && !system });
    }
    out
}

/// KEY="value" pairs with lsblk's \xNN escapes decoded.
fn pairs(line: &str) -> Vec<(String, String)> {
    let mut out = vec![];
    let b = line.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && b[i] == b' ' {
            i += 1;
        }
        let ks = i;
        while i < b.len() && b[i] != b'=' {
            i += 1;
        }
        if i + 1 >= b.len() || b[i + 1] != b'"' {
            break;
        }
        let key = line[ks..i].to_string();
        i += 2;
        let mut val = vec![];
        while i < b.len() && b[i] != b'"' {
            if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' {
                let hex = |c: u8| (c as char).to_digit(16);
                if let (Some(h), Some(l)) = (hex(b[i + 2]), hex(b[i + 3])) {
                    val.push((h * 16 + l) as u8);
                    i += 4;
                    continue;
                }
            }
            val.push(b[i]);
            i += 1;
        }
        i += 1;
        out.push((key, String::from_utf8_lossy(&val).into_owned()));
    }
    out
}

impl Drive {
    pub fn mounted(&self) -> bool {
        !self.mounts.is_empty()
    }

    /// Name for lists: the label, else "500 GB Volume".
    pub fn title(&self) -> String {
        if !self.label.trim().is_empty() {
            self.label.clone()
        } else {
            format!("{} Volume", crate::fsx::fmt_size(self.size))
        }
    }

    /// An unmounted filesystem worth offering: not swap/encrypted/LVM containers, not
    /// tiny boot partitions (EFI, MSR) on internal disks.
    pub fn offer_mount(&self) -> bool {
        if self.mounted() || self.fstype.is_empty() {
            return false;
        }
        if matches!(self.fstype.as_str(), "swap" | "crypto_LUKS" | "LVM2_member" | "linux_raid_member" | "zfs_member" | "BitLocker") {
            return false;
        }
        self.removable || self.size >= 1 << 30
    }
}

pub fn list(t: &Tools) -> Vec<Drive> {
    let mut c = t.cmd("lsblk");
    c.args(["-P", "-b", "-o", "PATH,PKNAME,LABEL,FSTYPE,SIZE,RM,HOTPLUG,TYPE,MOUNTPOINTS"]);
    run(c, Duration::from_secs(5)).map(|o| parse_lsblk(&o)).unwrap_or_default()
}

/// Output of a command (stdout on success; the error text otherwise).
fn run(mut c: Command, timeout: Duration) -> Result<String, String> {
    let mut child = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    let (mut out, mut err) = (child.stdout.take(), child.stderr.take());
    let ro = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = out.as_mut() {
            let _ = std::io::Read::read_to_string(o, &mut s);
        }
        s
    });
    let re = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(e) = err.as_mut() {
            let _ = std::io::Read::read_to_string(e, &mut s);
        }
        s
    });
    let t0 = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break Some(st),
            Ok(None) if t0.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    let (o, e) = (ro.join().unwrap_or_default(), re.join().unwrap_or_default());
    match status {
        Some(st) if st.success() => Ok(o),
        Some(_) => Err(clean_error(&e, &o)),
        None => Err("Timed out".into()),
    }
}

/// The useful line of a udisks error ("…: Target is busy" rather than a D-Bus dump).
fn clean_error(err: &str, out: &str) -> String {
    let text = if err.trim().is_empty() { out } else { err };
    let line = text.lines().map(str::trim).filter(|l| !l.is_empty()).last().unwrap_or("Failed");
    let line = line.rsplit_once(": ").map(|(_, t)| t).unwrap_or(line);
    let mut s = line.trim().trim_end_matches('.').to_string();
    if let Some(c) = s.get(..1) {
        s = c.to_uppercase() + &s[1..];
    }
    s
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    Mount,
    Unmount,
    Eject,
}

/// Run an operation (may wait for a password prompt). Mount returns where it mounted.
pub fn perform(tools: &Tools, op: Op, d: &Drive, all: &[Drive]) -> Result<Option<PathBuf>, String> {
    // Password prompts can take a while; a stuck udisks shouldn't hang forever.
    let t = Duration::from_secs(120);
    let ctl = |args: &[&str]| {
        let mut c = tools.cmd("udisksctl");
        c.args(args);
        run(c, t)
    };
    match op {
        Op::Mount => {
            let out = ctl(&["mount", "-b", &d.dev])?;
            // "Mounted /dev/sdb1 at /run/media/user/LABEL"
            let at = out.split(" at ").nth(1).map(|s| PathBuf::from(s.trim().trim_end_matches('.')));
            Ok(at.or_else(|| list(tools).into_iter().find(|x| x.dev == d.dev).and_then(|x| x.mounts.first().cloned())))
        }
        Op::Unmount => ctl(&["unmount", "-b", &d.dev]).map(|_| None),
        Op::Eject => {
            if !d.removable {
                return Err("This drive can't be ejected".into());
            }
            for p in all.iter().filter(|x| x.disk == d.disk && x.mounted()) {
                ctl(&["unmount", "-b", &p.dev])?;
            }
            ctl(&["power-off", "-b", &d.disk]).map(|_| None)
        }
    }
}

impl Op {
    pub fn verb(self) -> &'static str {
        match self {
            Op::Mount => "mount",
            Op::Unmount => "unmount",
            Op::Eject => "eject",
        }
    }
}

thread_local! {
    /// Devices with an operation running (drawn with a spinner, not offered again).
    static BUSY: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

pub fn busy(dev: &str) -> bool {
    BUSY.with(|b| b.borrow().iter().any(|d| d == dev))
}

/// An operation running on a worker thread (udisks may wait on a password prompt).
pub struct Job {
    pub op: Op,
    pub drive: Drive,
    /// Panel to show the drive in once mounted (and whether in a new tab).
    pub open_in: Option<(u64, bool)>,
    pub rx: Receiver<Result<Option<PathBuf>, String>>,
}

impl Job {
    pub fn start(op: Op, drive: Drive, all: Vec<Drive>, open_in: Option<(u64, bool)>, ctx: &egui::Context) -> Result<Job, String> {
        let tools = Tools::current().ok_or("Drive tools are unavailable")?;
        if busy(&drive.dev) {
            return Err(format!("{} is busy", drive.title()));
        }
        BUSY.with(|b| b.borrow_mut().push(drive.dev.clone()));
        let (tx, rx) = std::sync::mpsc::channel();
        let (d, ctx) = (drive.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(perform(&tools, op, &d, &all));
            ctx.request_repaint();
        });
        Ok(Job { op, drive, open_in, rx })
    }

    /// The result once the operation finished (the device stops being busy).
    pub fn poll(&self) -> Option<Result<Option<PathBuf>, String>> {
        let r = match self.rx.try_recv() {
            Ok(r) => r,
            Err(std::sync::mpsc::TryRecvError::Empty) => return None,
            Err(_) => Err("Failed".into()),
        };
        BUSY.with(|b| b.borrow_mut().retain(|d| *d != self.drive.dev));
        Some(r)
    }
}

/// The drive mounted at `p`, if any.
pub fn at_mount<'a>(all: &'a [Drive], p: &Path) -> Option<&'a Drive> {
    all.iter().find(|d| d.mounts.iter().any(|m| m == p))
}

/// Can be unmounted from Maia: mounted somewhere that isn't part of the system.
pub fn can_unmount(d: &Drive) -> bool {
    d.mounted() && !d.mounts.iter().any(|m| system_mount(m))
}
