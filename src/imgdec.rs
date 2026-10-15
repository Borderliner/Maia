//! Image decoding without the `image` framework: direct PNG/JPEG/WebP/GIF decoders,
//! small BMP/ICO parsers, EXIF orientation and a box-filter downscaler.

use std::io::{BufReader, Cursor};
use std::path::Path;

pub struct Rgba {
    pub w: u32,
    pub h: u32,
    pub px: Vec<u8>,
}

/// Decode `path`, apply EXIF orientation, and downscale to fit `max`×`max`.
/// Returns the image and its original (oriented) dimensions.
pub fn load(path: &Path, max: u32) -> Option<(Rgba, [u32; 2])> {
    let meta = std::fs::metadata(path).ok()?;
    // Regular files only: reading a FIFO or device named "*.png" would block forever.
    if !meta.is_file() || meta.len() > 512 << 20 {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let r = shrink(decode(&bytes), max);
    release();
    r
}

fn shrink(d: Option<(Rgba, u8)>, max: u32) -> Option<(Rgba, [u32; 2])> {
    let (mut img, orient) = d?;
    if img.w == 0 || img.h == 0 {
        return None;
    }
    // Original size as displayed (orientations 5-8 swap width and height).
    let dims = if orient >= 5 { [img.h, img.w] } else { [img.w, img.h] };
    // Downscale first, then rotate the small result: same pixels, no full-size copy.
    if img.w > max || img.h > max {
        img = downscale(&img, max);
    }
    if orient > 1 {
        img = orient_image(img, orient);
    }
    Some((img, dims))
}

/// Decode an in-memory image (PNG, JPEG, WebP, GIF, BMP, ICO) without EXIF rotation.
pub fn decode_bytes(b: &[u8]) -> Option<Rgba> {
    let r = decode(b).map(|(i, _)| i);
    release();
    r
}

/// Largest image decoded at all (pixels). Checked from the header, before any pixel
/// buffer is allocated, so a huge or malformed file can't exhaust memory.
const MAX_PIXELS: u64 = 100_000_000;

/// Size check right before a format allocates its pixel buffer; on success the decode
/// also takes its share of the shared decode budget (see `reserve`).
fn dims_ok(w: u32, h: u32) -> bool {
    let ok = w > 0 && h > 0 && (w as u64) * (h as u64) <= MAX_PIXELS;
    if ok {
        reserve(w as u64 * h as u64 * 4);
    }
    ok
}

/// Full-size pixel buffers that may exist at once across decoding threads. A photo is
/// decoded at full resolution before it's downscaled, so without this three thumbnail
/// workers on 48 MP photos would briefly hold ~600 MB. One decode may always run alone.
const DECODE_BUDGET: u64 = 160 << 20;
static IN_FLIGHT: std::sync::Mutex<u64> = std::sync::Mutex::new(0);
static FREED: std::sync::Condvar = std::sync::Condvar::new();
thread_local! {
    static HELD: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn reserve(bytes: u64) {
    if HELD.with(|h| h.get()) > 0 {
        return; // nested decode (an ICO's embedded PNG) shares the outer reservation
    }
    let mut used = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    while *used > 0 && *used + bytes > DECODE_BUDGET {
        used = FREED.wait(used).unwrap_or_else(|e| e.into_inner());
    }
    *used += bytes;
    HELD.with(|h| h.set(bytes));
}

/// Return this thread's reservation (after the full-size buffer is gone).
fn release() {
    let n = HELD.with(|h| h.replace(0));
    if n > 0 {
        let mut used = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        *used = used.saturating_sub(n);
        FREED.notify_all();
    }
}

fn decode(b: &[u8]) -> Option<(Rgba, u8)> {
    if b.starts_with(b"\x89PNG") {
        return png(b).map(|i| (i, 1));
    }
    if b.starts_with(&[0xFF, 0xD8]) {
        return jpeg(b);
    }
    if b.len() > 12 && &b[0..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return webp(b).map(|i| (i, 1));
    }
    if b.starts_with(b"GIF8") {
        return gif(b).map(|i| (i, 1));
    }
    if b.starts_with(b"BM") {
        return bmp(b, false).map(|i| (i, 1));
    }
    if b.len() > 6 && b[0..4] == [0, 0, 1, 0] {
        return ico(b).map(|i| (i, 1));
    }
    None
}

/// A PNG and its tEXt chunks (the thumbnail cache keeps its metadata there).
pub fn png_with_text(b: &[u8]) -> Option<(Rgba, Vec<(String, String)>)> {
    let mut dec = png::Decoder::new(Cursor::new(b));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    let r = dec.read_info().ok()?;
    let text = r.info().uncompressed_latin1_text.iter().map(|t| (t.keyword.clone(), t.text.clone())).collect();
    let img = png_frame(r);
    release();
    Some((img?, text))
}

fn png(b: &[u8]) -> Option<Rgba> {
    let mut dec = png::Decoder::new(Cursor::new(b));
    dec.set_transformations(png::Transformations::normalize_to_color8());
    png_frame(dec.read_info().ok()?)
}

fn png_frame(mut r: png::Reader<Cursor<&[u8]>>) -> Option<Rgba> {
    if !dims_ok(r.info().width, r.info().height) {
        return None;
    }
    let mut buf = vec![0u8; r.output_buffer_size()];
    let info = r.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    let (w, h) = (info.width, info.height);
    let px = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => expand(&buf, 3, |p| [p[0], p[1], p[2], 255]),
        png::ColorType::Grayscale => expand(&buf, 1, |p| [p[0], p[0], p[0], 255]),
        png::ColorType::GrayscaleAlpha => expand(&buf, 2, |p| [p[0], p[0], p[0], p[1]]),
        png::ColorType::Indexed => return None, // expanded by normalize_to_color8
    };
    Some(Rgba { w, h, px })
}

fn expand(src: &[u8], n: usize, f: impl Fn(&[u8]) -> [u8; 4]) -> Vec<u8> {
    let mut out = Vec::with_capacity(src.len() / n * 4);
    for p in src.chunks_exact(n) {
        out.extend_from_slice(&f(p));
    }
    out
}

fn jpeg(b: &[u8]) -> Option<(Rgba, u8)> {
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGBA).set_max_width(1 << 16).set_max_height(1 << 16);
    let mut d = zune_jpeg::JpegDecoder::new_with_options(b, opts);
    d.decode_headers().ok()?;
    let orient = d.exif().map(|e| exif_orientation(e)).unwrap_or(1);
    let (w, h) = d.dimensions()?;
    if !dims_ok(w as u32, h as u32) {
        return None;
    }
    let px = d.decode().ok()?;
    if px.len() != w * h * 4 {
        return None;
    }
    Some((Rgba { w: w as u32, h: h as u32, px }, orient))
}

