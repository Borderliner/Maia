//! Formulas in Markdown, typeset by a TeX on this computer: `pdflatex` from TeX Live,
//! else `tectonic`; poppler's `pdftocairo` turns the pages into pictures. Every
//! formula of a document is one page of one PDF (the `preview` package crops each to
//! its ink and writes its depth below the baseline to the log), compiled once on a
//! thread and kept in the cache by its source. Without a TeX the formulas show as
//! written.
//!
//! A document from anywhere can hold TeX, so TeX runs fenced in: no shell commands,
//! files read and written only in its own folder, and a time limit.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::drives::Tools;
use crate::imgdec::Rgba;

/// Pixels per inch the pages are drawn at: a 10 pt em is about 41 pixels, sharp at
/// every font size and scale the preview shows text at.
pub const DPI: u32 = 300;
/// Changes when what a formula turns into does, so older pictures aren't reused.
const MAKE: &str = "1";
const TIME: Duration = Duration::from_secs(40);

// ------------------------------------------------------------------ finding them

/// `$…$`, `$$…$$` or `\(…\)` at byte `i` of `s`: (byte after it, the TeX, whether it
/// is shown on a line of its own). A `$` that opens is followed by something other
/// than a space, one that closes follows something other than a space and isn't
/// followed by a digit, so "$5 and $10" stays money.
pub fn inline_at(s: &str, i: usize) -> Option<(usize, String, bool)> {
    let rest = &s[i..];
    if let Some(body) = rest.strip_prefix("$$") {
        let end = body.find("$$")?;
        let tex = body[..end].trim();
        return (!tex.is_empty()).then(|| (i + 2 + end + 2, tex.to_string(), true));
    }
    if let Some(body) = rest.strip_prefix("\\(") {
        let end = body.find("\\)")?;
        let tex = body[..end].trim();
        return (!tex.is_empty()).then(|| (i + 2 + end + 2, tex.to_string(), false));
    }
    let body = rest.strip_prefix('$')?;
    if body.starts_with(char::is_whitespace) || body.starts_with('$') {
        return None;
    }
    let b = body.as_bytes();
    let mut k = 0;
    while k < b.len() {
        match b[k] {
            b'\\' => k += 2,
            b'$' => {
                let before = body[..k].chars().next_back();
                let after = body[k + 1..].chars().next();
                if before.is_some_and(|c| !c.is_whitespace()) && !after.is_some_and(|c| c.is_ascii_digit()) {
                    return Some((i + 1 + k + 1, body[..k].to_string(), false));
                }
                k += 1;
            }
            b'\n' if body[k + 1..].starts_with('\n') => return None,
            _ => k += 1,
        }
    }
    None
}

/// A formula block starting at `lines[at]`: `$$` … `$$`, `\[` … `\]`, or a fenced
/// ```` ```math ```` block. Returns the TeX and the line after it.
pub fn block_at(lines: &[&str], at: usize) -> Option<(String, usize)> {
    let first = lines[at].trim();
    let (open, close) = if first.starts_with("$$") {
        ("$$", "$$")
    } else if first.starts_with("\\[") {
        ("\\[", "\\]")
    } else {
        return None;
    };
    let after = &first[open.len()..];
    // On one line: "$$ x^2 $$".
    if let Some(end) = after.find(close) {
        let tex = after[..end].trim();
        return (!tex.is_empty() && after[end + close.len()..].trim().is_empty()).then(|| (tex.to_string(), at + 1));
    }
    let mut tex = vec![after];
    for (k, l) in lines.iter().enumerate().skip(at + 1) {
        let t = l.trim_end();
        if let Some(end) = t.find(close) {
            tex.push(&t[..end]);
            return Some((tex.join("\n").trim().to_string(), k + 1));
        }
        tex.push(t);
    }
    None
}

// ------------------------------------------------------------------ typesetting

/// A formula as a picture: white where its ink is (the preview tints it), and how far
/// below the baseline it reaches, in pixels.
pub struct Typeset {
    pub img: Rgba,
    pub depth: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Pdflatex,
    Tectonic,
}

/// The TeX to use, when there is one (and poppler to draw its pages).
pub fn engine(t: &Tools) -> Option<Engine> {
    if !t.has("pdftocairo") {
        return None;
    }
    if t.has("pdflatex") {
        Some(Engine::Pdflatex)
    } else if t.has("tectonic") {
        Some(Engine::Tectonic)
    } else {
        None
    }
}

/// Why formulas show as written, for the line under the first one.
pub fn missing(t: &Tools) -> &'static str {
    if !t.has("pdftocairo") {
        "Formulas show as written: typesetting them needs poppler's pdftocairo"
    } else {
        "Formulas show as written: install TeX Live to typeset them"
    }
}

pub fn cache_dir() -> PathBuf {
    let base = if cfg!(test) { None } else { dirs::cache_dir() };
    base.unwrap_or_else(std::env::temp_dir).join("maia/math")
}

fn key(e: Engine, tex: &str, display: bool) -> String {
    crate::thumbcache::md5_hex(format!("{MAKE}\0{e:?}\0{display}\0{tex}").as_bytes())
}

