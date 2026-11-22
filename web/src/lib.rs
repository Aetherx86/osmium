//! Osmium in the browser: the same model and file format as the command-line tool.
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn compress(data: &[u8]) -> Vec<u8> { osmium::compress(data, |_| {}) }

/// Decompressed bytes, or an error message prefixed with "error: " as UTF-8.
#[wasm_bindgen]
pub fn decompress(data: &[u8]) -> Result<Vec<u8>, JsError> {
    osmium::decompress(data, |_| {}).map_err(|e| JsError::new(&e.to_string()))
}
