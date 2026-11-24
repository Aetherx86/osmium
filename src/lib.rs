//! Osmium: a context-mixing compressor for text.
//!
//! Every bit is predicted by 19 hashed context models, a run model, and short/long
//! match models, combined by four gated logistic mixers and a final mixer, refined by
//! four SSE/APM stages and a learned SSE mixer, and coded with a binary arithmetic coder.
//! All arithmetic is integer, so compression and decompression agree bit for bit on any
//! platform.
//!
//! Two archive formats are supported:
//! - **`OSM\x02`** (v0.2.0 format, emitted by [`compress`]): minimal 12-byte header plus
//!   raw arithmetic bitstream, kept 100% bit-for-bit compatible with v0.2.0.
//! - **`OSM\x03`** (v0.3.0 framed format, emitted by [`compress_with_options`] and
//!   [`compress_stream`]): configurable memory presets (`level` 1..=7 or explicit
//!   `table_bits` 12..=23), optional reversible UTF-8/case/markup preprocessor,
//!   independent blocks for streaming and multi-threaded coding, per-block CRC-32C,
//!   and whole-stream SHA-256 verification.

pub mod checksum;
mod coder;
mod model;
pub mod transform;

use std::io::{Read, Write};

pub use coder::{Decoder, Encoder};
pub use model::{auto_table_bits, Predictor, MAX_TABLE_BITS, MIN_TABLE_BITS};

/// Crate version string.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// v0.2.0 file header magic: `OSM\x02` followed by original length as little-endian u64.
pub const MAGIC: &[u8; 4] = b"OSM\x02";

/// v0.3.0 framed archive header magic.
pub const MAGIC_V3: &[u8; 4] = b"OSM\x03";

/// Default streaming block size (4 MiB) when `Options::block_size == 0` in stream mode.
pub const DEFAULT_STREAM_BLOCK_SIZE: usize = 4 << 20;

/// Maximum uncompressed block size accepted in `OSM\x03` (256 MiB).
pub const MAX_BLOCK_SIZE: usize = 256 << 20;

const V3_HEADER_LEN: usize = 20;
const V3_BLOCK_HEADER_LEN: usize = 16;
const V3_TRAILER_LEN: usize = 40; // u64 total_len + [u8; 32] sha256

const FLAG_TEXT_TRANSFORM: u8 = 0x01;
const FLAG_KNOWN_LEN: u8 = 0x02;
const FLAGS_MASK: u8 = FLAG_TEXT_TRANSFORM | FLAG_KNOWN_LEN;

/// Archive container format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Legacy v0.2.0 raw format (`OSM\x02`).
    V2,
    /// Framed v0.3.0 format (`OSM\x03`) with presets, optional transform, blocks, CRC-32C and SHA-256.
    V3,
}

/// Compression and analysis configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    /// Archive format to write (`V2` or `V3`).
    pub format: Format,
    /// Memory preset: `0` = auto (v0.2.0 schedule), `1..=7` = compact (~7 MiB) to enwik9 (~9 GiB).
    pub level: u8,
    /// Explicit context table exponent (`12..=23`), overriding `level` when set.
    pub table_bits: Option<u8>,
    /// Maximum uncompressed bytes per block in `V3` (`0` = single block in slice mode,
    /// or [`DEFAULT_STREAM_BLOCK_SIZE`] in streaming mode).
    pub block_size: usize,
    /// Worker threads for multi-block `V3` (`0` = auto, `1` = single-threaded).
    pub threads: usize,
    /// Enable the reversible UTF-8/case/markup preprocessor (`V3` only).
    pub text_transform: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            format: Format::V3,
            level: 0,
            table_bits: None,
            block_size: 0,
            threads: 1,
            text_transform: false,
        }
    }
}

impl Options {
    /// Options matching v0.2.0 (`OSM\x02`, auto table size, raw bytes).
    pub fn v2() -> Self {
        Self {
            format: Format::V2,
            ..Self::default()
        }
    }

    /// Options for `OSM\x03` with a given memory preset (`0` = auto, `1..=7`).
    pub fn v3(level: u8) -> Self {
        Self {
            format: Format::V3,
            level,
            ..Self::default()
        }
    }

    /// Resolve the effective `table_bits` exponent and validate options.
    pub fn resolve_table_bits(&self, size_hint: usize) -> Result<u32, Error> {
        if let Some(tb) = self.table_bits {
            let tb = tb as u32;
            if !(MIN_TABLE_BITS..=MAX_TABLE_BITS).contains(&tb) {
                return Err(Error::InvalidOptions("table_bits must be in 12..=23"));
            }
            return Ok(tb);
        }
        level_to_table_bits(self.level, size_hint)
    }

    fn validate_for_compress(&self) -> Result<(), Error> {
        if self.block_size > MAX_BLOCK_SIZE {
            return Err(Error::InvalidOptions(
                "block_size exceeds MAX_BLOCK_SIZE (256 MiB)",
            ));
        }
        if self.format == Format::V2 {
            if self.text_transform {
                return Err(Error::InvalidOptions(
                    "text_transform requires OSM v3 format",
                ));
            }
            if self.block_size != 0 {
                return Err(Error::InvalidOptions("block_size requires OSM v3 format"));
            }
            if self.level != 0 || self.table_bits.is_some() {
                return Err(Error::InvalidOptions(
                    "custom level/table_bits requires OSM v3 format",
                ));
            }
        }
        let _ = self.resolve_table_bits(0)?;
        Ok(())
    }
}

/// Map preset `level` (`0` = auto, `1..=7`) to context table exponent (`12..=23`).
pub fn level_to_table_bits(level: u8, size_hint: usize) -> Result<u32, Error> {
    match level {
        0 => Ok(auto_table_bits(size_hint)),
        1 => Ok(13),
        2 => Ok(15),
        3 => Ok(17),
        4 => Ok(18),
        5 => Ok(20),
        6 => Ok(21),
        7 => Ok(23),
        _ => Err(Error::InvalidOptions("level must be 0 (auto) or 1..=7")),
    }
}

