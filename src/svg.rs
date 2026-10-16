//! A compact SVG renderer for icon themes: paths and basic shapes, transforms, fills and
//! strokes (joins, caps, dashes), linear/radial gradients (with href inheritance), CSS
//! classes and `currentColor`, group opacity, clip paths, `<use>`, embedded PNG images and
//! Gaussian-blur filters. Text, masks and other filter primitives are not supported (an
//! element whose filter is unsupported is drawn unfiltered).
//!
//! Rasterization uses signed-area coverage accumulation (exact analytic anti-aliasing for
//! line segments; curves are flattened first), composited in premultiplied linear-light-free
//! sRGB like browsers do.

use std::collections::HashMap;

// ------------------------------------------------------------------ XML

#[derive(Default, Clone)]
struct Node {
    tag: String,
    attrs: Vec<(String, String)>,
    kids: Vec<Node>,
    text: String,
}

impl Node {
    fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }
}

fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(j) = rest.find(';') else { break };
        let ent = &rest[1..j];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            e if e.starts_with("#x") => u32::from_str_radix(&e[2..], 16).ok().and_then(char::from_u32),
            e if e.starts_with('#') => e[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => out.push(c),
            None => out.push_str(&rest[..=j]),
        }
        rest = &rest[j + 1..];
    }
    out.push_str(rest);
    out
}

fn parse_xml(src: &str) -> Option<Node> {
    let b = src.as_bytes();
    let mut i = 0;
    let mut stack: Vec<Node> = vec![Node::default()];
    while i < b.len() {
        if b[i] == b'<' {
            if src[i..].starts_with("<!--") {
                i = src[i..].find("-->").map(|k| i + k + 3)?;
                continue;
            }
            if src[i..].starts_with("<![CDATA[") {
                let end = src[i..].find("]]>").map(|k| i + k)?;
                stack.last_mut()?.text.push_str(&src[i + 9..end]);
                i = end + 3;
                continue;
            }
            if src[i..].starts_with("<?") || src[i..].starts_with("<!") {
                // Skip declarations; a DOCTYPE may carry an internal subset in [...].
                let mut depth = 0;
                while i < b.len() {
                    match b[i] {
                        b'[' => depth += 1,
                        b']' => depth -= 1,
                        b'>' if depth <= 0 => break,
                        _ => {}
                    }
                    i += 1;
                }
                i += 1;
                continue;
            }
            if b.get(i + 1) == Some(&b'/') {
                let end = src[i..].find('>').map(|k| i + k)?;
                let n = stack.pop()?;
                if stack.is_empty() {
                    return Some(n);
                }
                stack.last_mut()?.kids.push(n);
                i = end + 1;
                continue;
            }
            // Start tag.
            let mut j = i + 1;
            while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' && b[j] != b'/' {
                j += 1;
            }
            let name = &src[i + 1..j];
            let tag = name.rsplit(':').next().unwrap_or(name).to_string();
            let mut node = Node { tag, ..Default::default() };
            let mut self_close = false;
            loop {
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j >= b.len() {
                    return None;
                }
                if b[j] == b'>' {
                    j += 1;
                    break;
                }
                if b[j] == b'/' {
                    self_close = true;
                    j += 1;
                    continue;
                }
                let ks = j;
                while j < b.len() && b[j] != b'=' && !b[j].is_ascii_whitespace() && b[j] != b'>' && b[j] != b'/' {
                    j += 1;
                }
                let key = &src[ks..j];
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                if b.get(j) != Some(&b'=') {
                    node.attrs.push((key.to_string(), String::new()));
                    continue;
                }
                j += 1;
                while j < b.len() && b[j].is_ascii_whitespace() {
                    j += 1;
                }
                let q = *b.get(j)?;
                let (vs, ve) = if q == b'"' || q == b'\'' {
                    let e = src[j + 1..].find(q as char).map(|k| j + 1 + k)?;
                    (j + 1, e)
                } else {
                    let mut e = j;
                    while e < b.len() && !b[e].is_ascii_whitespace() && b[e] != b'>' {
                        e += 1;
                    }
                    (j, e)
                };
                // xlink:href and href are the same thing for us.
                let k = if key == "xlink:href" { "href" } else { key };
                node.attrs.push((k.to_string(), decode_entities(&src[vs..ve])));
                j = if q == b'"' || q == b'\'' { ve + 1 } else { ve };
            }
            if self_close {
                stack.last_mut()?.kids.push(node);
            } else {
                stack.push(node);
            }
            i = j;
        } else {
            let end = src[i..].find('<').map(|k| i + k).unwrap_or(b.len());
            if let Some(top) = stack.last_mut() {
                if top.tag == "style" {
                    top.text.push_str(&decode_entities(&src[i..end]));
                }
            }
            i = end;
        }
    }
    let root = stack.into_iter().next()?;
    root.kids.into_iter().find(|n| n.tag == "svg")
}

// ------------------------------------------------------------------ geometry

#[derive(Clone, Copy, Debug)]
struct Mat([f32; 6]); // a b c d e f : x' = a x + c y + e ; y' = b x + d y + f

