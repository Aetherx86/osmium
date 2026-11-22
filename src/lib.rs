//! Osmium: a context-mixing compressor for text.
//!
//! Every bit is predicted by several models (order-0 to order-6 byte contexts, two word
//! contexts and a long-range match model), their predictions are combined by a gated
//! logistic mixer, refined by two SSE stages and fed to a binary arithmetic coder.
//! All arithmetic is integer, so compression and decompression agree bit for bit on any
//! platform.

mod coder;
mod model;

pub use coder::{Decoder, Encoder};
pub use model::Predictor;

/// File header: magic, format version, then the original length as a little-endian u64.
pub const MAGIC: &[u8; 4] = b"OSM\x02";

/// Compress `input`, calling `progress(done_bytes)` every 4 MiB.
pub fn compress(input: &[u8], mut progress: impl FnMut(usize)) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() / 4 + 64);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&(input.len() as u64).to_le_bytes());
    let mut p = Predictor::new(input.len());
    let mut enc = Encoder::new(out);
    for (i, &byte) in input.iter().enumerate() {
        for k in (0..8).rev() {
            let bit = (byte >> k) & 1;
            enc.encode(bit, p.p());
            p.update(bit);
        }
        if i & 0x3f_ffff == 0 { progress(i); }
    }
    enc.finish()
}

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    NotOsmium,
    Truncated,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Error::NotOsmium => "not an Osmium file (bad header)",
            Error::Truncated => "the file is truncated",
        })
    }
}

/// Decompress a buffer produced by [`compress`].
pub fn decompress(data: &[u8], mut progress: impl FnMut(usize)) -> Result<Vec<u8>, Error> {
    if data.len() < 12 || &data[..4] != MAGIC { return Err(Error::NotOsmium); }
    let n = u64::from_le_bytes(data[4..12].try_into().unwrap());
    // A corrupt length must not make us allocate absurd amounts: every input byte costs
    // at least a little output, and no real input compresses better than ~1:4000 here.
    if n > (data.len() as u64).saturating_mul(1 << 16) + (1 << 20) { return Err(Error::Truncated); }
    let n = n as usize;
    let mut out = Vec::with_capacity(n);
    let mut p = Predictor::new(n);
    let mut dec = Decoder::new(&data[12..]);
    for i in 0..n {
        let mut c = 0u8;
        for _ in 0..8 {
            let bit = dec.decode(p.p());
            p.update(bit);
            c = (c << 1) | bit;
        }
        out.push(c);
        if i & 0x3f_ffff == 0 { progress(i); }
    }
    if dec.overrun() { return Err(Error::Truncated); }
    Ok(out)
}
