//! A QR code for a short text, such as the address Send To Phone shares: byte mode, error
//! correction M (L when M doesn't fit), versions 1 to 10 (up to 213 bytes at M). Enough
//! for an address; nothing a crate would add is needed for that.

/// Error correction codewords per block and blocks, for versions 1 to 10 (index 0 unused):
/// level L, then level M.
const ECC_PER_BLOCK: [[u8; 11]; 2] = [[0, 7, 10, 15, 20, 26, 18, 20, 24, 30, 18], [0, 10, 16, 26, 18, 24, 16, 18, 22, 22, 26]];
const BLOCKS: [[u8; 11]; 2] = [[0, 1, 1, 1, 1, 1, 2, 2, 2, 2, 4], [0, 1, 1, 1, 2, 2, 4, 4, 4, 5, 5]];
const MOST: usize = 10;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    L,
    M,
}

impl Level {
    fn row(self) -> usize {
        match self {
            Level::L => 0,
            Level::M => 1,
        }
    }
    /// The two bits the format information carries.
    fn bits(self) -> u32 {
        match self {
            Level::L => 1,
            Level::M => 0,
        }
    }
}

/// A finished code: `size` × `size` modules, true for dark, without the quiet zone.
pub struct Code {
    pub size: usize,
    dark: Vec<bool>,
}

impl Code {
    pub fn get(&self, x: usize, y: usize) -> bool {
        x < self.size && y < self.size && self.dark[y * self.size + x]
    }
}

/// Modules in a code of version `v`, data and error correction together.
fn raw_modules(v: usize) -> usize {
    let mut r = (16 * v + 128) * v + 64;
    if v >= 2 {
        let n = v / 7 + 2;
        r -= (25 * n - 10) * n - 55;
        if v >= 7 {
            r -= 36;
        }
    }
    r
}

/// Data codewords a version holds at a level.
fn data_codewords(v: usize, l: Level) -> usize {
    raw_modules(v) / 8 - ECC_PER_BLOCK[l.row()][v] as usize * BLOCKS[l.row()][v] as usize
}

/// The code for `text`: M when it fits in version 10, else L; None when even L can't.
pub fn encode(text: &[u8]) -> Option<Code> {
    for l in [Level::M, Level::L] {
        for v in 1..=MOST {
            let count_bits = if v <= 9 { 8 } else { 16 };
            if 4 + count_bits + text.len() * 8 <= data_codewords(v, l) * 8 {
                return Some(build(text, v, l));
            }
        }
    }
    None
}

fn build(text: &[u8], v: usize, l: Level) -> Code {
    // The bits: byte mode, the count, the bytes, then the terminator and padding.
    let cap = data_codewords(v, l);
    let mut bits: Vec<bool> = Vec::with_capacity(cap * 8);
    let mut put = |val: u32, n: usize| {
        for i in (0..n).rev() {
            bits.push((val >> i) & 1 == 1);
        }
    };
    put(0b0100, 4);
    put(text.len() as u32, if v <= 9 { 8 } else { 16 });
    for &b in text {
        put(b as u32, 8);
    }
    let room = cap * 8;
    let end = (room - bits.len()).min(4);
    bits.extend(std::iter::repeat_n(false, end));
    while bits.len() % 8 != 0 {
        bits.push(false);
    }
    let mut data: Vec<u8> = bits.chunks(8).map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b as u8)).collect();
    for pad in [0xEC, 0x11].iter().cycle() {
        if data.len() >= cap {
            break;
        }
        data.push(*pad);
    }
    let words = interleave(&data, v, l);
    let size = v * 4 + 17;
    let mut q = Grid { size, dark: vec![false; size * size], fixed: vec![false; size * size] };
    q.patterns(v);
    q.place(&words);
    // The mask that leaves the fewest look-alikes of the finder patterns, runs and blocks.
    let mut best = (u32::MAX, 0);
    for m in 0..8 {
        q.mask(m);
        q.format(l, m);
        let p = q.penalty();
        if p < best.0 {
            best = (p, m);
        }
        q.mask(m);
    }
    q.mask(best.1);
    q.format(l, best.1);
    Code { size, dark: q.dark }
}

