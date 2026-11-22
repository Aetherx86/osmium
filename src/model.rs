//! Bit predictor: context models -> two gated mixers -> two SSE stages.
//! Probabilities inside the model are 12-bit (0..4095); `Predictor::p` returns 16-bit.

// ------------------------------------------------------------------ logistic helpers

const SQUASH_T: [i32; 33] = [
    1, 2, 3, 6, 10, 16, 27, 45, 73, 120, 194, 310, 488, 747, 1101, 1546, 2047, 2549, 2994, 3348, 3607,
    3785, 3901, 3975, 4022, 4050, 4068, 4079, 4085, 4089, 4092, 4093, 4094,
];

/// 1 / (1 + e^-x) with x in 1/256 units (clamped to +-2047), result in 12 bits.
#[inline]
fn squash(d: i32) -> i32 {
    if d > 2047 { return 4095; }
    if d < -2047 { return 1; }
    let w = d & 127;
    let i = ((d >> 7) + 16) as usize;
    (SQUASH_T[i] * (128 - w) + SQUASH_T[i + 1] * w + 64) >> 7
}

struct Stretch([i16; 4096]);

impl Stretch {
    fn new() -> Self {
        let mut t = [0i16; 4096];
        let mut pi = 0usize;
        for x in -2047..=2047 {
            let v = squash(x) as usize;
            for slot in t.iter_mut().take(v + 1).skip(pi) { *slot = x as i16; }
            pi = pi.max(v + 1);
        }
        for slot in t.iter_mut().skip(pi) { *slot = 2047; }
        Stretch(t)
    }
    #[inline]
    fn get(&self, p: i32) -> i32 { self.0[p as usize] as i32 }
}

// ------------------------------------------------------------------ adaptive counters

/// Probability (22 bits) and hit count (10 bits) packed in a u32. The update rate is
/// 1 / (n + 1.5) until `n` reaches the limit, so new contexts learn fast and old ones settle.
const COUNTER_INIT: u32 = 1 << 31;

struct Rates([u32; 1024]);

impl Rates {
    fn new() -> Self {
        let mut t = [0u32; 1024];
        for (n, r) in t.iter_mut().enumerate() { *r = (65536.0 * 2.0 / (2 * n as u32 + 3) as f64) as u32; }
        Rates(t)
    }
}

#[inline]
fn counter_p12(c: u32) -> i32 { (c >> 20) as i32 }

#[inline]
fn counter_update(c: &mut u32, bit: u8, limit: u32, rates: &Rates) {
    let n = *c & 1023;
    let p = (*c >> 10) as i64;
    let target = if bit == 1 { (1i64 << 22) - 1 } else { 0 };
    let p = p + (((target - p) * rates.0[n as usize] as i64) >> 16);
    *c = ((p as u32) << 10) | if n < limit { n + 1 } else { n };
}

// ------------------------------------------------------------------ bit histories

/// A bit history in one byte: counts of zeros (high nibble) and ones (low nibble), each
/// saturating at 15. On a bit, the opposite count is discounted (to 2 + half the excess), so
/// the history favours recent behaviour as in paq's nonstationary states.
struct NextState([[u8; 2]; 256]);

impl NextState {
    fn new() -> Self {
        let mut t = [[0u8; 2]; 256];
        for (st, row) in t.iter_mut().enumerate() {
            let (n0, n1) = ((st >> 4) as u8, (st & 15) as u8);
            let disc = |n: u8| if n > 5 { 5 + (n - 5) / 2 } else { n };
            row[0] = ((n0 + 1).min(15) << 4) | disc(n1);
            row[1] = (disc(n0) << 4) | (n1 + 1).min(15);
        }
        NextState(t)
    }
}

/// Adaptive map from bit history to probability, one per context model.
struct StateMap(Vec<u32>);

impl StateMap {
    fn new() -> Self {
        let mut t = vec![0u32; 256 * 8];
        for (i, c) in t.iter_mut().enumerate() {
            let st = i & 255;
            let (n0, n1) = ((st >> 4) as f64, (st & 15) as f64);
            let p = ((n1 + 0.4) / (n0 + n1 + 0.8) * (1 << 22) as f64) as u32;
            *c = p.min((1 << 22) - 1) << 10;
        }
        StateMap(t)
    }
}