impl Mat {
    const ID: Mat = Mat([1.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
    fn mul(self, o: Mat) -> Mat {
        let [a, b, c, d, e, f] = self.0;
        let [a2, b2, c2, d2, e2, f2] = o.0;
        Mat([a * a2 + c * b2, b * a2 + d * b2, a * c2 + c * d2, b * c2 + d * d2, a * e2 + c * f2 + e, b * e2 + d * f2 + f])
    }
    fn apply(self, x: f32, y: f32) -> (f32, f32) {
        let [a, b, c, d, e, f] = self.0;
        (a * x + c * y + e, b * x + d * y + f)
    }
    fn invert(self) -> Option<Mat> {
        let [a, b, c, d, e, f] = self.0;
        let det = a * d - b * c;
        if det.abs() < 1e-12 {
            return None;
        }
        let id = 1.0 / det;
        Some(Mat([d * id, -b * id, -c * id, a * id, (c * f - d * e) * id, (b * e - a * f) * id]))
    }
    fn scale_factor(self) -> f32 {
        let [a, b, c, d, ..] = self.0;
        (a * d - b * c).abs().sqrt()
    }
    fn translate(x: f32, y: f32) -> Mat {
        Mat([1.0, 0.0, 0.0, 1.0, x, y])
    }
    fn scale(x: f32, y: f32) -> Mat {
        Mat([x, 0.0, 0.0, y, 0.0, 0.0])
    }
}

fn numbers(s: &str) -> Vec<f32> {
    let mut v = vec![];
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c == b'-' || c == b'+' || c == b'.' || c.is_ascii_digit() {
            let st = i;
            i += 1;
            let mut seen_dot = c == b'.';
            let mut seen_e = false;
            while i < b.len() {
                let d = b[i];
                if d.is_ascii_digit() {
                    i += 1;
                } else if d == b'.' && !seen_dot && !seen_e {
                    seen_dot = true;
                    i += 1;
                } else if (d == b'e' || d == b'E') && !seen_e && b.get(i + 1).map(|n| n.is_ascii_digit() || *n == b'-' || *n == b'+').unwrap_or(false) {
                    seen_e = true;
                    i += 2;
                } else {
                    break;
                }
            }
            if let Ok(n) = s[st..i].parse() {
                v.push(n);
            }
        } else {
            i += 1;
        }
    }
    v
}

fn parse_transform(s: &str) -> Mat {
    let mut m = Mat::ID;
    let mut rest = s;
    while let Some(open) = rest.find('(') {
        let name = rest[..open].trim().trim_start_matches(',').trim();
        let Some(close) = rest[open..].find(')') else { break };
        let n = numbers(&rest[open + 1..open + close]);
        let g = |i: usize| n.get(i).copied().unwrap_or(0.0);
        let t = match name {
            "matrix" if n.len() >= 6 => Mat([g(0), g(1), g(2), g(3), g(4), g(5)]),
            "translate" => Mat::translate(g(0), g(1)),
            "scale" => Mat::scale(g(0), if n.len() > 1 { g(1) } else { g(0) }),
            "rotate" => {
                let (s, c) = g(0).to_radians().sin_cos();
                let r = Mat([c, s, -s, c, 0.0, 0.0]);
                if n.len() >= 3 { Mat::translate(g(1), g(2)).mul(r).mul(Mat::translate(-g(1), -g(2))) } else { r }
            }
            "skewX" => Mat([1.0, 0.0, g(0).to_radians().tan(), 1.0, 0.0, 0.0]),
            "skewY" => Mat([1.0, g(0).to_radians().tan(), 0.0, 1.0, 0.0, 0.0]),
            _ => Mat::ID,
        };
        m = m.mul(t);
        rest = &rest[open + close + 1..];
    }
    m
}

#[derive(Clone, Copy)]
enum Seg {
    Move(f32, f32),
    Line(f32, f32),
    Cubic(f32, f32, f32, f32, f32, f32),
    Close,
}

fn arc_to_cubics(out: &mut Vec<Seg>, x1: f32, y1: f32, mut rx: f32, mut ry: f32, phi: f32, large: bool, sweep: bool, x2: f32, y2: f32) {
    if rx.abs() < 1e-6 || ry.abs() < 1e-6 {
        out.push(Seg::Line(x2, y2));
        return;
    }
    rx = rx.abs();
    ry = ry.abs();
    let (sp, cp) = phi.to_radians().sin_cos();
    let dx = (x1 - x2) / 2.0;
    let dy = (y1 - y2) / 2.0;
    let x1p = cp * dx + sp * dy;
    let y1p = -sp * dx + cp * dy;
    let lam = (x1p * x1p) / (rx * rx) + (y1p * y1p) / (ry * ry);
    if lam > 1.0 {
        let s = lam.sqrt();
        rx *= s;
        ry *= s;
    }
    let num = rx * rx * ry * ry - rx * rx * y1p * y1p - ry * ry * x1p * x1p;
    let den = rx * rx * y1p * y1p + ry * ry * x1p * x1p;
    let mut coef = if den > 0.0 { (num / den).max(0.0).sqrt() } else { 0.0 };
    if large == sweep {
        coef = -coef;
    }
    let cxp = coef * rx * y1p / ry;
    let cyp = -coef * ry * x1p / rx;
    let cx = cp * cxp - sp * cyp + (x1 + x2) / 2.0;
    let cy = sp * cxp + cp * cyp + (y1 + y2) / 2.0;
    let ang = |ux: f32, uy: f32, vx: f32, vy: f32| {
        let a = (ux * vx + uy * vy) / ((ux * ux + uy * uy).sqrt() * (vx * vx + vy * vy).sqrt());
        let a = a.clamp(-1.0, 1.0).acos();
        if ux * vy - uy * vx < 0.0 { -a } else { a }
    };
    let th1 = ang(1.0, 0.0, (x1p - cxp) / rx, (y1p - cyp) / ry);
    let mut dth = ang((x1p - cxp) / rx, (y1p - cyp) / ry, (-x1p - cxp) / rx, (-y1p - cyp) / ry);
    if !sweep && dth > 0.0 {
        dth -= std::f32::consts::TAU;
    } else if sweep && dth < 0.0 {
        dth += std::f32::consts::TAU;
    }
    let n = (dth.abs() / std::f32::consts::FRAC_PI_2).ceil().max(1.0) as usize;
    let d = dth / n as f32;
    let k = 4.0 / 3.0 * (d / 4.0).tan();
    let pt = |t: f32| {
        let (s, c) = t.sin_cos();
        (cx + rx * c * cp - ry * s * sp, cy + rx * c * sp + ry * s * cp)
    };
    let der = |t: f32| {
        let (s, c) = t.sin_cos();
        (-rx * s * cp - ry * c * sp, -rx * s * sp + ry * c * cp)
    };
    let mut t = th1;
    for _ in 0..n {
        let (p0, d0) = (pt(t), der(t));
        let (p3, d3) = (pt(t + d), der(t + d));
        out.push(Seg::Cubic(p0.0 + k * d0.0, p0.1 + k * d0.1, p3.0 - k * d3.0, p3.1 - k * d3.1, p3.0, p3.1));
        t += d;
    }
}

fn parse_path(d: &str) -> Vec<Seg> {
    let mut out = vec![];
    let b = d.as_bytes();
    let mut i = 0;
    let (mut cx, mut cy, mut sx, mut sy) = (0f32, 0f32, 0f32, 0f32);
    let (mut lcx, mut lcy) = (0f32, 0f32); // last control point
    let mut last = b'M';
    let mut prev_cmd = b' ';
    // Number reader that understands "0.5.5", "-1-2" and arc flags "011".
    let read = |i: &mut usize, flag: bool| -> Option<f32> {
        while *i < b.len() && (b[*i].is_ascii_whitespace() || b[*i] == b',') {
            *i += 1;
        }
        if *i >= b.len() {
            return None;
        }
        if flag && (b[*i] == b'0' || b[*i] == b'1') {
            *i += 1;
            return Some((b[*i - 1] - b'0') as f32);
        }
        let st = *i;
        if b[*i] == b'-' || b[*i] == b'+' {
            *i += 1;
        }
        let mut dot = false;
        let mut digits = false;
        while *i < b.len() {
            let c = b[*i];
            if c.is_ascii_digit() {
                digits = true;
                *i += 1;
            } else if c == b'.' && !dot {
                dot = true;
                *i += 1;
            } else if (c == b'e' || c == b'E') && digits {
                *i += 1;
                if *i < b.len() && (b[*i] == b'-' || b[*i] == b'+') {
                    *i += 1;
                }
            } else {
                break;
            }
        }
        if !digits {
            *i = st;
            return None;
        }
        d[st..*i].parse().ok()
    };
    loop {
        while i < b.len() && (b[i].is_ascii_whitespace() || b[i] == b',') {
            i += 1;
        }
        if i >= b.len() {
            break;
        }
        let c = b[i];
        let cmd = if c.is_ascii_alphabetic() {
            i += 1;
            c
        } else {
            // Implicit repeat; after M it is L.
            match last {
                b'M' => b'L',
                b'm' => b'l',
                x => x,
            }
        };
        let rel = cmd.is_ascii_lowercase();
        let (ox, oy) = if rel { (cx, cy) } else { (0.0, 0.0) };
        macro_rules! n {
            () => {
                match read(&mut i, false) {
                    Some(v) => v,
                    None => break,
                }
            };
        }
        match cmd.to_ascii_uppercase() {
            b'M' => {
                let (x, y) = (n!() + ox, n!() + oy);
                out.push(Seg::Move(x, y));
                cx = x;
                cy = y;
                sx = x;
                sy = y;
            }
            b'L' => {
                let (x, y) = (n!() + ox, n!() + oy);
                out.push(Seg::Line(x, y));
                cx = x;
                cy = y;
            }
            b'H' => {
                let x = n!() + ox;
                out.push(Seg::Line(x, cy));
                cx = x;
            }
            b'V' => {
                let y = n!() + oy;
                out.push(Seg::Line(cx, y));
                cy = y;
            }
            b'C' => {
                let (x1, y1, x2, y2, x, y) = (n!() + ox, n!() + oy, n!() + ox, n!() + oy, n!() + ox, n!() + oy);
                out.push(Seg::Cubic(x1, y1, x2, y2, x, y));
                lcx = x2;
                lcy = y2;
                cx = x;
                cy = y;
            }
            b'S' => {
                let (x1, y1) = if matches!(prev_cmd.to_ascii_uppercase(), b'C' | b'S') { (2.0 * cx - lcx, 2.0 * cy - lcy) } else { (cx, cy) };
                let (x2, y2, x, y) = (n!() + ox, n!() + oy, n!() + ox, n!() + oy);
                out.push(Seg::Cubic(x1, y1, x2, y2, x, y));
                lcx = x2;
                lcy = y2;
                cx = x;
                cy = y;
            }
            b'Q' => {
                let (qx, qy, x, y) = (n!() + ox, n!() + oy, n!() + ox, n!() + oy);
                out.push(Seg::Cubic(cx + 2.0 / 3.0 * (qx - cx), cy + 2.0 / 3.0 * (qy - cy), x + 2.0 / 3.0 * (qx - x), y + 2.0 / 3.0 * (qy - y), x, y));
                lcx = qx;
                lcy = qy;
                cx = x;
                cy = y;
            }
            b'T' => {
                let (qx, qy) = if matches!(prev_cmd.to_ascii_uppercase(), b'Q' | b'T') { (2.0 * cx - lcx, 2.0 * cy - lcy) } else { (cx, cy) };
                let (x, y) = (n!() + ox, n!() + oy);
                out.push(Seg::Cubic(cx + 2.0 / 3.0 * (qx - cx), cy + 2.0 / 3.0 * (qy - cy), x + 2.0 / 3.0 * (qx - x), y + 2.0 / 3.0 * (qy - y), x, y));
                lcx = qx;
                lcy = qy;
                cx = x;
                cy = y;
            }
            b'A' => {
                let (rx, ry, rot) = (n!(), n!(), n!());
                let (Some(la), Some(sw)) = (read(&mut i, true), read(&mut i, true)) else { break };
                let (x, y) = (n!() + ox, n!() + oy);
                arc_to_cubics(&mut out, cx, cy, rx, ry, rot, la != 0.0, sw != 0.0, x, y);
                cx = x;
                cy = y;
            }
            b'Z' => {
                out.push(Seg::Close);
                cx = sx;
                cy = sy;
            }
            _ => break,
        }
        if !matches!(cmd.to_ascii_uppercase(), b'C' | b'S' | b'Q' | b'T') {
            lcx = cx;
            lcy = cy;
        }
        prev_cmd = cmd;
        last = cmd;
    }
    out
}

/// Flatten segments (already in device space) into closed/open polylines.
fn flatten(segs: &[Seg], m: Mat, tol: f32) -> Vec<(Vec<(f32, f32)>, bool)> {
    let mut polys: Vec<(Vec<(f32, f32)>, bool)> = vec![];
    let mut cur: Vec<(f32, f32)> = vec![];
    let mut start = (0.0, 0.0);
    let mut pos = (0.0f32, 0.0f32);
    for s in segs {
        match *s {
            Seg::Move(x, y) => {
                if cur.len() > 1 {
                    polys.push((std::mem::take(&mut cur), false));
                }
                cur.clear();
                pos = m.apply(x, y);
                start = pos;
                cur.push(pos);
            }
            Seg::Line(x, y) => {
                if cur.is_empty() {
                    cur.push(pos);
                }
                pos = m.apply(x, y);
                cur.push(pos);
            }
            Seg::Cubic(x1, y1, x2, y2, x, y) => {
                if cur.is_empty() {
                    cur.push(pos);
                }
                let p0 = pos;
                let p1 = m.apply(x1, y1);
                let p2 = m.apply(x2, y2);
                let p3 = m.apply(x, y);
                let dd = ((p0.0 - 2.0 * p1.0 + p2.0).hypot(p0.1 - 2.0 * p1.1 + p2.1)).max((p1.0 - 2.0 * p2.0 + p3.0).hypot(p1.1 - 2.0 * p2.1 + p3.1));
                let n = ((dd * 0.75 / tol).sqrt().ceil() as usize).clamp(1, 100);
                for k in 1..=n {
                    let t = k as f32 / n as f32;
                    let u = 1.0 - t;
                    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
                    cur.push((a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0, a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1));
                }
                pos = p3;
            }
            Seg::Close => {
                if cur.len() > 1 {
                    polys.push((std::mem::take(&mut cur), true));
                }
                pos = start;
                cur.clear();
            }
        }
    }
    if cur.len() > 1 {
        polys.push((cur, false));
    }
    polys
}

// ------------------------------------------------------------------ rasterizer

#[derive(Clone, Copy, PartialEq)]
enum Rule {
    NonZero,
    EvenOdd,
}

struct Raster {
    w: usize,
    h: usize,
    acc: Vec<f32>, // (w + 2) per row
    y0: usize,
    y1: usize,
}

impl Raster {
    fn new(w: usize, h: usize) -> Raster {
        Raster { w, h, acc: vec![0.0; (w + 2) * h + 2], y0: h, y1: 0 }
    }

