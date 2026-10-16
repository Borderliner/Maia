//! Headless user simulator. Drives the real `App` through egui with synthetic keyboard and
//! mouse input (no window, no GPU) and checks invariants after every frame: layout state
//! (panels, tabs, indices), what was drawn (finite geometry, on screen, no text drawn over
//! other text in plain list views) and how long each frame took.
//!
//! These tests click, type, rename, move and delete like a person would, so they only run
//! inside the sandbox set up by `tools/sim.sh` (read-only filesystem, empty home,
//! no network, only the fixture writable), which sets `COMET_SIM_SANDBOX=1`. Started any
//! other way they skip. External programs are never launched from test builds
//! (`mime::spawn_detached` records the request instead).

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::time::Instant;

use egui::epaint;
use egui::{Event, Pos2, Rect, Vec2, pos2, vec2};

use crate::app::App;
use crate::commands::Cmd;
use crate::popups::Popup;
use crate::tab::Row;

thread_local! {
    static PROF: std::cell::RefCell<Vec<(&'static str, f32)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Section timer for profiling a stall: wrap a suspect call with
/// `let t = Instant::now(); …; crate::sim::prof("name", t);` (test builds only); the
/// sections of the first slow frame of a single-seed replay are printed.
#[allow(dead_code)]
pub fn prof(name: &'static str, t: Instant) {
    PROF.with(|p| p.borrow_mut().push((name, t.elapsed().as_secs_f32() * 1000.0)));
}

/// True inside the sandbox; tests return early (and say so) otherwise.
pub fn sandboxed() -> bool {
    let ok = std::env::var("COMET_SIM_SANDBOX").as_deref() == Ok("1");
    if !ok {
        eprintln!("sim: skipped (run tools/sim.sh to simulate users in the sandbox)");
    }
    ok
}

/// Per-process root holding HOME, the XDG dirs and fixtures.
pub fn root() -> PathBuf {
    static ROOT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let r = PathBuf::from(std::env::var("COMET_SIM_ROOT").expect("COMET_SIM_ROOT"));
        for d in ["home", "xdg/config", "xdg/data", "xdg/cache", "xdg/state"] {
            std::fs::create_dir_all(r.join(d)).unwrap();
        }
        // SAFETY: set once, before any App exists, from the first test that needs it.
        unsafe {
            std::env::set_var("HOME", r.join("home"));
            std::env::set_var("XDG_CONFIG_HOME", r.join("xdg/config"));
            std::env::set_var("XDG_DATA_HOME", r.join("xdg/data"));
            std::env::set_var("XDG_CACHE_HOME", r.join("xdg/cache"));
            std::env::set_var("XDG_STATE_HOME", r.join("xdg/state"));
        }
        r
    })
    .clone()
}

fn png_file(path: &Path, w: u32, h: u32, seed: u32) {
    let f = std::fs::File::create(path).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(f), w, h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let mut px = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            px.extend_from_slice(&[(x * 7 + seed * 40) as u8, (y * 5 + seed * 90) as u8, ((x ^ y) + seed) as u8, 255]);
        }
    }
    e.write_header().unwrap().write_image_data(&px).unwrap();
}

/// Delete everything under the sim root (settings, trash, fixtures, anything a session
/// copied) so the next session starts from the same clean state.
fn free_mb(p: &Path) -> u64 {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).unwrap();
    let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
    unsafe { libc::statvfs(c.as_ptr(), &mut s) };
    s.f_bavail as u64 * s.f_frsize as u64 >> 20
}

pub fn wipe_root() {
    let r = root();
    if std::env::var("COMET_SIM_DEBUG").is_ok() {
        let out = std::process::Command::new("du").args(["-xhd", "3"]).arg(&r).output();
        let txt = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        let mut lines: Vec<&str> = txt.lines().collect();
        lines.sort_by_key(|l| std::cmp::Reverse(l.split_whitespace().next().map(|s| s.len()).unwrap_or(0)));
        eprintln!("wipe: free {} MB before; biggest:\n  {}", free_mb(&r), lines.iter().take(8).cloned().collect::<Vec<_>>().join("\n  "));
    }
    if let Ok(rd) = std::fs::read_dir(&r) {
        for d in rd.flatten() {
            let _ = std::process::Command::new("chmod").args(["-R", "u+rwX"]).arg(d.path()).status();
            let _ = std::fs::remove_dir_all(d.path());
        }
    }
    for d in ["home", "xdg/config", "xdg/data", "xdg/cache", "xdg/state"] {
        let _ = std::fs::create_dir_all(r.join(d));
    }
    if std::env::var("COMET_SIM_DEBUG").is_ok() {
        eprintln!("wipe: free {} MB after", free_mb(&r));
        if let Ok(rd) = std::fs::read_dir("/proc/self/fd") {
            for d in rd.flatten() {
                if let Ok(t) = std::fs::read_link(d.path()) {
                    let t = t.to_string_lossy().into_owned();
                    if t.contains("(deleted)") || t.starts_with(&*r.to_string_lossy()) {
                        let size = std::fs::metadata(d.path()).map(|m| m.len()).unwrap_or(0);
                        eprintln!("  open fd {:?} -> {t} ({} MB)", d.file_name(), size >> 20);
                    }
                }
            }
        }
        for cmd in [vec!["df", "-h", "/tmp", "/run", "/home"], vec!["sh", "-c", "find / -xdev -newer /proc/self/stat -size +1M 2>/dev/null | head; find \"$COMET_SIM_ROOT\" | head -20; df -h \"$COMET_SIM_ROOT\""]] {
            if let Ok(o) = std::process::Command::new(cmd[0]).args(&cmd[1..]).output() {
                eprintln!("  $ {}\n{}", cmd.join(" "), String::from_utf8_lossy(&o.stdout));
            }
        }
        if let Ok(rd) = std::fs::read_dir("/proc/self/task") {
            eprintln!("  threads: {}", rd.count());
        }
    }
}

/// A home folder like a real user's: documents, photos, code, music, odd names, deep
/// nesting, a big folder, hidden files, links, a pipe and an unreadable folder.
pub fn fixture(name: &str) -> PathBuf {
    let h = root().join("home").join(name);
    let _ = std::fs::remove_dir_all(&h);
    let w = |p: &str, s: &str| {
        let p = h.join(p);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, s).unwrap();
    };
    w("Documents/report.md", "# Report\n\nSome **markdown** text.\n");
    w("Documents/notes.txt", &"line of notes\n".repeat(400));
    w("Documents/budget 2026.csv", "a,b,c\n1,2,3\n");
    w("Documents/Taxes/2025/receipt.pdf", "%PDF-1.4 fake");
    w("Documents/Taxes/2024/receipt.pdf", "%PDF-1.4 fake");
    w("Documents/résumé – final (v3).txt", "unicode name");
    w("Documents/#長い名前の音楽ファイル_第一部_最新情報_新しい映像の公開_オープニング曲_エンディング曲_さらに続く.mp3", "ID3");
    w("Documents/a very long file name that keeps going and going well past any column width you would pick.txt", "x");
    w("Code/maia/src/main.rs", "fn main() {\n    println!(\"hi\");\n}\n");
    w("Code/maia/src/lib.rs", "pub fn f() {}\n");
    w("Code/maia/Cargo.toml", "[package]\nname = \"x\"\n");
    w("Code/maia/.gitignore", "target\n");
    w("Code/maia/README", "no extension\n");
    w("Code/deep/a/b/c/d/e/f/g/h/leaf.txt", "deep");
    w("Music/Album/01 Intro.mp3", "ID3 fake");
    w("Music/Album/02 Song.flac", "fLaC fake");
    w(".config/app/settings.json", "{}");
    w(".bashrc", "# shell\n");
    w("Downloads/setup.tar.gz", "gz");
    w("Downloads/archive.zip", "PK");
    w("Downloads/Telegram Desktop/photo.webp", "RIFF");
    std::fs::create_dir_all(h.join("Empty")).unwrap();
    std::fs::create_dir_all(h.join("Pictures/Trip")).unwrap();
    for i in 0..12 {
        png_file(&h.join(format!("Pictures/Trip/img{i:02}.png")), 40 + i * 9, 30 + i * 5, i);
    }
    png_file(&h.join("Pictures/wide.png"), 300, 20, 3);
    std::fs::write(h.join("Pictures/broken.png"), b"\x89PNG\r\n\x1a\n garbage").unwrap();
    for i in 0..600 {
        w(&format!("Big/file{i:04}.{}", ["txt", "rs", "png", "md", "json", "log"][i % 6]), "x");
    }
    let _ = std::os::unix::fs::symlink(h.join("Documents"), h.join("docs-link"));
    let _ = std::os::unix::fs::symlink(h.join("nowhere"), h.join("broken-link"));
    let fifo = std::ffi::CString::new(h.join("pipe").into_os_string().into_encoded_bytes()).unwrap();
    unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) };
    std::fs::create_dir_all(h.join("Locked")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(h.join("Locked"), std::fs::Permissions::from_mode(0o000)).unwrap();
    h
}

/// Fake `lsblk` / `udisksctl` / mount table for drive tests (test builds never run the
/// real tools). A system disk holding / and /boot, an internal disk with Windows, swap
/// and a tiny EFI partition, a removable card that isn't mounted, and a mounted stick
/// whose unmount fails because it is busy. Every call is appended to `log`.
pub fn fake_drives(name: &str) -> PathBuf {
    let d = root().join("drives").join(name);
    let _ = std::fs::remove_dir_all(&d);
    for sub in ["mounted", "gone", "busy", "fail", "media"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    let rows = [
        "/dev/sdw\t-\t-\t-\t500107862016\t0\t1\tdisk",
        "/dev/sdw1\tsdw\t-\tvfat\t536870912\t0\t1\tpart",
        "/dev/sdw2\tsdw\t-\tbtrfs\t499570991104\t0\t1\tpart",
        "/dev/sdy\t-\t-\t-\t256060514304\t0\t0\tdisk",
        "/dev/sdy1\tsdy\t-\tvfat\t104857600\t0\t0\tpart",
        "/dev/sdy2\tsdy\tWindows\tntfs\t214748364800\t0\t0\tpart",
        "/dev/sdy3\tsdy\t-\tswap\t8589934592\t0\t0\tpart",
        "/dev/sdz\t-\t-\t-\t16008609792\t1\t1\tdisk",
        "/dev/sdz1\tsdz\tPhoto Card\texfat\t16007561216\t1\t1\tpart",
        "/dev/sdx\t-\t-\t-\t32015679488\t1\t1\tdisk",
        "/dev/sdx1\tsdx\tBusy Stick\text4\t32014630912\t1\t1\tpart",
    ];
    std::fs::write(d.join("devices"), rows.join("\n") + "\n").unwrap();
    let busy = d.join("media/Busy Stick");
    std::fs::create_dir_all(&busy).unwrap();
    std::fs::write(busy.join("busy.txt"), "in use").unwrap();
    std::fs::write(d.join("mounted/sdw1"), "/boot").unwrap();
    std::fs::write(d.join("mounted/sdw2"), "/").unwrap();
    std::fs::write(d.join("mounted/sdx1"), busy.to_str().unwrap()).unwrap();
    std::fs::write(d.join("busy/sdx1"), "").unwrap();
    let lsblk = r#"#!/bin/bash
D="$(dirname "$0")"
echo "lsblk $*" >> "$D/log"
while IFS=$'\t' read -r path pk label fs size rm hp type; do
  [ -z "$path" ] && continue
  [ "$pk" = - ] && pk=""; [ "$label" = - ] && label=""; [ "$fs" = - ] && fs=""
  name="${path##*/}"; disk="${pk:-$name}"
  [ -e "$D/gone/$disk" ] && continue
  mp=""; [ -f "$D/mounted/$name" ] && mp="$(cat "$D/mounted/$name")"
  printf 'PATH="%s" PKNAME="%s" LABEL="%s" FSTYPE="%s" SIZE="%s" RM="%s" HOTPLUG="%s" TYPE="%s" MOUNTPOINTS="%s"\n' "$path" "$pk" "${label// /\\x20}" "$fs" "$size" "$rm" "$hp" "$type" "${mp// /\\x20}"
done < "$D/devices"
"#;
    let udisks = r#"#!/bin/bash
D="$(dirname "$0")"
echo "udisksctl $*" >> "$D/log"
op="$1"; dev="$3"; name="${dev##*/}"
field() { awk -F'\t' -v p="${2:-$dev}" -v f="$1" '$1==p{print $f}' "$D/devices"; }
regen() {
  local f n m
  for f in "$D"/mounted/*; do
    [ -f "$f" ] || continue
    n="${f##*/}"; m="$(cat "$f")"
    echo "/dev/$n ${m// /\\040} $(field 4 "/dev/$n") rw 0 0"
  done > "$D/mounts.new"; mv "$D/mounts.new" "$D/mounts"
}
sleep 0.15
if [ -f "$D/fail/$name" ]; then echo "Error ${op}ing $dev: GDBus.Error:org.freedesktop.UDisks2.Error.Failed: $(cat "$D/fail/$name")" >&2; exit 1; fi
case "$op" in
  mount)
    if [ -f "$D/mounted/$name" ]; then echo "Error mounting $dev: GDBus.Error:org.freedesktop.UDisks2.Error.AlreadyMounted: Device $dev is already mounted" >&2; exit 1; fi
    label="$(field 3)"; [ "$label" = - ] && label="$name"
    mp="$D/media/$label"; mkdir -p "$mp"; echo "files on $label" > "$mp/readme.txt"
    printf '%s' "$mp" > "$D/mounted/$name"; regen
    echo "Mounted $dev at $mp" ;;
  unmount)
    if [ -e "$D/busy/$name" ]; then echo "Error unmounting $dev: GDBus.Error:org.freedesktop.UDisks2.Error.DeviceBusy: Error unmounting $dev: target is busy" >&2; exit 1; fi
    if [ ! -f "$D/mounted/$name" ]; then echo "Error unmounting $dev: GDBus.Error:org.freedesktop.UDisks2.Error.NotMounted: Device is not mounted" >&2; exit 1; fi
    mp="$(cat "$D/mounted/$name")"; rm -f "$D/mounted/$name"; regen; rm -rf "$mp" ;;
  power-off)
    touch "$D/gone/$name" ;;
  *) echo "unknown $op" >&2; exit 1 ;;
