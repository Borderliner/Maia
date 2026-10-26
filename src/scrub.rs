//! A photo as it may be shared: without where it was taken and without the camera that
//! took it (make, model, lens, owner, serial numbers, the maker's own notes, which can
//! hold any of them). The original is never touched; a cleaned copy is made in memory.
//!
//! JPEG, PNG, WebP and TIFF are cleaned here, in place: the fields' bytes are blanked
//! and the location's directory emptied, so nothing moves and the picture itself is
//! never decoded or saved again. Copies kept elsewhere in the file go too: the XMP
//! packet, the IPTC record and the text a PNG writer may have put the Exif in again. Other kinds (HEIC above all) go through exiftool when it
//! is installed, and can't be cleaned without it.

use std::path::Path;

/// The picture kinds cleaned without help.
pub fn built_in(ext: &str) -> bool {
    matches!(ext.to_ascii_lowercase().as_str(), "jpg" | "jpeg" | "jpe" | "png" | "webp" | "tif" | "tiff")
}

/// Pictures that carry a place: what sharing without it applies to.
pub fn applies(ext: &str) -> bool {
    built_in(ext) || matches!(ext.to_ascii_lowercase().as_str(), "heic" | "heif" | "avif" | "dng" | "cr2" | "cr3" | "nef" | "arw" | "orf" | "rw2" | "raf")
}

/// Whether a picture of this kind can be cleaned on this machine.
pub fn can(ext: &str, tools: Option<&crate::drives::Tools>) -> bool {
    built_in(ext) || applies(ext) && tools.is_some_and(|t| t.has("exiftool"))
}

/// The file's bytes without its place and camera; an error says why it can't be.
pub fn clean(path: &Path, tools: Option<&crate::drives::Tools>) -> Result<Vec<u8>, String> {
    let ext = path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    if built_in(&ext) {
        let mut b = std::fs::read(path).map_err(|e| e.to_string())?;
        bytes(&mut b);
        return Ok(b);
    }
    let Some(t) = tools.filter(|t| t.has("exiftool")) else { return Err("needs exiftool".into()) };
    let mut c = crate::media::tool_cmd(t.cmd("exiftool"));
    c.args(["-q", "-q", "-o", "-", "-gps:all=", "-xmp:all=", "-makernotes:all=", "-Make=", "-Model=", "-LensMake=", "-LensModel=", "-SerialNumber=", "-InternalSerialNumber=", "-BodySerialNumber=", "-LensSerialNumber=", "-CameraOwnerName=", "-OwnerName="]).arg(path).stdout(std::process::Stdio::piped());
    let out = c.output().map_err(|e| e.to_string())?;
    if !out.status.success() || out.stdout.is_empty() {
        return Err("exiftool couldn't read it".into());
    }
    Ok(out.stdout)
}

/// Clean a JPEG, PNG, WebP or TIFF file held in `b`, keeping its length.
pub fn bytes(b: &mut [u8]) {
    if let Some(block) = crate::exif::block(b) {
        let at = block.as_ptr() as usize - b.as_ptr() as usize;
        let len = block.len();
        let tiff = &mut b[at..at + len];
        let skip = if tiff.starts_with(b"Exif\0\0") { 6 } else { 0 };
        tiff_fields(&mut tiff[skip..]);
        // A PNG's eXIf chunk ends with a checksum of its type and data.
        if b.starts_with(b"\x89PNG\r\n\x1a\n") && at >= 4 && at + len + 4 <= b.len() {
            let crc = crc32(&b[at - 4..at + len]);
            b[at + len..at + len + 4].copy_from_slice(&crc.to_be_bytes());
        }
    }
    xmp(b);
}

/// IFD0's Make (0x010F), Model (0x0110) and Artist (0x013B); the Exif IFD's owner, lens
/// and serial numbers (0xA430 to 0xA435, but 0xA432, the lens's focal range) and its
/// MakerNote (0x927C).
const IFD0_GONE: [u16; 3] = [0x010F, 0x0110, 0x013B];
/// A TIFF's own copies of the rest: its XMP packet (700) and IPTC record (33723, which
/// can name the city and the country).
const IFD0_PACKETS: [u16; 2] = [0x02BC, 0x83BB];
const EXIF_GONE: [u16; 6] = [0xA430, 0xA431, 0xA433, 0xA434, 0xA435, 0x927C];