/// The page for one formula: shown big (display) or as in a sentence. A display
/// environment (align, gather…) becomes the form that fits inside a formula.
fn page(tex: &str, display: bool) -> String {
    let mut t = tex.to_string();
    for (env, inner) in [("align*", "aligned"), ("align", "aligned"), ("gather*", "gathered"), ("gather", "gathered"), ("equation*", ""), ("equation", "")] {
        let (b, e) = (format!("\\begin{{{env}}}"), format!("\\end{{{env}}}"));
        if inner.is_empty() {
            t = t.replace(&b, "").replace(&e, "");
        } else {
            t = t.replace(&b, &format!("\\begin{{{inner}}}")).replace(&e, &format!("\\end{{{inner}}}"));
        }
    }
    if display {
        format!("\\begin{{preview}}$\\displaystyle {t}$\\end{{preview}}\n")
    } else {
        format!("\\begin{{preview}}${t}$\\end{{preview}}\n")
    }
}

fn document(items: &[(&str, bool)]) -> String {
    let mut s = String::from("\\documentclass{article}\n\\usepackage{amsmath,amssymb}\n\\usepackage[active,tightpage]{preview}\n\\setlength\\PreviewBorder{1pt}\n\\pagestyle{empty}\n\\begin{document}\n");
    for (tex, display) in items {
        s += &page(tex, *display);
    }
    s += "\\end{document}\n";
    s
}

/// What `preview` wrote for each formula: height and depth (in TeX's scaled points).
pub fn depths(log: &str) -> Vec<(f64, f64)> {
    // "Preview: Snippet 1 ended.(368640+1505299x1347574)."
    log.lines()
        .filter_map(|l| {
            let at = l.find("Preview: Snippet")?;
            let open = l[at..].find('(')? + at + 1;
            let plus = l[open..].find('+')? + open;
            let x = l[plus..].find('x')? + plus;
            Some((l[open..plus].trim().parse().ok()?, l[plus + 1..x].trim().parse().ok()?))
        })
        .collect()
}

/// Typeset `items` (TeX, shown big), from the cache where it has them. One result each:
/// a picture, or why that formula can't be typeset.
pub fn typeset(t: &Tools, e: Engine, items: &[(String, bool)], stop: &dyn Fn() -> bool) -> Vec<Result<Typeset, String>> {
    let dir = cache_dir();
    let _ = std::fs::create_dir_all(&dir);
    let mut out: Vec<Option<Result<Typeset, String>>> = items.iter().map(|(tex, d)| load(&dir, &key(e, tex, *d)).map(Ok)).collect();
    let todo: Vec<usize> = (0..items.len()).filter(|&i| out[i].is_none()).collect();
    if !todo.is_empty() && !stop() {
        let batch: Vec<(&str, bool)> = todo.iter().map(|&i| (items[i].0.as_str(), items[i].1)).collect();
        match compile(t, e, &batch, stop) {
            Ok(pages) => {
                for (&i, p) in todo.iter().zip(pages) {
                    out[i] = Some(keep(&dir, &key(e, &items[i].0, items[i].1), p));
                }
            }
            // One formula TeX can't read stops the lot: each is tried alone to find it.
            Err(why) if todo.len() > 1 => {
                for &i in &todo {
                    if stop() {
                        break;
                    }
                    let one = compile(t, e, &[(items[i].0.as_str(), items[i].1)], stop).map(|mut v| v.remove(0));
                    out[i] = Some(match one {
                        Ok(p) => keep(&dir, &key(e, &items[i].0, items[i].1), p),
                        Err(w) => Err(if w.is_empty() { why.clone() } else { w }),
                    });
                }
            }
            Err(why) => out[todo[0]] = Some(Err(why)),
        }
    }
    out.into_iter().map(|o| o.unwrap_or_else(|| Err(String::new()))).collect()
}

/// A page as drawn by pdftocairo (black ink on nothing) and its depth as a share of
/// its height: stored, and made into a white picture for tinting.
fn keep(dir: &Path, key: &str, (png, depth): (Vec<u8>, f64)) -> Result<Typeset, String> {
    let _ = std::fs::write(dir.join(format!("{key}.png")), &png);
    let _ = std::fs::write(dir.join(format!("{key}.depth")), depth.to_string());
    whiten(&png, depth).ok_or_else(|| "The formula's picture couldn't be read".to_string())
}

fn load(dir: &Path, key: &str) -> Option<Typeset> {
    let png = std::fs::read(dir.join(format!("{key}.png"))).ok()?;
    let depth: f64 = std::fs::read_to_string(dir.join(format!("{key}.depth"))).ok()?.trim().parse().ok()?;
    whiten(&png, depth)
}

fn whiten(png: &[u8], depth: f64) -> Option<Typeset> {
    let mut img = crate::imgdec::decode_bytes(png)?;
    for px in img.px.chunks_exact_mut(4) {
        // Ink is dark: how dark, times how solid, is how much of it there is.
        let lum = (px[0] as u32 * 54 + px[1] as u32 * 183 + px[2] as u32 * 19) >> 8;
        let a = (px[3] as u32 * (255 - lum.min(255))) / 255;
        px.copy_from_slice(&[255, 255, 255, a as u8]);
    }
    let depth = (depth * img.h as f64) as f32;
    Some(Typeset { img, depth })
}