/// Orientation (1..=8) from a raw EXIF/TIFF block.
fn exif_orientation(e: &[u8]) -> u8 {
    let d = e.strip_prefix(b"Exif\0\0").unwrap_or(e);
    if d.len() < 8 {
        return 1;
    }
    let le = &d[0..2] == b"II";
    let u16_at = |i: usize| -> Option<u16> {
        let s = d.get(i..i + 2)?;
        Some(if le { u16::from_le_bytes([s[0], s[1]]) } else { u16::from_be_bytes([s[0], s[1]]) })
    };
    let u32_at = |i: usize| -> Option<u32> {
        let s = d.get(i..i + 4)?;
        Some(if le { u32::from_le_bytes([s[0], s[1], s[2], s[3]]) } else { u32::from_be_bytes([s[0], s[1], s[2], s[3]]) })
    };
    let Some(ifd) = u32_at(4) else { return 1 };
    let ifd = ifd as usize;
    let Some(n) = u16_at(ifd) else { return 1 };
    for k in 0..n as usize {
        let at = ifd + 2 + k * 12;
        if u16_at(at) == Some(0x0112) {
            return u16_at(at + 8).map(|v| v as u8).filter(|v| (1..=8).contains(v)).unwrap_or(1);
        }
    }
    1
}

fn webp(b: &[u8]) -> Option<Rgba> {
    let mut d = image_webp::WebPDecoder::new(Cursor::new(b)).ok()?;
    let (w, h) = d.dimensions();
    if !dims_ok(w, h) {
        return None;
    }
    let alpha = d.has_alpha();
    let mut buf = vec![0u8; d.output_buffer_size()?];
    d.read_image(&mut buf).ok()?;
    let px = if alpha { buf } else { expand(&buf, 3, |p| [p[0], p[1], p[2], 255]) };
    Some(Rgba { w, h, px })
}

fn gif(b: &[u8]) -> Option<Rgba> {
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    let mut d = opts.read_info(BufReader::new(Cursor::new(b))).ok()?;
    let (w, h) = (d.width() as u32, d.height() as u32);
    if !dims_ok(w, h) {
        return None;
    }
    let mut canvas = vec![0u8; w as usize * h as usize * 4];
    // First frame, composited onto the logical screen.
    let f = d.read_next_frame().ok()??;
    for y in 0..f.height as u32 {
        for x in 0..f.width as u32 {
            let (cx, cy) = (x + f.left as u32, y + f.top as u32);
            if cx >= w || cy >= h {
                continue;
            }
            let s = (y as usize * f.width as usize + x as usize) * 4;
            let t = (cy as usize * w as usize + cx as usize) * 4;
            let (Some(src), Some(dst)) = (f.buffer.get(s..s + 4), canvas.get_mut(t..t + 4)) else { continue };
            dst.copy_from_slice(src);
        }
    }
    Some(Rgba { w, h, px: canvas })
}