/// Errors returned by decompression, inspection, verification, and streaming operations.
#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    NotOsmium,
    Truncated,
    CorruptHeader,
    ChecksumMismatch,
    CorruptTransform,
    TrailingData,
    InvalidOptions(&'static str),
    Io(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotOsmium => f.write_str("not an Osmium file (bad header)"),
            Error::Truncated => f.write_str("the file is truncated"),
            Error::CorruptHeader => f.write_str("corrupt or unsupported Osmium v3 header"),
            Error::ChecksumMismatch => {
                f.write_str("integrity check failed (CRC-32C or SHA-256 mismatch)")
            }
            Error::CorruptTransform => f.write_str("corrupt transformed text stream"),
            Error::TrailingData => f.write_str("unexpected trailing data after archive trailer"),
            Error::InvalidOptions(msg) => write!(f, "invalid options: {msg}"),
            Error::Io(msg) => write!(f, "I/O error: {msg}"),
        }
    }
}

impl std::error::Error for Error {}

/// Per-block metadata reported by [`inspect`] and [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockInfo {
    pub index: usize,
    pub raw_len: u32,
    pub coded_len: u32,
    pub comp_len: u32,
    pub crc32c: u32,
}

/// Archive metadata reported by [`inspect`] and [`verify`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveInfo {
    pub format: Format,
    pub original_len: u64,
    pub compressed_len: u64,
    pub table_bits: u32,
    pub text_transform: bool,
    pub block_size: u32,
    pub blocks: Vec<BlockInfo>,
    pub sha256: Option<[u8; 32]>,
    pub verified: bool,
}

impl ArchiveInfo {
    /// Compressed bits per uncompressed byte (`0.0` for empty inputs).
    pub fn bits_per_byte(&self) -> f64 {
        if self.original_len == 0 {
            0.0
        } else {
            self.compressed_len as f64 * 8.0 / self.original_len as f64
        }
    }

    /// Compression ratio `original / compressed` (`0.0` if `compressed == 0`).
    pub fn ratio(&self) -> f64 {
        if self.compressed_len == 0 {
            0.0
        } else {
            self.original_len as f64 / self.compressed_len as f64
        }
    }

    /// Serialize metadata as a JSON object string.
    pub fn to_json(&self) -> String {
        let fmt = match self.format {
            Format::V2 => "OSM2",
            Format::V3 => "OSM3",
        };
        let sha = match &self.sha256 {
            Some(d) => format!("\"{}\"", checksum::hex_digest(d)),
            None => "null".to_string(),
        };
        let mut blocks_json = String::from("[");
        for (i, b) in self.blocks.iter().enumerate() {
            if i > 0 {
                blocks_json.push(',');
            }
            blocks_json.push_str(&format!(
                "{{\"index\":{},\"raw_len\":{},\"coded_len\":{},\"comp_len\":{},\"crc32c\":\"{:08x}\"}}",
                b.index, b.raw_len, b.coded_len, b.comp_len, b.crc32c
            ));
        }
        blocks_json.push(']');
        format!(
            "{{\"format\":\"{fmt}\",\"original_len\":{},\"compressed_len\":{},\"bits_per_byte\":{:.6},\"ratio\":{:.6},\"table_bits\":{},\"text_transform\":{},\"block_size\":{},\"block_count\":{},\"sha256\":{sha},\"verified\":{},\"blocks\":{blocks_json}}}",
            self.original_len,
            self.compressed_len,
            self.bits_per_byte(),
            self.ratio(),
            self.table_bits,
            self.text_transform,
            self.block_size,
            self.blocks.len(),
            self.verified,
        )
    }
}

/// Summary statistics returned by [`compress_stream`] and [`decompress_stream`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamStats {
    pub input_bytes: u64,
    pub output_bytes: u64,
    pub blocks: usize,
    pub sha256: Option<[u8; 32]>,
}

/// Per-stage cross-entropy (bits per uncompressed input byte).
#[derive(Debug, Clone, PartialEq)]
pub struct StageEntropy {
    pub mixer1: f64,
    pub mixer2: f64,
    pub mixer3: f64,
    pub mixer4: f64,
    pub mixer_final: f64,
    pub apm1: f64,
    pub apm2: f64,
    pub apm3: f64,
    pub apm4: f64,
    pub sse_mixer: f64,
    pub final_pred: f64,
}

/// Information-theoretic diagnostics produced by [`analyze`].
#[derive(Debug, Clone, PartialEq)]
pub struct AnalysisReport {
    pub input_bytes: usize,
    pub coded_bytes: usize,
    pub compressed_bytes: usize,
    pub table_bits: u32,
    pub text_transform: bool,
    pub shannon_bpb: f64,
    pub coded_bpb: f64,
    pub stages: StageEntropy,
    pub short_match_hit_rate: f64,
    pub short_match_avg_len: f64,
    pub long_match_hit_rate: f64,
    pub long_match_avg_len: f64,
    pub context_hit_rates: Vec<(String, f64)>,
}

