# Osmium

A context-mixing compressor for text, written in Rust and measured on the
[Hutter Prize](http://prize.hutter1.net/) data: `enwik8` and `enwik9`, the first 100 MB and 1 GB
of an English Wikipedia dump. One small program both compresses and decompresses, and every
result below is a verified round trip (decompressed output compared byte for byte with the input).

Live demo: <https://omakasekim.github.io/osmium/> runs the same code, compiled to WebAssembly,
in the browser. Its `.osm` files are byte-identical to the command-line tool's.

## Results

Apple M4 MacBook Air, one core. Peak memory from `/usr/bin/time -l`. Every result is a verified
round trip.

| File | Version | Compressed | bits/byte | Compress | Decompress | Memory |
| :--- | :--- | ---: | ---: | ---: | ---: | ---: |
| enwik8 (100 MB) | **0.2.0** | **18,485,784** | 1.479 | 267 s | 269 s | 2.1 GB |
| enwik8 (100 MB) | 0.1.0 | 20,312,078 | 1.625 | 95 s | 118 s | 1.2 GB |
| enwik9 (1 GB) | 0.1.0 | 168,149,041 | 1.345 | 1,347 s | 1,001 s | 3.5 GB |

0.2.0 has not been run on enwik9 yet (it needs about 9 GB of memory there and roughly an hour
each way); the enwik9 row is 0.1.0's verified result. The 0.2.0 timings were taken while the
machine was shared with other jobs.

Same files with standard tools (`-9`; xz with `-9e`, single thread):

| File | gzip | bzip2 | xz | Osmium |
| :--- | ---: | ---: | ---: | ---: |
| enwik8 | 36,475,811 | 29,008,758 | 24,831,648 | 18,485,784 (0.2.0) |
| enwik9 | 322,789,249 | 253,977,891 | 211,776,220 | 168,149,041 (0.1.0) |

### Against the Hutter Prize

The prize scores the compressed `enwik9` plus the size of the decompressor. The record listed
on July 24, 2026 is **100,424,672 bytes** (Vladimir Ivanov, fx2-cmix-T). Osmium 0.1.0's total is
168,311,575 bytes (archive plus the zipped decompressor), so it is not a contender: record
entries run thousands of models, LSTM or transformer mixers and large preprocessing
dictionaries for days. Osmium is a compact, readable member of the same family (about 900
lines) that beats the general-purpose tools by a wide margin and is meant to be built on.

## How it works

Each bit of the input is predicted, then coded with a binary arithmetic coder using that
probability. Prediction:

- **Context models.** Nineteen contexts: byte orders 0 to 6 and 8; the current word, the last
  two and three words, the current word with the word two back, word plus previous byte,
  previous word plus previous byte; a wiki-markup context (innermost open `[`, `{`, `<`, `(`,
  `|`, `=` or `'` and the first byte of the line) alone and with the current word; the column
  within the line; and two indirect contexts (the bytes that last followed the previous one and
  two bytes).
- **Bit histories.** Each context keeps, per bit of the current nibble, a one-byte history
  (counts of zeros and ones, discounted toward recent behaviour) in 16-byte slots, four per
  cache line with an 8-bit check and least-used replacement. Adaptive state maps, one per
  model and bit position, turn histories into probabilities.
- **Run model.** Per context, the byte that last followed it and how many times in a row.
- **Match models.** A 7-byte and a 24-byte hash find the previous occurrence of the recent
  text; while a match holds, its next bit is predicted with a confidence learned per length.
- **Mixing.** Four gated logistic mixers (selected by the partial byte; by match length, bit
  position and previous byte; by the previous byte; and by which high orders have been seen)
  combine 62 inputs, and a final mixer combines their outputs.
- **SSE.** Four adaptive probability maps refine the result (partial byte; previous byte;
  previous two bytes; match state), and a small mixer learns how to weigh them.

All arithmetic is integer, so archives are identical across platforms, and the browser build
decodes the command-line tool's files and the other way round. Table sizes depend on the input
length, which is stored in the header (inputs over 512 MB use about 9 GB).

## Usage

```sh
cargo build --release
./target/release/osmium c enwik8 enwik8.osm    # compress (OSM v2, bit-identical to 0.2.0)
./target/release/osmium d enwik8.osm enwik8.out # decompress (reads OSM v2 and OSM v3)
./target/release/osmium t enwik8                # compress, decompress in memory, verify, report
cargo test --release
```

### Framed v3 archives, streaming, presets, and diagnostics (0.3.0)

Plain `osmium c` and `osmium::compress` keep the minimal `OSM\x02` format byte-identical to
0.2.0. Passing any v3 option (`--v3`, `-l/--level`, `--table-bits`, `-b/--block-size`,
`-x/--transform`) or piping through `-` writes the framed `OSM\x03` format, which adds:

- **Integrity verification**: Castagnoli CRC-32C per block and SHA-256 over the full stream.
- **Memory presets**: `-l 1` (`table_bits = 13`, ~7 MiB) through `-l 7` (`table_bits = 23`,
  ~9 GiB), `-l 0` (auto), or explicit `--table-bits 12..23`.
- **Reversible text/markup transform (`-x`)**: bijective case folding and static Wikipedia/XML
  token compaction using non-UTF-8 marker bytes (zero escaping overhead on UTF-8, lossless on
  arbitrary binary data).
- **Parallel blocks and streaming**: `-b <size> -j <threads>` splits into independent blocks
  encoded deterministically across worker threads; `-` reads/writes streams in bounded memory.
- **Inspection, verification, and entropy analysis**: `i` (inspect), `v` (verify), and `a`
  (per-stage cross-entropy and 19-context hit-rate breakdown), all with `--json`.

```sh
# Framed v3 with low-memory preset, text transform, 64 KiB blocks, 4 threads, JSON output
./target/release/osmium c sample.txt sample.osm -l 1 -b 64K -j 4 -x --json

# Inspect headers without decompressing, then verify CRC-32C and SHA-256
./target/release/osmium i sample.osm
./target/release/osmium v sample.osm -j 4 --json

# Stream via stdin/stdout in bounded memory
./target/release/osmium c - - -l 1 -b 64K -x < sample.txt | ./target/release/osmium d - - > sample.out

# Per-stage cross-entropy and context hit-rate diagnostics
./target/release/osmium a sample.txt -l 1 -x
```

The browser crate (`web/`) preserves `compress` and `decompress` (supporting both `OSM\x02`
and `OSM\x03`) and exports `compress_v3`, `inspect_json`, `verify_json`, `analyze_json`, and
`version`.

The data: `curl -LO http://mattmahoney.net/dc/enwik9.zip && unzip enwik9.zip`, and
`head -c 100000000 enwik9 > enwik8`. SHA-256 of `enwik9`:
`159b85351e5f76e60cbe32e04c677847a9ecba3adc79addab6f4c6c7aa3744bc`.

## License

MIT, see [LICENSE](LICENSE).