// ------------------------------------------------------------------ hashed nibble slots

/// One context's 15 bit histories for a nibble (index 1..15 = partial nibble with a leading
/// 1) and an 8-bit check. Four slots share a cache line (4-way set associative).
#[derive(Clone, Copy)]
#[repr(C)]
struct Slot {
    check: u8,
    s: [u8; 15],
}

#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Bucket([Slot; 4]);

const EMPTY_BUCKET: Bucket = Bucket([Slot { check: 0, s: [0; 15] }; 4]);

struct SlotTable {
    buckets: Vec<Bucket>,
    mask: usize,
}

impl SlotTable {
    /// `bits` counts cache lines (as before); each holds four contexts.
    fn new(bits: u32) -> Self { SlotTable { buckets: vec![EMPTY_BUCKET; 1 << bits], mask: (1 << bits) - 1 } }

    /// (bucket, way) for `h` and whether the context was already present. A new context
    /// replaces the way whose first bit history has seen the fewest bits.
    #[inline]
    fn find(&mut self, h: u64) -> (usize, usize, bool) {
        let bi = h as usize & self.mask;
        let check = (h >> 56) as u8 | 1;
        let b = &mut self.buckets[bi].0;
        for (w, sl) in b.iter().enumerate() {
            if sl.check == check { return (bi, w, true); }
        }
        let total = |sl: &Slot| if sl.check == 0 { 0 } else { 1 + (sl.s[0] >> 4) as u32 + (sl.s[0] & 15) as u32 };
        let w = (0..4).min_by_key(|&w| total(&b[w])).unwrap();
        b[w] = Slot { check, s: [0; 15] };
        (bi, w, false)
    }
}

/// Per byte context: the byte that last followed it and how many times in a row (16-bit check).
struct RunTable {
    t: Vec<u32>,
    mask: usize,
}

impl RunTable {
    fn new(bits: u32) -> Self { RunTable { t: vec![0; 1 << bits], mask: (1 << bits) - 1 } }
    #[inline]
    fn index(&mut self, h: u64) -> usize {
        let i = h as usize & self.mask;
        let check = (h >> 48) as u32;
        if self.t[i] >> 16 != check { self.t[i] = check << 16; }
        i
    }
}

#[inline]
fn hash2(a: u64, b: u64) -> u64 {
    let mut h = a.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ b.wrapping_add(0x632B_E59B_D9B4_E019).wrapping_mul(0xD6E8_FEB8_6659_FD93);
    h ^= h >> 32;
    h = h.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^ (h >> 29)
}

// ------------------------------------------------------------------ mixer

/// Gated linear mixer in the logistic domain: one weight vector per selector value.
struct Mixer {
    n: usize,
    weights: Vec<i32>,
    set: usize,
    lr: i32,
}

impl Mixer {
    fn new(n: usize, sets: usize, lr: i32) -> Self {
        Mixer { n, weights: vec![(1 << 16) * 3 / (2 * n as i32); n * sets], set: 0, lr }
    }
    #[inline]
    fn mix(&mut self, x: &[i32], set: usize) -> i32 {
        self.set = set * self.n;
        let w = &self.weights[self.set..self.set + self.n];
        let dot: i64 = x.iter().zip(w).map(|(&a, &b)| a as i64 * b as i64).sum();
        (dot >> 16).clamp(-2047, 2047) as i32
    }
    #[inline]
    fn update(&mut self, x: &[i32], err: i32) {
        let e = err * self.lr;
        let w = &mut self.weights[self.set..self.set + self.n];
        for (wi, &xi) in w.iter_mut().zip(x) {
            *wi += (xi * e) >> 14;
        }
    }
}

// ------------------------------------------------------------------ SSE / APM

/// Adaptive probability map: refines a probability given a small context, interpolating
/// between 33 buckets over the stretched input.
struct Apm {
    t: Vec<u16>,
    idx: usize,
    rate: u32,
}