    fn clear(&mut self) {
        let s = self.w + 2;
        if self.y0 < self.y1 {
            let end = (self.y1 * s).min(self.acc.len());
            self.acc[self.y0 * s..end].fill(0.0);
        }
        self.y0 = self.h;
        self.y1 = 0;
    }

    fn line(&mut self, (x0, y0): (f32, f32), (x1, y1): (f32, f32)) {
        if (y0 - y1).abs() < 1e-7 {
            return;
        }
        let (dir, p0, p1) = if y0 < y1 { (1.0f32, (x0, y0), (x1, y1)) } else { (-1.0f32, (x1, y1), (x0, y0)) };
        let wf = self.w as f32;
        let stride = self.w + 2;
        let dxdy = (p1.0 - p0.0) / (p1.1 - p0.1);
        let mut x = p0.0;
        let ystart = p0.1.max(0.0);
        if p0.1 < 0.0 {
            x -= p0.1 * dxdy;
        }
        let yend = p1.1.min(self.h as f32);
        if ystart >= yend {
            return;
        }
        let (ys, ye) = (ystart as usize, (yend.ceil() as usize).min(self.h));
        self.y0 = self.y0.min(ys);
        self.y1 = self.y1.max(ye);
        for y in ys..ye {
            let row = y * stride;
            let dy = ((y + 1) as f32).min(p1.1) - (y as f32).max(p0.1);
            let xnext = x + dxdy * dy;
            let d = dy * dir;
            let (xa, xb) = if x < xnext { (x, xnext) } else { (xnext, x) };
            // Clamp into [0, w]; coverage left of the canvas still counts for the row.
            let (xa, xb) = (xa.clamp(0.0, wf), xb.clamp(0.0, wf));
            let x0f = xa.floor();
            let x0i = x0f as usize;
            let x1c = xb.ceil();
            let x1i = x1c as usize;
            if x1i <= x0i + 1 {
                let xm = 0.5 * (xa + xb) - x0f;
                self.acc[row + x0i] += d - d * xm;
                self.acc[row + x0i + 1] += d * xm;
            } else {
                let s = 1.0 / (xb - xa);
                let xf0 = xa - x0f;
                let a0 = 0.5 * s * (1.0 - xf0) * (1.0 - xf0);
                let xf1 = xb - x1c + 1.0;
                let am = 0.5 * s * xf1 * xf1;
                self.acc[row + x0i] += d * a0;
                if x1i == x0i + 2 {
                    self.acc[row + x0i + 1] += d * (1.0 - a0 - am);
                } else {
                    let a1 = s * (1.5 - xf0);
                    self.acc[row + x0i + 1] += d * (a1 - a0);
                    for xi in x0i + 2..x1i - 1 {
                        self.acc[row + xi] += d * s;
                    }
                    let a2 = a1 + (x1i - x0i - 3) as f32 * s;
                    self.acc[row + x1i - 1] += d * (1.0 - a2 - am);
                }
                self.acc[row + x1i] += d * am;
            }
            x = xnext;
        }
    }

    fn polys(&mut self, polys: &[Vec<(f32, f32)>]) {
        for p in polys {
            for k in 0..p.len() {
                let a = p[k];
                let b = p[(k + 1) % p.len()];
                self.line(a, b);
            }
        }
    }

    /// Visit covered pixels: f(x, y, coverage).
    fn for_each(&self, rule: Rule, mut f: impl FnMut(usize, usize, f32)) {
        let stride = self.w + 2;
        for y in self.y0..self.y1 {
            let mut acc = 0.0f32;
            let row = y * stride;
            for x in 0..self.w {
                acc += self.acc[row + x];
                let a = match rule {
                    Rule::NonZero => acc.abs().min(1.0),
                    Rule::EvenOdd => {
                        let t = acc.abs() % 2.0;
                        if t > 1.0 { 2.0 - t } else { t }
                    }
                };
                if a > 0.002 {
                    f(x, y, a);
                }
            }
        }
    }
}

// ------------------------------------------------------------------ paint

type Rgba = [f32; 4]; // straight alpha, 0..1

#[derive(Clone)]
struct Grad {
    linear: bool,
    coords: [f32; 5], // x1 y1 x2 y2 _ | cx cy r fx fy
    bbox_units: bool,
    transform: Mat,
    spread: u8, // 0 pad 1 reflect 2 repeat
    stops: Vec<(f32, Rgba)>,
}

enum Paint {
    None,
    Solid(Rgba),
    Grad(Grad),
}

fn named(n: &str) -> Option<[u8; 3]> {
    Some(match n {
        "black" => [0, 0, 0],
        "white" => [255, 255, 255],
        "red" => [255, 0, 0],
        "green" => [0, 128, 0],
        "lime" => [0, 255, 0],
        "blue" => [0, 0, 255],
        "yellow" => [255, 255, 0],
        "cyan" | "aqua" => [0, 255, 255],
        "magenta" | "fuchsia" => [255, 0, 255],
        "gray" | "grey" => [128, 128, 128],
        "silver" => [192, 192, 192],
        "darkgray" | "darkgrey" => [169, 169, 169],
        "lightgray" | "lightgrey" => [211, 211, 211],
        "orange" => [255, 165, 0],
        "purple" => [128, 0, 128],
        "navy" => [0, 0, 128],
        "teal" => [0, 128, 128],
        "maroon" => [128, 0, 0],
        "olive" => [128, 128, 0],
        "brown" => [165, 42, 42],
        "pink" => [255, 192, 203],
        "gold" => [255, 215, 0],
        "whitesmoke" => [245, 245, 245],
        _ => return None,
    })
}

fn parse_color(s: &str, current: Rgba) -> Option<Rgba> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("currentcolor") {
        return Some(current);
    }
    if s == "transparent" {
        return Some([0.0; 4]);
    }
    if let Some(h) = s.strip_prefix('#') {
        let v: Vec<u8> = h.chars().filter_map(|c| c.to_digit(16).map(|d| d as u8)).collect();
        let c = match v.len() {
            3 => [v[0] * 17, v[1] * 17, v[2] * 17, 255],
            4 => [v[0] * 17, v[1] * 17, v[2] * 17, v[3] * 17],
            6 => [v[0] * 16 + v[1], v[2] * 16 + v[3], v[4] * 16 + v[5], 255],
            8 => [v[0] * 16 + v[1], v[2] * 16 + v[3], v[4] * 16 + v[5], v[6] * 16 + v[7]],
            _ => return None,
        };
        return Some(c.map(|x| x as f32 / 255.0));
    }
    if s.starts_with("rgb") {
        let pct = s.contains('%');
        let n = numbers(s);
        let ch = |i: usize| (n.get(i).copied().unwrap_or(0.0) / if pct { 100.0 } else { 255.0 }).clamp(0.0, 1.0);
        let a = if n.len() > 3 { n[3].clamp(0.0, 1.0) } else { 1.0 };
        return Some([ch(0), ch(1), ch(2), a]);
    }
    named(&s.to_ascii_lowercase()).map(|c| [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0, 1.0])
}

