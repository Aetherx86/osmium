# Osmium

A context-mixing compressor for text, written in Rust and measured on the
[Hutter Prize](http://prize.hutter1.net/) data: `enwik8` and `enwik9`, the first 100 MB and 1 GB
of an English Wikipedia dump. One small program both compresses and decompresses, and every
result below is a verified round trip (decompressed output compared byte for byte with the input).

Live demo: <https://aetherx86.github.io/osmium/> runs the same code, compiled to WebAssembly,
in the browser. Its `.osm` files are byte-identical to the command-line tool's.

## Results

Apple M4 MacBook Air, one core (the enwik9 runs shared the machine with other jobs, so their times are pessimistic). Peak memory from `/usr/bin/time -l`.

| File | Original | Osmium 0.1.0 | bits/byte | Compress | Decompress | Memory |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: |
| enwik8 | 100,000,000 | **20,312,078** | 1.625 | 95 s | 118 s | 1.2 GB |
| enwik9 | 1,000,000,000 | 168,149,041 | 1.345 | 1,347 s | 1,001 s | 3.5 GB |

Same files with standard tools (`-9`; xz with `-9e`, single thread):

| File | gzip | bzip2 | xz | Osmium |
| :--- | ---: | ---: | ---: | ---: |
| enwik8 | 36,475,811 | 29,008,758 | 24,831,648 | 20,312,078 |
| enwik9 | 322,789,249 | 253,977,891 | 211,776,220 | 168,149,041 |

### Against the Hutter Prize

The prize scores the compressed `enwik9` plus the size of the decompressor. The record listed
on July 24, 2026 is **100,424,672 bytes** (Vladimir Ivanov, fx2-cmix-T). Osmium's total is
168,311,575 bytes (archive plus the zipped decompressor), so it is not a contender: the
record holders run thousands of models, LSTM mixers and large preprocessing dictionaries for
days. Osmium is a compact, readable baseline of the same family (about 600 lines) that beats
the general-purpose tools by a wide margin and is meant to be built on.

## How it works

Each bit of the input is predicted, then coded with a binary arithmetic coder using that
probability. Prediction:

- **Context models.** Twelve contexts are hashed into tables of 64-byte slots (one cache
  line per context and nibble, with a check word against collisions): orders 0 to 6 and 8,
  the current word, the last two and three words, the word plus the previous byte, and a
  markup context (the innermost open `[`, `{`, `<`, `(`, `|`, `=` or `'` and the first byte of
  the line). Each slot entry holds two counters: a slow 16-bit one and a fast 12-bit one, with
  adaptation rates of 1/(n+1.5) capped at different counts.
- **Match model.** A hash of the last 7 bytes finds the previous occurrence; while the match
  holds, its next bit is predicted with a confidence learned per match length.
- **Mixing.** Three gated logistic mixers (selected by the partial byte; by match length,
  bit position and previous byte; and by the previous byte and bit position) combine the
  stretched predictions, and a final mixer combines their outputs.
- **SSE.** Three adaptive probability maps refine the result using the partial byte, the
  previous byte and the previous two bytes.

All arithmetic is integer, so archives are identical across platforms, and the browser build
decodes the command-line tool's files and the other way round. Table sizes depend on the input
length, which is stored in the header.

## Usage

```sh
cargo build --release
./target/release/osmium c enwik8 enwik8.osm    # compress
./target/release/osmium d enwik8.osm enwik8.out # decompress
./target/release/osmium t enwik8                # compress, decompress in memory, compare
cargo test --release
```

The data: `curl -LO http://mattmahoney.net/dc/enwik9.zip && unzip enwik9.zip`, and
`head -c 100000000 enwik9 > enwik8`. SHA-256 of `enwik9`:
`159b85351e5f76e60cbe32e04c677847a9ecba3adc79addab6f4c6c7aa3744bc`.

## License

MIT, see [LICENSE](LICENSE).
