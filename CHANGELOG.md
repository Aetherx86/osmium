# Changelog

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