// ------------------------------------------------------------------ styles

#[derive(Clone)]
struct Style {
    fill: String,
    fill_opacity: f32,
    fill_rule: Rule,
    stroke: String,
    stroke_width: f32,
    stroke_opacity: f32,
    cap: u8,  // 0 butt 1 round 2 square
    join: u8, // 0 miter 1 round 2 bevel
    miter: f32,
    dash: Vec<f32>,
    dash_offset: f32,
    color: Rgba,
    visible: bool,
    clip_rule: Rule,
}

impl Default for Style {
    fn default() -> Self {
        Style { fill: "#000".into(), fill_opacity: 1.0, fill_rule: Rule::NonZero, stroke: "none".into(), stroke_width: 1.0, stroke_opacity: 1.0, cap: 0, join: 0, miter: 4.0, dash: vec![], dash_offset: 0.0, color: [0.0, 0.0, 0.0, 1.0], visible: true, clip_rule: Rule::NonZero }
    }
}

struct Css {
    rules: Vec<(String, Vec<(String, String)>)>, // selector -> declarations
}

fn parse_decls(s: &str) -> Vec<(String, String)> {
    s.split(';').filter_map(|d| d.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().trim_end_matches("!important").trim().to_string())).collect()
}

fn parse_css(src: &str) -> Css {
    let mut rules = vec![];
    let mut rest = src;
    // Strip comments.
    let cleaned: String;
    if rest.contains("/*") {
        let mut out = String::new();
        let mut r = rest;
        while let Some(i) = r.find("/*") {
            out.push_str(&r[..i]);
            r = r[i..].find("*/").map(|j| &r[i + j + 2..]).unwrap_or("");
        }
        out.push_str(r);
        cleaned = out;
        rest = &cleaned;
    }
    while let Some(o) = rest.find('{') {
        let Some(c) = rest[o..].find('}') else { break };
        let decls = parse_decls(&rest[o + 1..o + c]);
        for sel in rest[..o].split(',') {
            let sel = sel.trim();
            if !sel.is_empty() && !sel.starts_with('@') {
                rules.push((sel.to_string(), decls.clone()));
            }
        }
        rest = &rest[o + c + 1..];
    }
    Css { rules }
}

impl Css {
    fn matches(sel: &str, n: &Node) -> bool {
        // Simple compound selectors only: tag, .class, #id, tag.class, *.
        let sel = sel.rsplit(|c: char| c.is_whitespace() || c == '>').next().unwrap_or(sel);
        if sel == "*" {
            return true;
        }
        let mut tag = sel;
        let mut classes = vec![];
        let mut id = None;
        if let Some(i) = sel.find(['.', '#']) {
            tag = &sel[..i];
            let mut rest = &sel[i..];
            while !rest.is_empty() {
                let kind = rest.as_bytes()[0];
                let end = rest[1..].find(['.', '#']).map(|k| k + 1).unwrap_or(rest.len());
                let v = &rest[1..end];
                if kind == b'.' { classes.push(v) } else { id = Some(v) }
                rest = &rest[end..];
            }
        }
        if !tag.is_empty() && tag != "*" && tag != n.tag {
            return false;
        }
        if let Some(id) = id {
            if n.attr("id") != Some(id) {
                return false;
            }
        }
        let have: Vec<&str> = n.attr("class").map(|c| c.split_whitespace().collect()).unwrap_or_default();
        classes.iter().all(|c| have.contains(c))
    }
}

fn length(s: &str) -> f32 {
    numbers(s).first().copied().unwrap_or(0.0)
}

fn apply_prop(st: &mut Style, k: &str, v: &str) {
    let v = v.trim();
    if v == "inherit" {
        return;
    }
    match k {
        "fill" => st.fill = v.to_string(),
        "fill-opacity" => st.fill_opacity = opacity(v),
        "fill-rule" => st.fill_rule = if v == "evenodd" { Rule::EvenOdd } else { Rule::NonZero },
        "clip-rule" => st.clip_rule = if v == "evenodd" { Rule::EvenOdd } else { Rule::NonZero },
        "stroke" => st.stroke = v.to_string(),
        "stroke-width" => st.stroke_width = length(v),
        "stroke-opacity" => st.stroke_opacity = opacity(v),
        "stroke-linecap" => st.cap = match v { "round" => 1, "square" => 2, _ => 0 },
        "stroke-linejoin" => st.join = match v { "round" => 1, "bevel" => 2, _ => 0 },
        "stroke-miterlimit" => st.miter = length(v).max(1.0),
        "stroke-dasharray" => st.dash = if v == "none" { vec![] } else { numbers(v) },
        "stroke-dashoffset" => st.dash_offset = length(v),
        "color" => {
            if let Some(c) = parse_color(v, st.color) {
                st.color = c;
            }
        }
        "visibility" => st.visible = v == "visible",
        _ => {}
    }
}

fn opacity(v: &str) -> f32 {
    let n = length(v);
    (if v.trim_end().ends_with('%') { n / 100.0 } else { n }).clamp(0.0, 1.0)
}

// ------------------------------------------------------------------ renderer

struct Layer {
    px: Vec<Rgba>, // premultiplied
}

struct Ctx<'a> {
    w: usize,
    h: usize,
    ids: HashMap<String, &'a Node>,
    css: Css,
    layers: Vec<Layer>,
    raster: Raster,
    depth: u32,
}

impl<'a> Ctx<'a> {
    fn top(&mut self) -> &mut Vec<Rgba> {
        &mut self.layers.last_mut().expect("layer").px
    }

    fn style_of(&self, n: &Node, parent: &Style) -> (Style, f32, Option<String>, Option<String>, bool) {
        let mut st = parent.clone();
        st.visible = parent.visible;
        let mut op = 1.0;
        let mut clip = None;
        let mut filter = None;
        let mut display = true;
        let mut props: Vec<(String, String)> = n.attrs.iter().filter(|(k, _)| k != "style").cloned().collect();
        for (sel, decls) in &self.css.rules {
            if Css::matches(sel, n) {
                props.extend(decls.iter().cloned());
            }
        }
        if let Some(s) = n.attr("style") {
            props.extend(parse_decls(s));
        }
        // `color` first so currentColor in the same element resolves.
        for (k, v) in props.iter().filter(|(k, _)| k == "color") {
            apply_prop(&mut st, k, v);
        }
        for (k, v) in &props {
            match k.as_str() {
                "opacity" => op = opacity(v),
                "clip-path" => clip = url_id(v),
                "filter" => filter = url_id(v),
                "display" => display = v.trim() != "none",
                "color" => {}
                _ => apply_prop(&mut st, k, v),
            }
        }
        (st, op, clip, filter, display)
    }