impl AnalysisReport {
    /// Serialize the analysis report as a JSON object string.
    pub fn to_json(&self) -> String {
        let mut ctx_json = String::from("{");
        for (i, (name, rate)) in self.context_hit_rates.iter().enumerate() {
            if i > 0 {
                ctx_json.push(',');
            }
            ctx_json.push_str(&format!("\"{name}\":{rate:.6}"));
        }
        ctx_json.push('}');
        let s = &self.stages;
        format!(
            "{{\"input_bytes\":{},\"coded_bytes\":{},\"compressed_bytes\":{},\"table_bits\":{},\"text_transform\":{},\"shannon_bpb\":{:.6},\"coded_bpb\":{:.6},\"stages\":{{\"mixer1\":{:.6},\"mixer2\":{:.6},\"mixer3\":{:.6},\"mixer4\":{:.6},\"mixer_final\":{:.6},\"apm1\":{:.6},\"apm2\":{:.6},\"apm3\":{:.6},\"apm4\":{:.6},\"sse_mixer\":{:.6},\"final_pred\":{:.6}}},\"short_match\":{{\"hit_rate\":{:.6},\"avg_len\":{:.4}}},\"long_match\":{{\"hit_rate\":{:.6},\"avg_len\":{:.4}}},\"context_hit_rates\":{ctx_json}}}",
            self.input_bytes,
            self.coded_bytes,
            self.compressed_bytes,
            self.table_bits,
            self.text_transform,
            self.shannon_bpb,
            self.coded_bpb,
            s.mixer1,
            s.mixer2,
            s.mixer3,
            s.mixer4,
            s.mixer_final,
            s.apm1,
            s.apm2,
            s.apm3,
            s.apm4,
            s.sse_mixer,
            s.final_pred,
            self.short_match_hit_rate,
            self.short_match_avg_len,
            self.long_match_hit_rate,
            self.long_match_avg_len,
        )
    }
}

// ------------------------------------------------------------------ core raw block coding

fn encode_raw_block(coded: &[u8], table_bits: u32, mut progress: impl FnMut(usize)) -> Vec<u8> {
    let mut p = Predictor::with_table_bits(coded.len(), table_bits);
    let mut enc = Encoder::new(Vec::with_capacity(coded.len() / 4 + 32));
    for (i, &byte) in coded.iter().enumerate() {
        for k in (0..8).rev() {
            let bit = (byte >> k) & 1;
            enc.encode(bit, p.p());
            p.update(bit);
        }
        if i & 0x3f_ffff == 0 {
            progress(i);
        }
    }
    enc.finish()
}

fn decode_raw_block(
    payload: &[u8],
    coded_len: usize,
    table_bits: u32,
    mut progress: impl FnMut(usize),
) -> Result<Vec<u8>, Error> {
    if payload.len() < 4 {
        return Err(Error::Truncated);
    }
    // Anti-bomb ratio guard: no real input compresses better than ~1:65536 here.
    if (coded_len as u64) > (payload.len() as u64).saturating_mul(1 << 16) + (1 << 20) {
        return Err(Error::Truncated);
    }
    let init_cap = coded_len.min(payload.len().saturating_mul(8).max(4096));
    let mut out = Vec::with_capacity(init_cap);
    let mut p = Predictor::with_table_bits(coded_len, table_bits);
    let mut dec = Decoder::new(payload);
    for i in 0..coded_len {
        let mut c = 0u8;
        for _ in 0..8 {
            let bit = dec.decode(p.p());
            p.update(bit);
            c = (c << 1) | bit;
        }
        out.push(c);
        if i & 0x3f_ffff == 0 {
            progress(i);
        }
    }
    if dec.overrun() {
        return Err(Error::Truncated);
    }
    Ok(out)
}

fn encode_v3_block(raw: &[u8], table_bits: u32, text_transform: bool) -> (u32, u32, u32, Vec<u8>) {
    let crc = checksum::crc32c(raw);
    let (coded_len, payload) = if text_transform {
        let transformed = transform::encode(raw);
        let payload = encode_raw_block(&transformed, table_bits, |_| {});
        (transformed.len() as u32, payload)
    } else {
        let payload = encode_raw_block(raw, table_bits, |_| {});
        (raw.len() as u32, payload)
    };
    (raw.len() as u32, coded_len, crc, payload)
}

fn decode_v3_block(
    raw_len: usize,
    coded_len: usize,
    expected_crc: u32,
    payload: &[u8],
    table_bits: u32,
    text_transform: bool,
) -> Result<Vec<u8>, Error> {
    if text_transform {
        let max_coded = raw_len.saturating_mul(2).saturating_add(16);
        if coded_len > max_coded {
            return Err(Error::CorruptHeader);
        }
    } else if coded_len != raw_len {
        return Err(Error::CorruptHeader);
    }
    let decoded = decode_raw_block(payload, coded_len, table_bits, |_| {})?;
    let raw = if text_transform {
        transform::decode(&decoded, raw_len)?
    } else {
        decoded
    };
    if raw.len() != raw_len {
        return Err(Error::CorruptTransform);
    }
    if checksum::crc32c(&raw) != expected_crc {
        return Err(Error::ChecksumMismatch);
    }
    Ok(raw)
}

fn effective_threads(requested: usize, work_items: usize) -> usize {
    if work_items <= 1 {
        return 1;
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = requested;
        1
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let n = if requested == 0 {
            std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(1)
        } else {
            requested
        };
        n.max(1).min(work_items)
    }
}

// ------------------------------------------------------------------ v0.2.0 compatible API

/// Compress `input` into the v0.2.0 (`OSM\x02`) format, calling `progress(done_bytes)` every 4 MiB.
///
/// Output is bit-for-bit identical to Osmium 0.2.0.
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
        if i & 0x3f_ffff == 0 {
            progress(i);
        }
    }
    enc.finish()
}

/// Decompress a buffer produced by [`compress`] (`OSM\x02`) or [`compress_with_options`] (`OSM\x03`).
pub fn decompress(data: &[u8], progress: impl FnMut(usize)) -> Result<Vec<u8>, Error> {
    decompress_with_threads(data, 1, progress)
}

/// Decompress an `OSM\x02` or `OSM\x03` buffer, decoding multi-block `OSM\x03` archives
/// with up to `threads` worker threads (`0` = auto).
pub fn decompress_with_threads(
    data: &[u8],
    threads: usize,
    progress: impl FnMut(usize),
) -> Result<Vec<u8>, Error> {
    if data.len() < 4 {
        return Err(Error::NotOsmium);
    }
    match &data[..4] {
        b"OSM\x02" => decompress_v2(data, progress),
        b"OSM\x03" => decompress_v3(data, threads, progress),
        _ => Err(Error::NotOsmium),
    }
}

