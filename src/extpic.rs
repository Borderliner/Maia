//! Pictures Maia has no decoder of its own for (HEIC, AVIF, TIFF, JPEG XL, camera RAW),
//! decoded by an installed tool at the size shown: ImageMagick first, turned upright as
//! the photo says, else libheif's, libjxl's or LibRaw's own command. They come through
//! the thumbnail cache like any picture, on its workers.

use std::path::Path;
use std::time::Duration;

use crate::drives::Tools;
use crate::imgdec::Rgba;

/// A big photo or RAW file can take a while; past this it is given up on.
const TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy)]
enum Kind {
    Heif,
    Avif,
    Tiff,
    Jxl,
    Raw,
}

fn kind(ext: &str) -> Option<Kind> {
    Some(match ext {
        "heic" | "heif" => Kind::Heif,
        "avif" => Kind::Avif,
        "tif" | "tiff" => Kind::Tiff,
        "jxl" => Kind::Jxl,
        "cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" => Kind::Raw,
        _ => return None,
    })
}

/// The format's own tools, tried after ImageMagick.
fn others(k: Kind) -> &'static [&'static str] {
    match k {
        Kind::Heif | Kind::Avif => &["heif-dec", "heif-convert"],
        Kind::Jxl => &["djxl"],
        Kind::Raw => &["dcraw_emu", "dcraw"],
        Kind::Tiff => &[],
    }
}

pub fn is_ext(ext: &str) -> bool {
    kind(ext).is_some()
}

/// Whether an installed tool reads pictures like this.
pub fn can(t: &Tools, ext: &str) -> bool {
    kind(ext).is_some_and(|k| crate::picfix::magick(t).is_some() || others(k).iter().any(|n| t.has(n)))
}

/// What to install to see such a picture, when nothing here reads it.
pub fn install_note(ext: &str) -> Option<&'static str> {
    Some(match kind(ext)? {
        Kind::Heif => "Install ImageMagick to see HEIC photos",
        Kind::Avif => "Install ImageMagick to see AVIF pictures",
        Kind::Tiff => "Install ImageMagick to see TIFF pictures",
        Kind::Jxl => "Install ImageMagick or libjxl to see JPEG XL pictures",
        Kind::Raw => "Install ImageMagick to see camera RAW photos",
    })
}

/// `path` decoded, the right way up, within `max`×`max`, and its upright size.
pub fn load(t: &Tools, path: &Path, max: u32) -> Option<(Rgba, [u32; 2])> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    let k = kind(&ext)?;
    let im = crate::picfix::magick(t).and_then(|prog| magick(t, prog, path, &ext, max));
    im.or_else(|| others(k).iter().filter(|n| t.has(n)).find_map(|n| other(t, n, path, max)))
}

/// ImageMagick reading the picture on its standard input as what its name says it is
/// (a name with brackets or a colon is never read as one of its own), first frame or
/// page only, upright and scaled down in one go, written as a PAM followed by the
/// upright size it had.
fn magick(t: &Tools, prog: &str, path: &Path, ext: &str, max: u32) -> Option<(Rgba, [u32; 2])> {
    let input = std::fs::File::open(path).ok()?;
    let format = match ext {
        "heif" => "heic",
        "tif" => "tiff",
        e => e,
    };
    let mut c = t.cmd(prog);
    c.arg(format!("{format}:-[0]")).args(["-auto-orient", "-print", "%w %h\n", "-thumbnail"]).arg(format!("{max}x{max}>")).args(["-colorspace", "sRGB", "-depth", "8", "pam:-"]);
    let out = crate::errands::output_io(c, TIMEOUT, Some(input), None).ok()?;
    // ImageMagick 7.1.2 prints the size before the picture; earlier ones after it.
    let (head, body) = match out.first() {
        Some(b'P') => (&[][..], &out[..]),
        _ => out.split_at(out.iter().position(|&b| b == b'\n').map_or(0, |i| i + 1)),
    };
    let (img, rest) = pnm(body)?;
    let mut n = std::str::from_utf8(head).ok()?.split_whitespace().chain(std::str::from_utf8(rest).ok()?.split_whitespace()).filter_map(|v| v.parse().ok());
    let dims = n.next().zip(n.next()).map_or([img.w, img.h], |(w, h)| [w, h]);
    Some((img, dims))
}

/// libheif's or libjxl's decoder writing a PNG, or LibRaw's or dcraw's a PPM (at half
/// size, which is quicker and still more than a screen), scaled down here.
fn other(t: &Tools, tool: &str, path: &Path, max: u32) -> Option<(Rgba, [u32; 2])> {
    let mut c = t.cmd(tool);
    let img = if tool.starts_with("dcraw") {
        c.args(if tool == "dcraw" { &["-c", "-w", "-h"][..] } else { &["-w", "-h", "-Z", "-"] }).arg(path);
        pnm(&crate::errands::output(c, TIMEOUT).ok()?)?.0
    } else {
        let out = crate::thumbcache::scratch_png();
        c.arg(path).arg(&out);
        let png = crate::errands::output(c, TIMEOUT).ok().and_then(|_| std::fs::read(&out).ok());
        let _ = std::fs::remove_file(&out);
        crate::imgdec::decode_bytes(&png?)?
    };
    let dims = [img.w, img.h];
    Some((if img.w > max || img.h > max { crate::imgdec::downscale(&img, max) } else { img }, dims))
}

/// An 8-bit PAM, PPM or PGM as RGBA, and whatever follows it.
pub fn pnm(b: &[u8]) -> Option<(Rgba, &[u8])> {
    let (w, h, depth, maxval, at) = if b.starts_with(b"P7\n") {
        let end = b.windows(7).position(|x| x == b"ENDHDR\n")?;
        let head = std::str::from_utf8(&b[..end]).ok()?;
        let get = |k: &str| head.lines().find_map(|l| l.strip_prefix(k)?.trim().parse::<usize>().ok());
        (get("WIDTH ")?, get("HEIGHT ")?, get("DEPTH ")?, get("MAXVAL ")?, end + 7)
    } else if b.starts_with(b"P6") || b.starts_with(b"P5") {
        // Three numbers between blanks and comments, then one blank before the samples.
        let (mut i, mut n) = (2, [0usize; 3]);
        for v in &mut n {
            while let Some(&c) = b.get(i).filter(|c| c.is_ascii_whitespace() || **c == b'#') {
                i += 1;
                if c == b'#' {
                    while b.get(i).is_some_and(|c| *c != b'\n') {
                        i += 1;
                    }
                }
            }
            let start = i;
            while b.get(i).is_some_and(u8::is_ascii_digit) {
                i += 1;
            }
            *v = std::str::from_utf8(&b[start..i]).ok()?.parse().ok()?;
        }
        (n[0], n[1], if b[1] == b'6' { 3 } else { 1 }, n[2], i + 1)
    } else {
        return None;
    };
    if w == 0 || h == 0 || maxval != 255 || w.checked_mul(h)? > 1 << 28 {
        return None;
    }
    let len = w * h * depth;
    let px = crate::imgdec::rgba_from(b.get(at..at + len)?.to_vec(), depth)?;
    Some((Rgba { w: w as u32, h: h as u32, px }, &b[at + len..]))
}