    fn render(&mut self, n: &'a Node, m: Mat, parent: &Style) {
        if self.depth > 48 {
            return;
        }
        if matches!(n.tag.as_str(), "defs" | "clipPath" | "mask" | "linearGradient" | "radialGradient" | "pattern" | "symbol" | "style" | "title" | "desc" | "metadata" | "filter" | "marker" | "text") {
            return;
        }
        let (st, op, clip, filter, display) = self.style_of(n, parent);
        if !display {
            return;
        }
        let m = match n.attr("transform") {
            Some(t) => m.mul(parse_transform(t)),
            None => m,
        };
        // Blur filters are supported; anything else renders unfiltered.
        let blur = filter.as_ref().and_then(|id| self.ids.get(id.as_str()).copied()).and_then(|f| {
            let prims: Vec<&Node> = f.kids.iter().filter(|k| !k.tag.is_empty()).collect();
            if prims.len() == 1 && prims[0].tag == "feGaussianBlur" {
                Some(length(prims[0].attr("stdDeviation").unwrap_or("0")) * m.scale_factor())
            } else {
                None
            }
        });
        let clip_mask = clip.and_then(|id| self.clip_mask(&id, m));
        let isolate = op < 0.999 || clip_mask.is_some() || blur.map(|b| b > 0.05).unwrap_or(false);
        if isolate {
            self.layers.push(Layer { px: vec![[0.0; 4]; self.w * self.h] });
        }
        self.depth += 1;
        match n.tag.as_str() {
            "g" | "svg" | "a" | "switch" => {
                for k in &n.kids {
                    self.render(k, m, &st);
                }
            }
            "use" => {
                if let Some(target) = n.attr("href").and_then(|h| h.strip_prefix('#')).and_then(|id| self.ids.get(id).copied()) {
                    let t = Mat::translate(length(n.attr("x").unwrap_or("0")), length(n.attr("y").unwrap_or("0")));
                    if target.tag == "symbol" {
                        for k in &target.kids {
                            self.render(k, m.mul(t), &st);
                        }
                    } else {
                        self.render(target, m.mul(t), &st);
                    }
                }
            }
            "image" => self.image(n, m),
            _ => {
                if let Some(segs) = shape_segs(n) {
                    if st.visible {
                        self.shape(&segs, m, &st, op_single(isolate, op));
                    }
                }
            }
        }
        self.depth -= 1;
        if isolate {
            let mut layer = self.layers.pop().expect("layer").px;
            if let Some(b) = blur.filter(|b| *b > 0.05) {
                blur_layer(&mut layer, self.w, self.h, b);
            }
            let dst = self.top();
            for (i, s) in layer.iter().enumerate() {
                let mut k = op;
                if let Some(cm) = &clip_mask {
                    k *= cm[i];
                }
                if s[3] <= 0.0 || k <= 0.0 {
                    continue;
                }
                let sa = s[3] * k;
                let d = &mut dst[i];
                for c in 0..3 {
                    d[c] = s[c] * k + d[c] * (1.0 - sa);
                }
                d[3] = sa + d[3] * (1.0 - sa);
            }
        }
    }

    fn clip_mask(&mut self, id: &str, m: Mat) -> Option<Vec<f32>> {
        let cp = *self.ids.get(id)?;
        if cp.tag != "clipPath" {
            return None;
        }
        let cm = match cp.attr("transform") {
            Some(t) => m.mul(parse_transform(t)),
            None => m,
        };
        let mut mask = vec![0.0f32; self.w * self.h];
        let base = Style::default();
        for k in &cp.kids {
            let (st, ..) = self.style_of(k, &base);
            let km = match k.attr("transform") {
                Some(t) => cm.mul(parse_transform(t)),
                None => cm,
            };
            let segs = if k.tag == "use" {
                k.attr("href").and_then(|h| h.strip_prefix('#')).and_then(|i| self.ids.get(i).copied()).and_then(shape_segs)
            } else {
                shape_segs(k)
            };
            let Some(segs) = segs else { continue };
            let polys: Vec<Vec<(f32, f32)>> = flatten(&segs, km, 0.2).into_iter().map(|(p, _)| p).collect();
            self.raster.clear();
            self.raster.polys(&polys);
            let w = self.w;
            self.raster.for_each(st.clip_rule, |x, y, a| {
                let v = &mut mask[y * w + x];
                *v = (*v + a - *v * a).min(1.0);
            });
        }
        self.raster.clear();
        Some(mask)
    }

    fn shape(&mut self, segs: &[Seg], m: Mat, st: &Style, extra_op: f32) {
        let polys = flatten(segs, m, 0.2);
        if polys.is_empty() {
            return;
        }
        let bbox = segs_bbox(segs);
        // Fill
        let fill = self.paint(&st.fill, st, bbox, m);
        if !matches!(fill, Paint::None) {
            let closed: Vec<Vec<(f32, f32)>> = polys.iter().map(|(p, _)| p.clone()).collect();
            self.raster.clear();
            self.raster.polys(&closed);
            self.composite(&fill, st.fill_rule, st.fill_opacity * extra_op);
        }
        // Stroke
        let stroke = self.paint(&st.stroke, st, bbox, m);
        if !matches!(stroke, Paint::None) && st.stroke_width > 0.0 {
            let sw = st.stroke_width * m.scale_factor();
            let mut lines = polys.clone();
            if !st.dash.is_empty() && st.dash.iter().any(|d| *d > 0.0) {
                let pat: Vec<f32> = if st.dash.len() % 2 == 1 { st.dash.iter().chain(st.dash.iter()).map(|d| d * m.scale_factor()).collect() } else { st.dash.iter().map(|d| d * m.scale_factor()).collect() };
                lines = dash(&lines, &pat, st.dash_offset * m.scale_factor());
            }
            let outline = stroke_polys(&lines, sw.max(0.05), st.cap, st.join, st.miter);
            self.raster.clear();
            self.raster.polys(&outline);
            // Thin strokes keep their weight visually by fading instead of vanishing.
            let k = if sw < 1.0 { 1.0 } else { 1.0 };
            self.composite(&stroke, Rule::NonZero, st.stroke_opacity * extra_op * k);
        }
        self.raster.clear();
    }

    fn paint(&self, spec: &str, st: &Style, bbox: [f32; 4], m: Mat) -> Paint {
        let spec = spec.trim();
        if spec.is_empty() || spec == "none" {
            return Paint::None;
        }
        if spec.starts_with("url(") {
            let id = url_id(spec).unwrap_or_default();
            if let Some(g) = self.gradient(&id, 0) {
                let mut g = g;
                // Map device pixel -> gradient space.
                let unit = if g.bbox_units { Mat([bbox[2] - bbox[0], 0.0, 0.0, bbox[3] - bbox[1], bbox[0], bbox[1]]) } else { Mat::ID };
                let full = m.mul(unit).mul(g.transform);
                if let Some(inv) = full.invert() {
                    g.transform = inv;
                    if g.stops.len() == 1 {
                        return Paint::Solid(g.stops[0].1);
                    }
                    if g.stops.is_empty() {
                        return Paint::None;
                    }
                    return Paint::Grad(g);
                }
                return Paint::None;
            }
            // Fallback color after the url, else none.
            return spec.split(')').nth(1).and_then(|f| parse_color(f, st.color)).map(Paint::Solid).unwrap_or(Paint::None);
        }
        parse_color(spec, st.color).map(Paint::Solid).unwrap_or(Paint::None)
    }

