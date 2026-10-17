//! The few EXIF fields Maia uses: which way up a photo is, when it was taken and where.
//! Read from the TIFF block a JPEG, PNG, WebP or TIFF file carries, without a library.

use std::io::Read;
use std::path::Path;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Exif {
    /// 1..=8; 1 when the photo doesn't say.
    pub orientation: u8,
    /// When it was taken (DateTimeOriginal, else the file's DateTime): year, month, day,
    /// hour, minute, second as the camera's clock said.
    pub taken: Option<[u32; 6]>,
    /// Latitude and longitude in degrees, south and west negative.
    pub gps: Option<(f64, f64)>,
}

/// A TIFF block ("II*\0…" or "MM\0*…"), with or without the "Exif\0\0" a JPEG puts
/// before it.
pub fn parse(e: &[u8]) -> Exif {
    let d = e.strip_prefix(b"Exif\0\0").unwrap_or(e);
    let mut out = Exif { orientation: 1, ..Default::default() };
    if d.len() < 8 || !(d.starts_with(b"II") || d.starts_with(b"MM")) {
        return out;
    }
    let t = Tiff { d, le: d[0] == b'I' };
    let Some(ifd0) = t.u32(4) else { return out };
    let mut exif_at = None;
    let mut gps_at = None;
    let mut date = None;
    for (tag, ty, n, at) in t.entries(ifd0 as usize) {
        match tag {
            0x0112 => out.orientation = t.short(ty, at).map(|v| v as u8).filter(|v| (1..=8).contains(v)).unwrap_or(1),
            0x0132 => date = t.ascii(ty, n, at).and_then(|s| when(&s)),
            0x8769 => exif_at = t.u32(at),
            0x8825 => gps_at = t.u32(at),
            _ => {}
        }
    }
    if let Some(x) = exif_at {
        for (tag, ty, n, at) in t.entries(x as usize) {
            if tag == 0x9003 {
                if let Some(w) = t.ascii(ty, n, at).and_then(|s| when(&s)) {
                    date = Some(w);
                }
            }
        }
    }
    out.taken = date;
    if let Some(g) = gps_at {
        let (mut lat, mut lon, mut lat_ref, mut lon_ref) = (None, None, b'N', b'E');
        for (tag, ty, n, at) in t.entries(g as usize) {
            match tag {
                1 => lat_ref = t.ascii(ty, n, at).and_then(|s| s.bytes().next()).unwrap_or(b'N'),
                2 => lat = t.degrees(ty, n, at),
                3 => lon_ref = t.ascii(ty, n, at).and_then(|s| s.bytes().next()).unwrap_or(b'E'),
                4 => lon = t.degrees(ty, n, at),
                _ => {}
            }
        }
        if let (Some(a), Some(o)) = (lat, lon) {
            let a = if lat_ref == b'S' { -a } else { a };
            let o = if lon_ref == b'W' { -o } else { o };
            // 0, 0 is what a phone writes before it has a fix.
            if a.abs() <= 90.0 && o.abs() <= 180.0 && (a, o) != (0.0, 0.0) {
                out.gps = Some((a, o));
            }
        }
    }
    out
}

/// "2024:06:01 14:03:22" as its six numbers; a blank or zeroed date is none.
fn when(s: &str) -> Option<[u32; 6]> {
    let n: Vec<u32> = s.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).filter_map(|p| p.parse().ok()).collect();
    let v: [u32; 6] = n.get(..6)?.try_into().ok()?;
    ((1800..3000).contains(&v[0]) && (1..=12).contains(&v[1]) && (1..=31).contains(&v[2])).then_some(v)
}

struct Tiff<'a> {
    d: &'a [u8],
    le: bool,
}