impl Apm {
    fn new(contexts: usize, rate: u32) -> Self {
        let mut t = vec![0u16; contexts * 33];
        for c in 0..contexts {
            for j in 0..33 { t[c * 33 + j] = (squash((j as i32 - 16) * 128) * 16) as u16; }
        }
        Apm { t, idx: 0, rate }
    }
    #[inline]
    fn pp(&mut self, st: &Stretch, p: i32, cx: usize) -> i32 {
        let s = st.get(p) + 2048;
        let (lo, w) = ((s >> 7) as usize, s & 127);
        let base = cx * 33 + lo;
        self.idx = base + ((w >> 6) as usize);
        ((self.t[base] as i32 * (128 - w) + self.t[base + 1] as i32 * w) >> 11).clamp(1, 4095)
    }
    #[inline]
    fn update(&mut self, bit: u8) {
        let g = ((bit as i32) << 16) + ((bit as i32) << self.rate) - bit as i32 - bit as i32;
        let v = &mut self.t[self.idx];
        *v = (*v as i32 + ((g - *v as i32) >> self.rate)) as u16;
    }
}

// ------------------------------------------------------------------ predictor

const N_CTX: usize = 16;         // hashed context models
const N_IN: usize = 3 * N_CTX + 3;   // two per context + match model (2 inputs) + bias
const MATCH_MIN: usize = 7;

pub struct Predictor {
    st: Stretch,
    rates: Rates,
    tables: Vec<SlotTable>,
    runs: Vec<RunTable>,
    maps: Vec<StateMap>,
    next: NextState,
    way: [usize; N_CTX],
    run_at: [usize; N_CTX],
    ctx: [u64; N_CTX],
    slot: [usize; N_CTX],
    state: [u8; N_CTX],
    seen: u32,
    m4: Mixer,
    pr4: i32,
    run_ctr: Vec<u32>,
    run_idx: [usize; N_CTX],
    // byte state
    c0: u32,          // partial byte with a leading 1
    nib: usize,       // partial nibble with a leading 1
    bitpos: u32,
    c4: u64,          // last 8 bytes, most recent in the low byte
    word: u64,
    prev_word: u64,
    prev_word2: u64,
    col: u64,
    ind1: Vec<u16>,  // last two bytes that followed each byte
    ind2: Vec<u16>,  // ... and each pair of bytes        // bytes since the last newline
    markup: u64,     // innermost open wiki/XML delimiter and the line's first byte
    // match model
    buf: Vec<u8>,
    match_tab: Vec<u32>,
    match_bits: u32,
    match_ptr: usize,
    match_len: usize,
    match_byte: u32,
    match_ctr: [u32; 64],
    match_idx: usize,
    // mixing
    x: [i32; N_IN],
    m1: Mixer,
    m2: Mixer,
    m3: Mixer,
    mf: Mixer,
    xf: [i32; 5],
    pr_mix: i32,
    pr1: i32,
    pr2: i32,
    pr3: i32,
    a1: Apm,
    a2: Apm,
    a3: Apm,
    a4: Apm,
    pr: i32,
}