    fn gradient(&self, id: &str, depth: u32) -> Option<Grad> {
        let n = *self.ids.get(id)?;
        let linear = match n.tag.as_str() {
            "linearGradient" => true,
            "radialGradient" => false,
            _ => return None,
        };
        // Inherit from the href'd gradient first.
        let base = if depth < 8 { n.attr("href").and_then(|h| h.strip_prefix('#')).and_then(|i| self.gradient(i, depth + 1)) } else { None };
        let mut g = base.unwrap_or(Grad { linear, coords: if linear { [0.0, 0.0, 1.0, 0.0, 0.0] } else { [0.5, 0.5, 0.5, f32::NAN, f32::NAN] }, bbox_units: true, transform: Mat::ID, spread: 0, stops: vec![] });
        if g.linear != linear {
            g.coords = if linear { [0.0, 0.0, 1.0, 0.0, 0.0] } else { [0.5, 0.5, 0.5, f32::NAN, f32::NAN] };
            g.linear = linear;
        }
        let num = |k: &str| -> Option<f32> {
            n.attr(k).map(|v| {
                let x = length(v);
                if v.trim_end().ends_with('%') { x / 100.0 } else { x }
            })
        };
        if let Some(u) = n.attr("gradientUnits") {
            g.bbox_units = u != "userSpaceOnUse";
        }
        if let Some(t) = n.attr("gradientTransform") {
            g.transform = parse_transform(t);
        }
        if let Some(s) = n.attr("spreadMethod") {
            g.spread = match s { "reflect" => 1, "repeat" => 2, _ => 0 };
        }
        if linear {
            for (i, k) in ["x1", "y1", "x2", "y2"].iter().enumerate() {
                if let Some(v) = num(k) {
                    g.coords[i] = v;
                }
            }
        } else {
            for (i, k) in ["cx", "cy", "r", "fx", "fy"].iter().enumerate() {
                if let Some(v) = num(k) {
                    g.coords[i] = v;
                }
            }
        }
        let stops: Vec<(f32, Rgba)> = n
            .kids
            .iter()
            .filter(|k| k.tag == "stop")
            .map(|s| {
                let mut props: Vec<(String, String)> = s.attrs.clone();
                if let Some(st) = s.attr("style") {
                    props.extend(parse_decls(st));
                }
                for (sel, decls) in &self.css.rules {
                    if Css::matches(sel, s) {
                        props.extend(decls.iter().cloned());
                    }
                }
                let get = |k: &str| props.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.clone());
                let off = get("offset").map(|v| if v.trim_end().ends_with('%') { length(&v) / 100.0 } else { length(&v) }).unwrap_or(0.0).clamp(0.0, 1.0);
                let mut c = get("stop-color").and_then(|v| parse_color(&v, [0.0, 0.0, 0.0, 1.0])).unwrap_or([0.0, 0.0, 0.0, 1.0]);
                c[3] *= get("stop-opacity").map(|v| opacity(&v)).unwrap_or(1.0);
                (off, c)
            })
            .collect();
        if !stops.is_empty() {
            let mut last = 0.0f32;
            g.stops = stops.into_iter().map(|(o, c)| {
                let o = o.max(last);
                last = o;
                (o, c)
            }).collect();
        }
        Some(g)
    }

    fn composite(&mut self, paint: &Paint, rule: Rule, alpha: f32) {
        if alpha <= 0.0 {
            return;
        }
        let w = self.w;
        let raster = std::mem::replace(&mut self.raster, Raster::new(0, 0));
        let dst = &mut self.layers.last_mut().expect("layer").px;
        raster.for_each(rule, |x, y, cov| {
            let c = match paint {
                Paint::Solid(c) => *c,
                Paint::Grad(g) => grad_at(g, x as f32 + 0.5, y as f32 + 0.5),
                Paint::None => return,
            };
            let a = c[3] * cov * alpha;
            if a <= 0.0 {
                return;
            }
            let d = &mut dst[y * w + x];
            for k in 0..3 {
                d[k] = c[k] * a + d[k] * (1.0 - a);
            }
            d[3] = a + d[3] * (1.0 - a);
        });
        self.raster = raster;
    }

    fn image(&mut self, n: &Node, m: Mat) {
        let Some(href) = n.attr("href") else { return };
        let Some(b64) = href.strip_prefix("data:").and_then(|r| r.split_once(',')).filter(|(meta, _)| meta.contains("base64")).map(|(_, d)| d) else { return };
        let Some(bytes) = base64(b64) else { return };
        let Some(img) = crate::imgdec::decode_bytes(&bytes) else { return };
        let (x, y) = (length(n.attr("x").unwrap_or("0")), length(n.attr("y").unwrap_or("0")));
        let (iw, ih) = (img.w as f32, img.h as f32);
        let w = n.attr("width").map(length).unwrap_or(iw);
        let h = n.attr("height").map(length).unwrap_or(ih);
        let fit = n.attr("preserveAspectRatio").unwrap_or("xMidYMid");
        let (sx, sy, ox, oy) = if fit.starts_with("none") {
            (w / iw, h / ih, 0.0, 0.0)
        } else {
            let s = (w / iw).min(h / ih);
            (s, s, (w - iw * s) / 2.0, (h - ih * s) / 2.0)
        };
        let full = m.mul(Mat::translate(x + ox, y + oy)).mul(Mat::scale(sx, sy));
        let Some(inv) = full.invert() else { return };
        // Destination bounds.
        let corners = [full.apply(0.0, 0.0), full.apply(iw, 0.0), full.apply(0.0, ih), full.apply(iw, ih)];
        let x0 = corners.iter().map(|c| c.0).fold(f32::INFINITY, f32::min).floor().max(0.0) as usize;
        let x1 = (corners.iter().map(|c| c.0).fold(f32::NEG_INFINITY, f32::max).ceil().max(0.0) as usize).min(self.w);
        let y0 = corners.iter().map(|c| c.1).fold(f32::INFINITY, f32::min).floor().max(0.0) as usize;
        let y1 = (corners.iter().map(|c| c.1).fold(f32::NEG_INFINITY, f32::max).ceil().max(0.0) as usize).min(self.h);
        let wpx = self.w;
        let px = &img.px;
        let (iwu, ihu) = (img.w as usize, img.h as usize);
        let sample = |u: f32, v: f32| -> Rgba {
            let (u, v) = (u - 0.5, v - 0.5);
            let (x0, y0) = (u.floor(), v.floor());
            let (fx, fy) = (u - x0, v - y0);
            let mut out = [0.0f32; 4];
            for (dx, dy, wgt) in [(0, 0, (1.0 - fx) * (1.0 - fy)), (1, 0, fx * (1.0 - fy)), (0, 1, (1.0 - fx) * fy), (1, 1, fx * fy)] {
                let xi = (x0 as i64 + dx).clamp(0, iwu as i64 - 1) as usize;
                let yi = (y0 as i64 + dy).clamp(0, ihu as i64 - 1) as usize;
                let p = &px[(yi * iwu + xi) * 4..(yi * iwu + xi) * 4 + 4];
                let a = p[3] as f32 / 255.0;
                for c in 0..3 {
                    out[c] += p[c] as f32 / 255.0 * a * wgt;
                }
                out[3] += a * wgt;
            }
            out
        };
        let dst = self.top();
        for yy in y0..y1 {
            for xx in x0..x1 {
                let (u, v) = inv.apply(xx as f32 + 0.5, yy as f32 + 0.5);
                if u < 0.0 || v < 0.0 || u >= iw || v >= ih {
                    continue;
                }
                let s = sample(u, v);
                if s[3] <= 0.0 {
                    continue;
                }
                let d = &mut dst[yy * wpx + xx];
                for c in 0..3 {
                    d[c] = s[c] + d[c] * (1.0 - s[3]);
                }
                d[3] = s[3] + d[3] * (1.0 - s[3]);
            }
        }
    }
}

fn op_single(isolated: bool, op: f32) -> f32 {
    if isolated { 1.0 } else { op }
}

fn url_id(v: &str) -> Option<String> {
    let s = v.trim().strip_prefix("url(")?;
    let e = s.find(')')?;
    Some(s[..e].trim().trim_matches(['"', '\'']).trim_start_matches('#').to_string())
}

fn grad_at(g: &Grad, x: f32, y: f32) -> Rgba {
    let (u, v) = g.transform.apply(x, y);
    let t = if g.linear {
        let [x1, y1, x2, y2, _] = g.coords;
        let (dx, dy) = (x2 - x1, y2 - y1);
        let l2 = dx * dx + dy * dy;
        if l2 <= 1e-12 { 1.0 } else { ((u - x1) * dx + (v - y1) * dy) / l2 }
    } else {
        let [cx, cy, r, fx, fy] = g.coords;
        let (fx, fy) = (if fx.is_nan() { cx } else { fx }, if fy.is_nan() { cy } else { fy });
        if r <= 1e-9 {
            1.0
        } else if (fx - cx).abs() < 1e-6 && (fy - cy).abs() < 1e-6 {
            (u - cx).hypot(v - cy) / r
        } else {
            // Focal radial gradient: find t where (u,v) lies on the circle around
            // f + t (c - f) with radius t r.
            let (dx, dy) = (u - fx, v - fy);
            let (cdx, cdy) = (cx - fx, cy - fy);
            let a = cdx * cdx + cdy * cdy - r * r;
            let b = dx * cdx + dy * cdy;
            let c = dx * dx + dy * dy;
            if a.abs() < 1e-9 {
                if b.abs() < 1e-9 { 0.0 } else { c / (2.0 * b) }
            } else {
                let disc = b * b - a * c;
                if disc < 0.0 { 1.0 } else { (b - disc.sqrt()) / a }
            }
        }
    };
    let t = match g.spread {
        1 => {
            let m = t.abs() % 2.0;
            if m > 1.0 { 2.0 - m } else { m }
        }
        2 => t - t.floor(),
        _ => t.clamp(0.0, 1.0),
    };
    let s = &g.stops;
    if t <= s[0].0 {
        return s[0].1;
    }
    for k in 1..s.len() {
        if t <= s[k].0 {
            let (o0, c0) = s[k - 1];
            let (o1, c1) = s[k];
            let f = if o1 - o0 <= 1e-9 { 1.0 } else { (t - o0) / (o1 - o0) };
            // Interpolate premultiplied so fades to transparent don't darken.
            let (a0, a1) = (c0[3], c1[3]);
            let a = a0 + (a1 - a0) * f;
            if a <= 0.0 {
                return [0.0; 4];
            }
            let mut out = [0.0; 4];
            for i in 0..3 {
                out[i] = (c0[i] * a0 + (c1[i] * a1 - c0[i] * a0) * f) / a;
            }
            out[3] = a;
            return out;
        }
    }
    s[s.len() - 1].1
}