fn tiff_fields(d: &mut [u8]) {
    if d.len() < 8 || !(d.starts_with(b"II") || d.starts_with(b"MM")) {
        return;
    }
    let le = d[0] == b'I';
    let Some(ifd0) = rd32(d, 4, le) else { return };
    let mut sub = (None, None);
    for (tag, ty, n, at) in entries(d, ifd0 as usize, le) {
        match tag {
            0x8769 => sub.0 = rd32(d, at, le),
            0x8825 => sub.1 = rd32(d, at, le),
            t if IFD0_GONE.contains(&t) || IFD0_PACKETS.contains(&t) => blank(d, ty, n, at, le),
            // Photoshop's resources, which hold the IPTC record once more.
            0x8649 if matches!(ty, 1 | 7) && n > 4 => {
                if let Some(o) = rd32(d, at, le).map(|o| o as usize) {
                    if let Some(v) = o.checked_add(n as usize).and_then(|e| d.get_mut(o..e)) {
                        resources(v);
                    }
                }
            }
            _ => {}
        }
    }
    if let Some(x) = sub.0 {
        for (tag, ty, n, at) in entries(d, x as usize, le) {
            if EXIF_GONE.contains(&tag) {
                blank(d, ty, n, at, le);
            }
        }
    }
    // The location's directory: every value blanked, then no entries at all.
    if let Some(g) = sub.1.map(|g| g as usize) {
        let list = entries(d, g, le);
        for &(_, ty, n, at) in &list {
            blank(d, ty, n, at, le);
        }
        let end = (g + 2 + list.len() * 12).min(d.len());
        if g + 2 <= d.len() {
            d[g..g + 2].fill(0);
            d[g + 2..end].fill(0);
        }
    }
}

/// Zero a field's value, wherever it is kept.
fn blank(d: &mut [u8], ty: u16, n: u32, at: usize, le: bool) {
    let unit = match ty {
        1 | 2 | 6 | 7 => 1,
        3 | 8 => 2,
        4 | 9 | 11 => 4,
        5 | 10 | 12 => 8,
        _ => return,
    };
    let size = unit * n as usize;
    let o = if size <= 4 { at } else { match rd32(d, at, le) { Some(o) => o as usize, None => return } };
    if let Some(v) = o.checked_add(size).and_then(|end| d.get_mut(o..end)) {
        v.fill(0);
    }
}

fn entries(d: &[u8], at: usize, le: bool) -> Vec<(u16, u16, u32, usize)> {
    let Some(n) = rd16(d, at, le) else { return vec![] };
    (0..n.min(512) as usize).filter_map(|k| {
        let e = at + 2 + k * 12;
        Some((rd16(d, e, le)?, rd16(d, e + 2, le)?, rd32(d, e + 4, le)?, e + 8))
    }).collect()
}

fn rd16(d: &[u8], i: usize, le: bool) -> Option<u16> {
    let s = d.get(i..i + 2)?;
    Some(if le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) })
}

fn rd32(d: &[u8], i: usize, le: bool) -> Option<u32> {
    let s = d.get(i..i + 4)?;
    Some(if le { u32::from_le_bytes([s[0], s[1], s[2], s[3]]) } else { u32::from_be_bytes([s[0], s[1], s[2], s[3]]) })
}

/// The XMP packet, where editors keep a second copy of the place and the camera:
/// spaces over all of it (a packet may be padded with them), in a JPEG's APP1, a PNG's
/// iTXt (its checksum written again) or a WebP's XMP chunk.
fn xmp(b: &mut [u8]) {
    const NS: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";
    const EXT: &[u8] = b"http://ns.adobe.com/xmp/extension/\0";
    if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i + 4 <= b.len() && b[i] == 0xFF {
            let m = b[i + 1];
            if m == 0xDA || m == 0xD9 {
                return;
            }
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            let (s, e) = (i + 4, (i + 2 + len).min(b.len()));
            if m == 0xE1 && s <= e {
                let seg = &mut b[s..e];
                for head in [NS, EXT] {
                    if seg.starts_with(head) {
                        seg[head.len()..].fill(b' ');
                    }
                }
            }
            if m == 0xED && s <= e {
                iptc(&mut b[s..e]);
            }
            i += 2 + len;
        }
        return;
    }
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        let mut i = 8;
        while i + 12 <= b.len() {
            let len = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
            let Some(end) = (i + 8).checked_add(len).filter(|&e| e + 4 <= b.len()) else { return };
            if &b[i + 4..i + 8] == b"iTXt" && b[i + 8..end].starts_with(b"XML:com.adobe.xmp\0") {
                // Keyword, NUL, compressed flag, method, language, NUL, translated
                // keyword, NUL, then the text: spaces over that, stored uncompressed.
                let data = i + 8;
                let flag = data + 18;
                let nul = |from: usize| b.get(from..end).and_then(|s| s.iter().position(|&c| c == 0)).map(|k| from + k);
                let text = nul(flag + 2).and_then(|l| nul(l + 1)).map(|t| t + 1);
                if let Some(t) = text.filter(|&t| t <= end) {
                    b[flag] = 0;
                    b[flag + 1] = 0;
                    b[t..end].fill(b' ');
                    let crc = crc32(&b[i + 4..end]);
                    b[end..end + 4].copy_from_slice(&crc.to_be_bytes());
                }
            }
            if matches!(&b[i + 4..i + 8], b"tEXt" | b"zTXt" | b"iTXt") && text_says_too_much(&b[i + 8..end]) {
                // ImageMagick and others write the Exif again as text ("exif:GPSLatitude",
                // "Raw profile type exif"): a blank comment of the same length instead.
                b[i + 4..i + 8].copy_from_slice(b"tEXt");
                let key: &[u8] = if len >= 8 { b"Comment\0" } else { b"C\0" };
                if len >= key.len() {
                    b[i + 8..i + 8 + key.len()].copy_from_slice(key);
                    b[i + 8 + key.len()..end].fill(b' ');
                } else {
                    b[i + 8..end].fill(b' ');
                }
                let crc = crc32(&b[i + 4..end]);
                b[end..end + 4].copy_from_slice(&crc.to_be_bytes());
            }
            if &b[i + 4..i + 8] == b"IEND" {
                return;
            }
            i = end + 4;
        }
        return;
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        let mut i = 12;
        while i + 8 <= b.len() {
            let len = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
            let end = (i + 8 + len).min(b.len());
            if &b[i..i + 4] == b"XMP " {
                b[i + 8..end].fill(b' ');
            }
            i += 8 + len + (len & 1);
        }
    }
}