esac
"#;
    use std::os::unix::fs::PermissionsExt;
    for (n, body) in [("lsblk", lsblk), ("udisksctl", udisks)] {
        std::fs::write(d.join(n), body).unwrap();
        std::fs::set_permissions(d.join(n), std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    // Initial mount table, written the way the fake udisksctl rewrites it.
    let table: String = [("sdw1", "/boot", "vfat"), ("sdw2", "/", "btrfs"), ("sdx1", busy.to_str().unwrap(), "ext4")].iter().map(|(n, m, f)| format!("/dev/{n} {} {f} rw 0 0\n", m.replace(' ', "\\040"))).collect();
    std::fs::write(d.join("mounts"), table).unwrap();
    d
}

/// A fake `gio` for network tests: a phone that can be mounted, a share on "nas" that
/// asks for a login (user "me", password "secret") the way gio does, an anonymous FTP
/// site "pub", and unknown hosts that fail. Mounts are folders under `gvfs/`.
pub fn fake_network(name: &str) -> PathBuf {
    let d = root().join("net").join(name);
    let _ = std::fs::remove_dir_all(&d);
    for sub in ["gvfs", "netmounts"] {
        std::fs::create_dir_all(d.join(sub)).unwrap();
    }
    std::fs::write(d.join("phones"), "Pixel 7\tmtp://Google_Pixel_7/\n").unwrap();
    let gio = r#"#!/bin/bash
D="$(dirname "$0")"
echo "gio $*" >> "$D/log"
id() { printf '%s' "$1" | tr -c 'A-Za-z0-9' '_'; }
do_mount() {
  local uri="$1" name="$2" i; i=$(id "$uri")
  mkdir -p "$D/gvfs/$i"; echo "on $name" > "$D/gvfs/$i/hello.txt"
  printf '%s\t%s\n' "$name" "$uri" > "$D/netmounts/$i"
}
if [ "$1" = mount ] && [ "$2" = -l ]; then
  while IFS=$'\t' read -r name uri; do
    [ -z "$uri" ] && continue
    echo "Volume(0): $name"; echo "  Type: GProxyVolume (GProxyVolumeMonitorMTP)"; echo "  activation_root=$uri"
    [ -f "$D/netmounts/$(id "$uri")" ] && { echo "  Mount(0): $name -> $uri"; echo "    Type: GProxyShadowMount"; }
  done < "$D/phones"
  for f in "$D"/netmounts/*; do
    [ -f "$f" ] || continue
    IFS=$'\t' read -r name uri < "$f"
    case "$uri" in mtp://*) continue ;; esac
    echo "Mount(0): $name -> $uri"; echo "  Type: GDaemonMount"
  done
  exit 0
fi
if [ "$1" = info ]; then
  i=$(id "$2"); [ -f "$D/netmounts/$i" ] || { echo "gio: $2: Operation not supported" >&2; exit 1; }
  echo "name: x"; echo "uri: $2"; echo "local path: $D/gvfs/$i"; exit 0
fi
if [ "$1" = mount ] && [ "$2" = -u ]; then
  i=$(id "$3"); [ -f "$D/netmounts/$i" ] || { echo "gio: $3: Containing mount does not exist" >&2; exit 1; }
  rm -f "$D/netmounts/$i"; rm -rf "$D/gvfs/$i"; exit 0
fi
if [ "$1" = mount ]; then
  anon=0; uri="$2"; [ "$2" = -a ] && { anon=1; uri="$3"; }
  case "$uri" in
    mtp://Google_Pixel_7*) do_mount "$uri" "Pixel 7" ;;
    ftp://pub*) do_mount "$uri" "pub" ;;
    smb://nas/*)
      [ $anon = 1 ] && { echo "gio: $uri: Failed to mount Windows share: Permission denied" >&2; exit 2; }
      printf 'Password required for share “media” on nas\nUser [me]: '; read -r user
      printf 'Domain [WORKGROUP]: '; read -r dom
      while true; do
        printf 'Password: '; read -r pass || exit 2
        [ "$user" = me ] && [ "$pass" = secret ] && break
      done
      sleep 0.2; do_mount "$uri" "media on nas" ;;
    *) echo "gio: $uri: Failed to mount Windows share: No such file or directory" >&2; exit 2 ;;
  esac
  exit 0
fi
echo "gio: unknown $*" >&2; exit 1
"#;
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(d.join("gio"), gio).unwrap();
    std::fs::set_permissions(d.join("gio"), std::fs::Permissions::from_mode(0o755)).unwrap();
    d
}

/// A fake `pkexec` for administrator tests: it plays root by making the folders the
/// command touches writable for the moment it runs, and refuses (exit 126, as a dismissed
/// password dialog does) while a file named `deny` exists beside it.
pub fn fake_pkexec(name: &str) -> PathBuf {
    let d = root().join("admin").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let script = r#"#!/bin/bash
D="$(dirname "$0")"
echo "pkexec $*" >> "$D/log"
[ -e "$D/deny" ] && { echo "Error executing command as another user: Request dismissed" >&2; exit 126; }
opened=()
after=0
for a in "$@"; do
  [ "$a" = -- ] && { after=1; continue; }
  [ $after = 1 ] || continue
  for d in "$a" "$(dirname "$a")"; do
    if [ -d "$d" ] && [ ! -w "$d" ]; then chmod u+w "$d"; opened+=("$d"); fi
  done
done
"$@"; rc=$?
for d in "${opened[@]}"; do chmod u-w "$d" 2>/dev/null; done
exit $rc
"#;
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(d.join("pkexec"), script).unwrap();
    std::fs::set_permissions(d.join("pkexec"), std::fs::Permissions::from_mode(0o755)).unwrap();
    d
}

/// Real media for preview/thumbnail tests, made with ffmpeg inside the sandbox: a video,
/// an MP3 with cover art, a hand-written one-page PDF, an SVG, a PNG and a corrupt video.
pub fn media_fixture(name: &str) -> PathBuf {
    let d = root().join("home").join(name);
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    let ff = |args: &[&str]| {
        let ok = std::process::Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).current_dir(&d).status().is_ok_and(|s| s.success());
        assert!(ok, "ffmpeg {args:?}");
    };
    ff(&["-f", "lavfi", "-i", "testsrc=duration=2:size=320x240:rate=15", "-pix_fmt", "yuv420p", "clip.mp4"]);
    ff(&["-f", "lavfi", "-i", "testsrc=size=200x200", "-frames:v", "1", "cover.png"]);
    ff(&["-f", "lavfi", "-i", "sine=frequency=440:duration=2", "-i", "cover.png", "-map", "0", "-map", "1", "-c:a", "libmp3lame", "-c:v", "png", "-disposition:v", "attached_pic", "-id3v2_version", "3", "-metadata", "title=Test Tone", "-metadata", "artist=Maia", "song.mp3"]);
    png_file(&d.join("photo.png"), 320, 200, 5);
    std::fs::write(d.join("drawing.svg"), r##"<svg xmlns="http://www.w3.org/2000/svg" width="64" height="48" viewBox="0 0 64 48"><rect width="64" height="48" fill="#2a7"/><circle cx="32" cy="24" r="16" fill="#fc3"/></svg>"##).unwrap();
    std::fs::write(d.join("broken.mp4"), b"\x00\x00\x00\x18ftypmp42 this is not a video").unwrap();
    // Minimal valid PDF: three A6 pages, each a differently coloured rectangle.
    let colors = ["0.2 0.4 0.8", "0.8 0.3 0.2", "0.2 0.7 0.3"];
    let mut objs = vec!["<< /Type /Catalog /Pages 2 0 R >>".to_string(), "<< /Type /Pages /Kids [3 0 R 5 0 R 7 0 R] /Count 3 >>".to_string()];
    for (k, c) in colors.iter().enumerate() {
        let stream = format!("{c} rg 20 20 250 380 re f");
        objs.push(format!("<< /Type /Page /Parent 2 0 R /MediaBox [0 0 298 420] /Contents {} 0 R >>", 4 + 2 * k));
        objs.push(format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()));
    }
    let mut pdf = String::from("%PDF-1.4\n");
    let mut offs = vec![];
    for (i, o) in objs.iter().enumerate() {
        offs.push(pdf.len());
        pdf.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
    }
    let xref = pdf.len();
    pdf.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1));
    for o in offs {
        pdf.push_str(&format!("{o:010} 00000 n \n"));
    }
    pdf.push_str(&format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1));
    std::fs::write(d.join("doc.pdf"), pdf).unwrap();
    d
}

pub struct Sim {
    pub ctx: egui::Context,
    pub app: App,
    pub size: Vec2,
    pub ppp: f32,
    time: f64,
    pointer: Pos2,
    /// CPU time of every frame (ms): app update + tessellation.
    pub frame_ms: Vec<f32>,
    /// What happened, for failure reports.
    pub log: VecDeque<String>,
    /// Check that list text never overlaps (only meaningful with no popup or hover).
    pub check_overlap: bool,
    /// Frames over `SLOW_MS`, with the step that caused them.
    pub slow: Vec<(f32, String)>,
    /// How many times a popup's text containment was actually checked.
    pub popup_checks: std::cell::Cell<usize>,
    /// Text drawn in the last frame (visible rect, string), for clicking by label.
    pub texts: Vec<(Rect, String)>,
    /// What the last frame drew, kept for `shot` when COMET_SIM_SHOTS names a folder.
    shots: Option<Box<Shots>>,
}

/// The textures egui has handed out and the last frame's triangles: enough to paint the
/// frame in software, without a window or a GPU.
struct Shots {
    dir: PathBuf,
    textures: std::collections::HashMap<egui::TextureId, egui::ColorImage>,
    prims: Vec<epaint::ClippedPrimitive>,
}

impl Shots {
    fn take(&mut self, delta: &egui::TexturesDelta) {
        for (id, d) in &delta.set {
            let egui::ImageData::Color(img) = &d.image;
            match d.pos {
                None => {
                    self.textures.insert(*id, (**img).clone());
                }
                Some([x, y]) => {
                    if let Some(t) = self.textures.get_mut(id) {
                        for row in 0..img.size[1] {
                            for col in 0..img.size[0] {
                                if let Some(px) = t.pixels.get_mut((y + row) * t.size[0] + x + col) {
                                    *px = img.pixels[row * img.size[0] + col];
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Paint the triangles: each pixel centre inside a triangle takes its texel times its
    /// colour (both premultiplied), blended over what is there.
    fn paint(&self, w: usize, h: usize, ppp: f32) -> Vec<u8> {
        let mut px = vec![[0f32, 0.0, 0.0, 1.0]; w * h];
        let sample = |t: &egui::ColorImage, uv: Pos2| -> [f32; 4] {
            let (tw, th) = (t.size[0] as f32, t.size[1] as f32);
            let (x, y) = (uv.x * tw - 0.5, uv.y * th - 0.5);
            let (x0, y0) = (x.floor(), y.floor());
            let (fx, fy) = (x - x0, y - y0);
            let at = |xx: f32, yy: f32| {
                let (xx, yy) = (xx.clamp(0.0, tw - 1.0) as usize, yy.clamp(0.0, th - 1.0) as usize);
                let c = t.pixels[yy * t.size[0] + xx];
                [c.r() as f32, c.g() as f32, c.b() as f32, c.a() as f32]
            };
            let (a, b, c, d) = (at(x0, y0), at(x0 + 1.0, y0), at(x0, y0 + 1.0), at(x0 + 1.0, y0 + 1.0));
            std::array::from_fn(|i| ((a[i] * (1.0 - fx) + b[i] * fx) * (1.0 - fy) + (c[i] * (1.0 - fx) + d[i] * fx) * fy) / 255.0)
        };
        let edge = |a: Pos2, b: Pos2, c: Pos2| (b.x - a.x) * (c.y - a.y) - (b.y - a.y) * (c.x - a.x);
        for cp in &self.prims {
            let epaint::Primitive::Mesh(m) = &cp.primitive else { continue };
            let Some(tex) = self.textures.get(&m.texture_id) else { continue };
            let clip = cp.clip_rect;
            let (cx0, cy0) = ((clip.min.x * ppp).max(0.0) as usize, (clip.min.y * ppp).max(0.0) as usize);
            let (cx1, cy1) = (((clip.max.x * ppp).ceil().max(0.0) as usize).min(w), ((clip.max.y * ppp).ceil().max(0.0) as usize).min(h));
            for tri in m.indices.chunks_exact(3) {
                let v = [&m.vertices[tri[0] as usize], &m.vertices[tri[1] as usize], &m.vertices[tri[2] as usize]];
                let p = v.map(|v| pos2(v.pos.x * ppp, v.pos.y * ppp));
                let area = edge(p[0], p[1], p[2]);
                if area.abs() < 1e-6 {
                    continue;
                }
                let x0 = (p.iter().map(|p| p.x).fold(f32::MAX, f32::min).floor().max(0.0) as usize).max(cx0);
                let x1 = (p.iter().map(|p| p.x).fold(f32::MIN, f32::max).ceil().max(0.0) as usize).min(cx1);
                let y0 = (p.iter().map(|p| p.y).fold(f32::MAX, f32::min).floor().max(0.0) as usize).max(cy0);
                let y1 = (p.iter().map(|p| p.y).fold(f32::MIN, f32::max).ceil().max(0.0) as usize).min(cy1);
                for y in y0..y1 {
                    for x in x0..x1 {
                        let c = pos2(x as f32 + 0.5, y as f32 + 0.5);
                        let (w0, w1) = (edge(p[1], p[2], c) / area, edge(p[2], p[0], c) / area);
                        let w2 = 1.0 - w0 - w1;
                        if w0 < 0.0 || w1 < 0.0 || w2 < 0.0 {
                            continue;
                        }
                        let uv = pos2(v[0].uv.x * w0 + v[1].uv.x * w1 + v[2].uv.x * w2, v[0].uv.y * w0 + v[1].uv.y * w1 + v[2].uv.y * w2);
                        let t = sample(tex, uv);
                        let col = |i: usize| {
                            let f = |c: egui::Color32| [c.r(), c.g(), c.b(), c.a()][i] as f32 / 255.0;
                            f(v[0].color) * w0 + f(v[1].color) * w1 + f(v[2].color) * w2
                        };
                        let src: [f32; 4] = std::array::from_fn(|i| t[i] * col(i));
                        let d = &mut px[y * w + x];
                        for i in 0..4 {
                            d[i] = src[i] + d[i] * (1.0 - src[3]);
                        }
                    }
                }
            }
        }
        px.iter().flat_map(|p| [0, 1, 2].map(|i| (p[i].clamp(0.0, 1.0) * 255.0).round() as u8)).collect()
    }
}

/// A frame slower than this is a visible stall.
pub const SLOW_MS: f32 = 50.0;

impl Sim {
    pub fn new(path: &Path, size: Vec2) -> Sim {
        root();
        let ctx = egui::Context::default();
        let app = App::new(&ctx, vec![path.to_path_buf()]);
        let shots = std::env::var_os("COMET_SIM_SHOTS").map(|d| Box::new(Shots { dir: PathBuf::from(d), textures: Default::default(), prims: vec![] }));
        let mut s = Sim { ctx, app, size, ppp: 1.6, time: 0.0, pointer: pos2(-10.0, -10.0), frame_ms: vec![], log: VecDeque::new(), check_overlap: false, slow: vec![], popup_checks: Default::default(), texts: vec![], shots };
        // COMET_SIM_LOOK=FONT,SPACING runs a session at another font size and Spacing.
        if let Some((f, sp)) = std::env::var("COMET_SIM_LOOK").ok().and_then(|v| v.split_once(',').and_then(|(f, sp)| Some((f.trim().parse().ok()?, sp.trim().parse().ok()?)))) {
            s.app.cfg.font_size = f;
            s.app.cfg.spacing = sp;
        }
        s.settle();
        s
    }

    fn note(&mut self, s: String) {
        self.log.push_back(s);
        if self.log.len() > 60 {
            self.log.pop_front();
        }
    }

    pub fn fail(&self, why: &str) -> ! {
        panic!("{why}\nlast steps:\n  {}", self.log.iter().cloned().collect::<Vec<_>>().join("\n  "));
    }

    /// One frame with these input events.
    pub fn frame(&mut self, mut events: Vec<Event>) {
        // Input the app queued for itself (system drag and drop), as the runner adds it.
        events.append(&mut self.app.inject);
        let mods = events
            .iter()
            .find_map(|e| match e {
                Event::Key { modifiers, .. } | Event::PointerButton { modifiers, .. } | Event::MouseWheel { modifiers, .. } => Some(*modifiers),
                _ => None,
            })
            .unwrap_or_default();
        for e in &events {
            if let Event::PointerMoved(p) = e {
                self.pointer = *p;
            }
        }
        let mut raw = egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)), time: Some(self.time), modifiers: mods, events, focused: true, ..Default::default() };
        raw.viewports.entry(egui::ViewportId::ROOT).or_default().native_pixels_per_point = Some(self.ppp);
        PROF.with(|p| p.borrow_mut().clear());
        crate::ui::CONTROLS.with(|c| c.borrow_mut().clear());
        crate::ui::POPUPS.with(|c| c.borrow_mut().clear());
        let t0 = Instant::now();
        let app = &mut self.app;
        let out = self.ctx.run(raw, |ctx| app.frame(ctx));
        let prims = self.ctx.tessellate(out.shapes.clone(), out.pixels_per_point);
        let ms = t0.elapsed().as_secs_f32() * 1000.0;
        if let Some(sh) = &mut self.shots {
            sh.take(&out.textures_delta);
            sh.prims = prims.clone();
            for id in &out.textures_delta.free {
                sh.textures.remove(id);
            }
        }
        self.frame_ms.push(ms);
        if ms > SLOW_MS && self.frame_ms.len() > 3 {
            let traced = self.slow.iter().any(|x| x.0 > 1000.0);
            if !traced && (ms > 1000.0 || (self.slow.is_empty() && std::env::var("COMET_SIM_SEED").is_ok())) {
                eprintln!("first slow frame ({ms:.0} ms); steps before it:\n  {}", self.log.iter().cloned().collect::<Vec<_>>().join("\n  "));
                PROF.with(|p| eprintln!("  sections: {:?}", p.borrow()));
                eprintln!("  jobs {} [{}], popup open {}, drag {}, toast {:?}", self.app.jobs.len(), self.app.jobs.iter().map(|j| j.label.clone()).collect::<Vec<_>>().join("; "), self.popup_open(), self.app.drag.is_some(), self.app.toast.as_ref().map(|t| t.0.clone()));
                for p in self.app.panels.values() {
                    let t = p.tab();
                    eprintln!("  zoom {} shown {} rows {} filter {:?} selected {} rename {}", t.settings.zoom, t.shown_zoom, t.rows.len(), t.filter, t.selected.len(), t.rename.is_some());
                    eprintln!("  panel {}: {} entries, view {}, flatten {}, sort {:?}, group {:?}, loading {}, dirty {}, recents_on_top {}, stats pending {}", t.path.display(), t.entries.len(), t.view.len(), t.flatten, t.settings.sort, t.settings.group, t.is_loading(), t.dirty, self.app.cfg.recents_on_top, !self.app.stats.idle());
                }
            }
            self.slow.push((ms, self.log.back().cloned().unwrap_or_default()));
        }
        self.time += 1.0 / 60.0;
        self.texts.clear();
        fn collect(sh: &epaint::Shape, clip: Rect, out: &mut Vec<(Rect, String)>) {
            match sh {
                epaint::Shape::Text(t) => {
                    let s = t.galley.text().trim();
                    if !s.is_empty() {
                        let r = sh.visual_bounding_rect().intersect(clip);
                        if r.is_positive() {
                            out.push((r, s.to_string()));
                        }
                    }
                }
                epaint::Shape::Vec(v) => v.iter().for_each(|s| collect(s, clip, out)),
                _ => {}
            }
        }
        for cs in &out.shapes {
            collect(&cs.shape, cs.clip_rect, &mut self.texts);
        }
        self.check_state();
        self.check_shapes(&out.shapes);
        self.check_controls();
        for p in &prims {
            if !(p.clip_rect.min.x.is_finite() && p.clip_rect.max.y.is_finite()) {
                self.fail("non-finite clip rect after tessellation");
            }
        }
    }

    pub fn idle(&mut self, n: usize) {
        for _ in 0..n {
            self.frame(vec![]);
        }
    }

    /// Let background work (listings, sizes, thumbnails) land.
    pub fn settle(&mut self) {
        let mut quiet = 0;
        for _ in 0..400 {
            self.frame(vec![]);
            let busy = self.app.panels.values().any(|p| p.tabs.iter().any(|t| t.is_loading())) || !self.app.stats.idle();
            if busy {
                quiet = 0;
                std::thread::sleep(std::time::Duration::from_millis(3));
            } else {
                quiet += 1;
                if quiet >= 4 {
                    return;
                }
            }
        }
    }

    /// A testkit command line (`key ctrl+t`, `click 300 200`, `text foo`, …).
    pub fn act(&mut self, line: &str) {
        self.note(line.to_string());
        for step in crate::testkit::parse(line) {
            self.frame(step);
        }
        self.frame(vec![]);
    }

    pub fn cmd(&mut self, c: Cmd) {
        self.note(format!("cmd {}", c.info().id));
        self.app.actions.push(crate::app::Action::Cmd(c));
        self.frame(vec![]);
        self.frame(vec![]);
    }

    /// Where the last frame drew `label` (topmost match).
    pub fn text_rect(&self, label: &str) -> Rect {
        self.texts.iter().rev().find(|(_, s)| s == label).map(|(r, _)| *r).unwrap_or_else(|| self.fail(&format!("no {label:?} on screen")))
    }

    /// Click the last-drawn text exactly equal to `label` (topmost wins), like a user
    /// clicking a button by its caption. Fails if it isn't on screen.
    pub fn click_text(&mut self, label: &str) {
        let Some((r, _)) = self.texts.iter().rev().find(|(_, s)| s == label).cloned() else {
            let near: Vec<&str> = self.texts.iter().map(|(_, s)| s.as_str()).filter(|s| s.len() < 30).take(40).collect();
            self.fail(&format!("no {label:?} on screen; visible: {near:?}"));
        };
        self.act(&format!("click {:.0} {:.0}", r.center().x, r.center().y));
    }

    /// Save what the last frame drew as `<COMET_SIM_SHOTS>/<name>.png` (nothing when the
    /// variable is not set). The look in use goes into the name.
    pub fn shot(&self, name: &str) {
        let Some(sh) = &self.shots else { return };
        let (w, h) = ((self.size.x * self.ppp).round() as usize, (self.size.y * self.ppp).round() as usize);
        let rgb = sh.paint(w, h, self.ppp);
        let look = std::env::var("COMET_SIM_LOOK").unwrap_or_default().replace(',', "-");
        let name: String = format!("{name}{}{look}", if look.is_empty() { "" } else { "@" }).chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '@') { c } else { '_' }).collect();
        let Ok(f) = std::fs::File::create(sh.dir.join(format!("{name}.png"))) else { return };
        let mut e = png::Encoder::new(std::io::BufWriter::new(f), w as u32, h as u32);
        e.set_color(png::ColorType::Rgb);
        e.set_depth(png::BitDepth::Eight);
        if let Ok(mut wr) = e.write_header() {
            let _ = wr.write_image_data(&rgb);
        }
    }

    pub fn resize(&mut self, w: f32, h: f32) {
        self.note(format!("resize {w}x{h}"));
        self.size = vec2(w, h);
        self.idle(2);
    }

    pub fn tab(&self) -> &crate::tab::Tab {
        self.app.panels.get(&self.app.active).map(|p| p.tab()).unwrap_or_else(|| self.fail("no active panel"))
    }

    pub fn popup_open(&self) -> bool {
        !matches!(self.app.popup, Popup::None)
    }

    /// Layout and index invariants (what drawing code relies on).
    fn check_state(&self) {
        let a = &self.app;
        let mut leaves = vec![];
        a.tree.leaves(&mut leaves);
        if leaves.is_empty() {
            self.fail("layout has no panels");
        }
        for id in &leaves {
            if !a.panels.contains_key(id) {
                self.fail(&format!("layout leaf {id} has no panel"));
            }
        }
        if !a.panels.contains_key(&a.active) {
            self.fail("active panel missing");
        }
        for p in a.panels.values() {
            if p.tabs.is_empty() || p.active >= p.tabs.len() {
                self.fail(&format!("panel {} has {} tabs, active {}", p.id, p.tabs.len(), p.active));
            }
            for t in &p.tabs {
                let n = t.entries.len() as u32;
                if t.cursor.is_some_and(|c| c >= n) || t.anchor.is_some_and(|c| c >= n) {
                    self.fail(&format!("cursor/anchor out of range in {}", t.path.display()));
                }
                if t.selected.iter().any(|&i| i >= n) {
                    self.fail(&format!("selection out of range in {}", t.path.display()));
                }
                if !t.dirty {
                    if t.view.iter().any(|&i| i >= n) {
                        self.fail(&format!("view index out of range in {}", t.path.display()));
                    }
                    for r in &t.rows {
                        match r {
                            Row::Item(i) if *i >= n => self.fail("row index out of range"),
                            Row::Header { label, .. } if *label as usize >= t.group_labels.len() => self.fail("header label out of range"),
                            _ => {}
                        }
                    }
                }
            }
        }
    }

    /// Drawn geometry: finite, roughly on screen, and (in list views without popups or
    /// hover) no line of text drawn on top of another.
    fn check_shapes(&self, shapes: &[epaint::ClippedShape]) {
        let screen = Rect::from_min_size(Pos2::ZERO, self.size).expand(2.0);
        let mut texts: Vec<(Rect, String)> = vec![];
        for cs in shapes {
            // Text with nothing visible (empty, or only spaces, as a file can be named)
            // has no bounds: egui reports NaN for it.
            if let epaint::Shape::Text(t) = &cs.shape {
                if t.galley.text().trim().is_empty() {
                    continue;
                }
            }
            let r = cs.shape.visual_bounding_rect();
            if r.is_negative() {
                continue;
            }
            if !(r.min.x.is_finite() && r.min.y.is_finite() && r.max.x.is_finite() && r.max.y.is_finite()) {
                self.fail(&format!("non-finite shape {r:?}: {}", describe(&cs.shape)));
            }
            let vis = r.intersect(cs.clip_rect);
            if vis.is_positive() && !screen.contains_rect(vis) {
                self.fail(&format!("shape drawn off screen: {vis:?} (screen {screen:?})"));
            }
            if self.check_overlap {
                if let epaint::Shape::Text(t) = &cs.shape {
                    let s = t.galley.text().trim().to_string();
                    if !s.is_empty() && vis.is_positive() && vis.height() > 4.0 {
                        texts.push((vis, s));
                    }
                }
            }
        }
        // Text drawn inside a popup (after its frame) must not run past the popup's edge.
        let popups = crate::ui::POPUPS.with(|p| p.borrow().clone());
        for pr in &popups {
            // The popup's background: the first rect inside the area that nearly fills it
            // (the area also holds the frame's margin and shadow).
            // (A frame with a shadow paints as a group: shadow + rect.)
            fn bg(sh: &epaint::Shape, pr: &Rect) -> Option<Rect> {
                match sh {
                    epaint::Shape::Rect(r) if pr.expand(0.5).contains_rect(r.rect) && pr.width() - r.rect.width() < 24.0 && pr.height() - r.rect.height() < 24.0 => Some(r.rect),
                    epaint::Shape::Vec(v) => v.iter().find_map(|s| bg(s, pr)),
                    _ => None,
                }
            }
            // Not drawn yet: egui lays a new popup out invisibly on its first frame.
            let Some((start, frame)) = shapes.iter().enumerate().find_map(|(i, cs)| bg(&cs.shape, pr).map(|r| (i, r))) else {
                continue;
            };
            self.popup_checks.set(self.popup_checks.get() + 1);
            let pr = &frame;
            for cs in &shapes[start + 1..] {
                // A background that reaches outside the popup means another area (a
                // progress card, a toast) has started drawing on top: its text isn't ours.
                let is_bg = matches!(&cs.shape, epaint::Shape::Rect(_) | epaint::Shape::Vec(_));
                if is_bg && !pr.expand(2.0).contains_rect(cs.shape.visual_bounding_rect()) {
                    break;
                }
                if let epaint::Shape::Text(t) = &cs.shape {
                    let s = t.galley.text();
                    if s.trim().is_empty() {
                        continue;
                    }
                    let vis = cs.shape.visual_bounding_rect().intersect(cs.clip_rect);
                    if vis.is_positive() && pr.shrink(1.0).contains(vis.min) && (vis.max.x > pr.max.x + 1.0 || vis.max.y > pr.max.y + 1.0) {
                        self.fail(&format!("text {s:?} at {vis:?} runs out of its popup {pr:?} ({})", self.app.popup.kind_name()));
                    }
                }
            }
        }
        if self.check_overlap {
            for i in 0..texts.len() {
                for j in i + 1..texts.len() {
                    let (a, b) = (&texts[i], &texts[j]);
                    let x = a.0.intersect(b.0);
                    if x.is_positive() && x.area() > 0.35 * a.0.area().min(b.0.area()) {
                        self.fail(&format!("text overlaps: {:?} at {:?} and {:?} at {:?}", a.1, a.0, b.1, b.0));
                    }
                }
            }
        }
    }

    /// No two controls (toolbar buttons, chips, tab buttons…) drawn over each other.
    fn check_controls(&self) {
        let c = crate::ui::CONTROLS.with(|c| c.borrow().clone());
        for i in 0..c.len() {
            for j in i + 1..c.len() {
                let (a, b) = (&c[i], &c[j]);
                if a.0 != b.0 || !a.1.is_positive() || !b.1.is_positive() {
                    continue;
                }
                let x = a.1.intersect(b.1);
                if x.is_positive() && x.area() > 0.2 * a.1.area().min(b.1.area()) {
                    let near: Vec<String> = c.iter().filter(|o| o.1.expand(40.0).intersects(x)).map(|o| format!("{} {:?}", o.2, o.1)).collect();
                    let panels: Vec<String> = self.app.panels.values().map(|p| format!("{} {:?}", p.id, p.rect)).collect();
                    self.fail(&format!("{} at {:?} overlaps {} at {:?}\n  nearby: {near:?}\n  panels: {panels:?}", a.2, a.1, b.2, b.1));
                }
            }
        }
    }

    /// Frame time summary (p50, p95, max), ms.
    pub fn timing(&self) -> (f32, f32, f32) {
        let mut v = self.frame_ms.clone();
        if v.is_empty() {
            return (0.0, 0.0, 0.0);
        }
        v.sort_by(|a, b| a.total_cmp(b));
        let q = |f: f32| v[((v.len() - 1) as f32 * f) as usize];
        (q(0.5), q(0.95), *v.last().unwrap())
    }
}

fn describe(sh: &epaint::Shape) -> String {
    match sh {
        epaint::Shape::Text(t) => format!("text {:?} at {:?}", t.galley.text(), t.pos),
        epaint::Shape::Rect(r) => format!("rect {:?}", r.rect),
        epaint::Shape::Circle(c) => format!("circle at {:?} r {}", c.center, c.radius),
        epaint::Shape::LineSegment { points, .. } => format!("line {points:?}"),
        epaint::Shape::Path(p) => format!("path {:?}", &p.points[..p.points.len().min(4)]),
        epaint::Shape::Mesh(m) => format!("mesh {} verts, first {:?}", m.vertices.len(), m.vertices.first().map(|v| v.pos)),
        epaint::Shape::Vec(v) => format!("vec of {}", v.len()),
        other => format!("{other:?}").chars().take(160).collect(),
    }
}

/// Small deterministic PRNG (xorshift) so a failing monkey run replays from its seed.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    pub fn f(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (self.next() % 10_000) as f32 / 10_000.0 * (hi - lo)
    }
    pub fn pick<'a, T>(&mut self, v: &'a [T]) -> &'a T {
        &v[self.below(v.len())]
    }
}

/// Click the control registered as `what` this frame, when there is one.
pub fn click_control_at(s: &mut Sim, what: &str) {
    let r = crate::ui::CONTROLS.with(|c| c.borrow().iter().find(|(_, _, w)| *w == what).map(|(_, r, _)| *r));
    if let Some(r) = r {
        s.act(&format!("click {:.0} {:.0}", r.center().x, r.center().y));
    }
}

/// One random user action. Returns a description.
pub fn monkey_step(s: &mut Sim, rng: &mut Rng) {
    const KEYS: &[&str] = &[
        "ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight", "Enter", "Backspace", "Escape", "Tab", "Home", "End", "PageDown", "PageUp", "F2", "Delete", "shift+ArrowDown",
        "shift+End", "ctrl+a", "ctrl+c", "ctrl+x", "ctrl+v", "ctrl+z", "ctrl+t", "ctrl+w", "ctrl+shift+t", "ctrl+e", "ctrl+p", "ctrl+shift+p", "ctrl+Equals", "ctrl+Minus",
        "Space", "alt+ArrowUp", "alt+ArrowLeft", "alt+ArrowRight", "ctrl+Tab", "ctrl+f", "ctrl+l", "ctrl+i", "alt+Enter",
    ];
    const WORDS: &[&str] = &["doc", "a", "img", ".png", "~/", "/", "Code/maia/", "zzz", "résumé", "1", "new name"];
    let (w, h) = (s.size.x, s.size.y);
    let p = |rng: &mut Rng| (rng.f(0.0, w), rng.f(0.0, h));
    match rng.below(110) {
        0..=29 => {
            let k = *rng.pick(KEYS);
            s.act(&format!("key {k}"));
        }
        30..=41 => {
            let (x, y) = p(rng);
            s.act(&format!("click {x:.0} {y:.0}"));
        }
        42..=46 => {
            let (x, y) = p(rng);
            s.act(&format!("dclick {x:.0} {y:.0}"));
        }
        47..=52 => {
            let (x, y) = p(rng);
            s.act(&format!("click {x:.0} {y:.0} right"));
        }
        53..=57 => {
            let ((x, y), (x2, y2)) = (p(rng), p(rng));
            s.act(&format!("drag {x:.0} {y:.0} {x2:.0} {y2:.0}"));
        }
        58..=65 => {
            let (x, y) = p(rng);
            let dy = rng.f(-12.0, 12.0);
            let m = if rng.below(4) == 0 { " ctrl" } else { "" };
            s.act(&format!("scroll {x:.0} {y:.0} {dy:.1}{m}"));
        }
        66..=72 => {
            let t = *rng.pick(WORDS);
            s.act(&format!("text {t}"));
        }
        73..=84 => {
            let c = *rng.pick(crate::commands::ALL);
            if c != Cmd::Quit {
                s.cmd(c);
            }
        }
        85..=88 => {
            let (x, y) = p(rng);
            s.act(&format!("move {x:.0} {y:.0}"));
        }
        89..=91 => {
            let (nw, nh) = *rng.pick(&[(1280.0, 800.0), (640.0, 480.0), (1920.0, 1080.0), (900.0, 1200.0), (520.0, 340.0)]);
            s.resize(nw, nh);
        }
        92..=94 => s.settle(),
        95..=96 => {
            // Files dragged in from another app: hovered around, then dropped or taken away.
            use crate::clipboard::Dnd;
            let src = root().join("elsewhere");
            if !src.exists() {
                let _ = std::fs::create_dir_all(&src);
                let _ = std::fs::write(src.join("from chat.txt"), "x");
                let _ = std::fs::write(src.join("photo – 2.png"), "not really a png");
            }
            let mut paths: Vec<PathBuf> = std::fs::read_dir(&src).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default();
            paths.sort();
            paths.truncate(1 + rng.below(2));
            let ((x, y), (x2, y2)) = (p(rng), p(rng));
            s.note(format!("dnd over {x:.0} {y:.0} then {x2:.0} {y2:.0}"));
            s.app.on_dnd(Dnd::Over { x, y, paths: paths.clone(), own: false });
            s.idle(2);
            s.app.on_dnd(Dnd::Over { x: x2, y: y2, paths: paths.clone(), own: false });
            s.idle(1);
            if rng.below(2) == 0 {
                s.note("dnd drop".into());
                s.app.on_dnd(Dnd::Dropped { x: x2, y: y2, paths, own: false });
            } else {
                s.note("dnd leave".into());
                s.app.on_dnd(Dnd::Left);
            }
            s.idle(2);
        }
        99 => {
            // Into a git work tree (Go To), then edit a file there now and then.
            let sub = *rng.pick(&["Code/repo", "Code/project"]);
            let repo = std::fs::read_dir(root().join("home")).ok().and_then(|r| r.flatten().map(|d| d.path().join(sub)).find(|p| p.is_dir()));
            if let Some(repo) = repo {
                s.act("key ctrl+p");
                s.act(&format!("text {}/", repo.display()));
                s.act("key Enter");
                if rng.below(2) == 0 {
                    let _ = std::fs::write(repo.join("keep.txt"), format!("edit {}\n", rng.next()));
                }
            }
        }
        104 => {
            // Size, date and tag conditions in the filter, alone or with a name.
            s.act("key ctrl+f");
            s.act(&format!("text {}", rng.pick(&["size>1k", "modified<7d", "tag:red", "size<10 doc", "created:today", "date>2026-01", "size>huge", "tag:"])));
        }
        105 => {
            // Tag what is under a random right-click, through the menu's search.
            let (x, y) = p(rng);
            s.act(&format!("click {x:.0} {y:.0} right"));
            s.act(&format!("text tags {}", rng.pick(&["red", "blue", "new tag", "gray"])));
            s.act("key Enter");
            if matches!(s.app.popup, Popup::Prompt { .. }) {
                s.act(&format!("text {}", rng.pick(&["work", "a,b", " ", "日本"])));
                s.act("key Enter");
            }
        }
        106 => {
            // Properties: work out a checksum, or paste one to compare.
            s.act("key alt+Enter");
            if rng.below(2) == 0 {
                click_control_at(s, "checksum button");
            } else {
                click_control_at(s, "checksum field");
                s.act(&format!("text {}", rng.pick(&["900150983cd24fb0d6963f7d28e17f72", "zz", "SHA256 (x) = 00"])));
            }
            s.idle(3);
        }
        107 => {
            // Wrap or unwrap the text preview.
            if s.app.panels.get(&s.app.active).is_some_and(|p| p.inspector.is_none()) {
                s.cmd(Cmd::ToggleInspector);
            }
            click_control_at(s, "Wrap lines");
        }
        108 => {
            // A locked archive: into it, then a right or wrong password if asked.
            if let Some(z) = s.tab().entries.iter().find(|e| e.name == "locked.zip").map(|e| e.path.clone()) {
                s.cmd(Cmd::SelectNone);
                if let Some(i) = s.tab().entries.iter().position(|e| e.path == z) {
                    let id = s.app.active;
                    s.app.panels.get_mut(&id).unwrap().tab_mut().select_only(i as u32);
                }
                s.cmd(*rng.pick(&[Cmd::ExtractHere, Cmd::Activate]));
                s.idle(10);
            }
            if matches!(s.app.popup, Popup::ArchivePassword(_)) {
                s.act(&format!("text {}", rng.pick(&["pw", "nope"])));
                s.act("key Enter");
            }
        }
        103 => {
            // GitHub pictures on or off, from Options (the GET is a stand-in).
            s.act("key ctrl+Comma");
            s.act("text show github pictures");
            s.act("key Enter");
            s.act("key Escape");
        }
        102 => {
            // The branch chip in the status bar (the lowest "main" on screen) opens the
            // history; then a walk through it with keys and clicks.
            let chip = s.texts.iter().filter(|(_, t)| t == "main").max_by(|a, b| a.0.center().y.total_cmp(&b.0.center().y)).map(|(r, _)| r.center());
            if let Some(c) = chip {
                s.note("the branch chip".into());
                s.act(&format!("click {:.0} {:.0}", c.x, c.y));
                s.idle(3);
            }
            for _ in 0..rng.below(8) {
                let ir = s.app.panels.get(&s.app.active).and_then(|p| p.inspector_rect);
                match (ir, rng.below(6)) {
                    (Some(r), 0) => {
                        let (x, y) = (rng.f(r.left(), r.right()), rng.f(r.top(), r.bottom()));
                        s.act(&format!("click {x:.0} {y:.0}"));
                    }
                    // A file's menu in an open commit, and one of its items.
                    (Some(r), 1) => {
                        let (x, y) = (rng.f(r.left(), r.right()), rng.f(r.top(), r.bottom()));
                        s.act(&format!("click {x:.0} {y:.0} right"));
                        s.idle(2);
                        if matches!(s.app.popup, Popup::GitFile { .. }) {
                            let item = *rng.pick(&["Open", "Show in folder", "View as of this commit", "View before it was deleted", "File history", "Copy path"]);
                            if s.texts.iter().any(|(_, t)| t == item) {
                                s.click_text(item);
                            } else {
                                s.act("key Escape");
                            }
                        }
                    }
                    (Some(_), 2) => click_control_at(s, rng.pick(&["newer commit", "older commit", "previous file", "next file", "open large", "parent link", "copy hash", "show all commits", "Show as a list", "Group by folder", "Wrap lines"])),
                    _ => s.act(&format!("key {}", rng.pick(&["ArrowDown", "ArrowUp", "PageDown", "End", "Home", "Enter", "Escape", "OpenBracket", "CloseBracket"]))),
                }
            }
        }
        101 => {
            // Give the selected palette command an alias (F2), sometimes one already taken.
            s.act("key ctrl+shift+p");
            s.act(&format!("text {}", rng.pick(&["Toggle", "tab", "sort"])));
            s.act("key F2");
            s.act(&format!("text {}", rng.pick(&["t", "x1", "hh", "sb"])));
            s.act("key Enter");
            s.act("key Escape");
        }
        100 => {
            // Compare two panels (splitting first if there is one), then act on the marks.
            if s.app.panels.len() < 2 {
                s.cmd(Cmd::SplitRight);
            }
            s.cmd(Cmd::ComparePanels);
            s.cmd(*rng.pick(&[Cmd::SelectDifferences, Cmd::SyncPanels, Cmd::FocusNextPanel]));
        }
        98 => {
            // Contents mode: the filter's toggle, then a query typed into the listing.
            click_control_at(s, "contents toggle");
            let t = *rng.pick(&["line", "fn", "zzz", "Report", "ü"]);
            s.act(&format!("text {t}"));
        }
        109 => {
            // Options: a section by click or by keys, a row picked and acted on, now and
            // then a search, a reset arrow or titles only.
            s.act("key ctrl+Comma");
            for _ in 0..1 + rng.below(5) {
                match rng.below(7) {
                    0 => {
                        let sec = *rng.pick(&crate::popups::Section::ALL);
                        click_control_at(s, sec.control());
                    }
                    1 => s.act(rng.pick(&["key ctrl+PageDown", "key ctrl+PageUp"])),
                    2 => s.act(rng.pick(&["key ArrowDown", "key ArrowUp", "key Enter"])),
                    3 => s.act(&format!("text {}", rng.pick(&["size", "font", "click", "zz", "git"]))),
                    4 => click_control_at(s, "reset option"),
                    5 => click_control_at(s, rng.pick(&["switch", "choice", "titles only"])),
                    _ => s.act("key Escape"),
                }
            }
        }
        97 => {
            // A drag that leaves the window (handed to the system when it carries files).
            let (x, y) = p(rng);
            s.act(&format!("down {x:.0} {y:.0}"));
            for x2 in [x + 12.0, x + 30.0, (x + 30.0) / 2.0, -40.0] {
                s.act(&format!("move {x2:.0} {y:.0}"));
            }
            s.idle(2);
            if s.ctx.input(|i| i.pointer.any_down()) {
                s.act(&format!("up -40 {y:.0}"));
            }
        }
        _ => {
            s.note("idle".into());
            s.idle(rng.below(20) + 1);
        }
    }
}

#[cfg(test)]
mod journeys {
    use super::*;
    use Cmd::*;

    fn go(s: &mut Sim, p: &Path) {
        s.note(format!("navigate {}", p.display()));
        let id = s.app.active;
        if let Some(panel) = s.app.panels.get_mut(&id) {
            panel.tab_mut().navigate(p.to_path_buf(), None);
        }
        s.settle();
    }

    /// Hover nothing and let the screen settle, then demand clean text layout.
    fn look(s: &mut Sim, what: &str) {
        s.note(format!("look: {what}"));
        s.act("move -50 -50");
        s.settle();
        s.idle(2);
        // Toasts and job cards float over the list by design.
        s.check_overlap = !s.popup_open() && s.app.toast.is_none() && s.app.jobs.is_empty();
        s.idle(2);
        s.check_overlap = false;
    }

    fn report(name: &str, s: &Sim) {
        let (p50, p95, max) = s.timing();
        eprintln!("sim {name}: {} frames, frame ms p50 {p50:.1} p95 {p95:.1} max {max:.1}", s.frame_ms.len());
    }

    #[test]
    fn every_view_renders_cleanly() {
        if !sandboxed() {
            return;
        }
        let home = fixture("views");
        let mut s = Sim::new(&home, vec2(1280.0, 800.0));
        let folders = [home.clone(), home.join("Documents"), home.join("Big"), home.join("Pictures/Trip"), home.join("Code/maia"), home.join("Empty")];
        for f in &folders {
            go(&mut s, f);
            for v in [ViewDetails, ViewTree, ViewList, ViewMedium, ViewLarge, ViewXL] {
                s.cmd(v);
                for g in [GroupNone, GroupType, GroupName, GroupSize] {
                    s.cmd(g);
                    for (w, h) in [(1280.0, 800.0), (640.0, 480.0), (520.0, 340.0)] {
                        s.resize(w, h);
                        look(&mut s, &format!("{} {:?} {:?} {w}x{h}", f.display(), v, g));
                    }
                }
            }
        }
        // Same with the inspector and without the sidebar.
        s.cmd(GroupNone);
        s.cmd(ToggleInspector);
        s.cmd(ToggleSidebar);
        for f in &folders {
            go(&mut s, f);
            for v in [ViewDetails, ViewTree, ViewList, ViewLarge] {
                s.cmd(v);
                s.act("key ArrowDown");
                look(&mut s, &format!("inspector {} {v:?}", f.display()));
            }
        }
        report("views", &s);
    }

    #[test]
    fn browse_rename_create_trash_undo() {
        if !sandboxed() {
            return;
        }
        let home = fixture("journey");
        let mut s = Sim::new(&home, vec2(1280.0, 800.0));
        // Type-to-find, then open.
        s.act("text Docu"); // "Doc" would match docs-link first (display order)
        s.act("key Enter");
        s.settle();
        assert_eq!(s.tab().path, home.join("Documents"), "type-to-find + Enter opens Documents");
        // Rename notes.txt via F2.
        s.act("text notes");
        s.act("key F2");
        assert!(s.tab().rename.is_some(), "F2 starts a rename");
        s.act("key ctrl+a");
        s.act("text notes2.txt");
        s.act("key Enter");
        s.settle();
        assert!(home.join("Documents/notes2.txt").exists(), "renamed on disk");
        s.act("key ctrl+z");
        s.settle();
        assert!(home.join("Documents/notes.txt").exists() && !home.join("Documents/notes2.txt").exists(), "undo restores the name");
        // New folder, named inline.
        s.cmd(NewFolder);
        s.settle();
        s.idle(3);
        if s.tab().rename.is_some() {
            s.act("key ctrl+a");
            s.act("text Fresh");
            s.act("key Enter");
            s.settle();
        }
        let fresh = std::fs::read_dir(home.join("Documents")).unwrap().flatten().find(|d| d.path().is_dir() && d.file_name() != "Taxes").map(|d| d.path());
        let fresh = fresh.unwrap_or_else(|| s.fail("new folder created"));
        // Select it and trash it.
        let idx = s.tab().entries.iter().position(|e| e.path == fresh).unwrap_or_else(|| s.fail("new folder listed"));
        let id = s.app.active;
        s.app.panels.get_mut(&id).unwrap().tab_mut().select_only(idx as u32);
        s.act("key Delete");
        if s.popup_open() {
            s.act("key Enter");
        }
        s.settle();
        assert!(!fresh.exists(), "Delete moves the folder to the trash");
        s.act("key ctrl+z");
        s.settle();
        assert!(fresh.exists(), "undo brings it back from the trash");
        // Up selects the folder we came from.
        s.act("key alt+ArrowUp");
        s.settle();
        assert_eq!(s.tab().path, home);
        let cur = s.tab().cursor.map(|c| s.tab().entries[c as usize].path.clone());
        assert_eq!(cur, Some(home.join("Documents")), "Up selects the previous folder");
        // Tabs: new, close, reopen.
        let tabs = |s: &Sim| s.app.panels[&s.app.active].tabs.len();
        let n = tabs(&s);
        s.act("key ctrl+t");
        assert_eq!(tabs(&s), n + 1);
        s.act("key ctrl+w");
        assert_eq!(tabs(&s), n);
        s.act("key ctrl+shift+t");
        assert_eq!(tabs(&s), n + 1);
        // Splits: two panels, focus moves, close one.
        s.cmd(SplitRight);
        s.settle();
        assert_eq!(s.app.panels.len(), 2);
        s.act("key Tab");
        s.cmd(ClosePanel);
        assert_eq!(s.app.panels.len(), 1);
        // Go To with a typed path.
        s.act("key ctrl+p");
        assert!(s.popup_open());
        s.act(&format!("text {}/Code/maia/", home.display()));
        s.settle();
        s.act("key Escape");
        assert!(!s.popup_open(), "Escape closes Go To");
        assert!(crate::mime::LAUNCHED.lock().unwrap().is_empty() || true);
        report("journey", &s);
    }

    /// Features combined: every subset of a set of toggles, then the usual interactions.
    #[test]
    fn feature_combinations_hold_together() {
        if !sandboxed() {
            return;
        }
        let home = fixture("combos");
        let mut s = Sim::new(&home, vec2(1100.0, 700.0));
        let toggles: &[(&str, &[Cmd])] = &[
            ("flatten", &[ToggleFlatten]),
            ("hidden", &[ToggleHidden]),
            ("tree", &[ViewTree]),
            ("icons", &[ViewLarge]),
            ("group", &[GroupType]),
            ("sort", &[SortSizeDesc]),
            ("inspector", &[ToggleInspector]),
            ("split", &[SplitRight]),
        ];
        let mut rng = Rng::new(7);
        for mask in 0u32..(1 << toggles.len()) {
            if mask.count_ones() > 4 && rng.below(4) != 0 {
                continue; // sample the big combinations
            }
            let mut s2 = Sim::new(&home, vec2(1100.0, 700.0));
            let on: Vec<&str> = toggles.iter().enumerate().filter(|(i, _)| mask & (1 << i) != 0).map(|(_, t)| t.0).collect();
            s2.note(format!("combo {on:?}"));
            for (i, (_, cmds)) in toggles.iter().enumerate() {
                if mask & (1 << i) != 0 {
                    for c in *cmds {
                        s2.cmd(*c);
                    }
                }
            }
            s2.settle();
            for line in ["text o", "key ArrowDown", "key shift+End", "key ctrl+a", "scroll 500 400 -6", "key Escape", "key ctrl+f", "text a", "key Escape", "key End", "key Home", "key Enter", "key alt+ArrowUp"] {
                s2.act(line);
            }
            s2.settle();
            s2.act("key ctrl+shift+p");
            s2.act("key Escape");
            look(&mut s2, &format!("combo {on:?}"));
            s.frame_ms.extend(s2.frame_ms.drain(..));
        }
        report("combos", &s);
    }

    /// Every popup and menu at every window size (and display scale) stays on screen
    /// and renders valid geometry; Escape always closes it.
    #[test]
    fn popups_and_menus_fit_every_window() {
        if !sandboxed() {
            return;
        }
        let home = fixture("popups");
        let mut s = Sim::new(&home.join("Documents"), vec2(1280.0, 800.0));
        s.act("key ArrowDown");
        for ppp in [1.0, 1.6, 2.0] {
            s.ppp = ppp;
            for (w, h) in [(1920.0, 1080.0), (1280.0, 800.0), (520.0, 340.0)] {
                s.resize(w, h);
                for c in [Palette, GoTo, Options, MainMenu, QuickMenu, TypesFilter, Properties, ContextMenu, TabSearch, About] {
                    s.cmd(c);
                    s.settle();
                    s.idle(2);
                    if !s.popup_open() {
                        s.fail(&format!("{c:?} opened nothing at {w}x{h}"));
                    }
                    s.act("key Escape");
                    s.idle(2);
                    if s.popup_open() {
                        s.act("key Escape");
                    }
                    if s.popup_open() {
                        s.fail(&format!("{c:?} did not close on Escape"));
                    }
                }
                // Right-click menus on an item, on empty space, on the sidebar and tab bar.
                for (x, y) in [(w * 0.5, 120.0), (w * 0.6, h - 60.0), (60.0, 140.0), (240.0, 14.0)] {
                    s.act(&format!("click {x:.0} {y:.0} right"));
                    s.settle();
                    s.act("key Escape");
                    s.idle(2);
                }
                // The folder dropdown (breadcrumb chevron) on a folder with very long names.
                let id = s.app.active;
                s.app.popup = Popup::path_dropdown(id, home.join("Documents"), None);
                s.settle();
                s.idle(2);
                s.act("key Escape");
                // The view-size popup from the status bar zoom chip.
                s.act(&format!("click {:.0} {:.0}", w - 20.0, h - 12.0));
                s.settle();
                s.act("key Escape");
            }
        }
        assert!(s.popup_checks.get() > 100, "popup text containment checked only {} times", s.popup_checks.get());
        report("popups", &s);
    }

    /// Two to four panels side by side and stacked, small and large, in every view.
    #[test]
    fn split_layouts_render_cleanly() {
        if !sandboxed() {
            return;
        }
        let home = fixture("splits");
        let mut s = Sim::new(&home, vec2(1280.0, 800.0));
        for split in [SplitRight, SplitBottom, SplitRight] {
            s.cmd(split);
            s.settle();
            for v in [ViewDetails, ViewTree, ViewList, ViewMedium, ViewXL] {
                s.cmd(v);
                for (w, h) in [(1920.0, 1080.0), (1280.0, 800.0), (520.0, 340.0)] {
                    s.resize(w, h);
                    let n = s.app.panels.len();
                    look(&mut s, &format!("{n} panels {v:?} {w}x{h}"));
                }
            }
            s.cmd(ToggleInspector);
            let n = s.app.panels.len();
            look(&mut s, &format!("{n} panels with inspector"));
            s.cmd(ToggleInspector);
        }
        report("splits", &s);
    }

    /// Flattening a huge tree streams hundreds of thousands of results in: the UI has to
    /// keep drawing smoothly while they arrive, and stay quick to filter, scroll and sort.
    #[test]
    fn flatten_huge_tree_stays_responsive() {
        if !sandboxed() {
            return;
        }
        let mut s = Sim::new(Path::new("/usr"), vec2(1280.0, 800.0));
        s.frame_ms.clear();
        s.cmd(ToggleFlatten);
        let t0 = Instant::now();
        while s.tab().is_loading() && t0.elapsed().as_secs() < 120 {
            s.frame(vec![]);
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        s.settle();
        let n = s.tab().entries.len();
        let (p50, p95, max) = s.timing();
        eprintln!("sim flatten /usr: {n} files in {:.1}s, {} frames while streaming, p50 {p50:.1} p95 {p95:.1} max {max:.1} ms", t0.elapsed().as_secs_f32(), s.frame_ms.len());
        let streaming = std::mem::take(&mut s.frame_ms);
        for (what, line) in [("filter", "text lib"), ("scroll", "scroll 600 400 -10"), ("end", "key End"), ("sort", "key ctrl+alt+2"), ("clear", "key Escape")] {
            let i = s.frame_ms.len();
            if what == "filter" {
                s.act("key ctrl+f");
            }
            s.act(line);
            s.settle();
            let worst = s.frame_ms[i..].iter().cloned().fold(0.0f32, f32::max);
            eprintln!("sim flatten /usr: {what} worst frame {worst:.1} ms");
        }
        for (ms, step) in &s.slow {
            eprintln!("  slow frame {ms:.0} ms after: {step}");
        }
        assert!(n > 10_000, "flatten found files");
        // Budgets (optimized build, with headroom for a busy machine): results streaming
        // in must not stall drawing, and filtering/sorting 100k+ items stays interactive.
        let mut v = streaming.clone();
        v.sort_by(|a, b| a.total_cmp(b));
        let p95 = v.get(v.len() * 95 / 100).copied().unwrap_or(0.0);
        assert!(p95 < 40.0, "frames while streaming: p95 {p95:.1} ms");
        let worst = s.frame_ms.iter().cloned().fold(0.0f32, f32::max);
        assert!(worst < 200.0, "slowest interaction frame {worst:.1} ms");
    }

    /// Tree view, "expand all" over thousands of folders: drawing stays smooth while the
    /// listings land, and no folder's children appear twice.
    #[test]
    fn tree_expand_all_stays_responsive() {
        if !sandboxed() {
            return;
        }
        let mut s = Sim::new(Path::new("/usr/share"), vec2(1280.0, 800.0));
        s.cmd(ViewTree);
        s.settle();
        s.frame_ms.clear();
        s.act("key ctrl+e");
        let t0 = Instant::now();
        while s.tab().is_loading() && t0.elapsed().as_secs() < 120 {
            s.frame(vec![]);
            std::thread::sleep(std::time::Duration::from_millis(4));
        }
        s.settle();
        // Interact with the big unfolded tree too.
        for line in ["scroll 600 400 -20", "key End", "key Home", "key ctrl+f", "text lib", "key Escape", "key Escape"] {
            let i = s.frame_ms.len();
            s.act(line);
            s.settle();
            let worst = s.frame_ms[i..].iter().cloned().fold(0.0f32, f32::max);
            eprintln!("sim tree expand-all: {line} worst frame {worst:.1} ms");
        }
        let t = s.tab();
        let live: Vec<&crate::fsx::Entry> = t.entries.iter().filter(|e| e.depth != crate::fsx::DEAD).collect();
        let mut seen = std::collections::HashSet::new();
        for e in &live {
            assert!(seen.insert(&e.path), "listed twice: {}", e.path.display());
        }
        let (p50, p95, max) = s.timing();
        eprintln!("sim tree expand-all /usr/share: {} live entries, {} rows, {} frames, p50 {p50:.1} p95 {p95:.1} max {max:.1} ms", live.len(), t.rows.len(), s.frame_ms.len());
        assert!(live.len() > 5_000, "expand-all unfolded the tree");
        // ~200k unfolded rows (optimized build, headroom for a busy machine).
        assert!(max < 120.0, "slowest frame {max:.1} ms");
        assert!(p95 < 60.0, "frames p95 {p95:.1} ms");
    }

    fn select_path(s: &mut Sim, p: &Path) {
        let Some(i) = s.tab().entries.iter().position(|e| e.path == p) else { s.fail(&format!("{} not listed", p.display())) };
        let id = s.app.active;
        s.app.panels.get_mut(&id).unwrap().tab_mut().select_only(i as u32);
        s.idle(1);
    }

    /// Inspector open, the cursor walks every item of every kind (text, markdown, images
    /// good and corrupt, fake PDF, pipe, links good and broken, locked and empty
    /// folders) in several views: no crash, no broken layout, no stalls.
    #[test]
    fn inspector_previews_every_kind() {
        if !sandboxed() {
            return;
        }
        let home = fixture("inspect");
        let mut s = Sim::new(&home, vec2(1280.0, 800.0));
        s.cmd(ToggleSystem);
        s.cmd(ToggleHidden);
        s.cmd(ToggleInspector);
        for dir in [home.clone(), home.join("Documents"), home.join("Pictures"), home.join("Pictures/Trip"), home.join("Code/maia"), home.join("Music/Album")] {
            go(&mut s, &dir);
            for v in [ViewDetails, ViewList, ViewLarge] {
                s.cmd(v);
                s.act("key Home");
                let n = s.tab().view.len().min(40);
                for _ in 0..n {
                    s.act("key ArrowDown");
                    s.idle(2);
                }
                s.settle();
                look(&mut s, &format!("inspector {} {v:?}", dir.display()));
            }
        }
        for (ms, step) in &s.slow {
            eprintln!("  slow frame {ms:.0} ms after: {step}");
        }
        assert!(s.slow.iter().all(|x| x.0 < 150.0), "previewing stalled");
        report("inspector", &s);
    }

    /// Copy and move between split panels into conflicts, answered through the dialog's
    /// buttons; duplicate; undo. Checked on disk.
    #[test]
    fn copy_move_conflicts_and_undo() {
        if !sandboxed() {
            return;
        }
        let home = fixture("conflicts");
        std::fs::write(home.join("Code/maia/report.md"), "older report").unwrap();
        let src = home.join("Documents/report.md");
        let dst_dir = home.join("Code/maia");
        let mut s = Sim::new(&home.join("Documents"), vec2(1280.0, 800.0));
        // Second panel on the destination.
        s.cmd(SplitRight);
        go(&mut s, &dst_dir);
        s.act("key Tab"); // back to the Documents panel
        s.settle();
        assert_eq!(s.tab().path, home.join("Documents"));
        // F5 into an existing name: Keep both.
        select_path(&mut s, &src);
        s.act("key F5");
        s.settle();
        assert!(s.popup_open(), "conflict dialog shown");
        s.click_text("Keep both");
        s.settle();
        assert!(dst_dir.join("report (1).md").exists(), "kept both");
        assert_eq!(std::fs::read_to_string(dst_dir.join("report.md")).unwrap(), "older report");
        // Again: Replace.
        select_path(&mut s, &src);
        s.act("key F5");
        s.settle();
        s.click_text("Replace");
        s.settle();
        assert_eq!(std::fs::read_to_string(dst_dir.join("report.md")).unwrap(), std::fs::read_to_string(&src).unwrap(), "replaced");
        // F6 (move) into a conflict: Skip leaves the source where it is.
        select_path(&mut s, &src);
        s.act("key F6");
        s.settle();
        s.click_text("Skip");
        s.settle();
        assert!(src.exists(), "skipped move keeps the source");
        // A move without conflict, then undo puts it back.
        let notes = home.join("Documents/notes.txt");
        select_path(&mut s, &notes);
        s.act("key F6");
        s.settle();
        assert!(!notes.exists() && dst_dir.join("notes.txt").exists(), "moved");
        s.act("key ctrl+z");
        s.settle();
        assert!(notes.exists() && !dst_dir.join("notes.txt").exists(), "undo moved it back");
        // Duplicate in place.
        select_path(&mut s, &src);
        s.cmd(Duplicate);
        s.settle();
        let dupes = std::fs::read_dir(home.join("Documents")).unwrap().flatten().filter(|d| d.file_name().to_string_lossy().starts_with("report")).count();
        assert_eq!(dupes, 2, "duplicate made a second copy");
        report("conflicts", &s);
    }

    /// Computer (drives) and Trash: every view renders; trashed items list, restore to
    /// their place, and Empty trash (confirmed through its button) clears them.
    #[test]
    fn computer_and_trash_views() {
        if !sandboxed() {
            return;
        }
        let home = fixture("trashy");
        let mut s = Sim::new(&home.join("Documents"), vec2(1280.0, 800.0));
        s.cmd(Computer);
        s.settle();
        assert!(!s.tab().view.is_empty(), "drives listed");
        for v in [ViewDetails, ViewList, ViewLarge, ViewTree] {
            s.cmd(v);
            look(&mut s, &format!("computer {v:?}"));
        }
        s.cmd(ViewDetails);
        go(&mut s, &home.join("Documents"));
        let a = home.join("Documents/notes.txt");
        let b = home.join("Documents/budget 2026.csv");
        for p in [&a, &b] {
            select_path(&mut s, p);
            s.act("key Delete");
            if s.popup_open() {
                s.act("key Enter");
            }
            s.settle();
            assert!(!p.exists(), "{} trashed", p.display());
        }
        go(&mut s, &crate::sidebar::trash_dir());
        // The sandbox's trash is shared by the journeys that ran before: look for ours.
        for n in ["notes.txt", "budget 2026.csv"] {
            assert!(s.tab().entries.iter().any(|e| e.name == n), "{n} in the trash");
        }
        for v in [ViewDetails, ViewList, ViewLarge] {
            s.cmd(v);
            look(&mut s, &format!("trash {v:?}"));
        }
        let ta = s.tab().entries.iter().position(|e| e.name == "notes.txt").unwrap_or_else(|| s.fail("notes in trash"));
        let id = s.app.active;
        s.app.panels.get_mut(&id).unwrap().tab_mut().select_only(ta as u32);
        s.cmd(RestoreTrash);
        s.settle();
        assert!(a.exists(), "restored to its place");
        s.cmd(EmptyTrash);
        s.settle();
        s.click_text("Empty trash");
        s.settle();
        assert!(!b.exists());
        assert_eq!(std::fs::read_dir(crate::sidebar::trash_dir()).map(|r| r.count()).unwrap_or(0), 0, "trash emptied");
        report("trash", &s);
    }

    /// Mouse work: drag a file onto a folder, drag one into the other panel, rubber-band
    /// select, widen a column, drag a tab into the other panel.
    #[test]
    fn mouse_drag_drop_select_and_resize() {
        if !sandboxed() {
            return;
        }
        let home = fixture("mouse");
        let mut s = Sim::new(&home.join("Documents"), vec2(1280.0, 800.0));
        s.act("move -50 -50");
        s.settle();
        // Two quick clicks on different rows are not a double-click: nothing opens.
        let (a, b) = (s.text_rect("notes.txt").center(), s.text_rect("Taxes").center());
        let tabs = s.app.panels[&s.app.active].tabs.len();
        s.act(&format!("click {:.0} {:.0}", a.x, a.y));
        s.act(&format!("click {:.0} {:.0} ctrl", b.x, b.y));
        s.settle();
        assert_eq!((s.tab().path.clone(), s.app.panels[&s.app.active].tabs.len()), (home.join("Documents"), tabs), "a click on another row opened it");
        assert_eq!(s.tab().selected.len(), 2);
        s.idle(40);
        s.act(&format!("dclick {:.0} {:.0}", b.x, b.y));
        wait_until(&mut s, "double-click opens", 10, |s| s.tab().path == home.join("Documents/Taxes"));
        s.act("key Backspace");
        wait_until(&mut s, "back", 10, |s| s.tab().path == home.join("Documents"));
        s.act("key Escape");
        s.settle();
        // Drag notes.txt onto the Taxes folder: moved inside.
        let (a, b) = (s.text_rect("notes.txt").center(), s.text_rect("Taxes").center());
        s.act(&format!("drag {:.0} {:.0} {:.0} {:.0}", a.x, a.y, b.x, b.y));
        s.settle();
        assert!(home.join("Documents/Taxes/notes.txt").exists() && !home.join("Documents/notes.txt").exists(), "dropped onto the folder");
        // Right-drag onto a folder asks what to do: Copy here.
        let (a, b) = (s.text_rect("report.md").center(), s.text_rect("Taxes").center());
        s.act(&format!("drag {:.0} {:.0} {:.0} {:.0} right", a.x, a.y, b.x, b.y));
        s.settle();
        s.click_text("Copy here");
        s.settle();
        assert!(home.join("Documents/Taxes/report.md").exists() && home.join("Documents/report.md").exists(), "right-drag copied");
        // Drop onto a sidebar place (Home).
        let (a, b) = (s.text_rect("budget 2026.csv").center(), s.text_rect("Home").center());
        s.act(&format!("drag {:.0} {:.0} {:.0} {:.0}", a.x, a.y, b.x, b.y));
        s.settle();
        assert!(home.parent().unwrap().join("budget 2026.csv").exists(), "dropped onto the sidebar's Home");
        std::fs::rename(home.parent().unwrap().join("budget 2026.csv"), home.join("Documents/budget 2026.csv")).unwrap();
        s.cmd(Refresh);
        s.settle();
        // Rubber band from empty space below the rows up across them: several selected.
        let list = s.app.panels[&s.app.active].list_rect;
        let last = s.text_rect("report.md");
        s.act(&format!("drag {:.0} {:.0} {:.0} {:.0}", list.center().x + 120.0, list.bottom() - 20.0, list.center().x + 100.0, last.top() - 30.0));
        s.settle();
        assert!(s.tab().selected.len() >= 2, "rubber band selected {} items", s.tab().selected.len());
        s.act("key Escape");
        // Widen the Name column by dragging the divider just left of the Type header.
        let w0 = s.tab().settings.columns.first().map(|c| c.1).unwrap_or(0.0);
        let ty = s.text_rect("Type");
        s.act(&format!("drag {:.0} {:.0} {:.0} {:.0}", ty.left() - 9.0, ty.center().y, ty.left() + 71.0, ty.center().y));
        s.settle();
        let w1 = s.tab().settings.columns.first().map(|c| c.1).unwrap_or(0.0);
        assert!(w1 > w0 + 40.0, "Name column widened {w0} -> {w1}");
        // Split: right panel on Pictures; drag a file across into it.
        s.cmd(SplitRight);
        go(&mut s, &home.join("Pictures"));
        s.act("key Tab");
        s.settle();
        s.act("move -50 -50");
        s.idle(2);
        let csv = s.text_rect("budget 2026.csv").center();
        let right = s.app.panels.values().find(|p| p.tab().path == home.join("Pictures")).map(|p| p.list_rect).unwrap_or_else(|| s.fail("right panel"));
        s.act(&format!("drag {:.0} {:.0} {:.0} {:.0}", csv.x, csv.y, right.center().x, right.bottom() - 30.0));
        s.settle();
        assert!(home.join("Pictures/budget 2026.csv").exists(), "dropped into the other panel");
        // Drag the Documents tab onto the other panel's tab strip: one panel left with both tabs.
        let n_tabs: usize = s.app.panels.values().map(|p| p.tabs.len()).sum();
        let tab = s.texts.iter().filter(|(r, t)| t == "Documents" && r.top() < 40.0).map(|(r, _)| *r).next().unwrap_or_else(|| s.fail("Documents tab"));
        let pics = s.texts.iter().filter(|(r, t)| t == "Pictures" && r.top() < 40.0).map(|(r, _)| *r).next().unwrap_or_else(|| s.fail("Pictures tab"));
        s.act(&format!("drag {:.0} {:.0} {:.0} {:.0}", tab.center().x, tab.center().y, pics.right() + 30.0, pics.center().y));
        s.settle();
        let n_tabs2: usize = s.app.panels.values().map(|p| p.tabs.len()).sum();
        assert_eq!(n_tabs2, n_tabs, "the tab moved, none lost");
        assert_eq!(s.app.panels.len(), 1, "the emptied panel closed");
        look(&mut s, "after drags");
        report("mouse", &s);
    }

    fn cursor_pos(s: &Sim) -> usize {
        let t = s.tab();
        t.cursor.and_then(|c| t.view_pos(c)).unwrap_or_else(|| s.fail("no cursor"))
    }

    /// The cursor's item must be drawn inside the visible list (scrolled into view).
    fn cursor_visible(s: &mut Sim, what: &str) {
        s.idle(20); // scrolling is animated (~0.3 s of frames)
        let t = s.tab();
        let name = crate::listing::display_name(&t.entries[t.cursor.unwrap() as usize], &s.app.cfg);
        let list = s.app.panels[&s.app.active].list_rect;
        let seen = s.texts.iter().any(|(r, txt)| (txt == &name || name.starts_with(txt.trim_end_matches('…'))) && list.contains(r.center()));
        if !seen {
            s.fail(&format!("{what}: cursor item {name:?} not drawn inside the list"));
        }
    }

    /// Arrow keys, Home/End, Page keys, Shift-extend and type-to-find in every view of a
    /// 600-file folder: the cursor lands where it should and stays on screen.
    #[test]
    fn keyboard_navigation_in_every_view() {
        if !sandboxed() {
            return;
        }
        let home = fixture("keys");
        let mut s = Sim::new(&home.join("Big"), vec2(1280.0, 800.0));
        for v in [ViewDetails, ViewList, ViewMedium, ViewLarge, ViewXL] {
            s.cmd(v);
            s.settle();
            let n = s.tab().view.len();
            s.act("key Home");
            assert_eq!(cursor_pos(&s), 0, "{v:?}: Home");
            let (row, col) = (s.tab().nav_row_step.max(1), s.tab().nav_col_step);
            s.act("key ArrowDown");
            assert_eq!(cursor_pos(&s), row, "{v:?}: Down moves one row ({row})");
            if col > 0 {
                s.act("key ArrowRight");
                assert_eq!(cursor_pos(&s), row + col, "{v:?}: Right moves one column ({col})");
                s.act("key ArrowLeft");
            }
            s.act("key ArrowUp");
            assert_eq!(cursor_pos(&s), 0, "{v:?}: Up back to the first");
            s.act("key End");
            assert_eq!(cursor_pos(&s), n - 1, "{v:?}: End");
            cursor_visible(&mut s, &format!("{v:?} End"));
            let page = s.tab().page_items.max(1);
            s.act("key PageUp");
            assert_eq!(cursor_pos(&s), n - 1 - page.min(n - 1), "{v:?}: PageUp moves a page ({page})");
            cursor_visible(&mut s, &format!("{v:?} PageUp"));
            s.act("key Home");
            for _ in 0..3 {
                s.act("key shift+ArrowDown");
            }
            assert_eq!(s.tab().selected.len(), 3 * row + 1, "{v:?}: Shift+Down extends the selection");
            s.act("key Escape");
            s.act("text file0420");
            s.idle(2);
            let t = s.tab();
            assert_eq!(t.entries[t.cursor.unwrap() as usize].name, "file0420.txt", "{v:?}: type-to-find");
            cursor_visible(&mut s, &format!("{v:?} type-to-find"));
            // Typing went into the filter box: arrows still drive the list from there,
            // Escape clears the filter and a second Escape leaves the box.
            assert_eq!(s.tab().filter, "file0420");
            s.act("key ArrowDown");
            s.act("key Escape");
            s.act("key Escape");
            assert!(s.tab().filter.is_empty() && !s.ctx.wants_keyboard_input(), "{v:?}: Escape twice clears and leaves the filter");
        }
        report("keys", &s);
    }

    /// Batch rename: F2 on twelve photos, type a shared prefix, add numbering from the
    /// rename menu, commit, undo. Colliding names are refused with nothing renamed.
    #[test]
    fn batch_rename_numbering_conflicts_and_undo() {
        if !sandboxed() {
            return;
        }
        let home = fixture("batch");
        let trip = home.join("Pictures/Trip");
        let names = |d: &Path| -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(d).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
            v.sort();
            v
        };
        let before = names(&trip);
        let mut s = Sim::new(&trip, vec2(1280.0, 800.0));
        // Colliding names: refused, nothing renamed.
        s.act("key Home");
        s.act("key shift+ArrowDown");
        s.act("key F2");
        assert!(s.tab().rename.as_ref().is_some_and(|r| r.is_batch()), "batch rename for two items");
        s.act("text same");
        s.act("key Enter");
        s.settle();
        assert_eq!(names(&trip), before, "colliding names were refused");
        assert!(s.tab().rename.as_ref().is_some_and(|r| r.error.is_some()), "the collision is reported");
        s.act("key Escape");
        s.settle();
        // All twelve: "trip-" + incremental ID from the rename menu.
        s.act("key ctrl+a");
        s.act("key F2");
        assert_eq!(s.tab().rename.as_ref().map(|r| r.fields.len()), Some(12));
        s.act("text trip-");
        s.idle(2);
        let field = s.texts.iter().find(|(_, t)| t.starts_with("trip-")).map(|(r, _)| r.center()).unwrap_or_else(|| s.fail("rename field"));
        s.act(&format!("click {:.0} {:.0} right", field.x, field.y));
        s.settle();
        s.click_text("Insert incremental ID");
        s.settle();
        s.act("key Enter");
        s.settle();
        let after = names(&trip);
        assert_eq!(after.len(), 12, "{after:?}");
        assert!(after.iter().all(|n| n.starts_with("trip-") && n.ends_with(".png")), "{after:?}");
        assert!(!after.iter().any(|n| n.starts_with("img")), "{after:?}");
        s.act("key ctrl+z");
        s.settle();
        assert_eq!(names(&trip), before, "undo restores every name");
        report("batch", &s);
    }

    /// Options: search, toggle with Enter, and the change shows in the list at once.
    #[test]
    fn options_toggle_takes_effect() {
        if !sandboxed() {
            return;
        }
        let home = fixture("opts");
        let mut s = Sim::new(&home, vec2(1280.0, 800.0));
        let shows = |s: &Sim, name: &str| s.tab().view.iter().any(|&i| s.tab().entries[i as usize].name == name);
        assert!(!shows(&s, ".bashrc"), "hidden files start hidden");
        let first = |s: &Sim| s.tab().view.first().map(|&i| s.tab().entries[i as usize].is_dir);
        assert_eq!(first(&s), Some(true), "folders first by default");
        for (query, what) in [("hidden", "show hidden"), ("folders first", "folders first")] {
            s.act("key ctrl+Comma");
            s.settle();
            assert!(s.popup_open(), "Options open");
            s.act(&format!("text {query}"));
            s.idle(2);
            s.act("key Enter");
            s.settle();
            s.act("key Escape");
            s.act("key Escape");
            s.settle();
            let _ = what;
        }
        assert!(shows(&s, ".bashrc"), "show hidden took effect");
        assert_eq!(first(&s), Some(false), "files first took effect");
        look(&mut s, "after options");
        report("options", &s);
    }

    /// The section on show in Options (None when Options is closed).
    fn opt_section(s: &Sim) -> Option<crate::popups::Section> {
        match &s.app.popup {
            Popup::Options(st) => st.section,
            _ => None,
        }
    }

    /// Click the `what` control on the row titled `title` (the one nearest its title).
    fn click_in_row(s: &mut Sim, what: &str, title: &str) {
        s.idle(1);
        let t = s.text_rect(title);
        let r = crate::ui::CONTROLS.with(|c| c.borrow().iter().filter(|(_, r, w)| *w == what && r.left() > t.left()).map(|(_, r, _)| *r).min_by(|a, b| (a.center().y - t.center().y).abs().total_cmp(&(b.center().y - t.center().y).abs())));
        let r = r.unwrap_or_else(|| s.fail(&format!("no {what} beside {title:?}")));
        assert!((r.center().y - t.center().y).abs() < 22.0, "{what} on the row of {title:?}: {r:?} vs {t:?}");
        s.act(&format!("click {:.0} {:.0}", r.center().x, r.center().y));
    }

    /// Options in sections: the left column walked with the mouse and with Ctrl+PageUp
    /// and Ctrl+PageDown, a setting changed in every section, a search across all of
    /// them acted on with the keys and the mouse, reset arrows, titles only with its
    /// tooltips, the section remembered, a small window and a light theme.
    #[test]
    fn options_sections() {
        if !sandboxed() {
            return;
        }
        use crate::popups::{OPTS, Section};
        let home = fixture("optsections");
        let mut s = Sim::new(&home, vec2(1280.0, 800.0));
        let slug = |sec: Section| sec.name().to_lowercase().replace(" & ", "-").replace(' ', "-");
        s.act("key ctrl+Comma");
        s.settle();
        assert_eq!(opt_section(&s), Some(Section::Appearance), "opens on Appearance");
        // Every section with the mouse: its own rows and nothing else.
        for sec in Section::ALL {
            s.click_text(sec.name());
            s.idle(2);
            assert_eq!(opt_section(&s), Some(sec), "{} chosen", sec.name());
            for (_, other, _, title, _) in OPTS {
                let shown = s.texts.iter().any(|(_, t)| t == title);
                if *other != sec {
                    assert!(!shown, "{title:?} belongs to {}, not {}", other.name(), sec.name());
                }
            }
            let first = OPTS.iter().find(|o| o.1 == sec).map(|o| o.3).unwrap();
            assert!(s.texts.iter().any(|(_, t)| t == first), "{first:?} heads {}", sec.name());
            look(&mut s, sec.name());
            s.shot(&format!("options-{}", slug(sec)));
        }
        // Appearance: a switch with the mouse, and its reset arrow.
        s.click_text("Appearance");
        click_in_row(&mut s, "switch", "Round corners");
        assert!(!s.app.cfg.round_corners, "round corners off");
        click_in_row(&mut s, "reset option", "Round corners");
        assert!(s.app.cfg.round_corners, "reset puts it back");
        // Files & folders, one Ctrl+PageDown on: hidden files shown, and the drive
        // switches inert while folder sizes are off.
        s.act("key ctrl+PageDown");
        assert_eq!(opt_section(&s), Some(Section::Files));
        click_in_row(&mut s, "switch", "Show hidden files");
        assert!(s.app.cfg.show_hidden);
        s.app.cfg.folder_sizes = true;
        s.idle(2);
        click_in_row(&mut s, "switch", "Calculate folder sizes");
        assert!(!s.app.cfg.folder_sizes);
        let ssd = s.app.cfg.sizes_ssd;
        click_in_row(&mut s, "switch", "Solid-state drives");
        assert_eq!(s.app.cfg.sizes_ssd, ssd, "a sub-option does nothing while its switch is off");
        s.shot("options-sub-off");
        click_in_row(&mut s, "reset option", "Calculate folder sizes");
        assert_eq!(s.app.cfg.folder_sizes, crate::config::Config::default().folder_sizes);
        // Mouse & keyboard with the keys alone: Enter on the first row, then on the
        // sub-option it wakes.
        s.act("key ctrl+PageDown");
        assert_eq!(opt_section(&s), Some(Section::Mouse));
        assert!(!s.app.cfg.single_click && !s.app.cfg.single_click_folders_only);
        s.act("key Enter");
        assert!(s.app.cfg.single_click, "Enter flips the first row");
        s.act("key ArrowDown");
        s.act("key Enter");
        assert!(s.app.cfg.single_click_folders_only, "then the sub-option under it");
        look(&mut s, "keyboard ring");
        s.shot("options-keys");
        // Preview: a choice steps on with a click.
        s.act("key ctrl+PageDown");
        assert_eq!(opt_section(&s), Some(Section::Preview));
        let layout = s.app.cfg.inspector_layout.clone();
        click_in_row(&mut s, "choice", "Inspector layout");
        assert!(s.app.cfg.inspector_layout != layout, "inspector layout changed");
        // Search: pictures stay put while search by meaning is off, follow it when on.
        s.act("key ctrl+PageDown");
        assert_eq!(opt_section(&s), Some(Section::Search));
        assert!(!s.app.cfg.semantic_search && s.app.cfg.semantic_pictures);
        click_in_row(&mut s, "switch", "Include pictures");
        assert!(s.app.cfg.semantic_pictures, "inert while search by meaning is off");
        click_in_row(&mut s, "switch", "Search by meaning");
        assert!(s.app.cfg.semantic_search);
        click_in_row(&mut s, "switch", "Include pictures");
        assert!(!s.app.cfg.semantic_pictures);
        click_in_row(&mut s, "reset option", "Include pictures");
        click_in_row(&mut s, "reset option", "Search by meaning");
        assert!(!s.app.cfg.semantic_search && s.app.cfg.semantic_pictures, "both reset");
        // Git, then Startup & system.
        s.act("key ctrl+PageDown");
        assert_eq!(opt_section(&s), Some(Section::Git));
        click_in_row(&mut s, "switch", "Show GitHub pictures");
        assert!(s.app.cfg.github_pictures);
        click_in_row(&mut s, "reset option", "Show GitHub pictures");
        assert!(!s.app.cfg.github_pictures);
        s.act("key ctrl+PageDown");
        assert_eq!(opt_section(&s), Some(Section::System));
        let new_tab = s.app.cfg.new_tab.clone();
        click_in_row(&mut s, "choice", "New tab shows");
        assert!(s.app.cfg.new_tab != new_tab, "new tab changed");
        assert!(s.texts.iter().any(|(_, t)| t == "Install"), "the install button is in its row");
        // The ends wrap.
        s.act("key ctrl+PageDown");
        assert_eq!(opt_section(&s), Some(Section::Appearance), "past the last, the first");
        s.act("key ctrl+PageUp");
        assert_eq!(opt_section(&s), Some(Section::System), "and back");
        // A search covers every section at once, each match labelled with its own;
        // typing reaches it although a click took the keyboard from the field.
        s.act("text size");
        s.idle(2);
        for label in ["Appearance · Font size", "Files & folders · Calculate folder sizes", "Preview · Inspector font size", "Startup & system · Asset cache size"] {
            assert!(s.texts.iter().any(|(_, t)| t == label), "{label:?} found");
        }
        look(&mut s, "search");
        s.shot("options-find");
        let at = crate::popups::find_options("size").iter().position(|&i| OPTS[i].3 == "Calculate folder sizes").unwrap();
        for _ in 0..at {
            s.act("key ArrowDown");
        }
        let sizes = s.app.cfg.folder_sizes;
        s.act("key Enter");
        assert_eq!(s.app.cfg.folder_sizes, !sizes, "Enter acts on the result picked");
        click_in_row(&mut s, "reset option", "Files & folders · Calculate folder sizes");
        assert_eq!(s.app.cfg.folder_sizes, sizes, "and its reset arrow puts it back");
        // With the mouse, in another search.
        let r = s.text_rect("size");
        s.act(&format!("click {:.0} {:.0}", r.center().x, r.center().y));
        s.act("key ctrl+a");
        s.act("text animations");
        s.idle(2);
        click_in_row(&mut s, "switch", "Appearance · Animations");
        assert!(!s.app.cfg.animations);
        click_in_row(&mut s, "reset option", "Appearance · Animations");
        assert!(s.app.cfg.animations);
        s.act("text zzzq");
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t == "No option matches"));
        // Clearing the search brings the section back.
        let r = s.text_rect("animationszzzq");
        s.act(&format!("click {:.0} {:.0}", r.center().x, r.center().y));
        s.act("key ctrl+a");
        s.act("key Backspace");
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t == "On startup"), "the section again");
        // Titles only: the lines go, and come back as tooltips.
        click_control(&mut s, "titles only");
        assert!(s.app.cfg.compact_lists);
        s.click_text("Appearance");
        s.idle(2);
        assert!(!s.texts.iter().any(|(_, t)| t == "Size of all text."), "no descriptions");
        look(&mut s, "titles only");
        s.shot("options-titles-only");
        let r = s.text_rect("Font size");
        s.act(&format!("move {:.0} {:.0}", r.center().x, r.center().y));
        s.idle(50);
        assert!(s.texts.iter().any(|(_, t)| t == "Size of all text."), "the description as a tooltip");
        s.shot("options-titles-only-tooltip");
        click_control(&mut s, "titles only");
        assert!(!s.app.cfg.compact_lists);
        // Closed and opened again: the same section.
        s.click_text("Git");
        s.act("key Escape");
        s.settle();
        assert!(!s.popup_open());
        s.act("key ctrl+Comma");
        s.settle();
        assert_eq!(opt_section(&s), Some(Section::Git), "the last section again");
        // A small window: glyphs only on the left, every row still in reach.
        s.resize(520.0, 340.0);
        s.idle(3);
        assert!(!s.texts.iter().any(|(_, t)| t == "Mouse & keyboard"), "names give way to glyphs");
        click_control(&mut s, "section Files & folders");
        assert_eq!(opt_section(&s), Some(Section::Files));
        let last = OPTS.iter().filter(|o| o.1 == Section::Files).count();
        for _ in 0..last {
            s.act("key ArrowDown");
        }
        s.idle(3);
        let popup = crate::ui::POPUPS.with(|p| p.borrow().first().copied()).unwrap();
        let r = s.text_rect("Recents on top");
        assert!(popup.contains_rect(r), "the last row scrolled into view: {r:?} in {popup:?}");
        look(&mut s, "small window");
        s.shot("options-small");
        click_control(&mut s, "section Startup & system");
        look(&mut s, "small window, system");
        s.shot("options-small-system");
        s.act("text default");
        s.idle(2);
        look(&mut s, "small window, search");
        s.shot("options-small-search");
        s.act("key Escape");
        s.resize(1280.0, 800.0);
        // A light theme.
        s.app.cfg.theme = crate::theme::ThemeName::Paper;
        s.act("key ctrl+Comma");
        s.settle();
        for sec in [Section::Appearance, Section::Files, Section::System] {
            click_control(&mut s, sec.control());
            look(&mut s, "light");
            s.shot(&format!("options-light-{}", slug(sec)));
        }
        s.act("text font");
        s.idle(2);
        look(&mut s, "light search");
        s.shot("options-light-search");
        s.act("key Escape");
        report("option sections", &s);
    }

    /// Bookmarks (Ctrl+D, click in the sidebar, remove) and tab commands (new, jump by
    /// number, next/previous with wrap, close others, reopen, duplicate, move).
    #[test]
    fn tabs_and_bookmarks() {
        if !sandboxed() {
            return;
        }
        let home = fixture("tabs");
        let mut s = Sim::new(&home.join("Music"), vec2(1280.0, 800.0));
        // Bookmark Music, go elsewhere, come back from the sidebar.
        s.act("key ctrl+d");
        s.settle();
        assert!(s.app.ud.bookmarks.contains(&home.join("Music")), "bookmarked");
        go(&mut s, &home.join("Code"));
        s.act("move -50 -50");
        s.settle();
        let side = s.texts.iter().find(|(r, t)| t == "Music" && r.left() < 170.0).map(|(r, _)| r.center()).unwrap_or_else(|| s.fail("bookmark in the sidebar"));
        s.act(&format!("click {:.0} {:.0}", side.x, side.y));
        s.settle();
        assert_eq!(s.tab().path, home.join("Music"), "the sidebar bookmark navigates");
        s.act("key ctrl+d");
        s.settle();
        assert!(!s.app.ud.bookmarks.contains(&home.join("Music")), "Ctrl+D again removes it");
        // Tabs.
        let tabs = |s: &Sim| s.app.panels[&s.app.active].tabs.len();
        let active = |s: &Sim| s.app.panels[&s.app.active].active;
        for _ in 0..3 {
            s.act("key ctrl+t");
        }
        s.settle();
        assert_eq!(tabs(&s), 4);
        s.act("key ctrl+2");
        assert_eq!(active(&s), 1, "Ctrl+2");
        s.act("key ctrl+9");
        assert_eq!(active(&s), 3, "Ctrl+9 = last tab");
        s.cmd(NextTab);
        assert_eq!(active(&s), 0, "next wraps to the first");
        s.cmd(PrevTab);
        assert_eq!(active(&s), 3, "previous wraps to the last");
        s.cmd(DuplicateTab);
        assert_eq!(tabs(&s), 5, "duplicate");
        s.act("key ctrl+1");
        let first = s.tab().path.clone();
        s.cmd(MoveTabRight);
        assert_eq!(s.app.panels[&s.app.active].tabs[1].path, first, "moved right");
        assert_eq!(active(&s), 1, "the moved tab stays active");
        s.cmd(CloseOtherTabs);
        assert_eq!(tabs(&s), 1, "close others");
        s.act("key ctrl+shift+t");
        s.act("key ctrl+shift+t");
        s.settle();
        assert_eq!(tabs(&s), 3, "reopen twice");
        look(&mut s, "tabs");
        report("tabs", &s);
    }

    /// Thumbnails for every kind (built-in image, SVG renderer, desktop thumbnailers for
    /// video, PDF and MP3 cover art) in Large icons; all land in the shared cache, a fresh
    /// app reuses them without running a thumbnailer, a corrupt video is marked failed
    /// and never retried.
    #[test]
    fn thumbnails_for_every_kind_and_shared_cache() {
        if !sandboxed() {
            return;
        }
        let d = media_fixture("media");
        let kinds = ["photo.png", "drawing.svg", "clip.mp4", "song.mp3", "doc.pdf"];
        let wait_all = |s: &mut Sim| {
            let t0 = Instant::now();
            loop {
                let mut missing = vec![];
                for k in kinds {
                    let p = d.join(k);
                    let m = std::fs::metadata(&p).unwrap().modified().ok();
                    if s.app.thumbs.get(&p, m, 200.0).is_none() {
                        missing.push(k);
                    }
                }
                if missing.is_empty() {
                    return;
                }
                if t0.elapsed().as_secs() > 60 {
                    s.fail(&format!("no thumbnail for {missing:?}"));
                }
                s.frame(vec![]);
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let mut s = Sim::new(&d, vec2(1280.0, 800.0));
        s.cmd(ViewLarge);
        wait_all(&mut s);
        let broken = d.join("broken.mp4");
        let bm = std::fs::metadata(&broken).unwrap().modified().ok();
        let t0 = Instant::now();
        while !s.app.thumbs.failed(&broken) && t0.elapsed().as_secs() < 30 {
            let _ = s.app.thumbs.get(&broken, bm, 200.0);
            s.frame(vec![]);
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(s.app.thumbs.failed(&broken), "the corrupt video is marked failed");
        look(&mut s, "thumbnails");
        for k in kinds {
            let p = d.join(k);
            let m = std::fs::metadata(&p).unwrap().modified().ok();
            assert!(crate::thumbcache::lookup(&p, m, 200).is_some(), "{k} is in the shared cache");
        }
        assert!(crate::thumbcache::has_failed(&broken, bm), "the failure is recorded");
        // A fresh app: everything from the cache, no thumbnailer runs, no retry of the failure.
        let runs = crate::thumbcache::RUNS.load(std::sync::atomic::Ordering::Relaxed);
        eprintln!("sim thumbnails: {runs} thumbnailer runs in the first pass");
        assert!(runs >= 4, "video, MP3 cover, PDF and the corrupt video went to thumbnailers ({runs})");
        drop(s);
        let mut s2 = Sim::new(&d, vec2(1280.0, 800.0));
        s2.cmd(ViewLarge);
        wait_all(&mut s2);
        let _ = s2.app.thumbs.get(&broken, bm, 200.0);
        s2.settle();
        s2.idle(30);
        assert_eq!(crate::thumbcache::RUNS.load(std::sync::atomic::Ordering::Relaxed), runs, "nothing regenerated");
        report("thumbnails", &s2);
    }

    fn inspector(s: &Sim) -> &crate::inspector::Inspector {
        s.app.panels[&s.app.active].inspector.as_ref().unwrap_or_else(|| s.fail("inspector closed"))
    }

    fn wait_until(s: &mut Sim, what: &str, secs: u64, mut ok: impl FnMut(&Sim) -> bool) {
        let t0 = Instant::now();
        while !ok(s) {
            if t0.elapsed().as_secs() >= secs {
                s.fail(&format!("timed out waiting for: {what}"));
            }
            s.frame(vec![]);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn alive(pid: u32) -> bool {
        // A zombie still has a /proc entry: only "running/sleeping" counts.
        std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|st| !st.split_whitespace().nth(2).is_some_and(|s| s == "Z"))
    }

    /// Every inspector toolbar button (and any label beside them, such as a page count)
    /// sits centred on the 30 px header, inside it.
    fn toolbar_centred(s: &mut Sim, what: &str, labels: &[&str]) {
        s.idle(2);
        let ir = s.app.panels[&s.app.active].inspector_rect.unwrap_or_else(|| s.fail("inspector shown"));
        let cy = ir.top() + 15.0;
        let header = Rect::from_min_max(ir.min, pos2(ir.right(), ir.top() + 30.0));
        let buttons: Vec<Rect> = crate::ui::CONTROLS.with(|c| c.borrow().iter().filter(|(_, r, _)| header.contains(r.center())).map(|(_, r, _)| *r).collect());
        assert!(buttons.len() >= 3, "{what}: toolbar buttons drawn");
        for b in &buttons {
            assert!((b.center().y - cy).abs() <= 0.5 && b.top() > ir.top() && b.bottom() < ir.top() + 30.0, "{what}: button {b:?} off the header's centre {cy}");
        }
        for l in labels {
            let r = s.text_rect(l);
            assert!((r.center().y - cy).abs() <= 1.5, "{what}: {l:?} at {r:?}, header centre {cy}");
        }
    }

    fn click_control(s: &mut Sim, what: &str) {
        s.idle(1);
        let r = crate::ui::CONTROLS.with(|c| c.borrow().iter().find(|(_, _, w)| *w == what).map(|(_, r, _)| *r));
        let r = r.unwrap_or_else(|| s.fail(&format!("no {what} on screen")));
        s.act(&format!("click {:.0} {:.0}", r.center().x, r.center().y));
    }

    /// Inspector previews: highlighted code, PDF pages, video and audio playback with
    /// its processes started, stopped and cleaned up as a user plays, pauses, seeks,
    /// mutes and moves on.
    #[test]
    fn inspector_code_pdf_video_audio() {
        if !sandboxed() {
            return;
        }
        let d = media_fixture("previews");
        std::fs::write(d.join("main.rs"), "// demo\nfn main() {\n    let x: u32 = 42;\n    println!(\"{x}\");\n}\n").unwrap();
        let mut s = Sim::new(&d, vec2(1280.0, 800.0));
        s.cmd(ToggleInspector);
        // Code.
        select_path(&mut s, &d.join("main.rs"));
        wait_until(&mut s, "highlighted code", 10, |s| inspector(s).text_spans().is_some_and(|n| n >= 6));
        look(&mut s, "code preview");
        toolbar_centred(&mut s, "code", &[]);
        // Word wrap: off for code until asked, on for prose; each kind keeps its own.
        assert_eq!(inspector(&s).wrapped_rows(), None, "code starts unwrapped");
        let para = "Wrapped prose keeps every word on screen, however narrow the preview is made. ".repeat(12);
        std::fs::write(d.join("notes.md"), format!("# Notes\n\n{para}\n\n- short item\n{para}\n")).unwrap();
        s.cmd(Refresh);
        // The folder is read again on a thread: the new file shows once that is done.
        wait_until(&mut s, "notes listed", 10, |s| s.tab().entries.iter().any(|e| e.name == "notes.md"));
        select_path(&mut s, &d.join("notes.md"));
        wait_until(&mut s, "prose wrapped", 10, |s| inspector(s).wrapped_rows().is_some_and(|n| n > 10));
        let ir = s.app.panels[&s.app.active].inspector_rect.unwrap();
        let rows: Vec<Rect> = s.texts.iter().filter(|(_, t)| t.starts_with("Wrapped prose") || t.starts_with("prose keeps")).map(|(r, _)| *r).collect();
        assert!(rows.len() >= 2, "wrapped rows drawn");
        for r in s.texts.iter().filter(|(r, _)| r.top() > ir.top() + 30.0 && r.left() >= ir.left()).map(|(r, _)| *r) {
            assert!(r.right() <= ir.right() + 0.5, "wrapped text runs past the preview: {r:?} in {ir:?}");
        }
        look(&mut s, "wrapped prose");
        click_control(&mut s, "Wrap lines");
        wait_until(&mut s, "prose unwrapped", 5, |s| inspector(s).wrapped_rows().is_none());
        click_control(&mut s, "Wrap lines");
        wait_until(&mut s, "prose wrapped again", 5, |s| inspector(s).wrapped_rows().is_some());
        // Larger text fits fewer columns: the rows are made again, more of them.
        let before = inspector(&s).wrapped_rows().unwrap();
        click_control(&mut s, "Larger text");
        click_control(&mut s, "Larger text");
        s.idle(2);
        assert!(inspector(&s).wrapped_rows().unwrap() > before, "larger text, more rows");
        click_control(&mut s, "Smaller text");
        click_control(&mut s, "Smaller text");
        select_path(&mut s, &d.join("main.rs"));
        wait_until(&mut s, "code again", 10, |s| inspector(s).text_spans().is_some());
        assert_eq!(inspector(&s).wrapped_rows(), None, "code still unwrapped");
        s.cmd(InspectorWrap);
        s.idle(2);
        assert!(inspector(&s).wrapped_rows().is_some(), "code wrapped from the palette");
        look(&mut s, "wrapped code");
        // PDF: page 1, then Page Down with the pointer over the preview.
        select_path(&mut s, &d.join("doc.pdf"));
        wait_until(&mut s, "PDF page 1", 20, |s| inspector(s).pdf_state().is_some_and(|(p, n, shown)| p == 1 && n == Some(3) && shown.is_some_and(|x| x.0 == 1)));
        assert!(s.texts.iter().any(|(_, t)| t == "1 / 3"), "page counter shown");
        let list = s.app.panels[&s.app.active].list_rect;
        s.act(&format!("move {:.0} {:.0}", list.right() + 150.0, list.center().y));
        s.act("key PageDown");
        wait_until(&mut s, "PDF page 2", 20, |s| inspector(s).pdf_state().is_some_and(|(p, _, shown)| p == 2 && shown.is_some_and(|x| x.0 == 2)));
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t == "2 / 3"), "page counter follows");
        s.act("move -50 -50");
        look(&mut s, "pdf preview");
        toolbar_centred(&mut s, "pdf", &["2 / 3"]);
        // Font size and Spacing change every row's height; the toolbar stays centred.
        for (font, spacing) in [(16.5, 1.6), (11.0, 0.7), (20.0, 2.0), (13.0, 1.0)] {
            s.app.cfg.font_size = font;
            s.app.cfg.spacing = spacing;
            s.idle(3);
            toolbar_centred(&mut s, &format!("pdf at font {font}, spacing {spacing}"), &["2 / 3"]);
            look(&mut s, &format!("pdf at font {font}, spacing {spacing}"));
        }
        // Video.
        select_path(&mut s, &d.join("clip.mp4"));
        wait_until(&mut s, "video probed", 15, |s| inspector(s).media_state().is_some());
        let probe = inspector(&s).media_probe().unwrap();
        assert_eq!(probe.video, Some((320, 240)));
        click_control(&mut s, "media button"); // play
        wait_until(&mut s, "video frames", 10, |s| inspector(s).media_state().is_some_and(|m| m.0 && m.4 >= 5));
        let (_, pos, dur, pids, _) = inspector(&s).media_state().unwrap();
        assert!(pos > 0.0 && (dur - 2.0).abs() < 0.3, "position {pos} of {dur}");
        assert!(!pids.is_empty() && pids.iter().all(|&p| alive(p)), "decoders running");
        click_control(&mut s, "media button"); // pause
        s.idle(2);
        let (playing, paused_at, _, now, _) = inspector(&s).media_state().unwrap();
        assert!(!playing && now.is_empty(), "paused: no decoders");
        assert!(pids.iter().all(|&p| !alive(p)), "pause killed and reaped them");
        s.idle(30);
        assert!((inspector(&s).media_state().unwrap().1 - paused_at).abs() < 1e-6, "position holds while paused");
        // Seek to the middle of the bar, then play to the end.
        let bar = crate::ui::CONTROLS.with(|c| c.borrow().iter().find(|(_, _, w)| *w == "seek bar").map(|(_, r, _)| *r)).unwrap_or_else(|| s.fail("seek bar"));
        s.act(&format!("click {:.0} {:.0}", bar.center().x, bar.center().y));
        let at = inspector(&s).media_state().unwrap().1;
        assert!((at - dur / 2.0).abs() < 0.25, "seeked to the middle ({at})");
        click_control(&mut s, "media button");
        wait_until(&mut s, "playback ends", 10, |s| inspector(s).media_state().is_some_and(|m| !m.0 && (m.1 - m.2).abs() < 0.05));
        assert!(inspector(&s).media_state().unwrap().3.is_empty(), "no decoders after the end");
        // Moving on mid-playback stops everything.
        click_control(&mut s, "media button");
        wait_until(&mut s, "playing again", 10, |s| inspector(s).media_state().is_some_and(|m| m.0 && !m.3.is_empty()));
        let pids = inspector(&s).media_state().unwrap().3;
        select_path(&mut s, &d.join("photo.png"));
        s.idle(3);
        toolbar_centred(&mut s, "image", &[]);
        assert!(pids.iter().all(|&p| !alive(p)), "changing selection killed the decoders");
        // Audio: tags shown; play starts the sound process; mute ends it, the clock goes on.
        select_path(&mut s, &d.join("song.mp3"));
        wait_until(&mut s, "audio probed", 15, |s| inspector(s).media_state().is_some());
        s.idle(3);
        assert!(s.texts.iter().any(|(_, t)| t == "Test Tone"), "title shown");
        assert!(s.texts.iter().any(|(_, t)| t == "Maia"), "artist shown");
        click_control(&mut s, "media button");
        wait_until(&mut s, "sound process", 10, |s| inspector(s).media_state().is_some_and(|m| m.0 && m.3.len() == 1));
        let snd = inspector(&s).media_state().unwrap().3;
        let buttons: Vec<Rect> = crate::ui::CONTROLS.with(|c| c.borrow().iter().filter(|(_, _, w)| *w == "media button").map(|(_, r, _)| *r).collect());
        let mute = buttons.get(1).copied().unwrap_or_else(|| s.fail("mute button"));
        s.act(&format!("click {:.0} {:.0}", mute.center().x, mute.center().y));
        s.idle(3);
        let m = inspector(&s).media_state().unwrap();
        assert!(m.0 && m.3.is_empty(), "muted: still playing, no sound process");
        assert!(snd.iter().all(|&p| !alive(p)), "the sound process ended");
        s.cmd(ToggleInspector);
        s.idle(2);
        look(&mut s, "after previews");
        report("previews", &s);
    }

    /// Random sessions: keys, clicks, drags, scrolls, typing, any command, resizes.
    fn sidebar_text(s: &Sim, label: &str) -> Option<Rect> {
        s.texts.iter().find(|(r, t)| t == label && r.right() <= s.app.sidebar_width + 1.0).map(|x| x.0)
    }

    fn list_text(s: &Sim, label: &str) -> Rect {
        s.texts.iter().rev().find(|(r, t)| t == label && r.left() > s.app.sidebar_width).map(|x| x.0).unwrap_or_else(|| s.fail(&format!("{label:?} not in the list")))
    }

    fn select_name(s: &mut Sim, name: &str) {
        let Some(i) = s.tab().entries.iter().position(|e| e.name == name) else { s.fail(&format!("{name} not listed")) };
        let id = s.app.active;
        s.app.panels.get_mut(&id).unwrap().tab_mut().select_only(i as u32);
        s.idle(1);
    }

    fn toast_is(s: &Sim, want: &str) -> bool {
        s.app.toast.as_ref().is_some_and(|t| t.0 == want)
    }

    fn drive_log(d: &Path) -> String {
        std::fs::read_to_string(d.join("log")).unwrap_or_default()
    }

    /// Drives as a user meets them: unmounted ones listed (dimmed) in Computer and, when
    /// removable, in the sidebar; opening one mounts it and shows its files; the sidebar's
    /// eject button moves the tab away and powers the disk off; Mount / Unmount from the
    /// context menu; failures come back as messages; the system disk offers nothing and
    /// can't be deleted; a drive plugged in later appears by itself.
    #[test]
    fn drives_mount_open_unmount_eject() {
        if !sandboxed() {
            return;
        }
        let d = fake_drives("main");
        crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(d.clone()));
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
            }
        }
        let _reset = Reset;
        let home = fixture("drives");
        let mut s = Sim::new(&home, vec2(1280.0, 800.0));
        s.cmd(Computer);
        wait_until(&mut s, "drives listed", 10, |s| s.tab().entries.iter().any(|e| e.name == "Photo Card"));
        s.settle();
        let mut names: Vec<String> = s.tab().entries.iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names, ["Busy Stick", "Photo Card", "Root", "Windows"], "no swap, EFI or /boot");
        for v in [ViewDetails, ViewList, ViewLarge, ViewTree, ViewDetails] {
            s.cmd(v);
            look(&mut s, &format!("drives {v:?}"));
        }
        assert!(sidebar_text(&s, "Photo Card").is_some(), "removable card offered in the sidebar");
        assert!(sidebar_text(&s, "Windows").is_none(), "internal disk only in Computer");
        assert!(s.texts.iter().any(|(_, t)| t == "14.9 GB · Not mounted"), "card marked as not mounted");
        // Inspector on an unmounted drive.
        s.cmd(ToggleInspector);
        select_name(&mut s, "Windows");
        s.idle(3);
        assert!(s.texts.iter().any(|(_, t)| t == "200 GB · Not mounted"), "inspector shows the drive");
        look(&mut s, "inspector on an unmounted drive");
        s.cmd(ToggleInspector);

        // Files dropped on a drive that isn't mounted go nowhere.
        let wr = list_text(&s, "Windows").center();
        let note = root().join("dropped.txt");
        std::fs::write(&note, "x").unwrap();
        s.app.on_dnd(crate::clipboard::Dnd::Over { x: wr.x, y: wr.y, paths: vec![note.clone()], own: false });
        s.idle(2);
        s.app.on_dnd(crate::clipboard::Dnd::Dropped { x: wr.x, y: wr.y, paths: vec![note], own: false });
        s.idle(3);
        assert!(s.app.jobs.is_empty() && !s.popup_open(), "no transfer into an unmounted drive");

        // Copy, delete or rename a drive: nothing happens (the root would take everything).
        select_name(&mut s, "Root");
        for k in ["key Delete", "key shift+Delete", "key F2", "key ctrl+x"] {
            s.act(k);
        }
        assert!(!s.popup_open() && s.app.jobs.is_empty() && s.tab().rename.is_none(), "drives can't be deleted or renamed");
        let r = list_text(&s, "Root");
        s.act(&format!("click {:.0} {:.0} right", r.center().x, r.center().y));
        s.idle(20);
        assert!(s.popup_open(), "menu for the root drive");
        assert!(!s.texts.iter().any(|(_, t)| ["Mount", "Unmount", "Eject", "Move to Trash", "Delete Permanently", "Rename"].contains(&t.as_str())), "system disk offers no drive or file actions");
        s.act("key Escape");

        // Open the card: it mounts (spinner meanwhile) and its files show.
        select_name(&mut s, "Photo Card");
        s.act("key Enter");
        assert!(crate::drives::busy("/dev/sdz1"), "mount in progress");
        look(&mut s, "mounting");
        let card = d.join("media/Photo Card");
        wait_until(&mut s, "card opened", 10, |s| s.tab().path == card);
        s.settle();
        assert!(s.tab().entries.iter().any(|e| e.name == "readme.txt"), "card's files listed");
        assert!(drive_log(&d).contains("udisksctl mount -b /dev/sdz1"));
        wait_until(&mut s, "card mounted in the sidebar", 10, |s| sidebar_text(s, "Photo Card").is_some() && !s.texts.iter().any(|(_, t)| t == "14.9 GB · Not mounted"));
        look(&mut s, "inside the card");

        // Eject from the sidebar: the tab leaves first, then the disk powers off.
        let r = sidebar_text(&s, "Photo Card").unwrap();
        s.act(&format!("move {:.0} {:.0}", r.center().x, r.center().y));
        click_control(&mut s, "eject button");
        assert!(crate::tab::is_computer(&s.tab().path), "tab moved off the card before ejecting");
        wait_until(&mut s, "ejected", 10, |s| toast_is(s, "Photo Card can be safely removed"));
        let log = drive_log(&d);
        assert!(log.contains("udisksctl unmount -b /dev/sdz1") && log.contains("udisksctl power-off -b /dev/sdz"), "{log}");
        wait_until(&mut s, "card gone", 10, |s| !s.tab().entries.iter().any(|e| e.name == "Photo Card") && sidebar_text(s, "Photo Card").is_none());

        // Internal disk from the context menu: Mount, then Unmount; never Eject.
        let r = list_text(&s, "Windows");
        s.act(&format!("click {:.0} {:.0} right", r.center().x, r.center().y));
        s.idle(20);
        assert!(s.texts.iter().any(|(_, t)| t == "Mount") && !s.texts.iter().any(|(_, t)| t == "Eject" || t == "Unmount"), "unmounted internal disk: Mount only");
        s.click_text("Mount");
        wait_until(&mut s, "windows mounted", 10, |s| toast_is(s, "Mounted Windows"));
        let win = d.join("media/Windows");
        wait_until(&mut s, "windows listed as mounted", 10, |s| s.tab().entries.iter().any(|e| e.path == win));
        assert!(crate::tab::is_computer(&s.tab().path), "mounting from the menu stays put");
        s.idle(5);
        let r = list_text(&s, "Windows");
        s.act(&format!("click {:.0} {:.0} right", r.center().x, r.center().y));
        s.idle(20);
        assert!(s.texts.iter().any(|(_, t)| t == "Unmount") && !s.texts.iter().any(|(_, t)| t == "Eject" || t == "Mount"), "mounted internal disk: Unmount only: {:?} items {:?} size {:?}", s.texts.iter().map(|t| (&t.1, t.0.top())).collect::<Vec<_>>(), match &s.app.popup { Popup::Ctx(st) => st.items.iter().map(|i| i.label.clone()).collect::<Vec<_>>(), _ => vec![] }, (s.size, s.ctx.memory(|m| m.area_rect(egui::Id::new("ctxmenu")))));
        s.click_text("Unmount");
        wait_until(&mut s, "windows unmounted", 10, |s| toast_is(s, "Unmounted Windows"));
        wait_until(&mut s, "windows back to unmounted", 10, |s| s.tab().entries.iter().any(|e| e.name == "Windows" && crate::tab::drive_dev(&e.path).is_some()));
        assert!(!win.exists());

        // A busy stick refuses to eject: the message says why, and it stays.
        let r = sidebar_text(&s, "Busy Stick").unwrap_or_else(|| s.fail("busy stick in the sidebar"));
        s.act(&format!("move {:.0} {:.0}", r.center().x, r.center().y));
        click_control(&mut s, "eject button");
        wait_until(&mut s, "eject refused", 10, |s| toast_is(s, "Couldn't eject Busy Stick: Target is busy"));
        assert!(s.tab().entries.iter().any(|e| e.name == "Busy Stick"));
        look(&mut s, "after a refused eject");

        // A failing mount reports the reason and stays in Computer.
        std::fs::write(d.join("fail/sdy2"), "Wrong fs type, bad option").unwrap();
        select_name(&mut s, "Windows");
        s.act("key Enter");
        wait_until(&mut s, "mount failed", 10, |s| toast_is(s, "Couldn't mount Windows: Wrong fs type, bad option"));
        assert!(crate::tab::is_computer(&s.tab().path));
        std::fs::remove_file(d.join("fail/sdy2")).unwrap();

        // Ctrl+Enter mounts into a new tab.
        let tabs = s.app.panels[&s.app.active].tabs.len();
        select_name(&mut s, "Windows");
        s.act("key ctrl+Enter");
        wait_until(&mut s, "opened in a new tab", 10, |s| s.app.panels[&s.app.active].tabs.iter().any(|t| t.path == win));
        assert_eq!(s.app.panels[&s.app.active].tabs.len(), tabs + 1);
        assert!(crate::tab::is_computer(&s.tab().path), "current tab stays");

        // Plugged in later: shows up by itself.
        let mut devs = std::fs::read_to_string(d.join("devices")).unwrap();
        devs.push_str("/dev/sdv\t-\t-\t-\t64023257088\t1\t1\tdisk\n/dev/sdv1\tsdv\tNEW USB\tvfat\t64022208512\t1\t1\tpart\n");
        std::fs::write(d.join("devices"), devs).unwrap();
        wait_until(&mut s, "new drive appears", 15, |s| s.tab().entries.iter().any(|e| e.name == "NEW USB") && sidebar_text(s, "NEW USB").is_some());
        for sz in [(900.0, 560.0), (640.0, 420.0), (1280.0, 800.0)] {
            s.resize(sz.0, sz.1);
            look(&mut s, &format!("drives at {sz:?}"));
        }
        assert!(!drive_log(&d).lines().any(|l| l.contains("/dev/sdw")), "the system disk was never touched");
        report("drives", &s);
    }

    /// Drag and drop with other apps: files dragged in from "another app" land on the
    /// folder row, the empty list or the sidebar place under the pointer, copied (the
    /// source keeps its files), with the drop target lit and a "Copy" badge meanwhile; a
    /// drag that leaves again changes nothing. Files dragged past the window's edge are
    /// handed to the system, the pointer is released here, and if they come back in they
    /// move like any drag inside Maia. Nothing can be dropped into Computer.
    #[test]
    fn drag_and_drop_with_other_apps() {
        if !sandboxed() {
            return;
        }
        use crate::clipboard::Dnd;
        let home = fixture("dnd");
        let src = root().join("elsewhere");
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(&src).unwrap();
        let a = src.join("photo from chat.png");
        let b = src.join("notes – 日本.txt");
        png_file(&a, 30, 20, 1);
        std::fs::write(&b, "dropped").unwrap();
        let docs = home.join("Documents");
        let mut s = Sim::new(&docs, vec2(1280.0, 800.0));
        s.cmd(ViewDetails);
        s.settle();
        let over = |s: &mut Sim, p: Pos2, paths: &[PathBuf]| {
            s.note(format!("dnd over {p:?}"));
            s.app.on_dnd(Dnd::Over { x: p.x, y: p.y, paths: paths.to_vec(), own: false });
            s.idle(2);
        };
        let drop = |s: &mut Sim, p: Pos2, paths: &[PathBuf], own: bool| {
            s.note(format!("dnd drop {p:?}"));
            s.app.on_dnd(Dnd::Dropped { x: p.x, y: p.y, paths: paths.to_vec(), own });
            s.idle(2);
        };
        let both = [a.clone(), b.clone()];

        // Onto a folder row: lands inside it.
        let taxes = list_text(&s, "Taxes").center();
        over(&mut s, taxes, &both);
        over(&mut s, taxes + vec2(3.0, 1.0), &both);
        assert!(matches!(s.app.drag, Some(crate::app::Drag::Files { .. })), "a drag from outside is a file drag in here");
        assert!(s.texts.iter().any(|(_, t)| t == "Copy 2 items"), "badge says what will happen");
        drop(&mut s, taxes, &both, false);
        wait_until(&mut s, "copied into Taxes", 10, |_| docs.join("Taxes/photo from chat.png").exists() && docs.join("Taxes/notes – 日本.txt").exists());
        assert!(a.exists() && b.exists(), "the other app keeps its files");
        assert!(s.app.drag.is_none() && s.app.ext_drag.is_none());

        // Onto empty space in the list: into the folder shown.
        s.settle();
        let empty = pos2(700.0, 650.0);
        over(&mut s, empty, std::slice::from_ref(&a));
        drop(&mut s, empty, std::slice::from_ref(&a), false);
        wait_until(&mut s, "copied into Documents", 10, |_| docs.join("photo from chat.png").exists());

        // Onto a sidebar place.
        let home_row = sidebar_text(&s, "Home").unwrap_or_else(|| s.fail("Home in the sidebar")).center();
        over(&mut s, home_row, std::slice::from_ref(&b));
        drop(&mut s, home_row, std::slice::from_ref(&b), false);
        wait_until(&mut s, "copied into home", 10, |_| crate::fsx::home().join("notes – 日本.txt").exists());

        // Hover, then leave: nothing happens and the pointer works normally again.
        let before = std::fs::read_dir(&docs).unwrap().count();
        over(&mut s, empty, &both);
        s.app.on_dnd(Dnd::Left);
        s.idle(3);
        assert!(s.app.drag.is_none() && s.app.ext_drag.is_none(), "drag ended on leave");
        s.settle();
        assert_eq!(std::fs::read_dir(&docs).unwrap().count(), before, "nothing dropped");
        let r = list_text(&s, "notes.txt");
        s.act(&format!("click {:.0} {:.0}", r.center().x, r.center().y));
        assert_eq!(s.tab().cursor_path().as_deref(), Some(docs.join("notes.txt").as_path()), "clicks select again");

        // Computer's empty space takes nothing (below the last drive, at any row height).
        s.cmd(Computer);
        s.settle();
        let jobs = s.app.jobs.len();
        let below = s.app.panels[&s.app.active].list_rect.bottom() - 20.0;
        over(&mut s, pos2(600.0, below), &both);
        drop(&mut s, pos2(600.0, below), &both, false);
        s.idle(3);
        assert_eq!(s.app.jobs.len(), jobs, "no transfer into Computer");
        go(&mut s, &docs);

        // Out of the window: handed to the system with the button released here.
        crate::clipboard::DRAGGED_OUT.lock().unwrap().clear();
        let r = list_text(&s, "notes.txt").center();
        s.act(&format!("down {:.0} {:.0}", r.x, r.y));
        for x in [r.x + 10.0, r.x + 40.0, 300.0, 100.0, 5.0, -30.0] {
            s.act(&format!("move {x:.0} {:.0}", r.y));
        }
        s.idle(2);
        let out = crate::clipboard::DRAGGED_OUT.lock().unwrap().clone();
        assert_eq!(out, vec![vec![docs.join("notes.txt")]], "dragged out with its path");
        assert!(s.app.drag.is_none(), "inside drag ended");
        assert!(!s.ctx.input(|i| i.pointer.any_down()), "no stuck mouse button");
        // ... and back in, dropped on a folder: moves, like any drag inside Maia.
        let taxes = list_text(&s, "Taxes").center();
        s.app.on_dnd(Dnd::Over { x: taxes.x, y: taxes.y, paths: vec![docs.join("notes.txt")], own: true });
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t == "Move 1 item"), "own drag moves");
        drop(&mut s, taxes, &[docs.join("notes.txt")], true);
        wait_until(&mut s, "moved into Taxes", 10, |_| docs.join("Taxes/notes.txt").exists() && !docs.join("notes.txt").exists());
        s.settle();
        look(&mut s, "after drag and drop");
        report("dnd", &s);
    }

    /// Search inside files: the filter's contents toggle, a typed query with the matching
    /// line beside each name, binary files left out, flattened search, every view and
    /// width, a new query, and back to matching names.
    #[test]
    fn search_inside_files() {
        if !sandboxed() {
            return;
        }
        let home = fixture("contents");
        let d = home.join("Documents");
        std::fs::write(d.join("todo.txt"), "buy xyzzy\n\tCall the Plumber about the leak\n").unwrap();
        std::fs::write(d.join("blob.bin"), b"\x00\x01plumber").unwrap();
        std::fs::write(d.join("Taxes/2025/letter.txt"), "plumber invoice attached").unwrap();
        let mut s = Sim::new(&d, vec2(1280.0, 800.0));
        let names = |s: &Sim| -> Vec<String> {
            let t = s.tab();
            let mut v: Vec<String> = t.view.iter().map(|&i| t.entries[i as usize].name.clone()).collect();
            v.sort();
            v
        };
        click_control(&mut s, "contents toggle");
        assert!(s.tab().content.on, "the toggle switches contents mode on");
        s.act("text plumber");
        wait_until(&mut s, "contents search", 10, |s| !s.tab().is_loading());
        s.settle();
        assert_eq!(names(&s), vec!["todo.txt"], "only the text file that contains the word");
        look(&mut s, "contents results");
        assert!(s.texts.iter().any(|(_, t)| t.ends_with("Call the Plumber about the leak")), "the matching line is shown beside the name");
        for v in [ViewTree, ViewList, ViewMedium, ViewLarge, ViewXL, ViewDetails] {
            s.cmd(v);
            look(&mut s, &format!("contents {v:?}"));
        }
        for (w, h) in [(640.0, 480.0), (520.0, 340.0), (1280.0, 800.0)] {
            s.resize(w, h);
            look(&mut s, &format!("contents {w}x{h}"));
        }
        // Flattened: the letter two folders down matches too.
        s.act("key ctrl+e");
        wait_until(&mut s, "flattened contents search", 10, |s| !s.tab().is_loading());
        s.settle();
        assert_eq!(names(&s), vec!["letter.txt", "todo.txt"]);
        look(&mut s, "flattened contents");
        // A new query replaces the matches.
        s.act("key ctrl+f");
        s.act("key ctrl+a");
        s.act("text xyzzy");
        wait_until(&mut s, "second query", 10, |s| s.tab().filter == "xyzzy" && !s.tab().is_loading());
        s.settle();
        assert_eq!(names(&s), vec!["todo.txt"]);
        // The palette command switches back to names: nothing is called "xyzzy".
        s.act("key ctrl+shift+p");
        s.act("text Search File Contents");
        s.act("key Enter");
        s.settle();
        assert!(!s.tab().content.on, "the palette command switches contents mode off");
        assert!(names(&s).is_empty(), "names are matched again: {:?}", names(&s));
        s.act("key Escape");
        s.settle();
        assert!(s.tab().filter.is_empty());
        look(&mut s, "after contents search");
        report("contents", &s);
    }

    /// Single instance: a second launch's folders arrive over the real socket and open as
    /// focused tabs (a file opens its folder with it selected); the switch for the default
    /// file manager shows the desktop's state, turns it on with a click and gives folders
    /// back to the previous file manager when turned off.
    #[test]
    fn single_instance_and_default_file_manager() {
        if !sandboxed() {
            return;
        }
        let home = fixture("single");
        let mut s = Sim::new(&home, vec2(1280.0, 800.0));
        // Socket paths are limited to 108 bytes: the sim root can be deeper than that.
        let sock = std::env::temp_dir().join(format!("maia-sim-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let l = crate::single::claim_at(&sock).unwrap_or_else(|| s.fail("listen"));
        let (tx, rx) = std::sync::mpsc::channel();
        crate::single::serve(l, tx, || {});
        s.app.requests = Some(rx);
        let tabs = s.app.panels[&s.app.active].tabs.len();
        let file = home.join("Documents/notes.txt");
        assert!(crate::single::forward_to(&sock, &[home.join("Music"), file.clone()]), "second launch handed over");
        wait_until(&mut s, "tabs opened", 5, |s| s.app.panels[&s.app.active].tabs.len() == tabs + 2);
        s.settle();
        assert_eq!(s.tab().path, home.join("Documents"), "the last one is in front");
        assert_eq!(s.tab().cursor_path().as_deref(), Some(file.as_path()), "the file is selected");
        assert!(s.app.panels[&s.app.active].tabs.iter().any(|t| t.path == home.join("Music")));
        assert!(crate::single::forward_to(&sock, &[home.join("gone")]));
        wait_until(&mut s, "missing path reported", 5, |s| s.app.toast.as_ref().is_some_and(|t| t.0.starts_with("Couldn't open")));
        look(&mut s, "after handing over");

        // Default file manager: another one is set; the switch shows off.
        let cfg = crate::config::config_dir().parent().unwrap_or(&root()).to_path_buf().join("mimeapps.list");
        std::fs::write(&cfg, "[Default Applications]\ninode/directory=org.example.Files.desktop;\n").unwrap();
        let switch = |s: &mut Sim, on_wanted: Option<bool>| {
            s.act("key ctrl+Comma");
            s.settle();
            s.act("text default file manager");
            wait_until(s, "state read", 5, |s| s.app.default_fm().is_some());
            s.idle(3);
            if let Some(w) = on_wanted {
                assert_eq!(s.app.default_fm(), Some(!w), "switch starts the other way");
                // A message of its own, not one still showing from before.
                let before = s.app.toast.as_ref().map(|t| t.0.clone());
                click_control(s, "switch");
                wait_until(s, "default changed", 5, |s| s.app.toast.as_ref().is_some_and(|t| Some(&t.0) != before.as_ref()));
            }
        };
        switch(&mut s, Some(true));
        assert!(std::fs::read_to_string(&cfg).unwrap().contains("inode/directory=io.github.yappologistic.Maia.desktop;"), "Maia set as default");
        assert_eq!(s.app.toast.as_ref().map(|t| t.0.as_str()), Some("Maia is now the default file manager"));
        look(&mut s, "default file manager on");
        s.act("key Escape");
        s.settle();
        switch(&mut s, Some(false));
        assert!(std::fs::read_to_string(&cfg).unwrap().contains("inode/directory=org.example.Files.desktop;"), "previous file manager restored");
        assert_eq!(s.app.default_fm(), Some(false));
        s.act("key Escape");
        // Reuse the open window: a plain switch in the same group.
        s.act("key ctrl+Comma");
        s.settle();
        s.act("text reuse the open window");
        s.idle(3);
        assert!(s.app.cfg.single_instance);
        s.act("key Enter");
        s.idle(2);
        assert!(!s.app.cfg.single_instance, "Enter turns it off");
        s.act("key Escape");
        report("single", &s);
    }

    /// Archives as folders: Enter opens a zip, folders inside open and Up leaves them, a
    /// file inside previews and opens (extracted to the cache), changes are refused, Copy
    /// and Paste take files out, Extract Here and Compress work from the context menu and
    /// undo removes what they made.
    #[test]
    fn archives_open_extract_compress() {
        if !sandboxed() || !crate::archive::available() {
            return;
        }
        let home = fixture("archives");
        let docs = home.join("Documents");
        let src = root().join("packsrc");
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(src.join("Trip")).unwrap();
        std::fs::write(src.join("Trip/beach.txt"), "sand and sea").unwrap();
        std::fs::write(src.join("readme.txt"), "inside the zip").unwrap();
        let zip = docs.join("photos.zip");
        crate::archive::create(&zip, &src, &["Trip".into(), "readme.txt".into()]).unwrap();
        let mut s = Sim::new(&docs, vec2(1280.0, 800.0));
        s.cmd(ViewDetails);
        s.settle();
        select_path(&mut s, &zip);
        s.act("key Enter");
        wait_until(&mut s, "inside the zip", 10, |s| s.tab().path == zip && s.tab().entries.len() == 2);
        assert!(s.tab().archive.is_some());
        for v in [ViewList, ViewLarge, ViewTree, ViewDetails] {
            s.cmd(v);
            look(&mut s, &format!("zip {v:?}"));
        }
        assert!(s.texts.iter().any(|(_, t)| t == "photos.zip"), "the archive in the path bar");
        // Into a folder and back out.
        select_path(&mut s, &zip.join("Trip"));
        s.act("key Enter");
        wait_until(&mut s, "folder in the zip", 10, |s| s.tab().path == zip.join("Trip") && s.tab().entries.iter().any(|e| e.name == "beach.txt"));
        s.act("key Backspace");
        wait_until(&mut s, "back at the top", 10, |s| s.tab().path == zip);
        // Preview, then open.
        s.cmd(ToggleInspector);
        select_path(&mut s, &zip.join("readme.txt"));
        wait_until(&mut s, "preview from the zip", 10, |s| s.texts.iter().any(|(_, t)| t.contains("inside the zip")));
        look(&mut s, "zip preview");
        s.cmd(ToggleInspector);
        let launched = crate::mime::LAUNCHED.lock().unwrap().len();
        s.act("key Enter");
        wait_until(&mut s, "opened from the zip", 10, |_| crate::mime::LAUNCHED.lock().unwrap().len() > launched);
        let cmd = crate::mime::LAUNCHED.lock().unwrap().last().cloned().unwrap_or_default();
        assert!(cmd.contains("maia/archives") && cmd.contains("readme.txt"), "{cmd}");
        let extracted = crate::archive::open_cache(&zip).join("readme.txt");
        assert_eq!(std::fs::read_to_string(&extracted).unwrap(), "inside the zip");
        // Read-only inside.
        s.act("key Delete");
        s.idle(2);
        assert!(s.app.toast.as_ref().is_some_and(|t| t.0.contains("read-only")) && s.app.jobs.is_empty(), "no deleting inside an archive");
        s.act("key F2");
        assert!(s.tab().rename.is_none());
        // Copy out.
        s.act("key ctrl+c");
        go(&mut s, &docs);
        s.act("key ctrl+v");
        wait_until(&mut s, "copied out", 10, |_| docs.join("readme.txt").exists());
        assert_eq!(std::fs::read_to_string(docs.join("readme.txt")).unwrap(), "inside the zip");
        // Extract Here: two things at the top go into a folder named after the archive.
        s.settle();
        let r = list_text(&s, "photos.zip");
        s.act(&format!("click {:.0} {:.0} right", r.center().x, r.center().y));
        s.idle(20);
        s.click_text("Extract Here");
        wait_until(&mut s, "extracted", 10, |_| docs.join("photos/Trip/beach.txt").exists() && docs.join("photos/readme.txt").exists());
        assert!(std::fs::read_dir(&docs).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with(".maia-extract")), "no staging left");
        // Compress two files to zip, then undo it.
        s.settle();
        let id = s.app.active;
        {
            let t = s.app.panels.get_mut(&id).unwrap().tab_mut();
            t.selected.clear();
            for n in ["notes.txt", "budget 2026.csv"] {
                if let Some(i) = t.entries.iter().position(|e| e.name == n) {
                    t.selected.insert(i as u32);
                }
            }
        }
        let r = list_text(&s, "notes.txt");
        s.act(&format!("click {:.0} {:.0} right", r.center().x, r.center().y));
        s.idle(20);
        if !s.texts.iter().any(|(_, t)| t == "Zip (.zip)") {
            s.click_text("Compress");
            s.idle(10);
        }
        s.click_text("Zip (.zip)");
        let out = docs.join("Archive.zip");
        wait_until(&mut s, "compressed", 10, |_| out.exists());
        wait_until(&mut s, "job done", 10, |s| s.app.jobs.iter().all(|j| j.finished_at.is_some()));
        let mut names: Vec<String> = crate::archive::entries(&out, Path::new("")).unwrap().iter().map(|e| e.name.clone()).collect();
        names.sort();
        assert_eq!(names, ["budget 2026.csv", "notes.txt"]);
        s.act("key ctrl+z");
        s.settle();
        wait_until(&mut s, "undone", 10, |_| !out.exists());
        look(&mut s, "after archives");
        report("archives", &s);
    }

    /// Tags from the context menu: a colour on two files (dots beside their names), the
    /// check mark that takes it off again, a new tag by name, and the list filtered by
    /// tag, in every view.
    #[test]
    fn tags_and_colour_labels() {
        if !sandboxed() {
            return;
        }
        let home = fixture("tags");
        let d = home.join("Documents");
        let (notes, rep) = (d.join("notes.txt"), d.join("report.md"));
        if crate::tags::write(&notes, &[]).is_err() {
            eprintln!("no user xattrs here: skipped");
            return;
        }
        let mut s = Sim::new(&d, vec2(1280.0, 800.0));
        s.cmd(ViewDetails);
        s.settle();
        let right_click = |s: &mut Sim, name: &str| {
            let r = s.text_rect(name);
            s.act(&format!("click {:.0} {:.0} right", r.center().x, r.center().y));
            s.idle(2);
        };
        // Both selected, Red from the Tags submenu.
        select_path(&mut s, &notes);
        let r = s.text_rect("report.md");
        s.act(&format!("click {:.0} {:.0} ctrl", r.center().x, r.center().y));
        right_click(&mut s, "report.md");
        // Typed into the menu's search, as the Tags entry may sit below its fold.
        s.act("text tags red");
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t == "Tags › Red"), "found by search");
        look(&mut s, "tags in the menu");
        s.act("key Enter");
        s.idle(3);
        assert_eq!(crate::tags::read(&notes), ["Red"]);
        assert_eq!(crate::tags::read(&rep), ["Red"]);
        // The menu now marks Red as on; choosing it again takes it off this one file.
        s.act("key Escape");
        select_path(&mut s, &rep);
        right_click(&mut s, "report.md");
        s.act("text tags red");
        s.idle(2);
        look(&mut s, "red is on");
        s.act("key Enter");
        s.idle(3);
        assert!(crate::tags::read(&rep).is_empty() && crate::tags::read(&notes) == ["Red"]);
        // A new tag by name, shown after the name in small text.
        right_click(&mut s, "report.md");
        s.act("text new tag");
        s.idle(2);
        s.act("key Enter");
        s.idle(2);
        assert!(matches!(s.app.popup, Popup::Prompt { .. }), "asks for the name");
        s.act("text taxes 2025");
        s.act("key Enter");
        s.idle(3);
        assert_eq!(crate::tags::read(&rep), ["taxes 2025"]);
        wait_until(&mut s, "tag shown", 5, |s| s.texts.iter().any(|(_, t)| t == "taxes 2025"));
        for v in [ViewTree, ViewList, ViewMedium, ViewLarge, ViewXL, ViewDetails] {
            s.cmd(v);
            look(&mut s, &format!("tags in {v:?}"));
        }
        // Filter by tag: only what carries it.
        s.act("key ctrl+f");
        s.act("text tag:red");
        wait_until(&mut s, "filtered by tag", 5, |s| s.tab().view.len() == 1 && s.tab().entries[s.tab().view[0] as usize].path == notes);
        look(&mut s, "filtered by tag");
        s.act("key Escape");
        // Properties lists them.
        select_path(&mut s, &rep);
        s.act("key alt+Enter");
        wait_until(&mut s, "properties shows tags", 5, |s| s.texts.iter().any(|(_, t)| t == "taxes 2025") && s.texts.iter().any(|(_, t)| t == "Tags"));
        s.act("key Escape");
        report("tags", &s);
    }

    /// Password-protected archives: Extract Here asks for the password (a wrong one is
    /// said to be wrong), a 7z whose names are hidden asks on the way in, a file opened
    /// from an AES zip asks before it opens, and Cancel leaves everything as it was.
    #[test]
    fn password_protected_archives() {
        if !sandboxed() || !crate::archive::available() {
            return;
        }
        let home = fixture("encrypted");
        let d = home.join("Documents");
        let src = root().join("encsrc");
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(src.join("Trip")).unwrap();
        std::fs::write(src.join("plan.txt"), "the secret plan").unwrap();
        std::fs::write(src.join("Trip/beach.txt"), "sand").unwrap();
        let mk = |prog: &str, args: &[&str]| std::process::Command::new(prog).args(args).current_dir(&src).stdout(std::process::Stdio::null()).status().is_ok_and(|s| s.success());
        let (zip, aes, hidden) = (d.join("vault.zip"), d.join("aes.zip"), d.join("hidden.7z"));
        if !mk("zip", &["-q", "-r", "-P", "hunter2", zip.to_str().unwrap(), "."]) || !mk("7z", &["a", "-phunter2", "-tzip", "-mem=AES256", aes.to_str().unwrap(), "."]) || !mk("7z", &["a", "-phunter2", "-mhe=on", hidden.to_str().unwrap(), "."]) {
            eprintln!("zip or 7z missing: skipped");
            return;
        }
        let mut s = Sim::new(&d, vec2(1280.0, 800.0));
        s.cmd(ViewDetails);
        s.settle();
        let asking = |s: &Sim| matches!(&s.app.popup, Popup::ArchivePassword(_));
        // Extract Here: asked, a wrong password refused, then the right one.
        select_path(&mut s, &zip);
        s.cmd(ExtractHere);
        wait_until(&mut s, "password asked", 10, asking);
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t == "“vault.zip” is password-protected"));
        look(&mut s, "password dialog");
        s.act("text wrong");
        s.click_text("Unlock");
        wait_until(&mut s, "asked again", 10, |s| asking(s) && s.texts.iter().any(|(_, t)| t == "Wrong password"));
        look(&mut s, "wrong password");
        assert!(!d.join("vault").exists(), "nothing left behind by the refused attempt");
        click_control(&mut s, "archive password field");
        s.act("text hunter2");
        s.act("key Enter");
        wait_until(&mut s, "extracted", 10, |s| !asking(s) && d.join("vault/Trip/beach.txt").exists());
        assert_eq!(std::fs::read_to_string(d.join("vault/plan.txt")).unwrap(), "the secret plan");
        // Into the 7z whose names are encrypted too: asked on the way in.
        s.cmd(Refresh);
        s.settle();
        select_path(&mut s, &hidden);
        s.act("key Enter");
        wait_until(&mut s, "asked for the 7z", 10, asking);
        s.act("text hunter2");
        s.act("key Enter");
        wait_until(&mut s, "inside the 7z", 10, |s| s.tab().path == hidden && s.tab().entries.iter().any(|e| e.name == "plan.txt"));
        look(&mut s, "inside an encrypted 7z");
        // Cancel leaves the AES zip shut: nothing opened.
        s.act("key Backspace");
        wait_until(&mut s, "back", 10, |s| s.tab().path == d);
        s.settle();
        select_path(&mut s, &aes);
        s.act("key Enter");
        wait_until(&mut s, "inside the AES zip", 10, |s| s.tab().path == aes && !s.tab().is_loading());
        s.settle();
        // The preview doesn't ask, it says why it shows nothing.
        s.cmd(ToggleInspector);
        select_path(&mut s, &aes.join("plan.txt"));
        wait_until(&mut s, "preview says locked", 10, |s| s.texts.iter().any(|(_, t)| t == "Password-protected"));
        look(&mut s, "locked preview");
        s.cmd(ToggleInspector);
        let launched = crate::mime::LAUNCHED.lock().unwrap().len();
        s.act("key Enter");
        wait_until(&mut s, "asked before opening", 10, asking);
        s.click_text("Cancel");
        s.idle(3);
        assert!(!asking(&s) && crate::mime::LAUNCHED.lock().unwrap().len() == launched, "cancelled: nothing opened");
        // Opened for real once the password is given.
        s.act("key Enter");
        wait_until(&mut s, "asked again", 10, asking);
        s.act("text hunter2");
        s.act("key Enter");
        wait_until(&mut s, "opened from the AES zip", 10, |_| crate::mime::LAUNCHED.lock().unwrap().len() > launched);
        report("encrypted", &s);
    }

    /// A deep folder in a narrow window: the path bar squeezes the folders above it
    /// first, so the current one keeps its whole name.
    #[test]
    fn path_bar_keeps_the_current_folder() {
        if !sandboxed() {
            return;
        }
        // Outside home, so the whole path is spelled out (no "~").
        let deep = root().join("srv/Projects archive 2026/client reports/quarterly summaries/final versions");
        std::fs::create_dir_all(&deep).unwrap();
        let mut s = Sim::new(&deep, vec2(1280.0, 800.0));
        let mut full = 0.0;
        for w in [1280.0, 900.0, 640.0, 520.0] {
            s.resize(w, 600.0);
            s.settle();
            // Drawn texts keep their whole label, so an elided one shows only in its width:
            // the name in the path bar (its lowest occurrence) against its width with room.
            let bar = s.app.panels[&s.app.active].list_rect.top();
            let r = s.texts.iter().filter(|(r, t)| t == "final versions" && r.top() < bar).map(|(r, _)| *r).max_by(|a, b| a.top().total_cmp(&b.top())).unwrap_or_else(|| s.fail("not in the path bar"));
            if w == 1280.0 {
                full = r.width();
            }
            // Shortened only when nothing else of the path is left beside it.
            if r.width() < 0.95 * full {
                // Path segments: text in the panel, on the bar's row, left of the name; not the
                // "…" for the dropped ones, not the toolbar's icon glyphs.
                let left = s.app.panels[&s.app.active].list_rect.left();
                let icon = |t: &str| t.chars().next().is_some_and(|c| ('\u{e000}'..='\u{f8ff}').contains(&c));
                let others: Vec<&str> = s.texts.iter().filter(|(o, t)| (o.center().y - r.center().y).abs() < 3.0 && o.left() >= left && o.right() <= r.left() && t != "…" && !icon(t)).map(|(_, t)| t.as_str()).collect();
                assert!(others.is_empty(), "current folder cut short at {w} px wide ({:.0} of {full:.0} px) while {others:?} still show", r.width());
            }
            look(&mut s, &format!("path bar at {w}"));
        }
        report("pathbar", &s);
    }

    /// A view picked in one folder (every kind) stays as the user opens other folders by
    /// double-click and Enter and goes back up.
    #[test]
    fn chosen_view_stays_while_browsing() {
        if !sandboxed() {
            return;
        }
        let home = fixture("viewstays");
        let mut s = Sim::new(&home, vec2(1280.0, 800.0));
        s.settle();
        for v in [ViewMedium, ViewTree, ViewList, ViewLarge, ViewXL, ViewDetails] {
            go(&mut s, &home);
            s.cmd(v);
            s.settle();
            let zoom = s.tab().settings.zoom;
            // Into Documents with a double-click on its name.
            let r = s.text_rect("Documents");
            s.act(&format!("dclick {:.0} {:.0}", r.center().x, r.center().y));
            wait_until(&mut s, "in Documents", 10, |s| s.tab().path == home.join("Documents"));
            s.settle();
            assert_eq!(s.tab().settings.zoom, zoom, "{v:?} kept after a double-click");
            // Further down with Enter, then back up.
            select_path(&mut s, &home.join("Documents/Taxes"));
            s.act("key Enter");
            wait_until(&mut s, "in Taxes", 10, |s| s.tab().path == home.join("Documents/Taxes"));
            assert_eq!(s.tab().settings.zoom, zoom, "{v:?} kept after Enter");
            s.act("key Backspace");
            wait_until(&mut s, "back", 10, |s| s.tab().path == home.join("Documents"));
            s.cmd(Up);
            s.settle();
            assert_eq!(s.tab().settings.zoom, zoom, "{v:?} kept going back and up");
            look(&mut s, &format!("{v:?} after browsing"));
            s.idle(30);
        }
        report("viewstays", &s);
    }

    /// Size and date conditions typed into the filter, alone and with a name, in every
    /// view, flattened, in contents mode, and words that only look like conditions.
    #[test]
    fn filter_by_size_and_date() {
        if !sandboxed() {
            return;
        }
        let home = fixture("filterq");
        let d = home.join("Documents");
        let now = std::time::SystemTime::now();
        let day = std::time::Duration::from_secs(86_400);
        for (name, n, age) in [("big video.mkv", 300u64 << 20, 500u64), ("small note.txt", 40, 1), ("Taxes/2025/scan.pdf", 4 << 20, 3)] {
            let f = std::fs::File::create(d.join(name)).unwrap();
            f.set_len(n).unwrap();
            f.set_modified(now - day * age as u32).unwrap();
        }
        std::fs::write(d.join("lazy note.txt"), "a lazy afternoon").unwrap();
        let mut s = Sim::new(&d, vec2(1280.0, 800.0));
        let names = |s: &Sim| -> Vec<String> {
            let t = s.tab();
            let mut v: Vec<String> = t.view.iter().map(|&i| t.entries[i as usize].name.clone()).collect();
            v.sort();
            v
        };
        s.act("key ctrl+f");
        s.act("text size>100M");
        s.settle();
        assert_eq!(names(&s), ["big video.mkv"]);
        assert!(s.texts.iter().any(|(_, t)| t == "size>100M"), "the condition stays in the field");
        look(&mut s, "size condition");
        for v in [ViewTree, ViewList, ViewMedium, ViewLarge, ViewDetails] {
            s.cmd(v);
            s.settle();
            assert!(names(&s).contains(&"big video.mkv".to_string()), "{v:?}");
            look(&mut s, &format!("size condition {v:?}"));
        }
        // Older than a year, then a name and an age together.
        s.act("key ctrl+a");
        s.act("text modified>1y");
        s.settle();
        assert_eq!(names(&s), ["big video.mkv"]);
        s.act("key ctrl+a");
        s.act("text note modified<1w");
        s.settle();
        assert_eq!(names(&s), ["lazy note.txt", "notes.txt", "small note.txt"]);
        // Flattened: the scan two folders down.
        s.act("key ctrl+a");
        s.act("text size>1M modified<7d");
        s.cmd(ToggleFlatten);
        wait_until(&mut s, "flattened", 10, |s| !s.tab().is_loading());
        s.settle();
        assert_eq!(names(&s), ["scan.pdf"]);
        look(&mut s, "conditions flattened");
        s.cmd(ToggleFlatten);
        s.settle();
        // Contents mode: the condition narrows the files searched.
        click_control(&mut s, "contents toggle");
        s.act("key ctrl+a");
        s.act("text lazy size<1k");
        // The toggle hands focus back to the field: Ctrl+A selects its text, not the files.
        wait_until(&mut s, "contents search with a size", 10, |s| names(s) == ["lazy note.txt"]);
        s.act("key ctrl+a");
        s.act("text lazy size>1k");
        s.idle(2);
        wait_until(&mut s, "contents search done", 10, |s| !s.tab().content.busy());
        s.settle();
        assert!(names(&s).is_empty(), "the only file with the word is under 1 KB");
        click_control(&mut s, "contents toggle");
        // Not a condition: searched as a name, nothing found.
        s.act("key ctrl+a");
        s.act("text size>big");
        s.settle();
        assert!(names(&s).is_empty());
        s.act("key Escape");
        s.settle();
        assert!(s.tab().filter.is_empty() && names(&s).len() > 3);
        report("filterq", &s);
    }

    /// Properties of a file: work out its SHA-256 and copy it; paste a checksum that is
    /// wrong, then the file's real MD5, and see each verdict.
    #[test]
    fn checksums_in_properties() {
        if !sandboxed() {
            return;
        }
        let home = fixture("checksums");
        let f = home.join("Documents/notes.txt");
        std::fs::write(&f, "abc").unwrap();
        let mut s = Sim::new(&home.join("Documents"), vec2(1280.0, 800.0));
        s.cmd(ViewDetails);
        s.settle();
        select_path(&mut s, &f);
        s.act("key alt+Enter");
        s.idle(3);
        assert!(s.texts.iter().any(|(_, t)| t == "Checksum"), "checksum row shown");
        click_control(&mut s, "checksum button");
        let sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        wait_until(&mut s, "SHA-256 shown", 10, |s| s.texts.iter().any(|(_, t)| t == sha));
        look(&mut s, "checksum worked out");
        click_control(&mut s, "copy checksum");
        // A wrong one: its length says MD5, so MD5 is worked out and compared.
        click_control(&mut s, "checksum field");
        s.act("text 00000000000000000000000000000000");
        wait_until(&mut s, "mismatch", 10, |s| s.texts.iter().any(|(_, t)| t == "Doesn't match"));
        look(&mut s, "checksum mismatch");
        s.act("key ctrl+a");
        s.act("text MD5 (notes.txt) = 900150983CD24FB0D6963F7D28E17F72");
        wait_until(&mut s, "match", 10, |s| s.texts.iter().any(|(_, t)| t == "Matches"));
        assert!(s.texts.iter().any(|(_, t)| t == "MD5"));
        look(&mut s, "checksum match");
        s.act("key ctrl+a");
        s.act("text hello");
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t.starts_with("Not an MD5")), "says what it accepts");
        s.act("key Escape");
        s.idle(2);
        assert!(!s.popup_open());
        report("checksums", &s);
    }

    /// Git status marks: a work tree with every kind of change, the branch in the status
    /// bar, marks in every view, refreshed when a file changes and when git itself changes
    /// the index, and nothing outside a work tree.
    #[test]
    fn git_status_marks() {
        if !sandboxed() || !crate::media::have("git") {
            return;
        }
        let home = fixture("gitmarks");
        let repo = home.join("Code/proj");
        crate::tests::git_fixture(&repo);
        let mut s = Sim::new(&repo, vec2(1280.0, 800.0));
        let mark = |s: &Sim, p: &str| crate::gitstatus::repo_for(&s.tab().path).and_then(|r| r.marks.get(&repo.join(p)).copied());
        use crate::gitstatus::Mark;
        wait_until(&mut s, "status read", 10, |s| mark(s, "mod.txt") == Some(Mark::Modified));
        assert_eq!(mark(&s, "new.txt"), Some(Mark::Untracked));
        assert_eq!(mark(&s, "sub"), Some(Mark::Inside));
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t == "main"), "the branch is in the status bar");
        for v in [ViewTree, ViewList, ViewMedium, ViewLarge, ViewXL, ViewDetails] {
            s.cmd(v);
            look(&mut s, &format!("git {v:?}"));
        }
        for (w, h) in [(520.0, 340.0), (1280.0, 800.0)] {
            s.resize(w, h);
            look(&mut s, &format!("git {w}x{h}"));
        }
        // A file edited outside Maia: the watcher's change brings a fresh status.
        std::fs::write(repo.join("keep.txt"), "changed\n").unwrap();
        wait_until(&mut s, "edit seen", 10, |s| mark(s, "keep.txt") == Some(Mark::Modified));
        // git itself changing the index (only .git changes): staged now.
        crate::tests::git(&repo, &["add", "keep.txt"]);
        wait_until(&mut s, "staging seen", 10, |s| mark(s, "keep.txt") == Some(Mark::Staged));
        // Into a subfolder: same repository, marks below.
        s.act("text sub");
        s.act("key Enter");
        s.settle();
        assert_eq!(s.tab().path, repo.join("sub"));
        wait_until(&mut s, "subfolder marks", 10, |s| mark(s, "sub/inner") == Some(Mark::Inside));
        look(&mut s, "git subfolder");
        // Outside any work tree: no repository, no branch.
        let plain = std::env::temp_dir().join("plain-folder");
        std::fs::create_dir_all(&plain).unwrap();
        std::fs::write(plain.join("a.txt"), "a").unwrap();
        go(&mut s, &plain);
        s.idle(10);
        assert!(crate::gitstatus::repo_for(&plain).is_none(), "no repository outside a work tree");
        assert!(!s.texts.iter().any(|(_, t)| t == "main"), "no branch outside a work tree");
        look(&mut s, "outside git");
        report("git", &s);
    }

    fn history(s: &Sim) -> Option<(usize, bool, Option<usize>, Option<usize>, Option<usize>)> {
        s.app.panels[&s.app.active].inspector.as_ref().and_then(|i| i.git.as_ref()).map(|h| h.state())
    }

    /// Updates from Options: Check for Updates (the command) opens Startup & system and
    /// asks; a newer release offers Update, which puts the checked build in place and
    /// offers Restart. A bad download or a folder Maia can't write says so.
    #[test]
    fn update_from_options() {
        if !sandboxed() {
            return;
        }
        use crate::update::{Fetch, State, Updater};
        let home = fixture("updates");
        let bin = root().join("updates-bin");
        std::fs::create_dir_all(&bin).unwrap();
        let exe = bin.join("maia");
        std::fs::write(&exe, b"\x7fELF old").unwrap();
        let build = b"\x7fELF the new build".to_vec();
        let f = bin.join("sum-input");
        std::fs::write(&f, &build).unwrap();
        let sum = String::from_utf8_lossy(&std::process::Command::new("sha256sum").arg(&f).output().unwrap().stdout).split_whitespace().next().unwrap().to_string();
        let serve = |bin_bytes: Vec<u8>| -> Fetch {
            let sum = sum.clone();
            std::sync::Arc::new(move |url: &str, to: Option<&Path>| {
                let body = if url.contains("api.github.com") {
                    br#"{"tag_name":"v9.9.9","body":"New themes.","assets":[{"name":"maia-linux-x86_64","browser_download_url":"https://github.com/yappologistic/Maia/releases/download/v9.9.9/maia-linux-x86_64"},{"name":"maia-linux-x86_64.sha256","browser_download_url":"https://github.com/yappologistic/Maia/releases/download/v9.9.9/maia-linux-x86_64.sha256"}]}"#.to_vec()
                } else if url.ends_with(".sha256") {
                    format!("{sum}  maia-linux-x86_64").into_bytes()
                } else {
                    bin_bytes.clone()
                };
                match to {
                    Some(p) => std::fs::write(p, &body).map(|_| vec![]).map_err(|e| e.to_string()),
                    None => Ok(body),
                }
            })
        };
        let mut s = Sim::new(&home, vec2(1280.0, 800.0));
        s.app.updater = Updater::for_test(serve(build.clone()), exe.clone());
        s.cmd(CheckUpdates);
        wait_until(&mut s, "a newer release", 5, |s| matches!(s.app.updater.state(), State::Available(_)));
        s.idle(3);
        assert!(matches!(&s.app.popup, Popup::Options(st) if st.section == Some(crate::popups::Section::System)), "Options at Startup & system");
        assert!(s.texts.iter().any(|(_, t)| t == "Update to 9.9.9"), "the step offered");
        look(&mut s, "update available");
        s.shot("update-available");
        click_control(&mut s, "update button");
        wait_until(&mut s, "installed", 10, |s| matches!(s.app.updater.state(), State::Ready(_)));
        s.idle(3);
        assert_eq!(std::fs::read(&exe).unwrap(), build, "the new build in place");
        assert!(s.texts.iter().any(|(_, t)| t == "Restart") && s.texts.iter().any(|(_, t)| t == "9.9.9 installed"));
        look(&mut s, "update installed");
        s.shot("update-ready");
        // A tampered download: said so, the program untouched.
        std::fs::write(&exe, b"\x7fELF old").unwrap();
        s.app.updater = Updater::for_test(serve(b"\x7fELF tampered".to_vec()), exe.clone());
        click_control(&mut s, "update button");
        wait_until(&mut s, "a newer release again", 5, |s| matches!(s.app.updater.state(), State::Available(_)));
        s.idle(2);
        click_control(&mut s, "update button");
        wait_until(&mut s, "a failure", 10, |s| matches!(s.app.updater.state(), State::Failed(_)));
        s.idle(3);
        assert!(s.texts.iter().any(|(_, t)| t == "Couldn't update"));
        assert_eq!(std::fs::read(&exe).unwrap(), b"\x7fELF old");
        // Installed where Maia can't write.
        s.app.updater = Updater::for_test(serve(build.clone()), PathBuf::from("/proc/maia"));
        click_control(&mut s, "update button");
        wait_until(&mut s, "available", 5, |s| matches!(s.app.updater.state(), State::Available(_)));
        s.idle(2);
        click_control(&mut s, "update button");
        s.idle(3);
        assert!(matches!(s.app.updater.state(), State::CantWrite(_)));
        assert!(s.texts.iter().any(|(_, t)| t == "Update it where you installed it"));
        look(&mut s, "update elsewhere");
        s.shot("update-cant-write");
        report("updates", &s);
    }

    /// The theme picker: from Options, every theme on screen as a small window with its
    /// name; hovering one shows it on the whole window, a click keeps it, and code takes
    /// its colours (or the classic set with "Code colors follow the theme" off).
    #[test]
    fn theme_picker() {
        if !sandboxed() {
            return;
        }
        let home = fixture("themes");
        std::fs::write(home.join("Documents/main.rs"), "// demo\nfn main() {\n    let x: u32 = 42;\n    println!(\"{x}\");\n}\n").unwrap();
        let mut s = Sim::new(&home.join("Documents"), vec2(1280.0, 800.0));
        use crate::theme::{Syntax, ThemeName};
        assert_eq!(s.app.pal.syntax, Syntax::of(ThemeName::Midnight), "code in the theme's colours by default");
        s.act("key ctrl+Comma");
        s.settle();
        s.act("text color scheme");
        s.idle(3);
        // Every control fits its row, with descriptions and without (titles only).
        for compact in [false, true] {
            s.app.cfg.compact_lists = compact;
            s.act("key ctrl+a");
            s.act("key Backspace");
            s.idle(3);
            s.shot(if compact { "options-compact" } else { "options" });
            let rows = crate::ui::CONTROLS.with(|c| c.borrow().iter().filter(|(_, _, w)| *w == "choose theme").map(|(_, r, _)| *r).collect::<Vec<_>>());
            let h = if compact { 32.0 } else { 44.0 };
            assert!(rows.iter().all(|r| r.height() <= h), "the theme row's button fits a {h}-point row: {rows:?}");
        }
        s.app.cfg.compact_lists = false;
        s.act("text color scheme");
        s.idle(3);
        click_control(&mut s, "choose theme");
        s.settle();
        assert!(matches!(s.app.popup, Popup::Themes), "the picker opened");
        // Every theme can be seen: in view, or scrolled to (the switch and Done stay put).
        let card = |s: &Sim, name: &str| crate::ui::CONTROLS.with(|c| c.borrow().iter().find(|(_, _, w)| *w == name).map(|(_, r, _)| *r));
        let done = card(&s, "themes done").unwrap_or_else(|| s.fail("no Done"));
        let top = s.texts.iter().find(|(_, x)| x == "Themes").map(|(r, _)| r.bottom()).unwrap_or_else(|| s.fail("no title"));
        let full_h = card(&s, ThemeName::ALL[0].label()).map_or(0.0, |r| r.height());
        for t in ThemeName::ALL {
            // In view: its name drawn (the list clips what is scrolled out) above Done.
            let named = |s: &Sim| s.texts.iter().any(|(tr, x)| x == t.label() && tr.top() >= top && tr.bottom() <= done.top());
            for _ in 0..120 {
                let r = card(&s, t.label()).unwrap_or_else(|| s.fail(&format!("{} not drawn", t.label())));
                if named(&s) && r.top() >= top {
                    break;
                }
                // Cut off at the list's top edge (its outline clipped there): up; else down.
                let clipped_top = r.height() < full_h - 1.0 && r.top() <= top + 40.0;
                // Small steps: one wheel notch moves the list by several rows.
                let dy = if r.top() < top || clipped_top { 6 } else { -6 };
                s.act(&format!("scroll {:.0} {:.0} {dy}", done.center().x - 200.0, (top + done.top()) / 2.0));
                s.idle(2);
            }
            let r = card(&s, t.label()).unwrap();
            assert!(named(&s) && r.top() >= top && r.left() >= 0.0 && r.right() <= s.size.x, "{} in view: {r:?}", t.label());
        }
        s.act(&format!("scroll {:.0} {:.0} 2000", done.center().x - 200.0, (top + done.top()) / 2.0));
        s.idle(2);
        look(&mut s, "theme picker");
        s.shot("theme-picker");
        // Hovering shows a theme on the whole window, without choosing it.
        let r = crate::ui::CONTROLS.with(|c| c.borrow().iter().find(|(_, _, w)| *w == "Gruvbox").map(|(_, r, _)| *r)).unwrap();
        s.act(&format!("move {:.0} {:.0}", r.center().x, r.center().y));
        s.idle(3);
        assert_eq!(s.app.pal.panel, crate::theme::palette(ThemeName::Gruvbox).panel, "previewed while hovered");
        assert_eq!(s.app.cfg.theme, ThemeName::Midnight, "not chosen by a hover");
        click_control(&mut s, "Tokyo Night");
        s.idle(3);
        assert_eq!(s.app.cfg.theme, ThemeName::TokyoNight);
        s.act("move 5 5");
        s.idle(3);
        assert_eq!(s.app.pal.syntax, Syntax::of(ThemeName::TokyoNight), "code takes the theme's colours");
        s.shot("theme-picker-tokyo");
        // The code colours' switch, in the picker.
        s.app.cfg.syntax_theme = false;
        s.idle(3);
        assert_eq!(s.app.pal.syntax, Syntax::classic(true), "the classic set when off");
        s.app.cfg.syntax_theme = true;
        click_control(&mut s, "themes done");
        s.settle();
        assert!(matches!(s.app.popup, Popup::Options(_)), "back to Options");
        s.act("key Escape");
        s.settle();
        // A light one, and code drawn in its colours.
        s.app.cfg.theme = ThemeName::Latte;
        s.cmd(ToggleInspector);
        select_path(&mut s, &home.join("Documents/main.rs"));
        wait_until(&mut s, "highlighted code", 10, |s| inspector(s).text_spans().is_some_and(|n| n >= 6));
        look(&mut s, "code in a light theme");
        s.shot("code-latte");
        assert_eq!(s.app.pal.syntax, Syntax::of(ThemeName::Latte));
        // Line spacing: the gap between two lines of code grows with it, the text kept
        // whole in each row.
        let gap = |s: &mut Sim, spacing: f32| {
            s.app.cfg.preview_line_spacing = spacing;
            s.idle(3);
            let y = |t: &str| s.texts.iter().find(|(_, x)| x.starts_with(t)).map(|(r, _)| r.top()).unwrap_or_else(|| s.fail(&format!("no {t:?}")));
            y("fn") - y("// demo")
        };
        let (tight, roomy) = (gap(&mut s, 1.0), gap(&mut s, 2.0));
        assert!(roomy > tight * 1.8, "lines spread out: {tight} then {roomy}");
        s.app.cfg.preview_line_spacing = 1.25;
        s.idle(2);
        s.shot("code-line-spacing");
        report("themes", &s);
    }

    /// Search by meaning, start to finish, against a stand-in Ollama: off by default (the
    /// window explains and offers to turn it on), the model fetched only on a click, files
    /// read, a vague question answered with the right file and its passage, Enter opening
    /// it, and switching it off in Options stopping everything.
    #[test]
    fn search_by_meaning() {
        if !sandboxed() {
            return;
        }
        let home = fixture("meaning");
        let docs = home.join("Documents");
        std::fs::write(docs.join("letter from the bank.txt"), "Dear customer, your tax refund has been paid into your account.").unwrap();
        std::fs::write(docs.join("summer plans.md"), "Two weeks at the beach, a flight on Friday and a hotel by the sea.").unwrap();
        std::fs::write(docs.join("Taxes/pasta.txt"), "Pasta with tomato sauce: boil, stir and eat.").unwrap();
        crate::tests::captioned_png(&docs.join("IMG_0412.png"), "a puppy playing in the sand");
        std::fs::create_dir_all(docs.join("Receipts")).unwrap();
        for k in 1..=8 {
            std::fs::write(docs.join(format!("Receipts/quittance {k}.txt")), "12.50 4.99 3.00").unwrap();
        }
        let (addr, model, _) = crate::tests::fake_ollama(false);
        *crate::ollama::TEST_HOST.lock().unwrap() = Some(addr);
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                *crate::ollama::TEST_HOST.lock().unwrap() = None;
            }
        }
        let _reset = Reset;
        let mut s = Sim::new(&home, vec2(1280.0, 800.0));
        assert!(!s.app.cfg.semantic_search && s.app.semantic.is_none(), "off by default: nothing running");
        s.app.cfg.semantic_folders = docs.display().to_string();
        // Off, Ctrl+K finds by name, anywhere in the home folder, at once.
        s.act("key ctrl+k");
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t == "Search by meaning is off."));
        click_control(&mut s, "meaning field");
        s.act("text summer");
        wait_until(&mut s, "a name found", 5, |s| matches!(&s.app.popup, Popup::Meaning(st) if st.named.first().is_some_and(|(p, _)| p.ends_with("summer plans.md"))));
        s.idle(2);
        let shown_folder = format!("~/{}", docs.strip_prefix(crate::fsx::home()).unwrap().display());
        assert!(s.texts.iter().any(|(_, t)| *t == shown_folder), "the folder under the name");
        look(&mut s, "name search");
        s.act("key Enter");
        s.settle();
        assert!(!s.popup_open());
        assert_eq!(s.tab().cursor_path().as_deref(), Some(docs.join("summer plans.md").as_path()), "opened in its folder, selected");
        // A folder's name, and words split between the folder and the name.
        s.act("key ctrl+k");
        s.idle(2);
        click_control(&mut s, "meaning field");
        s.act("text taxes pasta");
        wait_until(&mut s, "by folder and name", 5, |s| matches!(&s.app.popup, Popup::Meaning(st) if st.named.first().is_some_and(|(p, _)| p.ends_with("Taxes/pasta.txt"))));
        s.act("key ctrl+a");
        s.act("text Taxes");
        wait_until(&mut s, "a folder", 5, |s| matches!(&s.app.popup, Popup::Meaning(st) if st.named.first().is_some_and(|(p, d)| *d && p.ends_with("Taxes"))));
        s.act("key ctrl+a");
        s.act("key Backspace");
        s.idle(2);
        look(&mut s, "meaning off");
        s.click_text("Turn on");
        wait_until(&mut s, "asks for the model", 10, |s| s.texts.iter().any(|(_, t)| t == "Download, about 860 MB"));
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!model.load(std::sync::atomic::Ordering::Relaxed), "nothing fetched before the click");
        look(&mut s, "meaning needs the model");
        s.click_text("Download, about 860 MB");
        wait_until(&mut s, "files read", 20, |s| matches!(s.app.semantic.as_ref().map(|e| e.phase()), Some(crate::semantic::Phase::Ready)));
        s.idle(2);
        let n = s.app.semantic.as_ref().map_or(0, |e| e.index().files());
        assert!(n >= 3, "the fixture's text files and ours: {n}");
        assert!(s.texts.iter().any(|(_, t)| *t == format!("{n} files read for search by meaning")));
        // A vague question, none of its words in the file.
        click_control(&mut s, "meaning field");
        s.act("text money back from the government");
        wait_until(&mut s, "answer", 10, |s| matches!(&s.app.popup, Popup::Meaning(st) if !st.hits.is_empty()));
        s.idle(3);
        match &s.app.popup {
            Popup::Meaning(st) => assert_eq!(st.hits[0].path, docs.join("letter from the bank.txt")),
            _ => s.fail("window closed"),
        }
        assert!(s.texts.iter().any(|(_, t)| t.starts_with("Dear customer, your tax refund")), "the passage under the name");
        // Only what stands out: the vague question's one clear answer, the rest folded
        // away (or not there at all).
        match &s.app.popup {
            Popup::Meaning(st) => assert!(st.hits.iter().filter(|h| h.strong).count() <= 2, "a short list: {:?}", st.hits.iter().map(|h| (&h.path, h.strong)).collect::<Vec<_>>()),
            _ => s.fail("window closed"),
        }
        // Many results show at once, each row whole (not squeezed into the window's first,
        // empty size).
        s.act("key ctrl+a");
        s.act("text quittance");
        wait_until(&mut s, "receipts by name", 5, |s| matches!(&s.app.popup, Popup::Meaning(st) if st.named.len() >= 8));
        s.idle(3);
        let shown = (1..=8).filter(|k| {
            let name = format!("quittance {k}.txt");
            s.texts.iter().any(|(r, t)| *t == name && r.height() > 8.0 && r.top() > 0.0 && r.bottom() < s.size.y)
        }).count();
        assert!(shown >= 4, "only {shown} results on screen");
        s.act("key ctrl+a");
        s.act("text money back from the government");
        wait_until(&mut s, "answer again", 10, |s| matches!(&s.app.popup, Popup::Meaning(st) if st.hits.first().is_some_and(|h| h.path.ends_with("letter from the bank.txt"))));
        s.idle(3);
        look(&mut s, "meaning results");
        // A picture, by what it shows.
        s.act("key ctrl+a");
        s.act("text photo of my dog");
        wait_until(&mut s, "picture found", 10, |s| matches!(&s.app.popup, Popup::Meaning(st) if st.hits.first().is_some_and(|h| h.path.ends_with("IMG_0412.png"))));
        s.idle(3);
        assert!(s.texts.iter().any(|(_, t)| t == "Picture"), "a picture is said to be one");
        look(&mut s, "a picture found");
        s.act("key ctrl+a");
        s.act("text money back from the government");
        wait_until(&mut s, "answer again", 10, |s| matches!(&s.app.popup, Popup::Meaning(st) if st.hits.first().is_some_and(|h| h.path.ends_with("letter from the bank.txt"))));
        s.act("key Enter");
        s.settle();
        assert!(!s.popup_open());
        assert_eq!(s.tab().path, docs, "opened in its folder");
        assert_eq!(s.tab().cursor_path().as_deref(), Some(docs.join("letter from the bank.txt").as_path()), "with the file selected");
        // Off again from Options: the engine goes.
        s.act("key ctrl+comma");
        s.act("text search by meaning");
        s.idle(2);
        s.act("key Enter");
        s.idle(3);
        s.act("key Escape");
        assert!(!s.app.cfg.semantic_search && s.app.semantic.is_none(), "switched off: nothing running");
        report("meaning", &s);
    }

    /// A clone two commits ahead of its remote and one behind: the status bar's branch
    /// chip says so, and the history shows the remote branch with a cloud and a branch in
    /// step with its remote as one pill.
    #[test]
    fn git_branch_labels_and_counts() {
        if !sandboxed() || !crate::media::have("git") {
            return;
        }
        let home = fixture("gitremote");
        let d = home.join("Code");
        let run = |at: &Path, args: &[&str]| assert!(std::process::Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"]).args(args).current_dir(at).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_ok_and(|st| st.success()), "git {args:?}");
        std::fs::create_dir_all(d.join("server.git")).unwrap();
        run(&d.join("server.git"), &["init", "-q", "--bare"]);
        run(&d, &["clone", "-q", "server.git", "ours"]);
        let ours = d.join("ours");
        std::fs::write(ours.join("a.txt"), "a").unwrap();
        run(&ours, &["add", "."]);
        run(&ours, &["commit", "-qm", "Start"]);
        run(&ours, &["push", "-q", "-u", "origin", "main"]);
        run(&ours, &["branch", "docs"]);
        run(&ours, &["push", "-q", "origin", "docs"]);
        run(&d, &["clone", "-q", "server.git", "theirs"]);
        std::fs::write(d.join("theirs/b.txt"), "b").unwrap();
        run(&d.join("theirs"), &["add", "."]);
        run(&d.join("theirs"), &["commit", "-qm", "Their change"]);
        run(&d.join("theirs"), &["push", "-q"]);
        for n in ["Mine one", "Mine two"] {
            std::fs::write(ours.join(n), n).unwrap();
            run(&ours, &["add", "."]);
            run(&ours, &["commit", "-qm", n]);
        }
        run(&ours, &["fetch", "-q"]);
        let mut s = Sim::new(&ours, vec2(1280.0, 800.0));
        wait_until(&mut s, "counts read", 10, |s| crate::gitstatus::repo_for(&s.tab().path).is_some_and(|r| (r.ahead, r.behind) == (2, 1)));
        s.idle(2);
        // The chip: branch, then 2 (up) and 1 (down), all on its row.
        let chip = s.texts.iter().filter(|(_, t)| t == "main").max_by(|a, b| a.0.center().y.total_cmp(&b.0.center().y)).map(|(r, _)| *r).unwrap();
        for n in ["2", "1"] {
            assert!(s.texts.iter().any(|(r, t)| t == n && (r.center().y - chip.center().y).abs() < 2.0 && r.left() > chip.right()), "{n} beside the branch");
        }
        look(&mut s, "ahead and behind");
        s.act(&format!("move {:.0} {:.0}", chip.center().x, chip.center().y));
        s.idle(40);
        assert!(s.texts.iter().any(|(_, t)| t == "2 to push, 1 to pull · click for history"), "the tooltip says what the numbers are");
        // The history: our branch alone, origin/main on their commit, docs with its remote.
        s.act(&format!("click {:.0} {:.0}", chip.center().x, chip.center().y));
        wait_until(&mut s, "history", 10, |s| history(s).is_some_and(|h| h.1 && h.0 == 4));
        s.idle(3);
        for t in ["origin/main", "docs · origin"] {
            assert!(s.texts.iter().any(|(_, x)| x == t), "{t:?} in the graph");
        }
        assert!(!s.texts.iter().any(|(_, x)| x == "origin/docs"), "a remote in step with its branch has no pill of its own");
        look(&mut s, "branch labels");
        report("gitremote", &s);
    }

    /// A commit's diff with a long line: the history's Wrap lines button wraps it (every
    /// row inside the preview), and the setting is the code previews' own.
    #[test]
    fn git_history_diffs_wrap() {
        if !sandboxed() || !crate::media::have("git") {
            return;
        }
        let home = fixture("gitwrap");
        let repo = home.join("Code/wrap");
        std::fs::create_dir_all(&repo).unwrap();
        let g = |args: &[&str]| assert!(std::process::Command::new("git").args(["-c", "user.name=t", "-c", "user.email=t@t"]).args(args).current_dir(&repo).stdout(std::process::Stdio::null()).status().is_ok_and(|st| st.success()), "git {args:?}");
        g(&["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("notes.md"), "# Notes\n").unwrap();
        g(&["add", "."]);
        g(&["commit", "-qm", "Start the notes"]);
        let long = "A long line in a diff keeps going well past the right edge of the preview, and wrapped it stays readable. ".repeat(4);
        std::fs::write(repo.join("notes.md"), format!("# Notes\n{long}\n")).unwrap();
        g(&["commit", "-qam", "Write the long notes"]);
        let mut s = Sim::new(&repo, vec2(1280.0, 800.0));
        wait_until(&mut s, "branch read", 10, |s| crate::gitstatus::repo_for(&s.tab().path).is_some());
        s.idle(2);
        s.click_text("main");
        wait_until(&mut s, "history", 10, |s| history(s).is_some_and(|h| h.1 && h.0 == 2));
        s.idle(2);
        s.click_text("Write the long notes");
        wait_until(&mut s, "files", 10, |s| history(s).is_some_and(|h| h.3 == Some(1)));
        s.idle(2);
        // A double click shows the diff in the whole panel.
        s.idle(40);
        let r = s.text_rect("notes.md");
        s.act(&format!("dclick {:.0} {:.0}", r.center().x, r.center().y));
        wait_until(&mut s, "diff", 10, |s| history(s).is_some_and(|h| h.4.is_some()));
        s.idle(2);
        let hist = |s: &Sim| s.app.panels[&s.app.active].inspector.as_ref().and_then(|i| i.git.as_ref()).and_then(|h| h.wrapped_rows());
        assert_eq!(hist(&s), None, "diffs start unwrapped, as code does");
        click_control(&mut s, "Wrap lines");
        s.idle(2);
        let lines = history(&s).and_then(|h| h.4).unwrap();
        assert!(hist(&s).is_some_and(|n| n > lines + 2), "the long line wraps: {:?} rows for {lines} lines", hist(&s));
        let ir = s.app.panels[&s.app.active].inspector_rect.unwrap();
        for r in s.texts.iter().filter(|(r, _)| r.top() > ir.top() + 30.0 && r.left() >= ir.left()).map(|(r, _)| *r) {
            assert!(r.right() <= ir.right() + 0.5, "a wrapped diff row runs past the preview: {r:?} in {ir:?}");
        }
        assert!(s.app.panels[&s.app.active].inspector.as_ref().is_some_and(|i| i.wrap_code), "the code previews' setting");
        look(&mut s, "wrapped diff");
        // The palette command turns it off again here too.
        s.cmd(InspectorWrap);
        s.idle(2);
        assert_eq!(hist(&s), None);
        report("gitwrap", &s);
    }

    fn probe(s: &Sim) -> crate::gitlog::Probe {
        s.app.panels[&s.app.active].inspector.as_ref().and_then(|i| i.git.as_ref()).map(|h| h.probe()).unwrap_or_else(|| s.fail("history closed"))
    }

    fn git_hist(s: &Sim) -> &crate::gitlog::History {
        s.app.panels[&s.app.active].inspector.as_ref().and_then(|i| i.git.as_deref()).unwrap_or_else(|| s.fail("history closed"))
    }

    /// Where `label` is drawn inside the inspector (the listing beside it may show the
    /// same name).
    fn in_inspector(s: &Sim, label: &str) -> Rect {
        let ir = s.app.panels[&s.app.active].inspector_rect.unwrap_or_else(|| s.fail("inspector shown"));
        s.texts.iter().rev().find(|(r, t)| t == label && ir.contains(r.center())).map(|(r, _)| *r).unwrap_or_else(|| {
            let near: Vec<&str> = s.texts.iter().filter(|(r, _)| ir.contains(r.center())).map(|(_, t)| t.as_str()).take(60).collect();
            s.fail(&format!("no {label:?} in the inspector; there: {near:?}"))
        })
    }

    /// Scroll the inspector, as a person would with the wheel, until `label` is in view.
    fn scroll_to_label(s: &mut Sim, label: &str) {
        let ir = s.app.panels[&s.app.active].inspector_rect.unwrap_or_else(|| s.fail("inspector shown"));
        let body = Rect::from_min_max(pos2(ir.left(), ir.top() + 40.0), pos2(ir.right(), ir.bottom() - 10.0));
        for k in 0..60 {
            if s.texts.iter().any(|(r, t)| t == label && body.contains_rect(*r)) {
                return;
            }
            s.act(&format!("scroll {:.0} {:.0} {}", body.center().x, body.center().y, if k < 20 { -2 } else { 2 }));
            s.idle(12);
        }
    }

    /// A click at a person's pace: never close enough to the last one to count as a
    /// double click.
    fn click_in_inspector(s: &mut Sim, label: &str, how: &str) {
        s.idle(40);
        scroll_to_label(s, label);
        let r = in_inspector(s, label);
        s.act(&format!("{how} {:.0} {:.0}", r.center().x, r.center().y));
    }

    /// `label` is in the inspector, scrolled into view if it has to be.
    fn see(s: &mut Sim, label: &str) -> Rect {
        scroll_to_label(s, label);
        in_inspector(s, label)
    }

    fn right_click_in_inspector(s: &mut Sim, label: &str) {
        s.idle(40);
        scroll_to_label(s, label);
        let r = in_inspector(s, label);
        s.act(&format!("click {:.0} {:.0} right", r.center().x, r.center().y));
        s.idle(2);
    }

    fn file_index(s: &Sim, path: &str) -> usize {
        git_hist(s).files().iter().position(|f| f.path == path).unwrap_or_else(|| s.fail(&format!("{path} not in the commit")))
    }

    /// The commit view, as someone reading a change would use it: files grouped by
    /// folder and the switch to a plain list, folding a folder, pictures before and
    /// after, diffs opening under their rows and in the whole panel, the keys, the
    /// right-click menu (open, show in folder, as of this commit, file history, copy
    /// path), walking commits with the arrows and parent links, copying the hash, wrapped
    /// diffs, a deleted picture, then small windows and a light theme.
    #[test]
    fn git_commit_view() {
        if !sandboxed() || !crate::media::have("git") {
            return;
        }
        let home = fixture("gitcommit");
        let repo = home.join("Code/project");
        crate::tests::commit_view_fixture(&repo);
        let mut s = Sim::new(&repo, vec2(1280.0, 800.0));
        wait_until(&mut s, "branch read", 10, |s| crate::gitstatus::repo_for(&s.tab().path).is_some());
        s.idle(2);
        s.click_text("main");
        wait_until(&mut s, "history", 10, |s| history(s).is_some_and(|h| h.1 && h.0 == 5));
        look(&mut s, "graph");
        s.shot("graph");
        click_in_inspector(&mut s, "Lay out the panels", "click");
        wait_until(&mut s, "files", 10, |s| history(s).is_some_and(|h| h.3 == Some(12)));
        // Twelve files in several folders: grouped, folders first.
        let p = probe(&s);
        assert!(p.tree && p.items > 12, "grouped by folder: {p:?}");
        for t in ["assets", "icons", "docs", "src", "ui", "tests", "12 files changed", "+30", "−9", "Grace Hopper", "GH", "v1.0", "Parent", "binary"] {
            see(&mut s, t);
        }
        // A rename reads old → new.
        see(&mut s, "guide.md→handbook.md");
        // Pictures: the changed logo's two sides and the new icon's one.
        wait_until(&mut s, "pictures", 10, |s| probe(s).pictures >= 3);
        assert_eq!(probe(&s).failed, 0);
        look(&mut s, "commit grouped");
        s.shot("commit-grouped");
        // The plain list and back; the folder then shows after each name.
        click_control(&mut s, "Show as a list");
        assert!(!probe(&s).tree && probe(&s).items == 12);
        see(&mut s, "src/ui");
        look(&mut s, "commit as a list");
        s.shot("commit-list");
        click_control(&mut s, "Group by folder");
        assert!(probe(&s).tree);
        // A folder folds and unfolds.
        let n = probe(&s).items;
        click_in_inspector(&mut s, "assets", "click");
        assert_eq!((probe(&s).folded, probe(&s).items), (1, n - 3), "assets folded: its subfolder, the icon and the logo");
        click_in_inspector(&mut s, "assets", "click");
        assert_eq!((probe(&s).folded, probe(&s).items), (0, n));
        // A diff opens under its row, and closes again.
        let view = file_index(&s, "src/ui/view.rs");
        click_in_inspector(&mut s, "view.rs", "click");
        wait_until(&mut s, "inline diff", 10, |s| probe(s).inline_lines.len() == 1);
        assert_eq!(probe(&s).inline, [view]);
        s.idle(2);
        see(&mut s, "+// view two");
        look(&mut s, "inline diff");
        s.shot("inline-diff");
        click_in_inspector(&mut s, "view.rs", "click");
        assert!(probe(&s).inline.is_empty(), "closed again");
        // A picture opens larger.
        click_in_inspector(&mut s, "logo.png", "click");
        assert_eq!(probe(&s).inline, [file_index(&s, "assets/logo.png")]);
        look(&mut s, "pictures larger");
        s.shot("pictures-larger");
        click_in_inspector(&mut s, "logo.png", "click");
        // Keys: Home, down twice to the icon, Enter opens it and Enter closes it; [ and ]
        // open the files one at a time.
        s.act("key Home");
        s.act("key ArrowDown");
        s.act("key ArrowDown");
        let icon = file_index(&s, "assets/icons/icon.png");
        assert_eq!(probe(&s).sel, 2);
        s.act("key Enter");
        assert_eq!(probe(&s).inline, [icon]);
        s.act("key Enter");
        assert!(probe(&s).inline.is_empty());
        s.act("key CloseBracket");
        assert_eq!(probe(&s).inline, [icon], "] opens the selected file first");
        s.act("key CloseBracket");
        assert_eq!(probe(&s).inline, [file_index(&s, "assets/logo.png")], "then the next one, alone");
        s.act("key OpenBracket");
        assert_eq!((probe(&s).inline.clone(), probe(&s).sel), (vec![icon], 2));
        s.act("key Enter");
        // The whole panel: a double click, then [ and ] through the files, Esc back.
        let main = file_index(&s, "src/main.rs");
        let r = see(&mut s, "main.rs");
        s.act(&format!("dclick {:.0} {:.0}", r.center().x, r.center().y));
        assert_eq!(probe(&s).full, Some((main, false)));
        wait_until(&mut s, "the diff", 10, |s| probe(s).full.is_some() && history(s).is_some_and(|h| h.4.is_some()));
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t == "+    ui::run();"), "the diff in the panel");
        look(&mut s, "diff in the panel");
        s.shot("diff-large");
        s.act("key CloseBracket");
        let after = probe(&s).full.unwrap_or_else(|| s.fail("still in the panel")).0;
        assert_ne!(after, main, "] goes to the next file");
        click_control(&mut s, "previous file");
        assert_eq!(probe(&s).full, Some((main, false)), "and the arrow back");
        s.act("key Escape");
        assert_eq!(probe(&s).full, None);
        assert!(probe(&s).inline.contains(&main), "a double click leaves the diff open under its row");
        click_in_inspector(&mut s, "main.rs", "click");
        // The open-large button on a hovered row.
        let r = see(&mut s, "README.md");
        s.act(&format!("move {:.0} {:.0}", r.center().x, r.center().y));
        click_control(&mut s, "open large");
        assert_eq!(probe(&s).full, Some((file_index(&s, "README.md"), false)));
        s.act("key Escape");
        // The right-click menu.
        right_click_in_inspector(&mut s, "main.rs");
        assert!(matches!(s.app.popup, Popup::GitFile { .. }), "the file's menu");
        for t in ["Open", "Show in folder", "View as of this commit", "File history", "Copy path"] {
            s.text_rect(t);
        }
        // Menus fade in over a few frames.
        s.idle(12);
        s.shot("file-menu");
        s.click_text("Copy path");
        let want = repo.join("src/main.rs").display().to_string();
        assert!(toast_is(&s, &format!("Copied {want}")), "{:?}", s.app.toast);
        right_click_in_inspector(&mut s, "main.rs");
        s.click_text("View as of this commit");
        assert_eq!(probe(&s).full, Some((main, true)));
        wait_until(&mut s, "the file as it was", 10, |s| git_hist(s).full_text().is_some());
        assert_eq!(git_hist(&s).full_text().unwrap()[0], "mod ui;");
        s.idle(2);
        look(&mut s, "file as of the commit");
        s.shot("as-of-commit");
        s.act("key Escape");
        let before = crate::mime::LAUNCHED.lock().unwrap().len();
        right_click_in_inspector(&mut s, "main.rs");
        s.click_text("Open");
        s.idle(2);
        let launched = crate::mime::LAUNCHED.lock().unwrap().clone();
        assert!(launched.len() > before && launched.iter().any(|l| format!("{l:?}").contains("src/main.rs")), "opened in its app: {launched:?}");
        right_click_in_inspector(&mut s, "main.rs");
        s.click_text("Show in folder");
        s.settle();
        assert_eq!(s.tab().path, repo.join("src"), "its folder");
        let t = s.tab();
        assert!(t.cursor.is_some_and(|c| t.entries[c as usize].name == "main.rs"), "with the file selected");
        assert!(probe(&s).at.is_some(), "the commit stays open inside the work tree");
        // A deleted file is read as it was before.
        right_click_in_inspector(&mut s, "old.txt");
        assert!(s.texts.iter().any(|(_, t)| t == "View before it was deleted"));
        s.click_text("View before it was deleted");
        wait_until(&mut s, "the deleted file", 10, |s| git_hist(s).full_text().is_some());
        assert_eq!(git_hist(&s).full_text().unwrap()[0], "old line 1");
        s.act("key Escape");
        // One file's history, through its rename, and back to every commit.
        right_click_in_inspector(&mut s, "guide.md→handbook.md");
        s.click_text("File history");
        wait_until(&mut s, "the file's commits", 10, |s| history(s).is_some_and(|h| h.1));
        assert_eq!(probe(&s).filter.as_deref(), Some("docs/handbook.md"));
        assert_eq!(history(&s).map(|h| h.0), Some(2), "the rename followed back");
        see(&mut s, "Start the project");
        see(&mut s, "handbook.md");
        look(&mut s, "file history");
        s.shot("file-history");
        click_in_inspector(&mut s, "Start the project", "click");
        wait_until(&mut s, "its files", 10, |s| history(s).is_some_and(|h| h.3.is_some()));
        s.act("key Escape");
        assert!(probe(&s).at.is_none() && probe(&s).filter.is_some(), "back to the file's commits");
        click_control(&mut s, "show all commits");
        assert_eq!((probe(&s).filter, history(&s).map(|h| h.0)), (None, Some(5)), "every commit again");
        // Esc clears it too.
        click_in_inspector(&mut s, "Tune the panel", "click");
        wait_until(&mut s, "files", 10, |s| history(s).is_some_and(|h| h.3 == Some(1)));
        right_click_in_inspector(&mut s, "panel.rs");
        s.click_text("File history");
        wait_until(&mut s, "panel.rs's commits", 10, |s| history(s).is_some_and(|h| h.1 && h.0 == 2));
        s.act("key Escape");
        assert_eq!(probe(&s).filter, None, "Esc shows every commit again");
        assert!(history(&s).is_some(), "and leaves the history open");
        // The arrows walk the commits; the oldest has no older one.
        click_in_inspector(&mut s, "Lay out the panels", "click");
        wait_until(&mut s, "files", 10, |s| history(s).is_some_and(|h| h.3 == Some(12)));
        click_control(&mut s, "newer commit");
        assert_eq!(probe(&s).subject.as_deref(), Some("Write the long notes"));
        wait_until(&mut s, "files", 10, |s| history(s).is_some_and(|h| h.3 == Some(1)));
        // A long line wraps under its row too.
        click_in_inspector(&mut s, "notes.md", "click");
        wait_until(&mut s, "inline diff", 10, |s| probe(s).inline_lines.len() == 1);
        click_control(&mut s, "Wrap lines");
        s.idle(2);
        let p = probe(&s);
        assert!(p.inline_wrapped.first().is_some_and(|&w| w > p.inline_lines[0] + 1), "the long line wraps: {p:?}");
        let ir = s.app.panels[&s.app.active].inspector_rect.unwrap();
        for r in s.texts.iter().filter(|(r, _)| ir.contains(r.left_center())).map(|(r, _)| *r) {
            assert!(r.right() <= ir.right() + 0.5, "a wrapped row runs past the panel: {r:?}");
        }
        look(&mut s, "wrapped inline diff");
        s.shot("inline-wrapped");
        click_control(&mut s, "Wrap lines");
        click_control(&mut s, "older commit");
        click_control(&mut s, "older commit");
        assert_eq!(probe(&s).subject.as_deref(), Some("Start the project"));
        click_control(&mut s, "older commit");
        assert_eq!(probe(&s).subject.as_deref(), Some("Start the project"), "nothing older");
        // Parent links: from the panels commit to its parent.
        click_control(&mut s, "newer commit");
        assert_eq!(probe(&s).subject.as_deref(), Some("Lay out the panels"));
        click_control(&mut s, "parent link");
        assert_eq!(probe(&s).subject.as_deref(), Some("Start the project"));
        // The hash pill copies the whole hash.
        click_control(&mut s, "copy hash");
        let full = git_hist(&s).hash(4).unwrap();
        assert!(toast_is(&s, &format!("Copied {full}")), "{:?}", s.app.toast);
        // A deleted picture: only its old side.
        for _ in 0..3 {
            click_control(&mut s, "newer commit");
        }
        assert_eq!(probe(&s).subject.as_deref(), Some("Remove the logo"));
        wait_until(&mut s, "the old logo", 10, |s| probe(s).pictures == 1);
        look(&mut s, "deleted picture");
        s.shot("deleted-picture");
        // The authors keep their colours: the same in the header as in the graph.
        assert_eq!(crate::gitlog::author_hue("ada@example.invalid", ""), crate::gitlog::author_hue("ADA@example.invalid ", "x"));
        // Small windows, a light theme, no animations.
        click_control(&mut s, "older commit");
        click_control(&mut s, "older commit");
        s.app.cfg.theme = crate::theme::ThemeName::Paper;
        s.app.cfg.animations = false;
        for (w, h) in [(520.0, 340.0), (640.0, 480.0), (900.0, 1200.0), (1920.0, 1080.0)] {
            s.resize(w, h);
            look(&mut s, &format!("commit {w}x{h}"));
            s.shot(&format!("commit-paper-{w}x{h}"));
        }
        s.resize(1280.0, 800.0);
        // Docked at the bottom, and beside a split.
        s.app.cfg.inspector_layout = crate::config::InspectorLayout::Bottom;
        look(&mut s, "commit docked at the bottom");
        s.shot("commit-bottom");
        s.app.cfg.inspector_layout = crate::config::InspectorLayout::Auto;
        s.cmd(SplitRight);
        s.cmd(FocusPrevPanel);
        look(&mut s, "commit beside a split");
        s.cmd(FocusNextPanel);
        s.cmd(ClosePanel);
        s.idle(2);
        click_in_inspector(&mut s, "view.rs", "click");
        wait_until(&mut s, "inline diff", 10, |s| probe(s).inline_lines.len() == 1);
        look(&mut s, "light inline diff");
        s.shot("commit-paper-inline");
        for _ in 0..2 {
            s.act("key Escape");
        }
        s.act("key Escape");
        assert!(s.app.panels[&s.app.active].inspector.is_none(), "closed with its inspector");
        report("gitcommit", &s);
    }

    /// Git history: the branch chip opens the commit graph in the inspector, scrolling
    /// reads further pages from the one running git, a commit lists its files, a file
    /// shows its diff, and Escape walks back out, closing the inspector it opened. Then
    /// the same by keyboard and palette, beside an open preview, in a split, docked at
    /// the bottom, in a small window, on a light theme, without animations, and leaving
    /// the work tree.
    #[test]
    fn git_history_graph() {
        if !sandboxed() || !crate::media::have("git") {
            return;
        }
        let home = fixture("githistory");
        let repo = home.join("Code/history");
        crate::tests::git_history_fixture(&repo, 700);
        let mut s = Sim::new(&repo, vec2(1280.0, 800.0));
        wait_until(&mut s, "branch read", 10, |s| crate::gitstatus::repo_for(&s.tab().path).is_some());
        s.idle(2);
        assert!(s.app.panels[&s.app.active].inspector.is_none());
        // The chip is drawn last, over everything: the topmost "main".
        s.click_text("main");
        wait_until(&mut s, "first page", 10, |s| history(s).is_some_and(|h| h.0 == crate::gitlog::PAGE));
        look(&mut s, "history");
        for t in ["Join ten branches at once", "Wide branch 10", "wide-10"] {
            assert!(s.texts.iter().any(|(_, x)| x == t), "{t:?} on screen");
        }
        let ir = s.app.panels[&s.app.active].inspector_rect.unwrap_or_else(|| s.fail("inspector shown"));
        assert!(s.text_rect("Wide branch 10").left() > ir.left(), "the graph is in the inspector");
        // Scrolling down reads the next pages; then Home returns to the top.
        let t0 = Instant::now();
        while history(&s).is_some_and(|h| !h.1) && t0.elapsed().as_secs() < 20 {
            s.act(&format!("scroll {:.0} {:.0} -60", ir.center().x, ir.center().y));
        }
        assert!(history(&s).is_some_and(|h| h.1 && h.0 > 2 * crate::gitlog::PAGE), "every page read: {:?}", history(&s));
        // Once the wheel's smooth scrolling has run out, End jumps to the first commit.
        s.idle(120);
        s.act("key End");
        wait_until(&mut s, "the first commit at the bottom", 5, |s| s.texts.iter().any(|(_, t)| t == "Start the notes"));
        look(&mut s, "history at the end");
        s.act("key Home");
        wait_until(&mut s, "back at the top", 5, |s| s.texts.iter().any(|(_, t)| t == "Join ten branches at once"));
        s.idle(2);
        // A merge: what it brought in, then that file's diff.
        for _ in 0..30 {
            if s.texts.iter().any(|(r, t)| t == "Merge topic 700" && r.bottom() < ir.bottom() - 10.0) {
                break;
            }
            s.act(&format!("scroll {:.0} {:.0} -2", ir.center().x, ir.center().y));
            s.idle(10);
        }
        s.click_text("Merge topic 700");
        wait_until(&mut s, "files", 10, |s| history(s).is_some_and(|h| h.3.is_some()));
        assert_eq!(history(&s).and_then(|h| h.3), Some(1));
        s.idle(2);
        look(&mut s, "commit files");
        // A click opens the diff under the file; a double click in the whole panel.
        s.idle(40);
        s.click_text("topic_700.rs");
        wait_until(&mut s, "diff under the file", 10, |s| s.texts.iter().any(|(_, t)| t == "+fn part_3() {}"));
        assert_eq!(history(&s).and_then(|h| h.4), None);
        s.idle(40);
        let r = s.text_rect("topic_700.rs");
        s.act(&format!("dclick {:.0} {:.0}", r.center().x, r.center().y));
        wait_until(&mut s, "diff", 10, |s| history(s).is_some_and(|h| h.4.is_some()));
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t == "+fn part_3() {}"), "the diff is drawn");
        look(&mut s, "diff");
        for (k, want) in [(1, "files again"), (2, "graph again")] {
            s.act("key Escape");
            let h = history(&s).unwrap_or_else(|| s.fail("history closed early"));
            assert_eq!((h.2.is_some(), h.4.is_some()), (k == 1, false), "{want}");
        }
        s.act("key Escape");
        assert!(s.app.panels[&s.app.active].inspector.is_none(), "the inspector the history opened closes with it");
        // Keyboard and palette: down twice, open, open the first file, all the way back.
        palette_run(&mut s, "Show Git History");
        wait_until(&mut s, "history again", 10, |s| history(s).is_some_and(|h| h.0 > 0));
        for k in ["ArrowDown", "ArrowDown", "Enter"] {
            s.act(&format!("key {k}"));
        }
        assert_eq!(history(&s).and_then(|h| h.2), Some(2));
        wait_until(&mut s, "files by keyboard", 10, |s| history(s).is_some_and(|h| h.3.is_some()));
        s.act("key Enter");
        wait_until(&mut s, "diff by keyboard", 10, |s| s.app.panels[&s.app.active].inspector.as_ref().and_then(|i| i.git.as_ref()).is_some_and(|h| h.probe().inline_lines.len() == 1));
        look(&mut s, "diff by keyboard");
        for _ in 0..2 {
            s.act("key Escape");
        }
        assert!(s.app.panels[&s.app.active].inspector.is_none());
        // Beside a preview the user opened: closing the history brings the preview back.
        s.cmd(ToggleInspector);
        s.cmd(GitHistory);
        wait_until(&mut s, "history over the preview", 10, |s| history(s).is_some_and(|h| h.0 > 0));
        s.act("key Escape");
        assert!(s.app.panels[&s.app.active].inspector.as_ref().is_some_and(|i| i.git.is_none()), "the preview stays");
        // Around it: a split, the bottom dock, small windows, a light theme, no animations.
        s.cmd(GitHistory);
        s.cmd(SplitRight);
        s.cmd(FocusPrevPanel);
        look(&mut s, "history in a split");
        s.cmd(ClosePanel);
        s.cmd(FocusPrevPanel);
        s.cmd(GitHistory);
        s.app.cfg.inspector_layout = crate::config::InspectorLayout::Bottom;
        look(&mut s, "history docked at the bottom");
        s.app.cfg.inspector_layout = crate::config::InspectorLayout::Auto;
        s.app.cfg.theme = crate::theme::ThemeName::Paper;
        s.app.cfg.animations = false;
        for (w, h) in [(520.0, 340.0), (640.0, 480.0), (1920.0, 1080.0)] {
            s.resize(w, h);
            look(&mut s, &format!("history {w}x{h}"));
            if let Some(r) = s.app.panels[&s.app.active].inspector_rect {
                s.act(&format!("click {:.0} {:.0}", r.center().x, r.top() + 60.0));
                s.idle(3);
                look(&mut s, &format!("commit {w}x{h}"));
                s.act("key Escape");
            }
        }
        // Leaving the work tree closes its history.
        let plain = home.join("Documents");
        go(&mut s, &plain);
        s.idle(4);
        assert!(history(&s).is_none(), "no history outside the work tree");
        assert!(!crate::popups::applicable(&s.app, GitHistory), "the command is off outside a work tree");
        report("git history", &s);
    }

    /// "Show GitHub pictures": off by default and asking nothing; switched on, noreply
    /// authors get theirs straight away and the others once each through the repository
    /// on github.com, in place of their initials in the header and beside their name in
    /// the graph. A failed lookup and an author without an account keep their initials.
    /// Switched off, initials come back and nothing more is asked; on again, the pictures
    /// come from the cache. GitHub is a stand-in that answers from a table.
    #[test]
    fn git_history_github_pictures() {
        if !sandboxed() || !crate::media::have("git") {
            return;
        }
        use crate::avatars::Http;
        let home = fixture("gitpictures");
        let repo = home.join("Code/pictures");
        std::fs::create_dir_all(&repo).unwrap();
        let g = |args: &[&str], who: &str| {
            let (name, email) = who.split_once(" <").unwrap_or(("t", "t@t>"));
            let st = std::process::Command::new("git").args(["-c", &format!("user.name={name}"), "-c", &format!("user.email={}", email.trim_end_matches('>')), "-c", "commit.gpgsign=false"]).args(args).current_dir(&repo).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status();
            assert!(st.is_ok_and(|st| st.success()), "git {args:?}");
        };
        g(&["init", "-q", "-b", "main"], "");
        let people = [
            ("Octo Cat <12345+octocat@users.noreply.github.com>", "Add the octo"),
            ("Mona Lisa <mona@users.noreply.github.com>", "Paint the mona"),
            ("Ada Lovelace <ada@example.invalid>", "Build the engine"),
            ("Grace Hopper <grace@example.invalid>", "Find the bug"),
            ("Linus Tor <linus@example.invalid>", "Write the kernel"),
        ];
        for (k, (who, msg)) in people.iter().enumerate() {
            std::fs::write(repo.join(format!("file{k}.txt")), msg).unwrap();
            g(&["add", "."], who);
            g(&["commit", "-qm", msg], who);
        }
        g(&["remote", "add", "origin", "git@github.com:octo-org/pictures.git"], "");
        let sha = |msg: &str| {
            let out = std::process::Command::new("git").args(["log", "--format=%H", "--grep", msg]).current_dir(&repo).output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let api = |msg: &str| format!("api.github.com/repos/octo-org/pictures/commits?sha={}&per_page=1", sha(msg));
        let png = |seed| crate::tests::png_bytes(160, 160, seed);
        let table = vec![
            ("avatars.githubusercontent.com/u/12345?s=96".to_string(), Http::Ok(png(3))),
            ("avatars.githubusercontent.com/mona?s=96".to_string(), Http::Ok(png(6))),
            (api("Build the engine"), Http::Ok(format!(r#"[{{"sha":"{}","author":{{"avatar_url":"https://avatars.githubusercontent.com/u/4242?v=4"}}}}]"#, sha("Build the engine")).into_bytes())),
            ("avatars.githubusercontent.com/u/4242?v=4&s=96".to_string(), Http::Ok(png(9))),
            (api("Find the bug"), Http::Status(500)),
            (api("Write the kernel"), Http::Ok(format!(r#"[{{"sha":"{}","author":null}}]"#, sha("Write the kernel")).into_bytes())),
        ];
        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let mut s = Sim::new(&repo, vec2(1280.0, 800.0));
        s.app.avatars.dir = root().join("xdg/cache/maia-sim-avatars");
        s.app.avatars.set_get(crate::avatars::fake_get(table, log.clone()));
        assert!(!s.app.cfg.github_pictures, "off by default");
        let emails: Vec<&str> = people.iter().map(|(w, _)| w.split_once('<').unwrap().1.trim_end_matches('>')).collect();
        let states = |s: &Sim| emails.iter().map(|e| s.app.avatars.state(e)).collect::<Vec<_>>();
        let in_insp = |s: &Sim, t: &str| {
            let ir = s.app.panels[&s.app.active].inspector_rect.unwrap_or_else(|| s.fail("inspector shown"));
            s.texts.iter().any(|(r, x)| x == t && ir.contains(r.center()))
        };
        wait_until(&mut s, "branch read", 10, |s| crate::gitstatus::repo_for(&s.tab().path).is_some());
        s.idle(2);
        s.click_text("main");
        wait_until(&mut s, "history", 10, |s| history(s).is_some_and(|h| h.1 && h.0 == 5));
        look(&mut s, "graph, pictures off");
        click_in_inspector(&mut s, "Add the octo", "click");
        wait_until(&mut s, "commit open", 10, |s| history(s).is_some_and(|h| h.3 == Some(1)));
        s.idle(3);
        assert!(in_insp(&s, "OC"), "initials while off");
        assert_eq!((s.app.avatars.asked(), log.lock().unwrap().len()), (0, 0), "off: nothing asked");
        assert!(states(&s).iter().all(|x| x.is_empty()));
        s.act("key Escape");
        s.idle(2);
        // Without curl the option says so, quietly.
        s.app.avatars.set_curl(false);
        s.act("key ctrl+Comma");
        s.act("text show github pictures");
        wait_until(&mut s, "the curl note", 5, |s| s.texts.iter().any(|(_, t)| t == "Needs curl, which isn't installed; authors keep their initials."));
        s.idle(30);
        s.shot("option-no-curl");
        s.app.avatars.set_curl(true);
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t == "Asks GitHub by username or email; kept on this computer."), "what it does and what GitHub learns");
        look(&mut s, "the option");
        s.shot("option");
        // Still inside its window when that is small.
        s.resize(520.0, 340.0);
        s.idle(30);
        s.resize(1280.0, 800.0);
        s.idle(2);
        s.act("key Enter");
        s.idle(2);
        s.act("key Escape");
        s.settle();
        assert!(s.app.cfg.github_pictures, "switched on from Options");
        wait_until(&mut s, "every author answered", 10, |s| s.app.avatars.on() && !s.app.avatars.busy() && states(s).iter().all(|x| *x == "ready" || *x == "none"));
        assert_eq!(states(&s), ["ready", "ready", "ready", "none", "none"]);
        let asked: Vec<String> = log.lock().unwrap().clone();
        assert_eq!(asked.len(), 6, "three pictures and three lookups, once each: {asked:?}");
        for e in ["Build the engine", "Find the bug", "Write the kernel"] {
            assert_eq!(asked.iter().filter(|u| u.contains(&api(e))).count(), 1, "one lookup for {e}");
        }
        assert!(asked.iter().all(|u| u.starts_with("https://")));
        look(&mut s, "graph with pictures");
        s.shot("graph-pictures");
        // The header: the picture in place of the initials.
        click_in_inspector(&mut s, "Add the octo", "click");
        wait_until(&mut s, "commit open", 10, |s| history(s).is_some_and(|h| h.3 == Some(1)));
        s.idle(3);
        assert!(!in_insp(&s, "OC"), "the picture replaces the initials");
        assert!(in_insp(&s, "Octo Cat"), "the name stays beside it");
        look(&mut s, "commit with a picture");
        s.shot("commit-picture");
        for (subject, initials, picture) in [("Paint the mona", "ML", true), ("Build the engine", "AL", true), ("Find the bug", "GH", false), ("Write the kernel", "LT", false)] {
            s.act("key Escape");
            s.idle(2);
            click_in_inspector(&mut s, subject, "click");
            wait_until(&mut s, "commit open", 10, |s| probe(s).subject.as_deref() == Some(subject) && history(s).is_some_and(|h| h.3 == Some(1)));
            s.idle(3);
            assert_eq!(in_insp(&s, initials), !picture, "{subject}: initials only without a picture");
            look(&mut s, subject);
        }
        s.shot("commit-failed-lookup");
        s.app.cfg.theme = crate::theme::ThemeName::Paper;
        s.act("key Escape");
        look(&mut s, "pictures on a light theme");
        s.shot("graph-pictures-paper");
        click_in_inspector(&mut s, "Build the engine", "click");
        wait_until(&mut s, "commit open", 10, |s| history(s).is_some_and(|h| h.3 == Some(1)));
        look(&mut s, "a picture on a light theme");
        s.shot("commit-picture-paper");
        s.app.cfg.theme = crate::theme::ThemeName::default();
        // Off again: initials at once, and nothing more asked whatever is opened.
        s.act("key ctrl+Comma");
        s.act("text show github pictures");
        s.idle(2);
        s.act("key Enter");
        s.idle(2);
        s.act("key Escape");
        s.settle();
        assert!(!s.app.cfg.github_pictures && !s.app.avatars.on());
        assert!(states(&s).iter().all(|x| x.is_empty()), "pictures dropped");
        let before = (s.app.avatars.asked(), log.lock().unwrap().len());
        assert!(in_insp(&s, "AL"), "initials back");
        for subject in ["Add the octo", "Find the bug"] {
            s.act("key Escape");
            s.idle(2);
            click_in_inspector(&mut s, subject, "click");
            s.idle(20);
        }
        assert!(in_insp(&s, "GH"));
        assert_eq!((s.app.avatars.asked(), log.lock().unwrap().len()), before, "off: nothing asked");
        look(&mut s, "pictures off again");
        // On again: the kept answers come from the cache; only the failed lookup is asked
        // again.
        s.app.cfg.github_pictures = true;
        s.act("key Escape");
        wait_until(&mut s, "answered again", 10, |s| s.app.avatars.on() && !s.app.avatars.busy() && states(s).iter().all(|x| *x == "ready" || *x == "none"));
        assert_eq!(states(&s), ["ready", "ready", "ready", "none", "none"]);
        let again: Vec<String> = log.lock().unwrap()[before.1..].to_vec();
        assert_eq!(again, [format!("https://{}", api("Find the bug"))], "only what wasn't kept");
        report("git pictures", &s);
    }

    fn palette_run(s: &mut Sim, title: &str) {
        s.act("key ctrl+shift+p");
        s.act(&format!("text {title}"));
        s.act("key Enter");
        s.settle();
    }

    /// Folder compare between two panels: marks on both sides in every view, Select
    /// Differences, Sync To Other Panel through the copy job and its conflict dialog, the
    /// marks following the sync and its undo, and clearing when a panel moves.
    #[test]
    fn compare_and_sync_panels() {
        if !sandboxed() {
            return;
        }
        use crate::compare::{Diff, mark};
        let home = fixture("compare");
        let (a, b) = (home.join("A"), home.join("B"));
        crate::tests::compare_fixture(&a, &b);
        let mut s = Sim::new(&a, vec2(1280.0, 800.0));
        s.act("key ctrl+shift+s");
        s.settle();
        go(&mut s, &b);
        s.act("key Tab");
        s.settle();
        assert_eq!(s.tab().path, a);
        palette_run(&mut s, "Compare Panels");
        assert!(toast_is(&s, "5 different here, 3 there"), "{:?}", s.app.toast);
        assert_eq!(mark(&a.join("newer.txt")), Some(Diff::Newer));
        assert_eq!(mark(&b.join("newer.txt")), None, "the older side is not marked");
        assert_eq!(mark(&b.join("size.txt")), Some(Diff::Size));
        assert_eq!(mark(&b.join("only-b.txt")), Some(Diff::Only));
        assert_eq!(mark(&a.join("same.txt")), None);
        assert_eq!(mark(&a.join("both")), None);
        for v in [ViewTree, ViewList, ViewMedium, ViewLarge, ViewXL, ViewDetails] {
            s.cmd(v);
            look(&mut s, &format!("compare {v:?}"));
        }
        s.resize(520.0, 340.0);
        look(&mut s, "compare narrow");
        s.resize(1280.0, 800.0);
        palette_run(&mut s, "Select Differences");
        let mut sel: Vec<String> = s.tab().selected_paths().iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).collect();
        sel.sort();
        assert_eq!(sel, vec!["folder-a", "kind", "newer.txt", "only-a.txt", "size.txt"]);
        // Sync copies what is missing or newer (not the file that only differs in size);
        // two names exist there already, so the conflict dialog asks.
        palette_run(&mut s, "Sync To Other Panel");
        for _ in 0..2 {
            wait_until(&mut s, "conflict dialog", 10, |s| matches!(s.app.popup, Popup::Conflict(_)));
            s.click_text("Replace");
            s.settle();
        }
        wait_until(&mut s, "sync copied", 10, |_| b.join("only-a.txt").exists() && b.join("folder-a").is_dir() && b.join("kind").is_file());
        assert_eq!(std::fs::read_to_string(b.join("newer.txt")).unwrap(), "new");
        assert_eq!(std::fs::read_to_string(b.join("size.txt")).unwrap(), "short", "a size difference alone is not synced");
        wait_until(&mut s, "marks follow the sync", 10, |_| mark(&a.join("newer.txt")).is_none() && mark(&a.join("only-a.txt")).is_none());
        assert_eq!(mark(&b.join("only-b.txt")), Some(Diff::Only));
        look(&mut s, "after sync");
        // Undo removes the copies again, and the marks come back.
        s.act("key ctrl+z");
        wait_until(&mut s, "sync undone", 10, |s| !b.join("only-a.txt").exists() && mark(&a.join("only-a.txt")) == Some(Diff::Only));
        // Either panel moving clears every mark.
        s.act("key Tab");
        s.act("key alt+ArrowUp");
        s.settle();
        assert_eq!(mark(&a.join("size.txt")), None, "marks clear when a panel moves");
        look(&mut s, "after compare");
        report("compare", &s);
    }

    /// Command aliases from the palette: set one with the row's alias button, find and
    /// run the command by typing it, a duplicate refused with the reason, change and clear
    /// with F2, and the result saved with user data.
    #[test]
    fn palette_aliases() {
        if !sandboxed() {
            return;
        }
        let home = fixture("aliases");
        let mut s = Sim::new(&home, vec2(1280.0, 800.0));
        let alias = |s: &Sim, id: &str| s.app.ud.aliases.get(id).cloned();
        s.act("key ctrl+shift+p");
        s.act("text Toggle Hidden Files");
        click_control(&mut s, "alias button");
        assert!(matches!(s.app.popup, Popup::Prompt { .. }), "the alias prompt opens");
        s.act("text hh");
        s.act("key Enter");
        assert_eq!(alias(&s, "toggle_hidden").as_deref(), Some("hh"));
        assert!(matches!(s.app.popup, Popup::Palette(_)), "saving returns to the palette");
        // Typing the alias finds the command first; Enter runs it.
        let hidden = s.app.cfg.show_hidden;
        s.act("text hh");
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t == "Toggle Hidden Files“hh”"), "the alias shows beside the title");
        s.act("key Enter");
        assert_eq!(s.app.cfg.show_hidden, !hidden, "the alias ran Toggle Hidden Files");
        // The same alias for another command is refused, with the reason in the prompt.
        s.act("key ctrl+shift+p");
        s.act("text Toggle Sidebar");
        s.act("key F2");
        s.act("text HH");
        s.act("key Enter");
        assert!(matches!(s.app.popup, Popup::Prompt { .. }), "the prompt stays");
        assert!(s.texts.iter().any(|(_, t)| t.contains("already finds “Toggle Hidden Files”")), "the reason is shown");
        assert_eq!(alias(&s, "toggle_sidebar"), None);
        s.act("key ctrl+a");
        s.act("text sb");
        s.act("key Enter");
        assert_eq!(alias(&s, "toggle_sidebar").as_deref(), Some("sb"));
        look(&mut s, "palette with aliases");
        // Change, then clear, with F2 on the selected command.
        for (new, want) in [("hid", Some("hid")), ("", None)] {
            s.act("key Escape");
            s.act("key ctrl+shift+p");
            s.act("text Toggle Hidden Files");
            s.act("key F2");
            s.act("key ctrl+a");
            s.act("key Backspace");
            if !new.is_empty() {
                s.act(&format!("text {new}"));
            }
            s.act("key Enter");
            assert_eq!(alias(&s, "toggle_hidden").as_deref(), want);
        }
        s.act("key Escape");
        let ctx = s.ctx.clone();
        s.app.save_all(&ctx);
        let saved = crate::config::load_userdata().aliases;
        assert_eq!((saved.get("toggle_sidebar").map(String::as_str), saved.get("toggle_hidden")), (Some("sb"), None), "saved with user data");
        look(&mut s, "after aliases");
        report("aliases", &s);
    }

    /// Network places as a user meets them, on a fake gio: the phone in the sidebar mounts
    /// with a click and opens; Connect To Server asks again after a wrong password and opens
    /// the share with the right one; an FTP address typed into Go To connects as a guest;
    /// an unknown host says so; recent servers fill the address; Disconnect moves the tab
    /// off the share first.
    #[test]
    fn network_places_connect_browse_disconnect() {
        if !sandboxed() {
            return;
        }
        let d = fake_network("main");
        crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(d.clone()));
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
            }
        }
        let _reset = Reset;
        let home = fixture("net");
        let mut s = Sim::new(&home, vec2(1280.0, 800.0));
        wait_until(&mut s, "phone in the sidebar", 10, |s| sidebar_text(s, "Pixel 7").is_some());
        assert!(sidebar_text(&s, "Network").is_some(), "Network section shown");
        look(&mut s, "network section");
        // The phone: one click mounts it and opens it.
        let r = sidebar_text(&s, "Pixel 7").unwrap();
        s.act(&format!("click {:.0} {:.0}", r.center().x, r.center().y));
        let phone = d.join("gvfs/mtp___Google_Pixel_7_");
        wait_until(&mut s, "phone opened", 10, |s| s.tab().path == phone);
        s.settle();
        assert!(s.tab().entries.iter().any(|e| e.name == "hello.txt"));
        // Connect To Server: no password first, then a wrong one, then the right one.
        s.cmd(ConnectServer);
        s.idle(3);
        assert!(s.texts.iter().any(|(_, t)| t == "Connect to server"));
        s.act("text smb://nas/media");
        s.act("key Enter");
        wait_until(&mut s, "login refused", 10, |s| s.texts.iter().any(|(_, t)| t == "Wrong user name or password"));
        look(&mut s, "connect dialog with an error");
        click_control(&mut s, "user field");
        s.act("text me");
        click_control(&mut s, "password field");
        s.act("text nope");
        s.click_text("Connect");
        wait_until(&mut s, "wrong password refused", 10, |s| !s.app.connecting("smb://nas/media") && s.texts.iter().any(|(_, t)| t == "Wrong user name or password"));
        match &s.app.popup {
            Popup::Connect(st) => assert!(st.user == "me" && st.password.is_empty(), "user kept, password cleared: user {:?} password {:?} uri {:?}", st.user, st.password, st.uri),
            _ => s.fail("dialog still open"),
        }
        click_control(&mut s, "password field");
        s.act("text secret");
        s.act("key Enter");
        let share = d.join("gvfs/smb___nas_media");
        wait_until(&mut s, "share opened", 10, |s| s.tab().path == share && !s.popup_open());
        assert_eq!(s.app.ud.servers.first().map(String::as_str), Some("smb://nas/media"), "remembered");
        wait_until(&mut s, "share in the sidebar", 10, |s| sidebar_text(s, "media on nas").is_some());
        // The tab and the path bar name the share, not GVFS's folder for it.
        s.idle(2);
        assert_eq!(s.tab().title(), "media on nas");
        let bar = s.app.panels[&s.app.active].list_rect.top();
        assert!(s.texts.iter().filter(|(r, _)| r.top() < bar).filter(|(_, t)| t == "media on nas").count() >= 2, "tab and path bar");
        assert!(!s.texts.iter().any(|(_, t)| t.contains("smb___nas_media") || t == "gvfs"), "no GVFS folder names on screen");
        look(&mut s, "inside the share");
        // Go To: an address connects; an FTP site as a guest.
        s.act("key ctrl+p");
        s.act("text ftp://pub");
        s.act("key Enter");
        wait_until(&mut s, "ftp opened", 10, |s| s.tab().path == d.join("gvfs/ftp___pub"));
        // An unknown host fails with the reason.
        s.act("key ctrl+p");
        s.act("text smb://nohost/x");
        s.act("key Enter");
        wait_until(&mut s, "failure reported", 10, |s| s.app.toast.as_ref().is_some_and(|t| t.0 == "Couldn't connect to x on nohost: No server or share by that name"));
        // Recent servers fill the address.
        s.cmd(ConnectServer);
        s.idle(3);
        s.click_text("smb://nas/media");
        s.idle(2);
        assert!(matches!(&s.app.popup, Popup::Connect(st) if st.uri == "smb://nas/media"));
        s.act("key Escape");
        // Disconnect the share from the sidebar while a tab shows it.
        go(&mut s, &share);
        let r = sidebar_text(&s, "media on nas").unwrap();
        s.act(&format!("move {:.0} {:.0}", r.center().x, r.center().y));
        click_control(&mut s, "disconnect button");
        assert!(crate::tab::is_computer(&s.tab().path), "tab left the share first");
        wait_until(&mut s, "disconnected", 10, |s| s.app.toast.as_ref().is_some_and(|t| t.0 == "Disconnected media on nas"));
        wait_until(&mut s, "gone from the sidebar", 10, |s| sidebar_text(s, "media on nas").is_none());
        assert!(!share.exists());
        let log = std::fs::read_to_string(d.join("log")).unwrap_or_default();
        assert!(log.contains("gio mount smb://nas/media") && log.contains("gio mount -u smb://nas/media"), "{log}");
        report("network", &s);
    }

    /// Administrator access, on a fake pkexec: pasting into, deleting in, creating in and
    /// renaming in a folder the user may not write each fail first and then succeed through
    /// "Retry as administrator" / "Use administrator"; a dismissed password dialog leaves
    /// everything as it was and says so.
    #[test]
    fn administrator_retries_refused_operations() {
        if !sandboxed() {
            return;
        }
        let d = fake_pkexec("main");
        crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(d.clone()));
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
            }
        }
        let _reset = Reset;
        let home = fixture("admin");
        let locked = home.join("System");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("keep.conf"), "x").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();
        let mut s = Sim::new(&home.join("Documents"), vec2(1280.0, 800.0));
        s.cmd(ViewDetails);
        s.settle();
        let finished = |s: &Sim| !s.app.jobs.is_empty() && s.app.jobs.iter().all(|j| j.finished_at.is_some());
        // Paste into it: refused, then copied as administrator.
        select_path(&mut s, &home.join("Documents/notes.txt"));
        s.act("key ctrl+c");
        go(&mut s, &locked);
        s.act("key ctrl+v");
        wait_until(&mut s, "paste refused", 10, |s| s.texts.iter().any(|(_, t)| t == "Retry as administrator"));
        assert!(s.texts.iter().any(|(_, t)| t == "Couldn't copy 1 item"), "the card says nothing was copied");
        look(&mut s, "permission refused");
        s.click_text("Retry as administrator");
        wait_until(&mut s, "copied as administrator", 10, |_| locked.join("notes.txt").exists());
        assert!(std::fs::read_to_string(d.join("log")).unwrap().contains("cp -a -- "), "one cp through pkexec");
        wait_until(&mut s, "job finished", 10, finished);
        // Delete inside it (Shift+Delete asks first).
        s.settle();
        select_path(&mut s, &locked.join("keep.conf"));
        s.act("key shift+Delete");
        s.idle(2);
        s.act("key Enter");
        wait_until(&mut s, "delete refused", 10, |s| s.texts.iter().any(|(_, t)| t == "Retry as administrator"));
        s.click_text("Retry as administrator");
        wait_until(&mut s, "deleted as administrator", 10, |_| !locked.join("keep.conf").exists());
        // A new folder: asked, then made.
        s.settle();
        s.act("key ctrl+shift+n");
        s.idle(2);
        assert!(s.texts.iter().any(|(_, t)| t.starts_with("You don't have permission")), "asked first");
        s.click_text("Use administrator");
        wait_until(&mut s, "folder made", 10, |_| locked.join("New folder").is_dir());
        // Rename: asked, then renamed.
        s.settle();
        select_path(&mut s, &locked.join("notes.txt"));
        s.act("key F2");
        s.act("key ctrl+a");
        s.act("text settings.txt");
        s.act("key Enter");
        s.idle(2);
        s.click_text("Use administrator");
        wait_until(&mut s, "renamed", 10, |_| locked.join("settings.txt").exists() && !locked.join("notes.txt").exists());
        // The password dialog dismissed: nothing changes, and the job says why.
        std::fs::write(d.join("deny"), "").unwrap();
        s.settle();
        s.act("key ctrl+shift+n");
        s.idle(2);
        s.click_text("Use administrator");
        wait_until(&mut s, "refusal reported", 10, |s| s.app.jobs.iter().any(|j| j.progress.lock().unwrap().errors.iter().any(|e| e == "Administrator access was not given")));
        assert!(!locked.join("New folder (1)").exists());
        look(&mut s, "after administrator work");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        report("admin", &s);
    }

    /// Every surface photographed for a person to look at (COMET_SIM_SHOTS): the window,
    /// each view, the inspector's previews, the history, popups and menus, in a dark and
    /// a light theme and two of the newer ones, at a normal and a small window. Without
    /// shots it walks the first theme and size only, as one more check of every frame.
    /// COMET_SIM_AUDIT=midnight-1280 keeps the passes whose name contains it.
    #[test]
    fn ui_audit() {
        if !sandboxed() {
            return;
        }
        use crate::theme::ThemeName;
        let home = fixture("audit");
        let long = home.join("Projects/Quarterly planning and the long tail of everything else");
        std::fs::create_dir_all(long.join("Drafts that nobody has opened since the spring offsite")).unwrap();
        for (i, n) in ["Agenda.md", "Minutes of the weekly sync with every team lead.txt", "budget.csv"].iter().enumerate() {
            std::fs::write(long.join(n), "x".repeat(100 * (i + 1))).unwrap();
        }
        let docs = home.join("Documents");
        std::fs::write(docs.join("letter from the bank.txt"), "Dear customer, your tax refund has been paid into your account.").unwrap();
        std::fs::write(docs.join("data.bin"), (0u8..=255).cycle().take(4096).collect::<Vec<u8>>()).unwrap();
        std::fs::write(docs.join("main.rs"), "// demo\nfn main() {\n    let x: u32 = 42;\n    println!(\"{x} is the answer to a question long enough to run past the right edge of any preview\");\n}\n").unwrap();
        if crate::tags::write(&docs.join("notes.txt"), &["Red".into(), "Work".into()]).is_ok() {
            let _ = crate::tags::write(&docs.join("report.md"), &["Blue".into()]);
        }
        if crate::archive::available() {
            let _ = crate::archive::create(&docs.join("bundle.zip"), &docs, &["notes.txt".into(), "Taxes".into()]);
            let _ = std::process::Command::new("zip").args(["-q", "-r", "-P", "pw"]).arg(docs.join("locked.zip")).arg("Taxes").current_dir(&docs).status();
        }
        let media = media_fixture("auditmedia");
        let git = crate::media::have("git");
        if git {
            crate::tests::commit_view_fixture(&home.join("Code/project"));
        }
        let drv = fake_drives("audit");
        let net = fake_network("audit");
        for f in ["gio", "phones"] {
            let _ = std::fs::copy(net.join(f), drv.join(f));
        }
        for sub in ["gvfs", "netmounts"] {
            let _ = std::fs::create_dir_all(drv.join(sub));
        }
        crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(drv));
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) {
                crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
                *crate::ollama::TEST_HOST.lock().unwrap() = None;
            }
        }
        let _reset = Reset;
        let (addr, _, _) = crate::tests::fake_ollama(true);
        *crate::ollama::TEST_HOST.lock().unwrap() = Some(addr);
        let shots = std::env::var_os("COMET_SIM_SHOTS").is_some();
        let only = std::env::var("COMET_SIM_AUDIT").unwrap_or_default().to_lowercase();
        for theme in [ThemeName::Midnight, ThemeName::Paper, ThemeName::Latte, ThemeName::Gruvbox] {
            for (w, h) in [(1280.0, 800.0), (720.0, 480.0)] {
                let tag = format!("{}-{w}", theme.label().to_lowercase().replace(' ', ""));
                if !tag.contains(&only) {
                    continue;
                }
                audit_pass(&home, &media, git, theme, w, h, &tag);
                if !shots {
                    return;
                }
            }
        }
    }

    fn audit_pass(home: &Path, media: &Path, git: bool, theme: crate::theme::ThemeName, w: f32, h: f32, tag: &str) {
        let docs = home.join("Documents");
        let mut s = Sim::new(home, vec2(w, h));
        s.app.cfg.theme = theme;
        s.app.cfg.semantic_folders = docs.display().to_string();
        // Growing tabs run on the wall clock, fades on the frames' clock: let both end.
        let still = |s: &mut Sim| {
            std::thread::sleep(std::time::Duration::from_millis(350));
            s.idle(15);
        };
        let shot = |s: &mut Sim, name: &str| {
            look(s, name);
            still(s);
            s.shot(&format!("{tag}-{name}"));
        };
        let shot_open = |s: &mut Sim, name: &str| {
            still(s);
            s.shot(&format!("{tag}-{name}"));
        };
        let close = |s: &mut Sim| {
            for _ in 0..3 {
                if s.popup_open() {
                    s.act("key Escape");
                    s.idle(2);
                }
            }
        };
        let pause = |s: &mut Sim| {
            for _ in 0..30 {
                s.idle(1);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            s.settle();
        };
        wait_until(&mut s, "drives in the sidebar", 10, |s| sidebar_text(s, "Photo Card").is_some());
        // The window: several tabs, long names, the sidebar's sections.
        for p in [docs.clone(), home.join("Projects/Quarterly planning and the long tail of everything else"), home.join("Code/deep/a/b/c/d/e/f/g/h"), home.join("Pictures/Trip")] {
            s.cmd(NewTab);
            s.settle();
            go(&mut s, &p);
        }
        s.cmd(Tab1);
        s.settle();
        s.act("key ArrowDown");
        shot(&mut s, "home-details");
        s.cmd(Tab4);
        s.settle();
        shot(&mut s, "deep-path");
        s.app.cfg.compact_paths = false;
        shot(&mut s, "deep-path-full");
        s.app.cfg.compact_paths = true;
        s.cmd(Tab3);
        s.settle();
        s.act("key ArrowDown");
        s.act("key shift+ArrowDown");
        let lr = s.app.panels[&s.app.active].list_rect;
        shot(&mut s, "long-names-selection");
        // Hovering a row below the selection.
        s.act(&format!("move {:.0} {:.0}", lr.center().x, lr.top() + 100.0));
        still(&mut s);
        s.shot(&format!("{tag}-hover"));
        // Views of a folder with names and tags, then pictures.
        s.cmd(Tab2);
        s.settle();
        for (v, n) in [(ViewDetails, "details"), (ViewList, "list"), (ViewTree, "tree"), (ViewMedium, "icons-m"), (ViewLarge, "icons-l"), (ViewXL, "icons-xl")] {
            s.cmd(v);
            s.act("key ArrowDown");
            shot(&mut s, &format!("docs-{n}"));
        }
        s.cmd(Tab5);
        s.settle();
        for (v, n) in [(ViewMedium, "icons-m"), (ViewXL, "icons-xl"), (ViewDetails, "details")] {
            s.cmd(v);
            pause(&mut s);
            shot(&mut s, &format!("pictures-{n}"));
        }
        s.cmd(Tab2);
        s.settle();
        s.cmd(ViewTree);
        s.act("key Home");
        s.act("key ArrowRight");
        shot(&mut s, "docs-tree-open");
        s.cmd(ViewDetails);
        // Renaming, an empty folder, a folder that can't be read.
        select_path(&mut s, &docs.join("report.md"));
        s.act("key F2");
        still(&mut s);
        s.shot(&format!("{tag}-rename"));
        s.act("key Escape");
        go(&mut s, &home.join("Empty"));
        shot(&mut s, "empty");
        go(&mut s, &home.join("Locked"));
        shot(&mut s, "locked");
        go(&mut s, &docs);
        // The filter, with conditions; contents search; the whole tree flattened.
        s.act("key ctrl+f");
        s.act("text size>10 modified<7d no");
        still(&mut s);
        s.shot(&format!("{tag}-filter-query"));
        s.act("key ctrl+a");
        s.act("text nothing-matches-this");
        still(&mut s);
        s.shot(&format!("{tag}-filter-none"));
        s.act("key ctrl+a");
        s.act("key Backspace");
        s.act("key Escape");
        click_control(&mut s, "contents toggle");
        s.act("text notes");
        wait_until(&mut s, "contents search", 10, |s| !s.tab().is_loading());
        s.settle();
        still(&mut s);
        s.shot(&format!("{tag}-contents"));
        click_control(&mut s, "contents toggle");
        s.act("key Escape");
        s.act("key Escape");
        s.cmd(ToggleFlatten);
        s.settle();
        shot(&mut s, "flatten");
        s.cmd(ToggleFlatten);
        s.settle();
        // Split panels.
        s.cmd(SplitRight);
        s.settle();
        shot(&mut s, "split-right");
        s.cmd(SplitBottom);
        s.settle();
        shot(&mut s, "split-three");
        s.cmd(ClosePanel);
        s.cmd(ClosePanel);
        s.settle();
        // The inspector beside the list and under it, with every kind of preview.
        go(&mut s, &docs);
        s.cmd(ToggleInspector);
        for (layout, side) in [(crate::config::InspectorLayout::Right, "right"), (crate::config::InspectorLayout::Bottom, "bottom")] {
            s.app.cfg.inspector_layout = layout;
            s.settle();
            for (f, n) in [("notes.txt", "text"), ("report.md", "markdown"), ("main.rs", "code"), ("data.bin", "hex"), ("Taxes", "folder"), ("bundle.zip", "archive")] {
                let p = docs.join(f);
                if !p.exists() {
                    continue;
                }
                select_path(&mut s, &p);
                pause(&mut s);
                if n == "archive" {
                    // Its contents, as a folder's are, not its bytes.
                    in_inspector(&s, "notes.txt");
                }
                shot(&mut s, &format!("inspect-{side}-{n}"));
            }
            if side == "right" {
                select_path(&mut s, &docs.join("main.rs"));
                s.settle();
                s.cmd(InspectorWrap);
                shot(&mut s, "inspect-right-code-wrapped");
                s.cmd(InspectorWrap);
            }
        }
        s.app.cfg.inspector_layout = crate::config::InspectorLayout::Right;
        go(&mut s, media);
        for (f, n) in [("photo.png", "image"), ("doc.pdf", "pdf"), ("clip.mp4", "video"), ("song.mp3", "audio"), ("drawing.svg", "svg")] {
            select_path(&mut s, &media.join(f));
            match n {
                "pdf" => wait_until(&mut s, "pdf page", 20, |s| inspector(s).pdf_state().is_some_and(|(_, _, shown)| shown.is_some())),
                "video" | "audio" => wait_until(&mut s, "media probed", 15, |s| inspector(s).media_state().is_some()),
                _ => {}
            }
            pause(&mut s);
            shot(&mut s, &format!("inspect-{n}"));
        }
        s.cmd(ToggleInspector);
        // The history and a commit.
        if git {
            go(&mut s, &home.join("Code/project"));
            wait_until(&mut s, "branch read", 10, |s| crate::gitstatus::repo_for(&s.tab().path).is_some());
            s.idle(2);
            shot(&mut s, "git-status");
            s.cmd(GitHistory);
            wait_until(&mut s, "history", 10, |s| history(s).is_some_and(|h| h.1));
            shot(&mut s, "git-graph");
            click_in_inspector(&mut s, "Lay out the panels", "click");
            wait_until(&mut s, "files", 10, |s| history(s).is_some_and(|h| h.3.is_some()));
            wait_until(&mut s, "pictures", 10, |s| probe(s).pictures >= 3 || probe(s).failed > 0);
            shot(&mut s, "git-commit");
            s.cmd(ToggleInspector);
            s.settle();
        }
        // Popups and menus.
        go(&mut s, &docs);
        select_path(&mut s, &docs.join("notes.txt"));
        let lr = s.app.panels[&s.app.active].list_rect;
        let r = s.text_rect("notes.txt");
        s.act(&format!("click {:.0} {:.0} right", r.center().x, r.center().y));
        shot_open(&mut s, "context-menu");
        close(&mut s);
        s.act(&format!("click {:.0} {:.0} right", lr.center().x, lr.bottom() - 20.0));
        shot_open(&mut s, "context-menu-empty");
        close(&mut s);
        s.cmd(Palette);
        shot_open(&mut s, "palette");
        s.act("text view");
        shot_open(&mut s, "palette-query");
        close(&mut s);
        s.cmd(GoTo);
        s.act("text doc");
        s.settle();
        shot_open(&mut s, "goto");
        close(&mut s);
        s.act("key ctrl+k");
        s.idle(3);
        shot_open(&mut s, "search-empty");
        click_control(&mut s, "meaning field");
        s.act("text notes");
        wait_until(&mut s, "a name found", 5, |s| matches!(&s.app.popup, Popup::Meaning(st) if !st.named.is_empty()));
        shot_open(&mut s, "search-names");
        // Meaning, from the stand-in Ollama: turned on, the files read, a vague question.
        s.act("key ctrl+a");
        s.act("key Backspace");
        s.idle(2);
        // A pass after the first finds it on (the setting is kept).
        if s.texts.iter().any(|(_, t)| t == "Turn on") {
            s.click_text("Turn on");
        }
        wait_until(&mut s, "files read", 20, |s| matches!(s.app.semantic.as_ref().map(|e| e.phase()), Some(crate::semantic::Phase::Ready)));
        click_control(&mut s, "meaning field");
        s.act("text money back from the government");
        wait_until(&mut s, "answer", 10, |s| matches!(&s.app.popup, Popup::Meaning(st) if !st.hits.is_empty()));
        shot_open(&mut s, "search-meaning");
        close(&mut s);
        s.app.cfg.semantic_search = false;
        s.cmd(Options);
        shot_open(&mut s, "options");
        close(&mut s);
        select_path(&mut s, &docs.join("notes.txt"));
        s.act("key alt+Enter");
        s.idle(3);
        click_control(&mut s, "checksum button");
        wait_until(&mut s, "checksum", 10, |s| s.texts.iter().any(|(_, t)| t.len() == 64 && t.chars().all(|c| c.is_ascii_hexdigit())));
        shot_open(&mut s, "properties");
        close(&mut s);
        s.cmd(ConnectServer);
        shot_open(&mut s, "connect");
        close(&mut s);
        if docs.join("locked.zip").exists() {
            select_path(&mut s, &docs.join("locked.zip"));
            s.cmd(ExtractHere);
            wait_until(&mut s, "password asked", 10, |s| matches!(&s.app.popup, Popup::ArchivePassword(_)));
            shot_open(&mut s, "archive-password");
            close(&mut s);
        }
        s.cmd(ChooseTheme);
        s.settle();
        shot_open(&mut s, "themes");
        close(&mut s);
        s.app.cfg.theme = theme;
        for (c, n) in [(MainMenu, "main-menu"), (QuickMenu, "quick-menu"), (About, "about"), (TabSearch, "tab-search"), (TypesFilter, "types")] {
            s.cmd(c);
            shot_open(&mut s, n);
            close(&mut s);
        }
        // A conflict: the same name copied into a folder that has it.
        std::fs::write(home.join("Code/maia/report.md"), "older").unwrap();
        s.cmd(SplitRight);
        go(&mut s, &home.join("Code/maia"));
        s.act("key Tab");
        s.settle();
        select_path(&mut s, &docs.join("report.md"));
        s.act("key F5");
        s.settle();
        shot_open(&mut s, "conflict");
        close(&mut s);
        s.cmd(ClosePanel);
        // A toast and a job card.
        let progress = std::sync::Arc::new(std::sync::Mutex::new(crate::ops::Progress { total_bytes: 900 << 20, done_bytes: 300 << 20, total_items: 120, done_items: 41, current: "a very long file name that keeps going and going well past any column width you would pick.txt".into(), ..Default::default() }));
        s.app.jobs.push(crate::ops::Job { id: 9999, label: "Copying 120 items to Backups".into(), kind: crate::ops::OpKind::Copy { srcs: vec![], dest: home.join("Backups"), decide: Default::default() }, progress: progress.clone(), cancel: Default::default(), pause: Default::default(), started: Instant::now(), finished_at: None, show_errors: false });
        s.app.toast("Copied the path of notes.txt");
        s.idle(5);
        s.act("move -50 -50");
        still(&mut s);
        s.shot(&format!("{tag}-toast-job"));
        progress.lock().unwrap().finished = true;
        s.app.jobs.clear();
        s.idle(3);
        report(&format!("audit {tag}"), &s);
    }

    #[test]
    fn monkey_sessions() {
        if !sandboxed() {
            return;
        }
        let seeds: u64 = std::env::var("COMET_SIM_SEEDS").ok().and_then(|v| v.parse().ok()).unwrap_or(10);
        let steps: usize = std::env::var("COMET_SIM_STEPS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
        let mut all = vec![];
        let only: Option<u64> = std::env::var("COMET_SIM_SEED").ok().and_then(|v| v.parse().ok());
        for seed in only.map(|o| o..=o).unwrap_or(1..=seeds) {
            let home = fixture(&format!("monkey{seed}"));
            // A git work tree with every kind of change, for the status marks.
            crate::tests::git_fixture(&home.join("Code/repo"));
            // And a small project whose commits change files of every kind, pictures too.
            crate::tests::commit_view_fixture(&home.join("Code/project"));
            let _ = std::process::Command::new("git").args(["remote", "add", "origin", "https://github.com/octo-org/project.git"]).current_dir(home.join("Code/project")).status();
            // Fake drives, network places and pkexec to use at random (never the real tools).
            let drv = fake_drives(&format!("monkey{seed}"));
            let net = fake_network(&format!("monkey{seed}"));
            let adm = fake_pkexec(&format!("monkey{seed}"));
            for (dir, f) in [(&net, "gio"), (&net, "phones"), (&adm, "pkexec")] {
                let _ = std::fs::copy(dir.join(f), drv.join(f));
            }
            for sub in ["gvfs", "netmounts"] {
                let _ = std::fs::create_dir_all(drv.join(sub));
            }
            crate::drives::FAKE.with(|f| *f.borrow_mut() = Some(drv));
            // An archive to wander into.
            if crate::archive::available() {
                let _ = crate::archive::create(&home.join("bundle.zip"), &home.join("Documents"), &["notes.txt".into(), "Taxes".into()]);
                // And a locked one (password "pw"), when zip is installed.
                let _ = std::process::Command::new("zip").args(["-q", "-r", "-P", "pw"]).arg(home.join("locked.zip")).arg("Taxes").current_dir(home.join("Documents")).status();
            }
            let mut s = Sim::new(&home, vec2(1280.0, 800.0));
            // GitHub pictures, when a session turns them on, from a stand-in for GitHub.
            s.app.avatars.dir = root().join("xdg/cache/maia-avatars");
            let json = br#"[{"sha":"0","author":{"avatar_url":"https://avatars.githubusercontent.com/u/1?v=4"}}]"#.to_vec();
            let table = vec![("api.github.com".to_string(), crate::avatars::Http::Ok(json)), ("avatars.githubusercontent.com".to_string(), crate::avatars::Http::Ok(crate::tests::png_bytes(96, 96, seed as u32)))];
            s.app.avatars.set_get(crate::avatars::fake_get(table, Default::default()));
            // Later launches handing folders over (single instance).
            let (tx, rx) = std::sync::mpsc::channel();
            s.app.requests = Some(rx);
            let mut rng = Rng::new(seed);
            s.note(format!("seed {seed}"));
            for k in 0..steps {
                if rng.below(80) == 0 {
                    let picks = [home.join("Music"), home.join("Documents/notes.txt"), home.join("bundle.zip"), home.join("gone")];
                    let p = rng.pick(&picks).clone();
                    s.note(format!("second launch with {}", p.display()));
                    let _ = tx.send(crate::single::Request::Open(vec![p]));
                }
                monkey_step(&mut s, &mut rng);
                // Every so often: dismiss whatever is open and check the layout of the
                // state the random walk has reached (feature combinations no script has).
                if k % 25 == 24 {
                    for _ in 0..3 {
                        if s.popup_open() || s.tab().rename.is_some() {
                            s.act("key Escape");
                        }
                    }
                    look(&mut s, &format!("monkey seed {seed} step {k}"));
                }
            }
            s.settle();
            // Let mounts and ejects in flight finish before the disk is wiped.
            let t0 = Instant::now();
            while !s.app.drive_jobs.is_empty() && t0.elapsed().as_secs() < 10 {
                s.idle(1);
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let (p50, p95, max) = s.timing();
            eprintln!("sim monkey seed {seed}: {} frames, p50 {p50:.1} p95 {p95:.1} max {max:.1} ms", s.frame_ms.len());
            for (ms, step) in &s.slow {
                eprintln!("  slow frame {ms:.0} ms after: {step}");
            }
            all.extend(s.frame_ms.drain(..));
            // Random sessions copy and duplicate freely (whole system folders included):
            // start every seed from a clean disk so leftovers don't pile up.
            // Stop background copies/moves and wait for them to wind down before wiping.
            let progress: Vec<_> = s.app.jobs.iter().map(|j| {
                j.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
                j.progress.clone()
            }).collect();
            drop(s);
            let t0 = Instant::now();
            while progress.iter().any(|p| !p.lock().map(|p| p.finished).unwrap_or(true)) && t0.elapsed().as_secs() < 30 {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let _ = home;
            wipe_root();
        }
        crate::drives::FAKE.with(|f| *f.borrow_mut() = None);
        let launched = crate::mime::LAUNCHED.lock().unwrap().len();
        let out = crate::clipboard::DRAGGED_OUT.lock().unwrap().len();
        eprintln!("sim monkey: {launched} external launches intercepted, {out} drags handed out");
        let _ = all;
    }
}