/// One run of TeX and pdftocairo over `items`: each page's PNG and its depth share.
fn compile(t: &Tools, e: Engine, items: &[(&str, bool)], stop: &dyn Fn() -> bool) -> Result<Vec<(Vec<u8>, f64)>, String> {
    let stage = cache_dir().join(format!(".part-{}-{}", std::process::id(), crate::thumbcache::md5_hex(format!("{:?}", Instant::now()).as_bytes())));
    std::fs::create_dir_all(&stage).map_err(|e| e.to_string())?;
    let r = compile_in(t, e, items, &stage, stop);
    let _ = std::fs::remove_dir_all(&stage);
    r
}

fn compile_in(t: &Tools, e: Engine, items: &[(&str, bool)], stage: &Path, stop: &dyn Fn() -> bool) -> Result<Vec<(Vec<u8>, f64)>, String> {
    std::fs::write(stage.join("f.tex"), document(items)).map_err(|e| e.to_string())?;
    let mut c = crate::media::tool_cmd(t.cmd(match e {
        Engine::Pdflatex => "pdflatex",
        Engine::Tectonic => "tectonic",
    }));
    c.current_dir(stage).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    match e {
        Engine::Pdflatex => {
            // Files read and written only here, no shell commands.
            c.env("openin_any", "p").env("openout_any", "p").args(["-interaction=nonstopmode", "-halt-on-error", "-no-shell-escape", "f.tex"]);
        }
        Engine::Tectonic => {
            // Nothing fetched: a tectonic that hasn't got its files says so instead.
            c.args(["--untrusted", "--only-cached", "--keep-logs", "--chatter", "minimal", "f.tex"]);
        }
    }
    let err = run(c, stop)?;
    let log = std::fs::read_to_string(stage.join("f.log")).unwrap_or_default();
    if !stage.join("f.pdf").is_file() {
        return Err(why(&log, &err, e));
    }
    let sizes = depths(&log);
    let mut p = crate::media::tool_cmd(t.cmd("pdftocairo"));
    p.current_dir(stage).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    p.args(["-png", "-transp", "-r", &DPI.to_string(), "f.pdf", "p"]);
    run(p, stop)?;
    // p-1.png, or p-01.png … with as many digits as the last page needs: each in its
    // page's place.
    let mut pages: Vec<Option<PathBuf>> = vec![None; items.len()];
    for d in std::fs::read_dir(stage).map_err(|e| e.to_string())?.flatten() {
        let n = d.file_name().to_string_lossy().into_owned();
        let Some(num) = n.strip_prefix("p-").and_then(|x| x.strip_suffix(".png")).and_then(|x| x.parse::<usize>().ok()) else { continue };
        match pages.get_mut(num.wrapping_sub(1)) {
            Some(slot) => *slot = Some(d.path()),
            None => return Err("TeX made more pages than there are formulas".into()),
        }
    }
    let pages: Vec<PathBuf> = pages.into_iter().collect::<Option<_>>().ok_or("TeX made fewer pages than there are formulas")?;
    pages
        .into_iter()
        .enumerate()
        .map(|(i, path)| {
            let png = std::fs::read(&path).map_err(|e| e.to_string())?;
            // The page holds the formula and a 1 pt border all round (65536 sp).
            let (h, d) = sizes.get(i).copied().unwrap_or((1.0, 0.0));
            let (d, all) = (d + 65536.0, h + d + 131072.0);
            Ok((png, d / all))
        })
        .collect()
}

/// TeX's complaint, plainly: the first error line of the log.
fn why(log: &str, err: &str, e: Engine) -> String {
    let l = err.to_lowercase();
    if e == Engine::Tectonic && (l.contains("only-cached") || l.contains("not available in the cache") || l.contains("not found in cache")) {
        return "Formulas show as written: run tectonic once from a terminal so it fetches its files".into();
    }
    log.lines()
        .chain(err.lines())
        .find_map(|l| l.strip_prefix("! ").or_else(|| l.strip_prefix("error: ")))
        .map(|m| format!("TeX: {}", m.trim().trim_end_matches('.')))
        .unwrap_or_else(|| "TeX couldn't typeset this".into())
}

/// Run to the end, the time limit or `stop`; what it said on stderr.
fn run(mut c: std::process::Command, stop: &dyn Fn() -> bool) -> Result<String, String> {
    let mut child = c.spawn().map_err(|e| e.to_string())?;
    let err = child.stderr.take();
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = err {
            use std::io::Read;
            let _ = e.by_ref().take(64 << 10).read_to_string(&mut s);
        }
        s
    });
    let t0 = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return Ok(reader.join().unwrap_or_default()),
            Ok(None) if t0.elapsed() < TIME && !stop() => std::thread::sleep(Duration::from_millis(15)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(if stop() { String::new() } else { "TeX took too long and was stopped".into() });
            }
        }
    }
}