fn segs_bbox(segs: &[Seg]) -> [f32; 4] {
    let mut b = [f32::INFINITY, f32::INFINITY, f32::NEG_INFINITY, f32::NEG_INFINITY];
    let mut add = |x: f32, y: f32| {
        b[0] = b[0].min(x);
        b[1] = b[1].min(y);
        b[2] = b[2].max(x);
        b[3] = b[3].max(y);
    };
    for s in segs {
        match *s {
            Seg::Move(x, y) | Seg::Line(x, y) => add(x, y),
            Seg::Cubic(a, b2, c, d, x, y) => {
                add(a, b2);
                add(c, d);
                add(x, y);
            }
            Seg::Close => {}
        }
    }
    if !b[0].is_finite() {
        return [0.0, 0.0, 1.0, 1.0];
    }
    if b[2] - b[0] < 1e-6 {
        b[2] = b[0] + 1e-6;
    }
    if b[3] - b[1] < 1e-6 {
        b[3] = b[1] + 1e-6;
    }
    b
}

fn shape_segs(n: &Node) -> Option<Vec<Seg>> {
    let f = |k: &str| n.attr(k).map(length).unwrap_or(0.0);
    let v = match n.tag.as_str() {
        "path" => parse_path(n.attr("d")?),
        "rect" => {
            let (x, y, w, h) = (f("x"), f("y"), f("width"), f("height"));
            if w <= 0.0 || h <= 0.0 {
                return None;
            }
            let (mut rx, mut ry) = (n.attr("rx").map(length), n.attr("ry").map(length));
            if rx.is_none() {
                rx = ry;
            }
            if ry.is_none() {
                ry = rx;
            }
            let (rx, ry) = (rx.unwrap_or(0.0).min(w / 2.0), ry.unwrap_or(0.0).min(h / 2.0));
            if rx <= 0.0 || ry <= 0.0 {
                vec![Seg::Move(x, y), Seg::Line(x + w, y), Seg::Line(x + w, y + h), Seg::Line(x, y + h), Seg::Close]
            } else {
                let k = 0.552_284_8;
                let (kx, ky) = (rx * k, ry * k);
                vec![
                    Seg::Move(x + rx, y),
                    Seg::Line(x + w - rx, y),
                    Seg::Cubic(x + w - rx + kx, y, x + w, y + ry - ky, x + w, y + ry),
                    Seg::Line(x + w, y + h - ry),
                    Seg::Cubic(x + w, y + h - ry + ky, x + w - rx + kx, y + h, x + w - rx, y + h),
                    Seg::Line(x + rx, y + h),
                    Seg::Cubic(x + rx - kx, y + h, x, y + h - ry + ky, x, y + h - ry),
                    Seg::Line(x, y + ry),
                    Seg::Cubic(x, y + ry - ky, x + rx - kx, y, x + rx, y),
                    Seg::Close,
                ]
            }
        }
        "circle" | "ellipse" => {
            let (cx, cy) = (f("cx"), f("cy"));
            let (rx, ry) = if n.tag == "circle" { (f("r"), f("r")) } else { (f("rx"), f("ry")) };
            if rx <= 0.0 || ry <= 0.0 {
                return None;
            }
            let k = 0.552_284_8;
            let (kx, ky) = (rx * k, ry * k);
            vec![
                Seg::Move(cx + rx, cy),
                Seg::Cubic(cx + rx, cy + ky, cx + kx, cy + ry, cx, cy + ry),
                Seg::Cubic(cx - kx, cy + ry, cx - rx, cy + ky, cx - rx, cy),
                Seg::Cubic(cx - rx, cy - ky, cx - kx, cy - ry, cx, cy - ry),
                Seg::Cubic(cx + kx, cy - ry, cx + rx, cy - ky, cx + rx, cy),
                Seg::Close,
            ]
        }
        "line" => vec![Seg::Move(f("x1"), f("y1")), Seg::Line(f("x2"), f("y2"))],
        "polyline" | "polygon" => {
            let p = numbers(n.attr("points")?);
            let mut v = vec![];
            for (i, c) in p.chunks_exact(2).enumerate() {
                v.push(if i == 0 { Seg::Move(c[0], c[1]) } else { Seg::Line(c[0], c[1]) });
            }
            if n.tag == "polygon" {
                v.push(Seg::Close);
            }
            v
        }
        _ => return None,
    };
    Some(v)
}

// ------------------------------------------------------------------ strokes

fn dash(lines: &[(Vec<(f32, f32)>, bool)], pat: &[f32], offset: f32) -> Vec<(Vec<(f32, f32)>, bool)> {
    let total: f32 = pat.iter().sum();
    if total <= 0.0 {
        return lines.to_vec();
    }
    let mut out = vec![];
    for (pts, closed) in lines {
        let mut pts = pts.clone();
        if *closed {
            pts.push(pts[0]);
        }
        let mut idx = 0;
        let mut left = pat[0];
        let mut on = true;
        let mut off = offset.rem_euclid(total);
        while off > 0.0 {
            if off >= left {
                off -= left;
                idx = (idx + 1) % pat.len();
                left = pat[idx];
                on = !on;
            } else {
                left -= off;
                off = 0.0;
            }
        }
        let mut cur: Vec<(f32, f32)> = if on { vec![pts[0]] } else { vec![] };
        for k in 1..pts.len() {
            let (mut a, b) = (pts[k - 1], pts[k]);
            let mut seg = (b.0 - a.0).hypot(b.1 - a.1);
            while seg > 0.0 {
                if seg <= left {
                    left -= seg;
                    if on {
                        cur.push(b);
                    }
                    seg = 0.0;
                } else {
                    let t = left / seg;
                    let p = (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t);
                    if on {
                        cur.push(p);
                        out.push((std::mem::take(&mut cur), false));
                    } else {
                        cur = vec![p];
                    }
                    seg -= left;
                    a = p;
                    on = !on;
                    idx = (idx + 1) % pat.len();
                    left = pat[idx];
                }
            }
        }
        if on && cur.len() > 1 {
            out.push((cur, false));
        }
    }
    out
}

