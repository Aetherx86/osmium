//! Binary arithmetic coder (carry-less range coder over 32-bit bounds, as in lpaq).
//! Probabilities are 16-bit: `p` is P(bit = 1) * 65536, clamped to [1, 65535].

pub struct Encoder {
    x1: u32,
    x2: u32,
    out: Vec<u8>,
}

#[inline]
fn split(x1: u32, x2: u32, p: u32) -> u32 {
    let range = x2 - x1;
    x1 + ((range as u64 * p as u64) >> 16) as u32
}

impl Encoder {
    pub fn new(out: Vec<u8>) -> Self { Encoder { x1: 0, x2: u32::MAX, out } }

    #[inline]
    pub fn encode(&mut self, bit: u8, p: u32) {
        let xmid = split(self.x1, self.x2, p);
        if bit == 1 { self.x2 = xmid } else { self.x1 = xmid + 1 }
        while (self.x1 ^ self.x2) & 0xff00_0000 == 0 {
            self.out.push((self.x2 >> 24) as u8);
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 255;
        }
    }

    pub fn finish(mut self) -> Vec<u8> {
        // Any value in [x1, x2] decodes correctly; emit enough bytes of x1 to pin it.
        self.out.extend_from_slice(&self.x1.to_be_bytes());
        self.out
    }
}

pub struct Decoder<'a> {
    x1: u32,
    x2: u32,
    x: u32,
    data: &'a [u8],
    pos: usize,
}

impl<'a> Decoder<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        let mut d = Decoder { x1: 0, x2: u32::MAX, x: 0, data, pos: 0 };
        for _ in 0..4 { d.x = (d.x << 8) | d.next() as u32; }
        d
    }

    #[inline]
    fn next(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }

    /// True if decoding needed bytes beyond the end of the input (a truncated file).
    pub fn overrun(&self) -> bool { self.pos > self.data.len() + 4 }

    #[inline]
    pub fn decode(&mut self, p: u32) -> u8 {
        let xmid = split(self.x1, self.x2, p);
        let bit = (self.x <= xmid) as u8;
        if bit == 1 { self.x2 = xmid } else { self.x1 = xmid + 1 }
        while (self.x1 ^ self.x2) & 0xff00_0000 == 0 {
            self.x1 <<= 8;
            self.x2 = (self.x2 << 8) | 255;
            self.x = (self.x << 8) | self.next() as u32;
        }
        bit
    }
}
