# Changelog

## [0.3.0] - 2026-11-24

### Added
- **Framed `OSM\x03` archive format** alongside full backward compatibility with `OSM\x02`:
  - `osmium::compress` and plain `osmium c` continue to emit byte-identical `OSM\x02` files, while `osmium::decompress` and `osmium d` decode both `OSM\x02` and `OSM\x03`.
  - Pure-Rust, zero-dependency per-block Castagnoli CRC-32C and whole-stream SHA-256 (FIPS 180-4) integrity verification.
  - Configurable memory presets (`--level 1..7` or `--table-bits 12..23`), scaling table memory from ~7 MiB (`level 1`, `table_bits = 13`) up to ~9 GiB (`level 7`, `table_bits = 23`), plus `level 0` (auto).
  - Independent block framing (`--block-size`) with deterministic multi-threaded compression and decompression (`--threads`), producing byte-for-byte identical archives regardless of thread count.
- **Reversible UTF-8 / case / markup text preprocessor (`--transform` / `-x`, `osmium::transform`)**:
  - Two composable bijective passes using bytes illegal in valid UTF-8 (`0xC0`, `0xC1`, `0xF5..=0xFF`) for zero escaping overhead on UTF-8 text and 100% lossless round trips on arbitrary binary data.
  - Folds ASCII capitalization (`CAP_ONE` / `CAP_WORD`) and compacts frequent Wikipedia/XML/English tokens before context mixing.
- **Streaming I/O and new CLI subcommands**:
  - `osmium::compress_stream` and `osmium::decompress_stream` over `std::io::Read` / `std::io::Write` in bounded block memory, exposed in the CLI via `-` for stdin/stdout.
  - `osmium i <archive> [--json]` (`osmium::inspect`): inspects `OSM\x02`/`OSM\x03` headers, blocks, and digests without decompressing.
  - `osmium v <archive> [--json]` (`osmium::verify`): verifies bitstream and checksum integrity.
  - `osmium a <input> [--json]` (`osmium::analyze`): reports order-0 Shannon entropy, coded bits/byte, per-stage cross-entropy (`m1..m4`, `mf`, `a1..a4`, `ms`, `final`), short/long match hit rates, and slot hit rates across all 19 context models.
  - `--json`, `--version`, and `--help` flags across the CLI.
- **Browser WebAssembly exports (`web/`)**:
  - Preserves `compress` and `decompress` (now decoding both `OSM\x02` and `OSM\x03`), and adds `compress_v3`, `inspect_json`, `verify_json`, `analyze_json`, and `version`.

### Fixed
- Hardened `OSM\x02` and `OSM\x03` decoders against hostile headers (32-bit `usize` truncation on `wasm32`, oversized pre-allocation before reading payload, short `<16`-byte `OSM\x02` buffers, invalid flags/table bits, trailing data, and truncated/corrupted transform streams).
- Guarded `match_tab` and `LongMatch::tab` position writes against 32-bit wraparound above 4 GiB.
- Implemented `std::error::Error` for `osmium::Error`.

## [0.2.0] - 2026-11-22

### Changed (new archive format: 0.2.0 does not read 0.1.0 files)
- Bit histories with adaptive state maps replace direct counters: one byte per bit context in
  16-byte, 4-way set-associative slots, so four times as many contexts fit in the same memory.
- Run model (last byte and repeat count per context), two indirect contexts, a 24-byte
  long-match model, and contexts for order 5, line column, skip-word, markup plus word and
  previous word plus byte.
- A fourth mixer selected by which high-order contexts have been seen; lower learning rates.
- A fourth SSE stage keyed by match state, and a learned combination of the SSE outputs.
- Inputs over 512 MB use tables twice as large (about 9 GB in total).

### Results
- enwik8: 20,312,078 -> 18,485,784 bytes (1.479 bits/byte), verified round trip.

## [0.1.0] - 2026-11-22

First release. enwik8 20,312,078 bytes, enwik9 168,149,041 bytes, both verified.