fn decompress_v2(data: &[u8], mut progress: impl FnMut(usize)) -> Result<Vec<u8>, Error> {
    if data.len() < 12 {
        return Err(Error::NotOsmium);
    }
    let n_u64 = u64::from_le_bytes(data[4..12].try_into().unwrap());
    // A corrupt length must not make us allocate absurd amounts: every input byte costs
    // at least a little output, and no real input compresses better than ~1:65536 here.
    if n_u64 > (data.len() as u64).saturating_mul(1 << 16) + (1 << 20) {
        return Err(Error::Truncated);
    }
    // Encoder::finish always emits 4 flush bytes after the 12-byte header.
    if data.len() < 16 {
        return Err(Error::Truncated);
    }
    let n = usize::try_from(n_u64).map_err(|_| Error::Truncated)?;
    let init_cap = n.min(data.len().saturating_mul(8).max(4096));
    let mut out = Vec::with_capacity(init_cap);
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
        if i & 0x3f_ffff == 0 {
            progress(i);
        }
    }
    if dec.overrun() {
        return Err(Error::Truncated);
    }
    Ok(out)
}

// ------------------------------------------------------------------ v0.3.0 framed API

fn write_v3_header(
    out: &mut Vec<u8>,
    flags: u8,
    table_bits: u32,
    original_len: u64,
    block_size: u32,
) {
    out.extend_from_slice(MAGIC_V3);
    out.push(flags);
    out.push(table_bits as u8);
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&original_len.to_le_bytes());
    out.extend_from_slice(&block_size.to_le_bytes());
}

fn write_v3_block_frame(out: &mut Vec<u8>, raw_len: u32, coded_len: u32, crc: u32, payload: &[u8]) {
    out.extend_from_slice(&raw_len.to_le_bytes());
    out.extend_from_slice(&coded_len.to_le_bytes());
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(&crc.to_le_bytes());
    out.extend_from_slice(payload);
}

fn write_v3_trailer(out: &mut Vec<u8>, total_len: u64, digest: &[u8; 32]) {
    out.extend_from_slice(&0u32.to_le_bytes()); // end-of-blocks sentinel
    out.extend_from_slice(&total_len.to_le_bytes());
    out.extend_from_slice(digest);
}

/// Compress `input` according to `options` (`OSM\x02` or `OSM\x03`).
pub fn compress_with_options(
    input: &[u8],
    options: &Options,
    mut progress: impl FnMut(usize),
) -> Result<Vec<u8>, Error> {
    options.validate_for_compress()?;
    if options.format == Format::V2 {
        return Ok(compress(input, progress));
    }

    let block_size = if options.block_size == 0 {
        // Single-block mode unless input exceeds MAX_BLOCK_SIZE.
        if input.len() > MAX_BLOCK_SIZE {
            MAX_BLOCK_SIZE
        } else {
            0
        }
    } else {
        options.block_size
    };

    let size_hint = if block_size > 0 {
        input.len().min(block_size)
    } else {
        input.len()
    };
    let table_bits = options.resolve_table_bits(size_hint)?;
    let flags = FLAG_KNOWN_LEN
        | if options.text_transform {
            FLAG_TEXT_TRANSFORM
        } else {
            0
        };

    let digest = checksum::sha256(input);
    let mut out = Vec::with_capacity(input.len() / 4 + 128);
    write_v3_header(
        &mut out,
        flags,
        table_bits,
        input.len() as u64,
        block_size as u32,
    );

    if input.is_empty() {
        write_v3_trailer(&mut out, 0, &digest);
        return Ok(out);
    }

    let chunks: Vec<&[u8]> = if block_size == 0 {
        vec![input]
    } else {
        input.chunks(block_size).collect()
    };

    let workers = effective_threads(options.threads, chunks.len());
    if workers <= 1 {
        let mut done = 0usize;
        for chunk in chunks {
            let (raw_len, coded_len, crc, payload) = if !options.text_transform && block_size == 0 {
                let crc = checksum::crc32c(chunk);
                let payload = encode_raw_block(chunk, table_bits, &mut progress);
                (chunk.len() as u32, chunk.len() as u32, crc, payload)
            } else {
                encode_v3_block(chunk, table_bits, options.text_transform)
            };
            write_v3_block_frame(&mut out, raw_len, coded_len, crc, &payload);
            done += chunk.len();
            if block_size > 0 {
                progress(done);
            }
        }
    } else {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::sync::atomic::{AtomicUsize, Ordering};
            use std::sync::Mutex;

            type EncodedBlock = (u32, u32, u32, Vec<u8>);
            let next_idx = AtomicUsize::new(0);
            let slots: Vec<Mutex<Option<EncodedBlock>>> =
                (0..chunks.len()).map(|_| Mutex::new(None)).collect();

            std::thread::scope(|s| {
                for _ in 0..workers {
                    s.spawn(|| loop {
                        let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                        if idx >= chunks.len() {
                            break;
                        }
                        let encoded =
                            encode_v3_block(chunks[idx], table_bits, options.text_transform);
                        *slots[idx].lock().unwrap() = Some(encoded);
                    });
                }
            });

            let mut done = 0usize;
            for slot in slots {
                let (raw_len, coded_len, crc, payload) = slot.into_inner().unwrap().unwrap();
                write_v3_block_frame(&mut out, raw_len, coded_len, crc, &payload);
                done += raw_len as usize;
                progress(done);
            }
        }
    }

    write_v3_trailer(&mut out, input.len() as u64, &digest);
    Ok(out)
}

struct ParsedV3<'a> {
    flags: u8,
    table_bits: u32,
    original_len: u64,
    block_size: u32,
    blocks: Vec<(BlockInfo, &'a [u8])>,
    total_len: u64,
    sha256: [u8; 32],
}