fn le16(b: &[u8], i: usize) -> Option<u32> {
    Some(u16::from_le_bytes(b.get(i..i + 2)?.try_into().ok()?) as u32)
}
fn le32(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(i..i + 4)?.try_into().ok()?))
}

/// Uncompressed / bitfield BMP (1, 4, 8, 16, 24, 32 bpp). `dib` = data starts at the
/// info header (ICO entries), where the height is doubled to include the AND mask.
fn bmp(b: &[u8], dib: bool) -> Option<Rgba> {
    let (hdr, data_off) = if dib { (0usize, None) } else { (14usize, Some(le32(b, 10)? as usize)) };
    let hsize = le32(b, hdr)? as usize;
    let w = le32(b, hdr + 4)? as i32;
    let mut h = le32(b, hdr + 8)? as i32;
    let bpp = le16(b, hdr + 14)?;
    let comp = le32(b, hdr + 16)?;
    if dib {
        h /= 2;
    }
    let top_down = h < 0;
    let (w, h) = (w.unsigned_abs(), h.unsigned_abs());
    if !dims_ok(w, h) || w > 32768 || h > 32768 || !(comp == 0 || comp == 3) || bpp > 32 {
        return None;
    }
    let colors = if bpp <= 8 {
        // The palette size comes from the file: never trust more than 2^bpp entries.
        let n = (match le32(b, hdr + 32)? { 0 => 1u32 << bpp, n => n } as usize).min(1 << bpp);
        let pal = hdr + hsize;
        (0..n).map(|i| b.get(pal + i * 4..pal + i * 4 + 4).map(|c| [c[2], c[1], c[0], 255]).unwrap_or([0, 0, 0, 255])).collect()
    } else {
        vec![]
    };
    let masks = if comp == 3 {
        let m = if hsize >= 52 { hdr + 40 } else { hdr + hsize };
        [le32(b, m)?, le32(b, m + 4)?, le32(b, m + 8)?, if hsize >= 56 { le32(b, m + 12)? } else { 0 }]
    } else if bpp == 16 {
        [0x7C00, 0x03E0, 0x001F, 0]
    } else {
        [0x00FF_0000, 0x0000_FF00, 0x0000_00FF, if bpp == 32 { 0xFF00_0000 } else { 0 }]
    };
    let off = data_off.unwrap_or(hdr + hsize + colors.len() * 4);
    let stride = ((w * bpp + 31) / 32 * 4) as usize;
    let chan = |v: u32, m: u32| -> u8 {
        if m == 0 {
            return 255;
        }
        let shift = m.trailing_zeros();
        let max = m >> shift;
        (((v & m) >> shift) * 255 / max.max(1)) as u8
    };
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        let row = off + y as usize * stride;
        let dy = if top_down { y } else { h - 1 - y };
        for x in 0..w {
            let c: [u8; 4] = match bpp {
                1 | 4 | 8 => {
                    let bit = x as usize * bpp as usize;
                    let byte = *b.get(row + bit / 8)?;
                    let idx = (byte >> (8 - bpp as usize - bit % 8)) & ((1u16 << bpp) - 1) as u8;
                    *colors.get(idx as usize).unwrap_or(&[0, 0, 0, 255])
                }
                16 => {
                    let v = le16(b, row + x as usize * 2)?;
                    [chan(v, masks[0]), chan(v, masks[1]), chan(v, masks[2]), 255]
                }
                24 => {
                    let p = b.get(row + x as usize * 3..row + x as usize * 3 + 3)?;
                    [p[2], p[1], p[0], 255]
                }
                32 => {
                    let v = le32(b, row + x as usize * 4)?;
                    let a = if masks[3] != 0 { chan(v, masks[3]) } else { 255 };
                    [chan(v, masks[0]), chan(v, masks[1]), chan(v, masks[2]), a]
                }
                _ => return None,
            };
            let t = ((dy * w + x) * 4) as usize;
            px[t..t + 4].copy_from_slice(&c);
        }
    }
    // 32-bit with an all-zero alpha channel means "no alpha".
    if bpp == 32 && px.chunks_exact(4).all(|p| p[3] == 0) {
        for p in px.chunks_exact_mut(4) {
            p[3] = 255;
        }
    }
    // ICO: apply the 1-bit AND mask when there is no real alpha.
    if dib && bpp < 32 {
        let mstride = ((w + 31) / 32 * 4) as usize;
        let moff = off + stride * h as usize;
        for y in 0..h {
            let dy = if top_down { y } else { h - 1 - y };
            for x in 0..w {
                if let Some(byte) = b.get(moff + y as usize * mstride + x as usize / 8) {
                    if byte >> (7 - x % 8) & 1 == 1 {
                        px[((dy * w + x) * 4 + 3) as usize] = 0;
                    }
                }
            }
        }
    }
    Some(Rgba { w, h, px })
}

