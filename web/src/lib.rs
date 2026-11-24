//! Osmium in the browser: the same model and file formats (OSM v2 and OSM v3)
//! as the command-line tool.
use osmium::{Format, Options};
use wasm_bindgen::prelude::*;

/// Compress into the v0.2.0 (`OSM\x02`) format, byte-identical to `osmium c`.
#[wasm_bindgen]
pub fn compress(data: &[u8]) -> Vec<u8> {
    osmium::compress(data, |_| {})
}

/// Decompress an `OSM\x02` or `OSM\x03` archive (verifying CRC-32C and SHA-256 on v3).
#[wasm_bindgen]
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, JsError> {
    osmium::decompress(data, |_| {}).map_err(|e| JsError::new(&e.to_string()))
}

/// Compress into the framed `OSM\x03` format with configurable memory preset (`level` 0..=7),
/// optional block size (0 = single block), and optional reversible text/markup preprocessor.
#[wasm_bindgen]
pub fn compress_v3(
    data: &[u8],
    level: u8,
    block_size: u32,
    text_transform: bool,
) -> Result<Vec<u8>, JsError> {
    let opts = Options {
        format: Format::V3,
        level,
        table_bits: None,
        block_size: block_size as usize,
        threads: 1,
        text_transform,
    };
    osmium::compress_with_options(data, &opts, |_| {}).map_err(|e| JsError::new(&e.to_string()))
}

/// Inspect an `OSM\x02` or `OSM\x03` archive header without decompressing, returning JSON.
#[wasm_bindgen]
pub fn inspect_json(data: &[u8]) -> Result<String, JsError> {
    osmium::inspect(data)
        .map(|info| info.to_json())
        .map_err(|e| JsError::new(&e.to_string()))
}

/// Verify an `OSM\x02` or `OSM\x03` archive's integrity, returning verified metadata JSON.
#[wasm_bindgen]
pub fn verify_json(data: &[u8]) -> Result<String, JsError> {
    osmium::verify(data, 1)
        .map(|info| info.to_json())
        .map_err(|e| JsError::new(&e.to_string()))
}

/// Run entropy and per-stage predictor diagnostics, returning a JSON report.
#[wasm_bindgen]
pub fn analyze_json(data: &[u8], level: u8, text_transform: bool) -> Result<String, JsError> {
    let opts = Options {
        format: Format::V3,
        level,
        table_bits: None,
        block_size: 0,
        threads: 1,
        text_transform,
    };
    osmium::analyze(data, &opts)
        .map(|report| report.to_json())
        .map_err(|e| JsError::new(&e.to_string()))
}

/// Return the Osmium crate version string.
#[wasm_bindgen]
pub fn version() -> String {
    osmium::VERSION.to_string()
}