fn parse_v3(data: &[u8]) -> Result<ParsedV3<'_>, Error> {
    if data.len() < V3_HEADER_LEN {
        return Err(Error::Truncated);
    }
    if &data[..4] != MAGIC_V3 {
        return Err(Error::NotOsmium);
    }
    let flags = data[4];
    if flags & !FLAGS_MASK != 0 {
        return Err(Error::CorruptHeader);
    }
    let table_bits = data[5] as u32;
    if !(MIN_TABLE_BITS..=MAX_TABLE_BITS).contains(&table_bits) {
        return Err(Error::CorruptHeader);
    }
    let reserved = u16::from_le_bytes(data[6..8].try_into().unwrap());
    if reserved != 0 {
        return Err(Error::CorruptHeader);
    }
    let original_len = u64::from_le_bytes(data[8..16].try_into().unwrap());
    if flags & FLAG_KNOWN_LEN == 0 && original_len != 0 {
        return Err(Error::CorruptHeader);
    }
    let block_size = u32::from_le_bytes(data[16..20].try_into().unwrap());
    if block_size as usize > MAX_BLOCK_SIZE {
        return Err(Error::CorruptHeader);
    }

    let mut pos = V3_HEADER_LEN;
    let mut blocks = Vec::new();
    let mut sum_raw: u64 = 0;
    let text_transform = flags & FLAG_TEXT_TRANSFORM != 0;

    loop {
        if pos + 4 > data.len() {
            return Err(Error::Truncated);
        }
        let raw_len = u32::from_le_bytes(data[pos..pos + 4].try_into().unwrap());
        if raw_len == 0 {
            pos += 4;
            break;
        }
        if pos + V3_BLOCK_HEADER_LEN > data.len() {
            return Err(Error::Truncated);
        }
        if raw_len as usize > MAX_BLOCK_SIZE {
            return Err(Error::CorruptHeader);
        }
        if block_size > 0 && raw_len > block_size {
            return Err(Error::CorruptHeader);
        }
        if block_size == 0 && !blocks.is_empty() {
            return Err(Error::CorruptHeader);
        }

        let coded_len = u32::from_le_bytes(data[pos + 4..pos + 8].try_into().unwrap());
        let comp_len = u32::from_le_bytes(data[pos + 8..pos + 12].try_into().unwrap());
        let crc32c = u32::from_le_bytes(data[pos + 12..pos + 16].try_into().unwrap());
        pos += V3_BLOCK_HEADER_LEN;

        if comp_len < 4 {
            return Err(Error::Truncated);
        }
        if text_transform {
            let max_coded = (raw_len as u64).saturating_mul(2).saturating_add(16);
            if coded_len as u64 > max_coded {
                return Err(Error::CorruptHeader);
            }
        } else if coded_len != raw_len {
            return Err(Error::CorruptHeader);
        }

        let comp_end = pos.checked_add(comp_len as usize).ok_or(Error::Truncated)?;
        if comp_end > data.len() {
            return Err(Error::Truncated);
        }
        let payload = &data[pos..comp_end];
        pos = comp_end;

        sum_raw = sum_raw
            .checked_add(raw_len as u64)
            .ok_or(Error::CorruptHeader)?;
        blocks.push((
            BlockInfo {
                index: blocks.len(),
                raw_len,
                coded_len,
                comp_len,
                crc32c,
            },
            payload,
        ));
    }

    if pos + V3_TRAILER_LEN > data.len() {
        return Err(Error::Truncated);
    }
    let total_len = u64::from_le_bytes(data[pos..pos + 8].try_into().unwrap());
    let sha256: [u8; 32] = data[pos + 8..pos + V3_TRAILER_LEN].try_into().unwrap();
    pos += V3_TRAILER_LEN;

    if pos != data.len() {
        return Err(Error::TrailingData);
    }
    if total_len != sum_raw {
        return Err(Error::CorruptHeader);
    }
    if flags & FLAG_KNOWN_LEN != 0 && original_len != total_len {
        return Err(Error::CorruptHeader);
    }

    Ok(ParsedV3 {
        flags,
        table_bits,
        original_len: total_len,
        block_size,
        blocks,
        total_len,
        sha256,
    })
}

fn decompress_v3(
    data: &[u8],
    threads: usize,
    mut progress: impl FnMut(usize),
) -> Result<Vec<u8>, Error> {
    let parsed = parse_v3(data)?;
    let total = usize::try_from(parsed.total_len).map_err(|_| Error::Truncated)?;
    let text_transform = parsed.flags & FLAG_TEXT_TRANSFORM != 0;

    if parsed.blocks.is_empty() {
        if checksum::sha256(&[]) != parsed.sha256 {
            return Err(Error::ChecksumMismatch);
        }
        return Ok(Vec::new());
    }

    let workers = effective_threads(threads, parsed.blocks.len());
    let init_cap = total.min(data.len().saturating_mul(8).max(4096));
    let mut out = Vec::with_capacity(init_cap);
    let mut hasher = checksum::Sha256::new();

    if workers <= 1 {
        for (info, payload) in &parsed.blocks {
            let block = decode_v3_block(
                info.raw_len as usize,
                info.coded_len as usize,
                info.crc32c,
                payload,
                parsed.table_bits,
                text_transform,
            )?;
            hasher.update(&block);
            out.extend_from_slice(&block);
            progress(out.len());
        }
    } else {
        #[cfg(not(target_arch = "wasm32"))]
        {
            use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
            use std::sync::Mutex;

            type DecodedBlock = Result<Vec<u8>, Error>;
            let next_idx = AtomicUsize::new(0);
            let failed = AtomicBool::new(false);
            let slots: Vec<Mutex<Option<DecodedBlock>>> =
                (0..parsed.blocks.len()).map(|_| Mutex::new(None)).collect();

            std::thread::scope(|s| {
                for _ in 0..workers {
                    s.spawn(|| loop {
                        if failed.load(Ordering::Relaxed) {
                            break;
                        }
                        let idx = next_idx.fetch_add(1, Ordering::Relaxed);
                        if idx >= parsed.blocks.len() {
                            break;
                        }
                        let (ref info, payload) = parsed.blocks[idx];
                        let res = decode_v3_block(
                            info.raw_len as usize,
                            info.coded_len as usize,
                            info.crc32c,
                            payload,
                            parsed.table_bits,
                            text_transform,
                        );
                        if res.is_err() {
                            failed.store(true, Ordering::Relaxed);
                        }
                        *slots[idx].lock().unwrap() = Some(res);
                    });
                }
            });

            for slot in slots {
                let block = slot
                    .into_inner()
                    .unwrap()
                    .unwrap_or(Err(Error::Truncated))?;
                hasher.update(&block);
                out.extend_from_slice(&block);
                progress(out.len());
            }
        }
    }

    if hasher.finalize() != parsed.sha256 {
        return Err(Error::ChecksumMismatch);
    }
    Ok(out)
}