impl Predictor {
    /// `n` is the input length; table sizes grow with it (and are part of the format).
    pub fn new(n: usize) -> Self {
        let big = if n > 256 << 20 { 22 } else if n > 32 << 20 { 21 } else if n > 4 << 20 { 20 } else { 18 };
        let bits = [12, 16, big - 2, big, big, big, big, big - 1, big, big - 2, big - 2, big - 1, big, big - 2, big - 2, big];
        let match_bits = (big + 2).min(24);
        Predictor {
            st: Stretch::new(),
            rates: Rates::new(),
            tables: bits.iter().map(|&b| SlotTable::new(b)).collect(),
            runs: bits.iter().map(|&b| RunTable::new(b + 2)).collect(),
            maps: (0..N_CTX).map(|_| StateMap::new()).collect(),
            next: NextState::new(),
            way: [0; N_CTX],
            run_at: [0; N_CTX],
            ctx: [0; N_CTX],
            slot: [0; N_CTX],
            state: [0; N_CTX],
            seen: 0,
            m4: Mixer::new(N_IN, 256, 3),
            pr4: 2048,
            run_ctr: vec![COUNTER_INIT; N_CTX * 64],
            run_idx: [usize::MAX; N_CTX],
            c0: 1,
            nib: 1,
            bitpos: 0,
            c4: 0,
            word: 0,
            prev_word: 0,
            prev_word2: 0,
            markup: 0,
            col: 0,
            ind1: vec![0; 256],
            ind2: vec![0; 65536],
            buf: Vec::with_capacity(n),
            match_tab: vec![0; 1 << match_bits],
            match_bits,
            match_ptr: 0,
            match_len: 0,
            match_byte: 0,
            match_ctr: [COUNTER_INIT; 64],
            match_idx: 0,
            x: [0; N_IN],
            m1: Mixer::new(N_IN, 256, 3),
            m2: Mixer::new(N_IN, 2048, 3),
            m3: Mixer::new(N_IN, 256 * 8, 3),
            mf: Mixer::new(5, 256, 4),
            xf: [0; 5],
            pr_mix: 2048,
            pr1: 2048,
            pr2: 2048,
            pr3: 2048,
            a1: Apm::new(256, 7),
            a2: Apm::new(65536, 7),
            a3: Apm::new(1 << 20, 7),
            a4: Apm::new(64 * 256, 7),
            pr: 2048,
        }
        .started()
    }

    fn started(mut self) -> Self {
        self.new_byte_contexts();
        self.find_slots();
        self.predict();
        self
    }

    /// P(next bit = 1) scaled to 16 bits, in [1, 65535].
    #[inline]
    pub fn p(&self) -> u32 { ((self.pr as u32) << 4).clamp(1, 65535) }

    fn new_byte_contexts(&mut self) {
        let c = self.c4;
        self.ctx = [
            0,
            hash2(1, c & 0xff),
            hash2(2, c & 0xffff),
            hash2(3, c & 0xff_ffff),
            hash2(4, c & 0xffff_ffff),
            hash2(5, c & 0xffff_ffff_ffff),
            hash2(6, self.word),
            hash2(7, hash2(self.word, self.prev_word)),
            hash2(8, c),
            hash2(9, hash2(hash2(self.word, self.prev_word), self.prev_word2)),
            hash2(10, hash2(self.markup, c & 0xffff)),
            hash2(11, hash2(self.word, c & 0xff)),
            hash2(12, c & 0xff_ffff_ffff),
            hash2(13, hash2(self.col.min(255), c & 0xff)),
            hash2(14, (c & 0xff) | (self.ind1[(c & 0xff) as usize] as u64) << 8),
            hash2(15, (c & 0xffff) | (self.ind2[(c & 0xffff) as usize] as u64) << 16),
        ];
    }

    fn find_slots(&mut self) {
        for i in 0..N_CTX {
            let (b, w, seen) = self.tables[i].find(hash2(self.ctx[i], self.c0 as u64));
            self.slot[i] = b;
            self.way[i] = w;
            if seen { self.seen |= 1 << i } else { self.seen &= !(1 << i) }
        }
        if self.bitpos == 0 {
            for i in 0..N_CTX { self.run_at[i] = self.runs[i].index(hash2(self.ctx[i], 0x52_55_4e)); }
        }
    }

    fn match_bucket(&self) -> usize {
        let l = self.match_len;
        if l == 0 { 0 } else if l < 16 { l } else { (16 + (l - 16) / 4).min(31) }
    }