/// Error correction added to each block, then the blocks' codewords taken in turn.
fn interleave(data: &[u8], v: usize, l: Level) -> Vec<u8> {
    let (n, ecc) = (BLOCKS[l.row()][v] as usize, ECC_PER_BLOCK[l.row()][v] as usize);
    let raw = raw_modules(v) / 8;
    let short = n - raw % n;
    let short_len = raw / n;
    let div = divisor(ecc);
    let mut blocks: Vec<Vec<u8>> = Vec::with_capacity(n);
    let mut k = 0;
    for i in 0..n {
        let len = short_len - ecc + usize::from(i >= short);
        let mut b = data[k..k + len].to_vec();
        k += len;
        let rem = remainder(&b, &div);
        if i < short {
            b.push(0);
        }
        b.extend(rem);
        blocks.push(b);
    }
    let mut out = Vec::with_capacity(raw);
    for i in 0..blocks[0].len() {
        for (j, b) in blocks.iter().enumerate() {
            // Short blocks have a hole where the long ones carry one more data codeword.
            if i != short_len - ecc || j >= short {
                out.push(b[i]);
            }
        }
    }
    out
}

/// Product in GF(256) with the QR polynomial.
fn mul(x: u8, y: u8) -> u8 {
    let mut z: u32 = 0;
    for i in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11D);
        z ^= ((y as u32 >> i) & 1) * x as u32;
    }
    z as u8
}

fn divisor(degree: usize) -> Vec<u8> {
    let mut r = vec![0u8; degree];
    r[degree - 1] = 1;
    let mut root = 1u8;
    for _ in 0..degree {
        for j in 0..degree {
            r[j] = mul(r[j], root);
            if j + 1 < degree {
                r[j] ^= r[j + 1];
            }
        }
        root = mul(root, 2);
    }
    r
}

fn remainder(data: &[u8], div: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; div.len()];
    for &b in data {
        let f = b ^ r[0];
        r.rotate_left(1);
        if let Some(last) = r.last_mut() {
            *last = 0;
        }
        for (x, &y) in r.iter_mut().zip(div) {
            *x ^= mul(y, f);
        }
    }
    r
}

struct Grid {
    size: usize,
    dark: Vec<bool>,
    /// Finder, timing, alignment and format modules: no data, no mask.
    fixed: Vec<bool>,
}

impl Grid {
    fn set(&mut self, x: usize, y: usize, dark: bool) {
        let i = y * self.size + x;
        self.dark[i] = dark;
        self.fixed[i] = true;
    }

    fn patterns(&mut self, v: usize) {
        let s = self.size;
        for i in 0..s {
            self.set(6, i, i % 2 == 0);
            self.set(i, 6, i % 2 == 0);
        }
        for (cx, cy) in [(3, 3), (s - 4, 3), (3, s - 4)] {
            for dy in -4i32..=4 {
                for dx in -4i32..=4 {
                    let (x, y) = (cx as i32 + dx, cy as i32 + dy);
                    if x >= 0 && y >= 0 && (x as usize) < s && (y as usize) < s {
                        let d = dx.abs().max(dy.abs());
                        self.set(x as usize, y as usize, d != 2 && d != 4);
                    }
                }
            }
        }
        let pos = align_positions(v);
        let last = pos.len().saturating_sub(1);
        for (i, &x) in pos.iter().enumerate() {
            for (j, &y) in pos.iter().enumerate() {
                if (i == 0 && j == 0) || (i == 0 && j == last) || (i == last && j == 0) {
                    continue;
                }
                for dy in -2i32..=2 {
                    for dx in -2i32..=2 {
                        self.set((x as i32 + dx) as usize, (y as i32 + dy) as usize, dx.abs().max(dy.abs()) != 1);
                    }
                }
            }
        }
        // Reserved for now; written once the mask is chosen.
        self.format(Level::M, 0);
        if v >= 7 {
            let mut rem = v as u32;
            for _ in 0..12 {
                rem = (rem << 1) ^ ((rem >> 11) * 0x1F25);
            }
            let bits = (v as u32) << 12 | rem;
            for i in 0..18 {
                let b = (bits >> i) & 1 == 1;
                let (a, c) = (s - 11 + i % 3, i / 3);
                self.set(a, c, b);
                self.set(c, a, b);
            }
        }
    }