// ------------------------------------------------------------------ streaming I/O API

fn read_chunk<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<usize, Error> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(e.to_string())),
        }
    }
    Ok(filled)
}

fn read_exact_or_err<R: Read>(reader: &mut R, buf: &mut [u8]) -> Result<(), Error> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => return Err(Error::Truncated),
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(Error::Io(e.to_string())),
        }
    }
    Ok(())
}

/// Compress a stream in bounded memory using the framed `OSM\x03` format.
///
/// Reads input in blocks of `options.block_size` (or [`DEFAULT_STREAM_BLOCK_SIZE`] when `0`)
/// and writes self-delimiting frames plus a SHA-256 trailer to `writer`.
pub fn compress_stream<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
    options: &Options,
    mut progress: impl FnMut(u64),
) -> Result<StreamStats, Error> {
    if options.format == Format::V2 {
        return Err(Error::InvalidOptions("compress_stream requires Format::V3"));
    }
    options.validate_for_compress()?;
    let block_size = if options.block_size == 0 {
        DEFAULT_STREAM_BLOCK_SIZE
    } else {
        options.block_size
    };
    let table_bits = options.resolve_table_bits(block_size)?;
    let flags = if options.text_transform {
        FLAG_TEXT_TRANSFORM
    } else {
        0
    };

    let mut hdr = Vec::with_capacity(V3_HEADER_LEN);
    write_v3_header(&mut hdr, flags, table_bits, 0, block_size as u32);
    writer
        .write_all(&hdr)
        .map_err(|e| Error::Io(e.to_string()))?;

    let mut buf = vec![0u8; block_size];
    let mut hasher = checksum::Sha256::new();
    let mut total_in: u64 = 0;
    let mut total_out: u64 = hdr.len() as u64;
    let mut blocks = 0usize;

    loop {
        let n = read_chunk(&mut reader, &mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        hasher.update(chunk);
        let (raw_len, coded_len, crc, payload) =
            encode_v3_block(chunk, table_bits, options.text_transform);

        let mut frame_hdr = [0u8; V3_BLOCK_HEADER_LEN];
        frame_hdr[0..4].copy_from_slice(&raw_len.to_le_bytes());
        frame_hdr[4..8].copy_from_slice(&coded_len.to_le_bytes());
        frame_hdr[8..12].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        frame_hdr[12..16].copy_from_slice(&crc.to_le_bytes());
        writer
            .write_all(&frame_hdr)
            .map_err(|e| Error::Io(e.to_string()))?;
        writer
            .write_all(&payload)
            .map_err(|e| Error::Io(e.to_string()))?;

        total_in += n as u64;
        total_out += (V3_BLOCK_HEADER_LEN + payload.len()) as u64;
        blocks += 1;
        progress(total_in);
    }

    let digest = hasher.finalize();
    let mut trailer = Vec::with_capacity(4 + V3_TRAILER_LEN);
    write_v3_trailer(&mut trailer, total_in, &digest);
    writer
        .write_all(&trailer)
        .map_err(|e| Error::Io(e.to_string()))?;
    writer.flush().map_err(|e| Error::Io(e.to_string()))?;
    total_out += trailer.len() as u64;

    Ok(StreamStats {
        input_bytes: total_in,
        output_bytes: total_out,
        blocks,
        sha256: Some(digest),
    })
}

/// Decompress an `OSM\x02` or `OSM\x03` stream into `writer`.
///
/// For `OSM\x03`, blocks are decoded and CRC-32C checked one block at a time in bounded
/// memory, followed by whole-stream SHA-256 verification. `OSM\x02` streams are buffered
/// and decoded via [`decompress`].
pub fn decompress_stream<R: Read, W: Write>(
    mut reader: R,
    mut writer: W,
    mut progress: impl FnMut(u64),
) -> Result<StreamStats, Error> {
    let mut magic = [0u8; 4];
    match read_exact_or_err(&mut reader, &mut magic) {
        Ok(()) => {}
        Err(Error::Truncated) => return Err(Error::NotOsmium),
        Err(e) => return Err(e),
    }

    if &magic == MAGIC {
        let mut all = Vec::from(magic);
        reader
            .read_to_end(&mut all)
            .map_err(|e| Error::Io(e.to_string()))?;
        let input_bytes = all.len() as u64;
        let out = decompress_v2(&all, |done| progress(done as u64))?;
        writer
            .write_all(&out)
            .map_err(|e| Error::Io(e.to_string()))?;
        writer.flush().map_err(|e| Error::Io(e.to_string()))?;
        return Ok(StreamStats {
            input_bytes,
            output_bytes: out.len() as u64,
            blocks: 1,
            sha256: None,
        });
    }

    if &magic != MAGIC_V3 {
        return Err(Error::NotOsmium);
    }

    let mut rest_hdr = [0u8; V3_HEADER_LEN - 4];
    read_exact_or_err(&mut reader, &mut rest_hdr)?;

    let flags = rest_hdr[0];
    if flags & !FLAGS_MASK != 0 {
        return Err(Error::CorruptHeader);
    }
    let table_bits = rest_hdr[1] as u32;
    if !(MIN_TABLE_BITS..=MAX_TABLE_BITS).contains(&table_bits) {
        return Err(Error::CorruptHeader);
    }
    let reserved = u16::from_le_bytes(rest_hdr[2..4].try_into().unwrap());
    if reserved != 0 {
        return Err(Error::CorruptHeader);
    }
    let original_len = u64::from_le_bytes(rest_hdr[4..12].try_into().unwrap());
    if flags & FLAG_KNOWN_LEN == 0 && original_len != 0 {
        return Err(Error::CorruptHeader);
    }
    let block_size = u32::from_le_bytes(rest_hdr[12..16].try_into().unwrap());
    if block_size as usize > MAX_BLOCK_SIZE {
        return Err(Error::CorruptHeader);
    }
    let text_transform = flags & FLAG_TEXT_TRANSFORM != 0;

    let mut total_in: u64 = V3_HEADER_LEN as u64;
    let mut total_out: u64 = 0;
    let mut blocks = 0usize;
    let mut hasher = checksum::Sha256::new();

    loop {
        let mut raw_len_bytes = [0u8; 4];
        read_exact_or_err(&mut reader, &mut raw_len_bytes)?;
        total_in += 4;
        let raw_len = u32::from_le_bytes(raw_len_bytes);
        if raw_len == 0 {
            break;
        }
        if raw_len as usize > MAX_BLOCK_SIZE {
            return Err(Error::CorruptHeader);
        }
        if block_size > 0 && raw_len > block_size {
            return Err(Error::CorruptHeader);
        }
        if block_size == 0 && blocks > 0 {
            return Err(Error::CorruptHeader);
        }

        let mut meta = [0u8; V3_BLOCK_HEADER_LEN - 4];
        read_exact_or_err(&mut reader, &mut meta)?;
        total_in += (V3_BLOCK_HEADER_LEN - 4) as u64;

        let coded_len = u32::from_le_bytes(meta[0..4].try_into().unwrap());
        let comp_len = u32::from_le_bytes(meta[4..8].try_into().unwrap());
        let crc32c = u32::from_le_bytes(meta[8..12].try_into().unwrap());
        if comp_len < 4 {
            return Err(Error::Truncated);
        }
        // Cap allocation against hostile comp_len: compressed block cannot legitimately exceed
        // 2 * MAX_BLOCK_SIZE + 1 MiB.
        if (comp_len as usize) > MAX_BLOCK_SIZE.saturating_mul(2) + (1 << 20) {
            return Err(Error::CorruptHeader);
        }

        let mut payload = vec![0u8; comp_len as usize];
        read_exact_or_err(&mut reader, &mut payload)?;
        total_in += comp_len as u64;

        let raw = decode_v3_block(
            raw_len as usize,
            coded_len as usize,
            crc32c,
            &payload,
            table_bits,
            text_transform,
        )?;
        hasher.update(&raw);
        writer
            .write_all(&raw)
            .map_err(|e| Error::Io(e.to_string()))?;

        total_out = total_out
            .checked_add(raw.len() as u64)
            .ok_or(Error::CorruptHeader)?;
        blocks += 1;
        progress(total_out);
    }

    let mut trailer = [0u8; V3_TRAILER_LEN];
    read_exact_or_err(&mut reader, &mut trailer)?;
    total_in += V3_TRAILER_LEN as u64;

    let expected_total = u64::from_le_bytes(trailer[0..8].try_into().unwrap());
    let expected_sha: [u8; 32] = trailer[8..V3_TRAILER_LEN].try_into().unwrap();

    // Ensure no trailing garbage follows the trailer.
    let mut extra = [0u8; 1];
    match reader.read(&mut extra) {
        Ok(0) => {}
        Ok(_) => return Err(Error::TrailingData),
        Err(e) => return Err(Error::Io(e.to_string())),
    }

    if expected_total != total_out {
        return Err(Error::CorruptHeader);
    }
    if flags & FLAG_KNOWN_LEN != 0 && original_len != total_out {
        return Err(Error::CorruptHeader);
    }
    let actual_sha = hasher.finalize();
    if actual_sha != expected_sha {
        return Err(Error::ChecksumMismatch);
    }
    writer.flush().map_err(|e| Error::Io(e.to_string()))?;

    Ok(StreamStats {
        input_bytes: total_in,
        output_bytes: total_out,
        blocks,
        sha256: Some(actual_sha),
    })
}

// ------------------------------------------------------------------ inspect, verify, analyze

/// Inspect archive headers and block framing without decompressing payload bytes.
pub fn inspect(data: &[u8]) -> Result<ArchiveInfo, Error> {
    if data.len() < 4 {
        return Err(Error::NotOsmium);
    }
    match &data[..4] {
        b"OSM\x02" => {
            if data.len() < 12 {
                return Err(Error::NotOsmium);
            }
            if data.len() < 16 {
                return Err(Error::Truncated);
            }
            let original_len = u64::from_le_bytes(data[4..12].try_into().unwrap());
            let n_usize = usize::try_from(original_len).unwrap_or(usize::MAX);
            Ok(ArchiveInfo {
                format: Format::V2,
                original_len,
                compressed_len: data.len() as u64,
                table_bits: auto_table_bits(n_usize),
                text_transform: false,
                block_size: 0,
                blocks: Vec::new(),
                sha256: None,
                verified: false,
            })
        }
        b"OSM\x03" => {
            let parsed = parse_v3(data)?;
            Ok(ArchiveInfo {
                format: Format::V3,
                original_len: parsed.original_len,
                compressed_len: data.len() as u64,
                table_bits: parsed.table_bits,
                text_transform: parsed.flags & FLAG_TEXT_TRANSFORM != 0,
                block_size: parsed.block_size,
                blocks: parsed.blocks.into_iter().map(|(b, _)| b).collect(),
                sha256: Some(parsed.sha256),
                verified: false,
            })
        }
        _ => Err(Error::NotOsmium),
    }
}

/// Decompress and verify an `OSM\x02` or `OSM\x03` archive, returning its verified [`ArchiveInfo`].
///
/// For `OSM\x03`, verifies every block's CRC-32C, transformed stream structure, total length,
/// absence of trailing bytes, and full-stream SHA-256. For `OSM\x02`, verifies bitstream
/// decoding completes without overrun.
pub fn verify(data: &[u8], threads: usize) -> Result<ArchiveInfo, Error> {
    let mut info = inspect(data)?;
    let _ = decompress_with_threads(data, threads, |_| {})?;
    info.verified = true;
    Ok(info)
}

#[inline]
fn bit_loss_bits(p12: i32, bit: u8) -> f64 {
    let p = (p12.clamp(1, 4095) as f64) / 4096.0;
    let q = if bit == 1 { p } else { 1.0 - p };
    -q.log2()
}

/// Run the predictor over `input` and report Shannon entropy, coded bits/byte, per-stage
/// cross-entropy breakdown, match hit rates, and per-context slot hit rates.
pub fn analyze(input: &[u8], options: &Options) -> Result<AnalysisReport, Error> {
    let coded = if options.text_transform {
        transform::encode(input)
    } else {
        input.to_vec()
    };
    let table_bits = options.resolve_table_bits(coded.len())?;

    // Order-0 Shannon entropy of the raw input.
    let mut counts = [0u64; 256];
    for &b in input {
        counts[b as usize] += 1;
    }
    let n_f = input.len() as f64;
    let shannon_bpb = if input.is_empty() {
        0.0
    } else {
        counts
            .iter()
            .filter(|&&c| c > 0)
            .map(|&c| {
                let p = c as f64 / n_f;
                -p * p.log2()
            })
            .sum()
    };

    let mut p = Predictor::with_table_bits(coded.len(), table_bits);
    let mut enc = Encoder::new(Vec::with_capacity(coded.len() / 4 + 32));

    let mut l_m1 = 0.0f64;
    let mut l_m2 = 0.0f64;
    let mut l_m3 = 0.0f64;
    let mut l_m4 = 0.0f64;
    let mut l_mf = 0.0f64;
    let mut l_a1 = 0.0f64;
    let mut l_a2 = 0.0f64;
    let mut l_a3 = 0.0f64;
    let mut l_a4 = 0.0f64;
    let mut l_ms = 0.0f64;
    let mut l_pr = 0.0f64;

    let mut short_hits: u64 = 0;
    let mut short_len_sum: u64 = 0;
    let mut long_hits: u64 = 0;
    let mut long_len_sum: u64 = 0;
    let mut ctx_hits = [0u64; model::CTX_NAMES.len()];
    let mut nibble_count: u64 = 0;

    for &byte in &coded {
        for k in (0..8).rev() {
            let bit = (byte >> k) & 1;
            let snap = p.snapshot();
            if snap.bitpos == 0 {
                if snap.match_len > 0 {
                    short_hits += 1;
                    short_len_sum += snap.match_len as u64;
                }
                if snap.long_match_len > 0 {
                    long_hits += 1;
                    long_len_sum += snap.long_match_len as u64;
                }
            }
            if snap.bitpos == 0 || snap.bitpos == 4 {
                nibble_count += 1;
                for (ci, hit) in ctx_hits.iter_mut().enumerate() {
                    if (snap.seen >> ci) & 1 != 0 {
                        *hit += 1;
                    }
                }
            }
            l_m1 += bit_loss_bits(snap.pr1, bit);
            l_m2 += bit_loss_bits(snap.pr2, bit);
            l_m3 += bit_loss_bits(snap.pr3, bit);
            l_m4 += bit_loss_bits(snap.pr4, bit);
            l_mf += bit_loss_bits(snap.pr_mix, bit);
            l_a1 += bit_loss_bits(snap.a1, bit);
            l_a2 += bit_loss_bits(snap.a2, bit);
            l_a3 += bit_loss_bits(snap.a3, bit);
            l_a4 += bit_loss_bits(snap.a4, bit);
            l_ms += bit_loss_bits(snap.pr_ms, bit);
            l_pr += bit_loss_bits(snap.pr, bit);

            enc.encode(bit, p.p());
            p.update(bit);
        }
    }
    let payload = enc.finish();
    let denom = if input.is_empty() { 1.0 } else { n_f };
    let coded_denom = if coded.is_empty() {
        1.0
    } else {
        coded.len() as f64
    };
    let nib_denom = if nibble_count == 0 {
        1.0
    } else {
        nibble_count as f64
    };

    let context_hit_rates = model::CTX_NAMES
        .iter()
        .enumerate()
        .map(|(i, &name)| (name.to_string(), ctx_hits[i] as f64 / nib_denom))
        .collect();

    Ok(AnalysisReport {
        input_bytes: input.len(),
        coded_bytes: coded.len(),
        compressed_bytes: payload.len(),
        table_bits,
        text_transform: options.text_transform,
        shannon_bpb,
        coded_bpb: if input.is_empty() {
            0.0
        } else {
            payload.len() as f64 * 8.0 / n_f
        },
        stages: StageEntropy {
            mixer1: l_m1 / denom,
            mixer2: l_m2 / denom,
            mixer3: l_m3 / denom,
            mixer4: l_m4 / denom,
            mixer_final: l_mf / denom,
            apm1: l_a1 / denom,
            apm2: l_a2 / denom,
            apm3: l_a3 / denom,
            apm4: l_a4 / denom,
            sse_mixer: l_ms / denom,
            final_pred: l_pr / denom,
        },
        short_match_hit_rate: short_hits as f64 / coded_denom,
        short_match_avg_len: if short_hits == 0 {
            0.0
        } else {
            short_len_sum as f64 / short_hits as f64
        },
        long_match_hit_rate: long_hits as f64 / coded_denom,
        long_match_avg_len: if long_hits == 0 {
            0.0
        } else {
            long_len_sum as f64 / long_hits as f64
        },
        context_hit_rates,
    })
}