    fn predict(&mut self) {
        for i in 0..N_CTX {
            let state = self.tables[i].buckets[self.slot[i]].0[self.way[i]].s[self.nib - 1];
            self.state[i] = state;
            let p = counter_p12(self.maps[i].0[self.bitpos as usize * 256 + state as usize]);
            self.x[2 * i] = self.st.get(p);
            // a second view: how one-sided the history is (deterministic contexts)
            let (n0, n1) = ((state >> 4) as i32, (state & 15) as i32);
            self.x[2 * i + 1] = if n0 == 0 && n1 > 0 { 64 * n1 } else if n1 == 0 && n0 > 0 { -64 * n0 } else { 0 };
        }
        // match model: does the byte at the match pointer still agree with this byte so far?
        let expected = if self.match_len > 0 && (self.match_byte + 256) >> (8 - self.bitpos) == self.c0 {
            Some((self.match_byte >> (7 - self.bitpos)) & 1)
        } else {
            None
        };
        let (mi, mx) = match expected {
            Some(b) => {
                self.match_idx = self.match_bucket() * 2 + b as usize;
                let p = self.st.get(counter_p12(self.match_ctr[self.match_idx]));
                let conf = (self.match_bucket() as i32 * 64).min(2047);
                (p, if b == 1 { conf } else { -conf })
            }
            None => { self.match_idx = usize::MAX; (0, 0) }
        };
        // run model: the byte that last followed each context, if it still agrees with this byte
        for i in 0..N_CTX {
            let r = self.runs[i].t[self.run_at[i]];
            let (rb, rc) = ((r >> 8) & 0xff, (r & 0xff) as usize);
            self.x[2 * N_CTX + 2 + i] = if rc > 0 && (rb + 256) >> (8 - self.bitpos) == self.c0 {
                let eb = ((rb >> (7 - self.bitpos)) & 1) as usize;
                self.run_idx[i] = i * 64 + rc.min(31) * 2 + eb;
                self.st.get(counter_p12(self.run_ctr[self.run_idx[i]]))
            } else {
                self.run_idx[i] = usize::MAX;
                0
            };
        }
        self.x[2 * N_CTX] = mi;
        self.x[2 * N_CTX + 1] = mx;
        self.x[3 * N_CTX + 2] = 256;
        let sel2 = (if expected.is_some() { self.match_bucket() } else { 0 }) << 6
            | (self.bitpos as usize) << 3
            | (self.c4 as usize >> 5 & 7);
        let s1 = self.m1.mix(&self.x, self.c0 as usize);
        let s2 = self.m2.mix(&self.x, sel2);
        let s3 = self.m3.mix(&self.x, (self.c4 as usize & 0xff) << 3 | self.bitpos as usize);
        self.pr1 = squash(s1);
        self.pr2 = squash(s2);
        self.pr3 = squash(s3);
        // orders 2, 3, 4, 5 and 6 present in the tables, with the bit position
        let sel4 = ((self.seen >> 2 & 7) | (self.seen >> 9 & 8) | (self.seen >> 1 & 16)) as usize;
        let s4 = self.m4.mix(&self.x, sel4 << 3 | self.bitpos as usize);
        self.pr4 = squash(s4);
        self.xf = [s1, s2, s3, s4, 256];
        self.pr_mix = squash(self.mf.mix(&self.xf, self.c0 as usize));
        let p1 = self.a1.pp(&self.st, self.pr_mix, self.c0 as usize);
        let h = (self.c0 as usize | ((self.c4 as usize & 0xff) << 8)) & 0xffff;
        let p2 = self.a2.pp(&self.st, self.pr_mix, h);
        let h3 = (hash2(self.c4 & 0xffff, self.c0 as u64) >> 44) as usize;
        let p3 = self.a3.pp(&self.st, self.pr_mix, h3);
        // match state: length bucket and expected bit (or none), with the partial byte
        let ms = if self.match_idx == usize::MAX { 0 } else { self.match_idx + 1 };
        let p4 = self.a4.pp(&self.st, self.pr_mix, (ms.min(63) << 8) | self.c0 as usize);
        self.pr = ((self.pr_mix + 2 * p1 + 2 * p2 + 2 * p3 + p4 + 4) >> 3).clamp(1, 4095);
    }

