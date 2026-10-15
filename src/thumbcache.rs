//! The desktop's shared thumbnail cache (freedesktop.org Thumbnail Managing Standard) and
//! its external thumbnailers (`/usr/share/thumbnailers/*.thumbnailer`), so Maia shows
//! the same thumbnails as other apps, reuses ones they already made, and gets video,
//! PDF, HEIF, office and audio-cover thumbnails without bundling any decoder.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

use crate::imgdec::Rgba;

/// `file://` URI as GLib writes it (the cache key is the MD5 of this exact string).
pub fn uri_for(path: &Path) -> String {
    let mut s = String::from("file://");
    for &b in path.as_os_str().as_encoded_bytes() {
        let keep = b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@/".contains(&b);
        if keep {
            s.push(b as char);
        } else {
            s.push_str(&format!("%{b:02X}"));
        }
    }
    s
}

/// Decode a `file://` URI back to a path (percent-decoding; other schemes → None).
pub fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    // "file://host/path": only the local host is ours.
    let rest = if rest.starts_with('/') { rest } else { rest.strip_prefix("localhost")? };
    let b = rest.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(v) = h {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    use std::os::unix::ffi::OsStringExt;
    Some(PathBuf::from(std::ffi::OsString::from_vec(out)))
}

// ------------------------------------------------------------------ md5 (RFC 1321)

pub fn md5_hex(data: &[u8]) -> String {
    const S: [u32; 64] = [7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21];
    let k: Vec<u32> = (0..64).map(|i| ((i as f64 + 1.0).sin().abs() * 4294967296.0) as u32).collect();
    let (mut a0, mut b0, mut c0, mut d0) = (0x67452301u32, 0xefcdab89u32, 0x98badcfeu32, 0x10325476u32);
    let mut msg = data.to_vec();
    let bits = (data.len() as u64).wrapping_mul(8);
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bits.to_le_bytes());
    for chunk in msg.chunks_exact(64) {
        let m: Vec<u32> = chunk.chunks_exact(4).map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]])).collect();
        let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let f = f.wrapping_add(a).wrapping_add(k[i]).wrapping_add(m[g]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(f.rotate_left(S[i]));
        }
        a0 = a0.wrapping_add(a);
        b0 = b0.wrapping_add(b);
        c0 = c0.wrapping_add(c);
        d0 = d0.wrapping_add(d);
    }
    [a0, b0, c0, d0].iter().flat_map(|v| v.to_le_bytes()).map(|b| format!("{b:02x}")).collect()
}

// ------------------------------------------------------------------ cache

pub fn cache_root() -> PathBuf {
    dirs::cache_dir().unwrap_or_else(|| crate::fsx::home().join(".cache")).join("thumbnails")
}

/// Cache flavour for a requested size: (folder, pixel size).
pub fn flavor(size: u32) -> (&'static str, u32) {
    match size {
        0..=128 => ("normal", 128),
        129..=256 => ("large", 256),
        257..=512 => ("x-large", 512),
        _ => ("xx-large", 1024),
    }
}

fn mtime_secs(t: Option<SystemTime>) -> Option<u64> {
    t?.duration_since(SystemTime::UNIX_EPOCH).ok().map(|d| d.as_secs())
}

fn thumb_path(root: &Path, uri: &str, folder: &str) -> PathBuf {
    root.join(folder).join(format!("{}.png", md5_hex(uri.as_bytes())))
}

/// A valid cached thumbnail for `path` at (at least) `size`, if the cache has one:
/// the stored URI and modification time must match the file now.
pub fn lookup(path: &Path, mtime: Option<SystemTime>, size: u32) -> Option<(Rgba, Option<[u32; 2]>)> {
    lookup_in(&cache_root(), path, mtime, size)
}

pub fn lookup_in(root: &Path, path: &Path, mtime: Option<SystemTime>, size: u32) -> Option<(Rgba, Option<[u32; 2]>)> {
    let secs = mtime_secs(mtime)?;
    let uri = uri_for(path);
    let (folder, _) = flavor(size);
    let bytes = std::fs::read(thumb_path(root, &uri, folder)).ok()?;
    let (img, text) = crate::imgdec::png_with_text(&bytes)?;
    let get = |k: &str| text.iter().find(|(kk, _)| kk == k).map(|(_, v)| v.as_str());
    if get("Thumb::URI") != Some(uri.as_str()) || get("Thumb::MTime").and_then(|v| v.parse::<u64>().ok()) != Some(secs) {
        return None;
    }
    let dim = |k: &str| get(k).and_then(|v| v.parse::<u32>().ok());
    let dims = dim("Thumb::Image::Width").zip(dim("Thumb::Image::Height")).map(|(w, h)| [w, h]);
    Some((img, dims))
}