/// A PNG text chunk (`data` from its keyword on) that holds the place or the camera: a
/// copy of the Exif, XMP or IPTC kept as text, or one of ImageMagick's "exif:" fields
/// naming them.
fn text_says_too_much(data: &[u8]) -> bool {
    let key = data.split(|&c| c == 0).next().unwrap_or_default().to_ascii_lowercase();
    if key.starts_with(b"raw profile type ") {
        let what = &key[17..];
        return [&b"exif"[..], b"xmp", b"app1", b"iptc", b"8bim"].contains(&what);
    }
    let Some(field) = key.strip_prefix(b"exif:") else { return false };
    [&b"gps"[..], b"make", b"model", b"serial", b"owner", b"artist", b"lens", b"maker"].iter().any(|w| field.windows(w.len()).any(|x| x == *w))
}

/// A JPEG's APP13 (Photoshop's resources): the IPTC record among them zeroed, the rest
/// left as they are.
fn iptc(seg: &mut [u8]) {
    const HEAD: &[u8] = b"Photoshop 3.0\0";
    if seg.starts_with(HEAD) {
        resources(&mut seg[HEAD.len()..]);
    }
}

/// Photoshop's resources ("8BIM" blocks): the IPTC record's zeroed.
fn resources(seg: &mut [u8]) {
    let mut i = 0;
    while i + 12 <= seg.len() && &seg[i..i + 4] == b"8BIM" {
        let id = u16::from_be_bytes([seg[i + 4], seg[i + 5]]);
        // A Pascal name, padded to an even length.
        let name = seg[i + 6] as usize;
        let at = i + 6 + (name + 2) / 2 * 2;
        let Some(size) = rd32(seg, at, false).map(|s| s as usize) else { return };
        let (s, e) = (at + 4, (at + 4).saturating_add(size).min(seg.len()));
        if id == 0x0404 && s <= e {
            seg[s..e].fill(0);
        }
        i = at + 4 + size + (size & 1);
    }
}

/// The checksum a PNG chunk ends with.
fn crc32(d: &[u8]) -> u32 {
    let mut c = 0xFFFF_FFFFu32;
    for &x in d {
        c ^= x as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
        }
    }
    !c
}

/// Copies of `files` to attach: the pictures cleaned, in a folder of their own under the
/// session's runtime folder (in memory, gone at logout: the mail program reads them when
/// it sends, which may be long after), everything else as it is.
pub fn copies(files: &[std::path::PathBuf], tools: Option<&crate::drives::Tools>) -> Result<Vec<std::path::PathBuf>, String> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    let leaf = format!("maia-shared/{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed));
    let mut dir: Option<std::path::PathBuf> = None;
    let mut out = vec![];
    for f in files {
        let ext = f.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or_default();
        if !applies(&ext) {
            out.push(f.clone());
            continue;
        }
        let name = f.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let b = clean(f, tools).map_err(|e| format!("Couldn't leave out the place in {name}: {e}"))?;
        // Made for the first picture; the temporary folder when there is no runtime
        // folder, or it can't be written.
        let dir = match &dir {
            Some(d) => d.clone(),
            None => {
                let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(|b| std::path::PathBuf::from(b).join(&leaf)).filter(|d| std::fs::create_dir_all(d).is_ok());
                let d = runtime.unwrap_or_else(|| std::env::temp_dir().join(&leaf));
                std::fs::create_dir_all(&d).map_err(|e| e.to_string())?;
                dir.insert(d).clone()
            }
        };
        let to = dir.join(&name);
        std::fs::write(&to, b).map_err(|e| e.to_string())?;
        out.push(to);
    }
    Ok(out)
}