/// Stroke outline as a union of positively-wound convex pieces (segment quads, joins,
/// caps); filled with the non-zero rule their union is the stroke.
fn stroke_polys(lines: &[(Vec<(f32, f32)>, bool)], w: f32, cap: u8, join: u8, miter: f32) -> Vec<Vec<(f32, f32)>> {
    let hw = w / 2.0;
    let mut out: Vec<Vec<(f32, f32)>> = vec![];
    let mut push = |mut p: Vec<(f32, f32)>| {
        // Orient every piece counter-clockwise so overlaps add up instead of cancelling.
        let area: f32 = (0..p.len()).map(|i| {
            let (a, b) = (p[i], p[(i + 1) % p.len()]);
            a.0 * b.1 - b.0 * a.1
        }).sum();
        if area < 0.0 {
            p.reverse();
        }
        if area.abs() > 1e-6 {
            out.push(p);
        }
    };
    let circle = |c: (f32, f32), r: f32| -> Vec<(f32, f32)> {
        let n = ((r * 2.0).ceil() as usize).clamp(8, 48);
        (0..n).map(|i| {
            let a = i as f32 / n as f32 * std::f32::consts::TAU;
            (c.0 + r * a.cos(), c.1 + r * a.sin())
        }).collect()
    };
    for (pts, closed) in lines {
        // Drop repeated points.
        let mut p: Vec<(f32, f32)> = vec![];
        for &q in pts {
            if p.last().map(|l: &(f32, f32)| (l.0 - q.0).hypot(l.1 - q.1) > 1e-4).unwrap_or(true) {
                p.push(q);
            }
        }
        if *closed && p.len() > 2 && (p[0].0 - p[p.len() - 1].0).hypot(p[0].1 - p[p.len() - 1].1) < 1e-4 {
            p.pop();
        }
        if p.len() < 2 {
            // Zero-length subpath: round/square caps draw a dot.
            if let Some(&c) = p.first() {
                match cap {
                    1 => push(circle(c, hw)),
                    2 => push(vec![(c.0 - hw, c.1 - hw), (c.0 + hw, c.1 - hw), (c.0 + hw, c.1 + hw), (c.0 - hw, c.1 + hw)]),
                    _ => {}
                }
            }
            continue;
        }
        let n = p.len();
        let segs = if *closed { n } else { n - 1 };
        let normal = |a: (f32, f32), b: (f32, f32)| {
            let (dx, dy) = (b.0 - a.0, b.1 - a.1);
            let l = dx.hypot(dy).max(1e-9);
            (-dy / l * hw, dx / l * hw)
        };
        for k in 0..segs {
            let (a, b) = (p[k], p[(k + 1) % n]);
            let (nx, ny) = normal(a, b);
            push(vec![(a.0 + nx, a.1 + ny), (b.0 + nx, b.1 + ny), (b.0 - nx, b.1 - ny), (a.0 - nx, a.1 - ny)]);
        }
        // Joins at interior vertices (all vertices when closed).
        let verts: Vec<usize> = if *closed { (0..n).collect() } else { (1..n - 1).collect() };
        for &k in &verts {
            let prev = p[(k + n - 1) % n];
            let cur = p[k];
            let next = p[(k + 1) % n];
            let (n1x, n1y) = normal(prev, cur);
            let (n2x, n2y) = normal(cur, next);
            let cross = (cur.0 - prev.0) * (next.1 - cur.1) - (cur.1 - prev.1) * (next.0 - cur.0);
            // Outer side of the turn.
            let s = if cross > 0.0 { -1.0 } else { 1.0 };
            let o1 = (cur.0 + n1x * s, cur.1 + n1y * s);
            let o2 = (cur.0 + n2x * s, cur.1 + n2y * s);
            match join {
                1 => push(circle(cur, hw)),
                2 => push(vec![cur, o1, o2]),
                _ => {
                    // Miter: the tip where the two offset edges meet, within the limit.
                    let (d1x, d1y) = (cur.0 - prev.0, cur.1 - prev.1);
                    let (d2x, d2y) = (next.0 - cur.0, next.1 - cur.1);
                    let den = d1x * d2y - d1y * d2x;
                    if den.abs() < 1e-9 {
                        push(vec![cur, o1, o2]);
                        continue;
                    }
                    let t = ((o2.0 - o1.0) * d2y - (o2.1 - o1.1) * d2x) / den;
                    let tip = (o1.0 + d1x * t, o1.1 + d1y * t);
                    let len = (tip.0 - cur.0).hypot(tip.1 - cur.1);
                    if len <= miter * hw {
                        push(vec![cur, o1, tip, o2]);
                    } else {
                        push(vec![cur, o1, o2]);
                    }
                }
            }
        }
        if !*closed {
            for (end, other) in [(p[0], p[1]), (p[n - 1], p[n - 2])] {
                match cap {
                    1 => push(circle(end, hw)),
                    2 => {
                        let (dx, dy) = (end.0 - other.0, end.1 - other.1);
                        let l = dx.hypot(dy).max(1e-9);
                        let (ex, ey) = (dx / l * hw, dy / l * hw);
                        let (nx, ny) = (-ey, ex);
                        push(vec![(end.0 + nx, end.1 + ny), (end.0 + nx + ex, end.1 + ny + ey), (end.0 - nx + ex, end.1 - ny + ey), (end.0 - nx, end.1 - ny)]);
                    }
                    _ => {}
                }
            }
        }
    }
    out
}

// ------------------------------------------------------------------ blur

fn blur_layer(px: &mut [Rgba], w: usize, h: usize, sigma: f32) {
    // Three box blurs approximate a Gaussian.
    let n = 3.0f32;
    let wi = ((12.0 * sigma * sigma / n + 1.0).sqrt()).floor();
    let wl = if (wi as i32) % 2 == 0 { wi - 1.0 } else { wi };
    let wu = wl + 2.0;
    let m = ((12.0 * sigma * sigma - n * wl * wl - 4.0 * n * wl - 3.0 * n) / (-4.0 * wl - 4.0)).round();
    let mut tmp = px.to_vec();
    for i in 0..3 {
        let r = (if (i as f32) < m { wl } else { wu } as i32 - 1) / 2;
        if r <= 0 {
            continue;
        }
        box_pass(px, &mut tmp, w, h, r as usize, true);
        box_pass(&tmp.clone(), px, w, h, r as usize, false);
    }
}

fn box_pass(src: &[Rgba], dst: &mut [Rgba], w: usize, h: usize, r: usize, horizontal: bool) {
    let (outer, inner) = if horizontal { (h, w) } else { (w, h) };
    let idx = |o: usize, i: usize| if horizontal { o * w + i } else { i * w + o };
    let norm = 1.0 / (2 * r + 1) as f32;
    for o in 0..outer {
        let mut acc = [0.0f32; 4];
        for i in 0..=r.min(inner - 1) {
            for c in 0..4 {
                acc[c] += src[idx(o, i)][c];
            }
        }
        for i in 0..inner {
            for c in 0..4 {
                dst[idx(o, i)][c] = acc[c] * norm;
            }
            if i + r + 1 < inner {
                for c in 0..4 {
                    acc[c] += src[idx(o, i + r + 1)][c];
                }
            }
            if i >= r {
                for c in 0..4 {
                    acc[c] -= src[idx(o, i - r)][c];
                }
            }
        }
    }
}

fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0;
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => continue, // whitespace and line breaks
        } as u32;
        buf = buf << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

// ------------------------------------------------------------------ entry point

fn collect_ids<'a>(n: &'a Node, out: &mut HashMap<String, &'a Node>, css: &mut String) {
    if let Some(id) = n.attr("id") {
        out.entry(id.to_string()).or_insert(n);
    }
    if n.tag == "style" {
        css.push_str(&n.text);
        css.push('\n');
    }
    for k in &n.kids {
        collect_ids(k, out, css);
    }
}

/// A document's own size (its width and height, else its viewBox's) and its viewBox.
fn view_size(root: &Node) -> (f32, f32, Option<Vec<f32>>) {
    let num = |k: &str| root.attr(k).filter(|v| !v.trim_end().ends_with('%')).map(length);
    let vb: Option<Vec<f32>> = root.attr("viewBox").map(numbers).filter(|v| v.len() == 4 && v[2] > 0.0 && v[3] > 0.0);
    let (vw, vh) = match (&vb, num("width"), num("height")) {
        (_, Some(w), Some(h)) if w > 0.0 && h > 0.0 => (w, h),
        (Some(v), _, _) => (v[2], v[3]),
        _ => (16.0, 16.0),
    };
    (vw, vh, vb)
}

/// The size an SVG document says it is, in pixels (not the size it is drawn at).
pub fn size(data: &[u8]) -> Option<[u32; 2]> {
    let (w, h, _) = view_size(&parse_xml(std::str::from_utf8(data).ok()?)?);
    Some([(w.round() as u32).max(1), (h.round() as u32).max(1)])
}

/// Render an SVG document so its larger side is `px` pixels. Returns (width, height,
/// premultiplied RGBA8).
pub fn render(data: &[u8], px: u32) -> Option<(u32, u32, Vec<u8>)> {
    let text = std::str::from_utf8(data).ok()?;
    let root = parse_xml(text)?;
    let (vw, vh, vb) = view_size(&root);
    let s = px as f32 / vw.max(vh);
    let (w, h) = (((vw * s).round() as usize).clamp(1, 4096), ((vh * s).round() as usize).clamp(1, 4096));
    let mut m = Mat::scale(s, s);
    if let Some(v) = &vb {
        // xMidYMid meet
        let k = (vw / v[2]).min(vh / v[3]);
        let (ox, oy) = ((vw - v[2] * k) / 2.0, (vh - v[3] * k) / 2.0);
        m = m.mul(Mat::translate(ox, oy)).mul(Mat::scale(k, k)).mul(Mat::translate(-v[0], -v[1]));
    }
    let mut ids = HashMap::new();
    let mut css_src = String::new();
    collect_ids(&root, &mut ids, &mut css_src);
    let mut cx = Ctx { w, h, ids, css: parse_css(&css_src), layers: vec![Layer { px: vec![[0.0; 4]; w * h] }], raster: Raster::new(w, h), depth: 0 };
    let base = Style::default();
    // The root's own attributes (fill, style, transform...) apply to its children.
    let (st, ..) = cx.style_of(&root, &base);
    for k in &root.kids {
        cx.render(k, m, &st);
    }
    let px = &cx.layers[0].px;
    let mut out = Vec::with_capacity(w * h * 4);
    for p in px {
        for c in p {
            out.push((c.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
        }
    }
    Some((w as u32, h as u32, out))
}