/// Save a thumbnail (already scaled to fit the flavour) for other apps and next time.
pub fn store(path: &Path, mtime: Option<SystemTime>, size: u32, img: &Rgba, file_size: u64, dims: Option<[u32; 2]>) {
    store_in(&cache_root(), path, mtime, size, img, file_size, dims)
}

pub fn store_in(root: &Path, path: &Path, mtime: Option<SystemTime>, size: u32, img: &Rgba, file_size: u64, dims: Option<[u32; 2]>) {
    let Some(secs) = mtime_secs(mtime) else { return };
    // Never cache thumbnails of the cache itself.
    if path.starts_with(root) {
        return;
    }
    let uri = uri_for(path);
    let (folder, _) = flavor(size);
    let dest = thumb_path(root, &uri, folder);
    let mut text = vec![("Thumb::URI", uri.clone()), ("Thumb::MTime", secs.to_string()), ("Thumb::Size", file_size.to_string()), ("Software", "Maia".to_string())];
    if let Some([w, h]) = dims {
        text.push(("Thumb::Image::Width", w.to_string()));
        text.push(("Thumb::Image::Height", h.to_string()));
    }
    write_png(&dest, img, &text);
}

/// Remember that no thumbnail can be made (until the file changes), like other apps do.
pub fn mark_failed(path: &Path, mtime: Option<SystemTime>) {
    let Some(secs) = mtime_secs(mtime) else { return };
    let uri = uri_for(path);
    let dest = cache_root().join("fail/maia").join(format!("{}.png", md5_hex(uri.as_bytes())));
    let img = Rgba { w: 1, h: 1, px: vec![0, 0, 0, 0] };
    write_png(&dest, &img, &[("Thumb::URI", uri), ("Thumb::MTime", secs.to_string()), ("Software", "Maia".to_string())]);
}

pub fn has_failed(path: &Path, mtime: Option<SystemTime>) -> bool {
    let Some(secs) = mtime_secs(mtime) else { return false };
    let uri = uri_for(path);
    let Ok(bytes) = std::fs::read(cache_root().join("fail/maia").join(format!("{}.png", md5_hex(uri.as_bytes())))) else { return false };
    crate::imgdec::png_with_text(&bytes).is_some_and(|(_, t)| t.iter().any(|(k, v)| k == "Thumb::MTime" && v.parse::<u64>().ok() == Some(secs)))
}