    pub fn update(&mut self, bit: u8) {
        // learn
        let nib = self.nib - 1;
        for i in 0..N_CTX {
            let s = self.slot[i];
            let st = self.state[i];
            counter_update(&mut self.maps[i].0[self.bitpos as usize * 256 + st as usize], bit, 1023, &self.rates);
            self.tables[i].buckets[s].0[self.way[i]].s[nib] = self.next.0[st as usize][bit as usize];
        }
        for i in 0..N_CTX {
            if self.run_idx[i] != usize::MAX { counter_update(&mut self.run_ctr[self.run_idx[i]], bit, 255, &self.rates); }
        }
        if self.match_idx != usize::MAX {
            counter_update(&mut self.match_ctr[self.match_idx], bit, 1023, &self.rates);
        }
        let y = (bit as i32) << 12;
        self.m1.update(&self.x, y - self.pr1);
        self.m2.update(&self.x, y - self.pr2);
        self.m3.update(&self.x, y - self.pr3);
        self.m4.update(&self.x, y - self.pr4);
        let xf = self.xf;
        self.mf.update(&xf, y - self.pr_mix);
        self.a1.update(bit);
        self.a2.update(bit);
        self.a3.update(bit);
        self.a4.update(bit);

        // advance bit state
        self.c0 = (self.c0 << 1) | bit as u32;
        self.nib = (self.nib << 1) | bit as usize;
        self.bitpos += 1;
        if self.bitpos == 8 {
            self.end_byte((self.c0 & 0xff) as u8);
            self.c0 = 1;
            self.nib = 1;
            self.bitpos = 0;
            self.new_byte_contexts();
            self.find_slots();
        } else if self.bitpos == 4 {
            self.nib = 1;
            self.find_slots();
        }
        self.predict();
    }

    fn end_byte(&mut self, b: u8) {
        let (i1, i2) = ((self.c4 & 0xff) as usize, (self.c4 & 0xffff) as usize);
        self.ind1[i1] = self.ind1[i1] << 8 | b as u16;
        self.ind2[i2] = self.ind2[i2] << 8 | b as u16;
        for i in 0..N_CTX {
            let r = &mut self.runs[i].t[self.run_at[i]];
            let (rb, rc) = ((*r >> 8) & 0xff, *r & 0xff);
            *r = (*r & 0xffff_0000) | if rc > 0 && rb == b as u32 { rb << 8 | (rc + 1).min(255) } else { (b as u32) << 8 | 1 };
        }
        self.c4 = (self.c4 << 8) | b as u64;
        // words: letters (case-folded) and digits; anything else ends the word
        let lower = b.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || b >= 128 {
            self.word = hash2(self.word, lower as u64);
        } else if self.word != 0 {
            self.prev_word2 = self.prev_word;
            self.prev_word = self.word;
            self.word = 0;
        }
        self.col = if b == b'\n' { 0 } else { self.col + 1 };
        let (open, line_first) = (self.markup & 0xff, self.markup >> 8 & 0xff);
        self.markup = match b {
            b'\n' => 0,
            b'[' | b'{' | b'<' | b'(' | b'|' | b'=' | b'\'' => (line_first << 8) | b as u64,
            b']' | b'}' | b'>' | b')' => line_first << 8,
            _ => (line_first << 8) | open,
        };
        if self.markup >> 8 == 0 && b != b'\n' && (self.c4 >> 8) & 0xff == b'\n' as u64 { self.markup |= (b as u64) << 8; }
        // match model
        self.buf.push(b);
        let pos = self.buf.len();
        if self.match_len > 0 {
            if self.match_byte as u8 == b {
                self.match_len = (self.match_len + 1).min(65535);
                self.match_ptr += 1;
            } else {
                self.match_len = 0;
            }
        }
        if pos >= MATCH_MIN {
            let h = (hash2(0x4d41_5443, u64::from_le_bytes({
                let mut a = [0u8; 8];
                a[..MATCH_MIN].copy_from_slice(&self.buf[pos - MATCH_MIN..pos]);
                a
            })) >> (64 - self.match_bits)) as usize;
            if self.match_len == 0 {
                let cand = self.match_tab[h] as usize;
                if cand > 0 {
                    let mut l = 0;
                    while l < 64 && l < cand && self.buf[cand - 1 - l] == self.buf[pos - 1 - l] { l += 1; }
                    if l >= MATCH_MIN {
                        self.match_len = l;
                        self.match_ptr = cand;
                    }
                }
            }
            self.match_tab[h] = pos as u32;
        }
        self.match_byte = if self.match_len > 0 { self.buf[self.match_ptr] as u32 } else { 0 };
    }
}