fn ico(b: &[u8]) -> Option<Rgba> {
    let n = le16(b, 4)? as usize;
    // Pick the largest entry.
    let mut best: Option<(u32, usize, usize)> = None;
    for i in 0..n {
        let e = 6 + i * 16;
        let w = match *b.get(e)? { 0 => 256, v => v as u32 };
        let size = le32(b, e + 8)? as usize;
        let off = le32(b, e + 12)? as usize;
        if best.map(|(bw, _, _)| w > bw).unwrap_or(true) {
            best = Some((w, off, size));
        }
    }
    let (_, off, size) = best?;
    let data = b.get(off..off + size)?;
    if data.starts_with(b"\x89PNG") { png(data) } else { bmp(data, true) }
}

fn orient_image(img: Rgba, o: u8) -> Rgba {
    let (w, h) = (img.w as usize, img.h as usize);
    let swap = o >= 5;
    let (nw, nh) = if swap { (h, w) } else { (w, h) };
    let mut out = vec![0u8; img.px.len()];
    for y in 0..h {
        for x in 0..w {
            let (nx, ny) = match o {
                2 => (w - 1 - x, y),
                3 => (w - 1 - x, h - 1 - y),
                4 => (x, h - 1 - y),
                5 => (y, x),
                6 => (h - 1 - y, x),
                7 => (h - 1 - y, w - 1 - x),
                8 => (y, w - 1 - x),
                _ => (x, y),
            };
            let s = (y * w + x) * 4;
            let t = (ny * nw + nx) * 4;
            out[t..t + 4].copy_from_slice(&img.px[s..s + 4]);
        }
    }
    Rgba { w: nw as u32, h: nh as u32, px: out }
}

/// Area-averaging downscale to fit within `max`×`max` (keeps aspect ratio).
pub fn downscale(img: &Rgba, max: u32) -> Rgba {
    let k = (max as f64 / img.w as f64).min(max as f64 / img.h as f64);
    let nw = ((img.w as f64 * k).round() as u32).max(1);
    let nh = ((img.h as f64 * k).round() as u32).max(1);
    let (sw, sh) = (img.w as usize, img.h as usize);
    // Precompute source spans for each destination column.
    let xs: Vec<(usize, usize)> = (0..nw as usize)
        .map(|x| {
            let a = x * sw / nw as usize;
            let b = ((x + 1) * sw / nw as usize).max(a + 1).min(sw);
            (a, b)
        })
        .collect();
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    let mut acc = vec![0u32; nw as usize * 4];
    for y in 0..nh as usize {
        let y0 = y * sh / nh as usize;
        let y1 = ((y + 1) * sh / nh as usize).max(y0 + 1).min(sh);
        acc.iter_mut().for_each(|v| *v = 0);
        for sy in y0..y1 {
            let row = &img.px[sy * sw * 4..(sy + 1) * sw * 4];
            for (x, &(a, b)) in xs.iter().enumerate() {
                let (mut r, mut g, mut bl, mut al) = (0u32, 0u32, 0u32, 0u32);
                for p in row[a * 4..b * 4].chunks_exact(4) {
                    // Premultiply so transparent pixels don't darken edges.
                    let pa = p[3] as u32;
                    r += p[0] as u32 * pa;
                    g += p[1] as u32 * pa;
                    bl += p[2] as u32 * pa;
                    al += pa;
                }
                let o = x * 4;
                acc[o] += r / 255;
                acc[o + 1] += g / 255;
                acc[o + 2] += bl / 255;
                acc[o + 3] += al;
            }
        }
        for (x, &(a, b)) in xs.iter().enumerate() {
            let n = ((b - a) * (y1 - y0)) as u32;
            let o = x * 4;
            let al = acc[o + 3];
            let t = (y * nw as usize + x) * 4;
            if al == 0 {
                out[t..t + 4].copy_from_slice(&[0, 0, 0, 0]);
            } else {
                // Un-premultiply.
                out[t] = (acc[o] * 255 / al).min(255) as u8;
                out[t + 1] = (acc[o + 1] * 255 / al).min(255) as u8;
                out[t + 2] = (acc[o + 2] * 255 / al).min(255) as u8;
                out[t + 3] = (al / n).min(255) as u8;
            }
        }
    }
    Rgba { w: nw, h: nh, px: out }
}