/// Atomic, private (0600 file, 0700 folders) as the standard asks.
fn write_png(dest: &Path, img: &Rgba, text: &[(&str, String)]) {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let Some(dir) = dest.parent() else { return };
    let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
    let tmp = dir.join(format!(".maia-{}-{}.png", std::process::id(), md5_hex(dest.as_os_str().as_encoded_bytes())));
    let ok = (|| -> Option<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp).ok()?;
        std::io::Write::write_all(&mut f, &encode_png(img, text)?).ok()
    })();
    if ok.is_some() {
        let _ = std::fs::rename(&tmp, dest);
    } else {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// An RGBA8 PNG with tEXt chunks: rows filtered against the one above (Up), compressed
/// with fdeflate. Written by hand because png's encoder brings a second deflate
/// implementation into the binary.
pub fn encode_png(img: &Rgba, text: &[(&str, String)]) -> Option<Vec<u8>> {
    let stride = img.w as usize * 4;
    if img.px.len() != stride * img.h as usize || img.w == 0 || img.h == 0 {
        return None;
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend(img.w.to_be_bytes());
    ihdr.extend(img.h.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]); // 8 bits, RGBA, deflate, adaptive filters, no interlace
    chunk(&mut out, b"IHDR", &ihdr);
    for (k, v) in text {
        let mut d = k.as_bytes().to_vec();
        d.push(0);
        d.extend(v.as_bytes());
        chunk(&mut out, b"tEXt", &d);
    }
    let mut z = fdeflate::Compressor::new(Vec::new()).ok()?;
    let mut row = vec![0u8; stride + 1];
    for (y, cur) in img.px.chunks_exact(stride).enumerate() {
        if y == 0 {
            row[0] = 0;
            row[1..].copy_from_slice(cur);
        } else {
            let prev = &img.px[(y - 1) * stride..y * stride];
            row[0] = 2;
            for (o, (c, p)) in row[1..].iter_mut().zip(cur.iter().zip(prev)) {
                *o = c.wrapping_sub(*p);
            }
        }
        z.write_data(&row).ok()?;
    }
    chunk(&mut out, b"IDAT", &z.finish().ok()?);
    chunk(&mut out, b"IEND", &[]);
    Some(out)
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(kind);
    out.extend(data);
    let mut c = !0u32;
    for &b in kind.iter().chain(data) {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    out.extend((!c).to_be_bytes());
}

// ------------------------------------------------------------------ thumbnailers

pub struct Thumbnailer {
    pub exec: String,
    pub mimes: Vec<String>,
}

fn thumbnailer_dirs() -> Vec<PathBuf> {
    let mut v = vec![dirs::data_dir().unwrap_or_else(|| crate::fsx::home().join(".local/share")).join("thumbnailers")];
    let sys = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    v.extend(sys.split(':').filter(|s| !s.is_empty()).map(|d| PathBuf::from(d).join("thumbnailers")));
    v
}

fn on_path(bin: &str) -> bool {
    if bin.contains('/') {
        return Path::new(bin).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

/// Installed thumbnailers (first definition of a file name wins, user dir first).
pub fn thumbnailers() -> &'static [Thumbnailer] {
    static T: OnceLock<Vec<Thumbnailer>> = OnceLock::new();
    T.get_or_init(|| {
        let mut seen = std::collections::HashSet::new();
        let mut out = vec![];
        for d in thumbnailer_dirs() {
            let Ok(rd) = std::fs::read_dir(&d) else { continue };
            let mut files: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "thumbnailer")).collect();
            files.sort();
            for f in files {
                let name = f.file_name().map(|n| n.to_os_string()).unwrap_or_default();
                if !seen.insert(name) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&f) else { continue };
                let (mut exec, mut mimes, mut try_exec) = (None, vec![], None);
                for line in text.lines() {
                    if let Some(v) = line.strip_prefix("Exec=") {
                        exec = Some(v.trim().to_string());
                    } else if let Some(v) = line.strip_prefix("MimeType=") {
                        mimes = v.split(';').map(str::trim).filter(|m| !m.is_empty()).map(String::from).collect();
                    } else if let Some(v) = line.strip_prefix("TryExec=") {
                        try_exec = Some(v.trim().to_string());
                    }
                }
                let Some(exec) = exec else { continue };
                let bin = try_exec.unwrap_or_else(|| exec.split_whitespace().next().unwrap_or("").to_string());
                if !bin.is_empty() && on_path(&bin) {
                    out.push(Thumbnailer { exec, mimes });
                }
            }
        }
        out
    })
}

/// The thumbnailer for a MIME type (memoized: asked for every visible item each frame).
pub fn thumbnailer_for(mime: &str) -> Option<&'static Thumbnailer> {
    if mime.is_empty() || mime == "application/octet-stream" {
        return None;
    }
    static MEMO: std::sync::Mutex<Option<std::collections::HashMap<String, Option<usize>>>> = std::sync::Mutex::new(None);
    let mut memo = MEMO.lock().unwrap_or_else(|e| e.into_inner());
    let map = memo.get_or_insert_with(Default::default);
    let i = *map.entry(mime.to_string()).or_insert_with(|| thumbnailers().iter().position(|t| t.mimes.iter().any(|m| m == mime)));
    i.map(|i| &thumbnailers()[i])
}

/// External thumbnailer runs so far (tests check cached thumbnails aren't remade).
pub static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Run a thumbnailer on `path` for `size` pixels (killed after `timeout`). Its PNG is
/// decoded and returned.
pub fn run_external(t: &Thumbnailer, path: &Path, size: u32, timeout: Duration) -> Option<Rgba> {
    let dir = std::env::temp_dir().join(format!("maia-thumbs-{}", unsafe { libc::getuid() }));
    {
        use std::os::unix::fs::DirBuilderExt;
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir);
    }
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let out = dir.join(format!("{}-{}.png", std::process::id(), N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    let uri = uri_for(path);
    // Desktop-entry style field codes; each token is one argument (no shell).
    let argv: Vec<String> = t
        .exec
        .split_whitespace()
        .map(|w| w.replace("%i", &path.to_string_lossy()).replace("%u", &uri).replace("%o", &out.to_string_lossy()).replace("%s", &size.to_string()).replace("%%", "%"))
        .collect();
    let (bin, args) = argv.split_first()?;
    RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut child = std::process::Command::new(bin).args(args).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().ok()?;
    let t0 = Instant::now();
    let ok = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.success(),
            Ok(None) if t0.elapsed() < timeout => std::thread::sleep(Duration::from_millis(15)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let img = if ok { std::fs::read(&out).ok().and_then(|b| crate::imgdec::decode_bytes(&b)) } else { None };
    let _ = std::fs::remove_file(&out);
    img
}