    fn format(&mut self, l: Level, mask: u32) {
        let s = self.size;
        let data = l.bits() << 3 | mask;
        let mut rem = data;
        for _ in 0..10 {
            rem = (rem << 1) ^ ((rem >> 9) * 0x537);
        }
        let bits = (data << 10 | rem) ^ 0x5412;
        let bit = |i: u32| (bits >> i) & 1 == 1;
        for i in 0..6 {
            self.set(8, i, bit(i as u32));
        }
        self.set(8, 7, bit(6));
        self.set(8, 8, bit(7));
        self.set(7, 8, bit(8));
        for i in 9..15 {
            self.set(14 - i, 8, bit(i as u32));
        }
        for i in 0..8 {
            self.set(s - 1 - i, 8, bit(i as u32));
        }
        for i in 8..15 {
            self.set(8, s - 15 + i, bit(i as u32));
        }
        self.set(8, s - 8, true);
    }

    /// The codewords up and down in pairs of columns from the right, around what is fixed.
    fn place(&mut self, words: &[u8]) {
        let s = self.size;
        let total = words.len() * 8;
        let mut i = 0;
        let mut right = s as i32 - 1;
        while right >= 1 {
            if right == 6 {
                right = 5;
            }
            for vert in 0..s {
                for j in 0..2 {
                    let x = (right - j) as usize;
                    let up = (right + 1) & 2 == 0;
                    let y = if up { s - 1 - vert } else { vert };
                    let k = y * s + x;
                    if !self.fixed[k] && i < total {
                        self.dark[k] = (words[i >> 3] >> (7 - (i & 7))) & 1 == 1;
                        i += 1;
                    }
                }
            }
            right -= 2;
        }
    }

    /// Flip the data modules the mask picks (twice undoes it).
    fn mask(&mut self, m: u32) {
        let s = self.size;
        for y in 0..s {
            for x in 0..s {
                let flip = match m {
                    0 => (x + y) % 2 == 0,
                    1 => y % 2 == 0,
                    2 => x % 3 == 0,
                    3 => (x + y) % 3 == 0,
                    4 => (x / 3 + y / 2) % 2 == 0,
                    5 => x * y % 2 + x * y % 3 == 0,
                    6 => (x * y % 2 + x * y % 3) % 2 == 0,
                    _ => ((x + y) % 2 + x * y % 3) % 2 == 0,
                };
                let k = y * s + x;
                if flip && !self.fixed[k] {
                    self.dark[k] = !self.dark[k];
                }
            }
        }
    }

    /// The standard's four penalties: runs of five or more, 2×2 blocks, finder look-alikes
    /// and an uneven share of dark.
    fn penalty(&self) -> u32 {
        let s = self.size;
        let at = |x: usize, y: usize| self.dark[y * s + x];
        let mut p = 0u32;
        for horiz in [true, false] {
            for a in 0..s {
                let line: Vec<bool> = (0..s).map(|b| if horiz { at(b, a) } else { at(a, b) }).collect();
                let mut run = 1;
                for b in 1..s {
                    if line[b] == line[b - 1] {
                        run += 1;
                        if run == 5 {
                            p += 3;
                        } else if run > 5 {
                            p += 1;
                        }
                    } else {
                        run = 1;
                    }
                }
                const A: [bool; 11] = [true, false, true, true, true, false, true, false, false, false, false];
                for b in 0..s.saturating_sub(10) {
                    let w = &line[b..b + 11];
                    if w == A || w.iter().rev().eq(A.iter()) {
                        p += 40;
                    }
                }
            }
        }
        for y in 0..s - 1 {
            for x in 0..s - 1 {
                let c = at(x, y);
                if c == at(x + 1, y) && c == at(x, y + 1) && c == at(x + 1, y + 1) {
                    p += 3;
                }
            }
        }
        let dark = self.dark.iter().filter(|&&d| d).count() as u32;
        let total = (s * s) as u32;
        let k = ((dark * 20).abs_diff(total * 10)).div_ceil(total).saturating_sub(1);
        p + k * 10
    }
}

fn align_positions(v: usize) -> Vec<usize> {
    if v == 1 {
        return vec![];
    }
    let n = v / 7 + 2;
    let step = (v * 4 + n * 2 + 1) / (n * 2 - 2) * 2;
    let size = v * 4 + 17;
    let mut r: Vec<usize> = (0..n - 1).map(|i| size - 7 - i * step).collect();
    r.push(6);
    r.reverse();
    r
}