impl Tiff<'_> {
    fn u16(&self, i: usize) -> Option<u16> {
        let s = self.d.get(i..i + 2)?;
        Some(if self.le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) })
    }

    fn u32(&self, i: usize) -> Option<u32> {
        let s = self.d.get(i..i + 4)?;
        Some(if self.le { u32::from_le_bytes([s[0], s[1], s[2], s[3]]) } else { u32::from_be_bytes([s[0], s[1], s[2], s[3]]) })
    }

    /// Each entry of the IFD at `at`: tag, type, count and where its value field is.
    fn entries(&self, at: usize) -> Vec<(u16, u16, u32, usize)> {
        let Some(n) = self.u16(at) else { return vec![] };
        (0..n.min(512) as usize).filter_map(|k| {
            let e = at + 2 + k * 12;
            Some((self.u16(e)?, self.u16(e + 2)?, self.u32(e + 4)?, e + 8))
        }).collect()
    }

    /// Where a value of `size` bytes is: in the field itself when it fits, else at the
    /// offset the field holds.
    fn value_at(&self, size: usize, at: usize) -> Option<usize> {
        if size <= 4 { Some(at) } else { self.u32(at).map(|o| o as usize) }
    }

    fn short(&self, ty: u16, at: usize) -> Option<u16> {
        match ty {
            3 => self.u16(at),
            4 => self.u32(at).map(|v| v as u16),
            _ => None,
        }
    }

    fn ascii(&self, ty: u16, n: u32, at: usize) -> Option<String> {
        if ty != 2 {
            return None;
        }
        let n = n as usize;
        let o = self.value_at(n, at)?;
        let b = self.d.get(o..o.checked_add(n)?)?;
        let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
        Some(String::from_utf8_lossy(&b[..end]).trim().to_string())
    }

    /// Three rationals, degrees, minutes and seconds, as degrees.
    fn degrees(&self, ty: u16, n: u32, at: usize) -> Option<f64> {
        if ty != 5 || n != 3 {
            return None;
        }
        let o = self.value_at(24, at)?;
        let r = |k: usize| -> Option<f64> {
            let (a, b) = (self.u32(o + k * 8)?, self.u32(o + k * 8 + 4)?);
            (b != 0).then(|| a as f64 / b as f64)
        };
        Some(r(0)? + r(1)? / 60.0 + r(2)? / 3600.0)
    }
}

/// The TIFF block in a file's first bytes: a JPEG's APP1, a PNG's eXIf chunk, a WebP's
/// EXIF chunk, or a TIFF file itself.
pub fn block(b: &[u8]) -> Option<&[u8]> {
    if b.starts_with(&[0xFF, 0xD8]) {
        let mut i = 2;
        while i + 4 <= b.len() && b[i] == 0xFF {
            let m = b[i + 1];
            if m == 0xDA || m == 0xD9 {
                return None;
            }
            let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
            let seg = b.get(i + 4..i + 2 + len)?;
            if m == 0xE1 && seg.starts_with(b"Exif\0\0") {
                return Some(seg);
            }
            i += 2 + len;
        }
        return None;
    }
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        let mut i = 8;
        while i + 8 <= b.len() {
            let len = u32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]) as usize;
            let kind = &b[i + 4..i + 8];
            if kind == b"eXIf" {
                return b.get(i + 8..i + 8 + len);
            }
            if kind == b"IDAT" || kind == b"IEND" {
                return None;
            }
            i += 12 + len;
        }
        return None;
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        let mut i = 12;
        while i + 8 <= b.len() {
            let len = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
            if &b[i..i + 4] == b"EXIF" {
                return b.get(i + 8..i + 8 + len);
            }
            i += 8 + len + (len & 1);
        }
        return None;
    }
    (b.starts_with(b"II*\0") || b.starts_with(b"MM\0*")).then_some(b)
}

/// What a photo file says about itself, reading no more than its first 256 KB (a JPEG's
/// EXIF comes first; a TIFF's IFDs are near the start too).
pub fn read(path: &Path) -> Option<Exif> {
    let mut f = std::fs::File::open(path).ok()?;
    if !f.metadata().ok()?.is_file() {
        return None;
    }
    let mut b = Vec::with_capacity(64 << 10);
    (&mut f).take(256 << 10).read_to_end(&mut b).ok()?;
    block(&b).map(parse)
}

/// "33.85980° S, 151.21120° E".
pub fn place(lat: f64, lon: f64) -> String {
    format!("{:.5}° {}, {:.5}° {}", lat.abs(), if lat < 0.0 { 'S' } else { 'N' }, lon.abs(), if lon < 0.0 { 'W' } else { 'E' })
}

/// The place on OpenStreetMap, marked, at street level.
pub fn map_url(lat: f64, lon: f64) -> String {
    format!("https://www.openstreetmap.org/?mlat={lat:.6}&mlon={lon:.6}#map=16/{lat:.6}/{lon:.6}")
}
