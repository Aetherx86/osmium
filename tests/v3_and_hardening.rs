//! Comprehensive tests for OSM v3 framing, presets, reversible text transform,
//! parallel/streaming differential equivalence, diagnostics, CLI, and hostile-input hardening.

use std::io::{self, Cursor, Read};
use std::process::{Command, Stdio};

use osmium::{checksum, transform, Error, Format, Options};

struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn next_u8(&mut self) -> u8 {
        self.next() as u8
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.next_u8()).collect()
    }
}

/// Reader wrapper that yields at most `max_chunk` bytes per `read` call,
/// exercising partial-read loops in streaming decompression/compression.
struct TrickleReader<'a> {
    data: &'a [u8],
    pos: usize,
    max_chunk: usize,
}

impl<'a> Read for TrickleReader<'a> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.data.len() {
            return Ok(0);
        }
        let n = buf
            .len()
            .min(self.max_chunk)
            .min(self.data.len() - self.pos);
        buf[..n].copy_from_slice(&self.data[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

fn wiki_sample(repeats: usize) -> Vec<u8> {
    let mut s = String::new();
    for i in 0..repeats {
        s.push_str(&format!(
            "<page><title>Article {i}: The History of Science</title>\
             <revision><timestamp>2026-11-24T00:{:02}:00Z</timestamp>\
             <contributor><username>User{}</username></contributor>\
             <text xml:space=\"preserve\">The [[University]] of [[History]] in the United States \
             published &quot;References&quot; and External Links at http://example.org/{i}. \
             #REDIRECT [[Category:Science]]</text></revision></page>\n",
            i % 60,
            i % 13
        ));
    }
    s.into_bytes()
}

// ------------------------------------------------------------------ property tests

#[test]
fn transform_property_bijection_random_and_adversarial() {
    let mut rng = XorShift64(0xDEAD_BEEF_CAFE_BABE);

    // 1. Completely arbitrary binary payloads (including reserved UTF-8 invalid bytes).
    for trial in 0..200 {
        let len = (rng.next() as usize) % 512;
        let data = rng.bytes(len);
        let enc = transform::encode(&data);
        let dec = transform::decode(&enc, data.len()).expect("arbitrary binary decode");
        assert_eq!(dec, data, "binary trial {trial}");
    }

    // 2. Adversarial alphabet concentrated on markers, escapes, and ASCII letters.
    let alphabet: &[u8] = &[
        b'a', b'b', b'z', b'A', b'B', b'Z', b' ', b'<', b'>', b'[', b']', 0xC0, 0xC1, 0xF5, 0xF6,
        0xFA, 0xFB, 0xFC, 0xFD, 0xFE, 0xFF, 0x00,
    ];
    for trial in 0..200 {
        let len = (rng.next() as usize) % 256;
        let data: Vec<u8> = (0..len)
            .map(|_| alphabet[(rng.next() as usize) % alphabet.len()])
            .collect();
        let enc = transform::encode(&data);
        let dec = transform::decode(&enc, data.len()).expect("adversarial decode");
        assert_eq!(dec, data, "adversarial trial {trial}");
    }

    // 3. Valid multibyte UTF-8 text (Korean, Japanese, Greek, accents, symbols).
    let utf8 = "Osmium 압축기 테스트: Wikipedia 위키백과 東京 Αλφαβήτο café naïve — HTTP/2 &amp; [[Category:Test]]\n".repeat(20);
    let enc = transform::encode(utf8.as_bytes());
    let dec = transform::decode(&enc, utf8.len()).expect("utf8 decode");
    assert_eq!(dec, utf8.as_bytes());
}

// ------------------------------------------------------------------ v3 presets, transform, differential

#[test]
fn v3_presets_and_explicit_table_bits_roundtrip() {
    let data = wiki_sample(40);
    for level in [1u8, 2, 3] {
        for text_transform in [false, true] {
            let opts = Options {
                format: Format::V3,
                level,
                table_bits: None,
                block_size: 0,
                threads: 1,
                text_transform,
            };
            let arc = osmium::compress_with_options(&data, &opts, |_| {}).unwrap();
            assert_eq!(&arc[..4], osmium::MAGIC_V3);
            let dec = osmium::decompress(&arc, |_| {}).unwrap();
            assert_eq!(dec, data);

            let info = osmium::inspect(&arc).unwrap();
            assert_eq!(info.format, Format::V3);
            assert_eq!(info.original_len, data.len() as u64);
            assert_eq!(info.text_transform, text_transform);
            assert_eq!(info.sha256, Some(checksum::sha256(&data)));
            assert!(!info.verified);

            let verified = osmium::verify(&arc, 1).unwrap();
            assert!(verified.verified);
            let json = verified.to_json();
            assert!(json.contains("\"format\":\"OSM3\""));
            assert!(json.contains("\"verified\":true"));
        }
    }

    // Explicit minimum table_bits = 12.
    let opts = Options {
        format: Format::V3,
        level: 0,
        table_bits: Some(12),
        block_size: 0,
        threads: 1,
        text_transform: true,
    };
    let arc = osmium::compress_with_options(&data, &opts, |_| {}).unwrap();
    assert_eq!(osmium::inspect(&arc).unwrap().table_bits, 12);
    assert_eq!(osmium::decompress(&arc, |_| {}).unwrap(), data);
}

#[test]
fn v3_empty_and_single_byte_roundtrip() {
    for text_transform in [false, true] {
        let opts = Options {
            format: Format::V3,
            level: 1,
            table_bits: None,
            block_size: 1024,
            threads: 2,
            text_transform,
        };
        for sample in [b"".as_slice(), b"x", b"A", &[0xFF], &[0xFE, 0xFD, 0xFC]] {
            let arc = osmium::compress_with_options(sample, &opts, |_| {}).unwrap();
            assert_eq!(osmium::decompress(&arc, |_| {}).unwrap(), sample);
            assert!(osmium::verify(&arc, 2).unwrap().verified);
        }
    }
}

#[test]
fn v3_parallel_matches_sequential_byte_for_byte() {
    let data = wiki_sample(120);
    let opts_1 = Options {
        format: Format::V3,
        level: 1,
        table_bits: None,
        block_size: 8192,
        threads: 1,
        text_transform: true,
    };
    let opts_4 = Options {
        threads: 4,
        ..opts_1.clone()
    };

    let arc_1 = osmium::compress_with_options(&data, &opts_1, |_| {}).unwrap();
    let arc_4 = osmium::compress_with_options(&data, &opts_4, |_| {}).unwrap();
    assert_eq!(
        arc_1, arc_4,
        "parallel and sequential v3 archives must be byte-identical"
    );

    let info = osmium::inspect(&arc_4).unwrap();
    assert!(info.blocks.len() > 3);

    let dec_1 = osmium::decompress_with_threads(&arc_4, 1, |_| {}).unwrap();
    let dec_4 = osmium::decompress_with_threads(&arc_4, 4, |_| {}).unwrap();
    assert_eq!(dec_1, data);
    assert_eq!(dec_4, data);
}

#[test]
fn streaming_differential_and_partial_reads() {
    let data = wiki_sample(80);
    let opts = Options {
        format: Format::V3,
        level: 1,
        table_bits: None,
        block_size: 4096,
        threads: 1,
        text_transform: true,
    };

    // Compress using a trickle reader (13 bytes at a time).
    let mut streamed_arc = Vec::new();
    let c_stats = osmium::compress_stream(
        TrickleReader {
            data: &data,
            pos: 0,
            max_chunk: 13,
        },
        &mut streamed_arc,
        &opts,
        |_| {},
    )
    .unwrap();
    assert_eq!(c_stats.input_bytes, data.len() as u64);
    assert_eq!(c_stats.output_bytes, streamed_arc.len() as u64);
    assert_eq!(c_stats.sha256, Some(checksum::sha256(&data)));

    // Slice API must decode streamed archive.
    assert_eq!(osmium::decompress(&streamed_arc, |_| {}).unwrap(), data);
    assert!(osmium::verify(&streamed_arc, 2).unwrap().verified);

    // Streaming decompress with trickle reader (7 bytes at a time).
    let mut restored = Vec::new();
    let d_stats = osmium::decompress_stream(
        TrickleReader {
            data: &streamed_arc,
            pos: 0,
            max_chunk: 7,
        },
        &mut restored,
        |_| {},
    )
    .unwrap();
    assert_eq!(restored, data);
    assert_eq!(d_stats.output_bytes, data.len() as u64);
    assert_eq!(d_stats.sha256, Some(checksum::sha256(&data)));

    // decompress_stream must also transparently decode legacy OSM v2 archives.
    let v2_arc = osmium::compress(&data[..2048], |_| {});
    let mut v2_restored = Vec::new();
    let v2_stats =
        osmium::decompress_stream(Cursor::new(&v2_arc), &mut v2_restored, |_| {}).unwrap();
    assert_eq!(v2_restored, &data[..2048]);
    assert_eq!(v2_stats.sha256, None);
}

#[test]
fn text_transform_improves_wiki_compression() {
    let data = wiki_sample(150);
    let raw_opts = Options {
        format: Format::V3,
        level: 1,
        table_bits: None,
        block_size: 0,
        threads: 1,
        text_transform: false,
    };
    let tx_opts = Options {
        text_transform: true,
        ..raw_opts.clone()
    };
    let raw_arc = osmium::compress_with_options(&data, &raw_opts, |_| {}).unwrap();
    let tx_arc = osmium::compress_with_options(&data, &tx_opts, |_| {}).unwrap();
    assert!(
        tx_arc.len() < raw_arc.len(),
        "expected text transform to shrink wiki XML: tx={} vs raw={}",
        tx_arc.len(),
        raw_arc.len()
    );
    assert_eq!(osmium::decompress(&tx_arc, |_| {}).unwrap(), data);
}

// ------------------------------------------------------------------ analyze diagnostics

#[test]
fn analyze_reports_consistent_entropy_and_context_stats() {
    let data = wiki_sample(50);
    let opts = Options::v3(1);
    let report = osmium::analyze(&data, &opts).unwrap();

    assert_eq!(report.input_bytes, data.len());
    assert_eq!(report.coded_bytes, data.len());
    assert_eq!(report.table_bits, 13);
    assert!(report.shannon_bpb > 3.0 && report.shannon_bpb < 8.0);
    assert!(report.coded_bpb > 0.0 && report.coded_bpb < report.shannon_bpb);
    // Arithmetic coding overhead over predictor cross-entropy should be tiny.
    assert!(
        (report.coded_bpb - report.stages.final_pred).abs() < 0.05,
        "coded_bpb={} vs final_pred={}",
        report.coded_bpb,
        report.stages.final_pred
    );
    assert_eq!(report.context_hit_rates.len(), 19);
    for (_, rate) in &report.context_hit_rates {
        assert!((0.0..=1.0).contains(rate));
    }
    assert!(report.short_match_hit_rate > 0.5);
    assert!(report.long_match_hit_rate > 0.3);

    let json = report.to_json();
    assert!(json.contains("\"shannon_bpb\":"));
    assert!(json.contains("\"context_hit_rates\":{"));
    assert!(json.contains("\"order0\":"));
    assert!(json.contains("\"prev_word_c1\":"));
}

// ------------------------------------------------------------------ hostile-input hardening

#[test]
fn v2_hostile_inputs_rejected_without_oom() {
    // Shorter than 16 bytes (12-byte header + 4-byte coder flush).
    for len in 0..16 {
        let mut buf = vec![0u8; len];
        if len >= 4 {
            buf[..4].copy_from_slice(osmium::MAGIC);
        }
        let err = osmium::decompress(&buf, |_| {}).unwrap_err();
        assert!(matches!(err, Error::NotOsmium | Error::Truncated));
    }

    // 32-bit truncation trap: n = (1 << 32) + 10 on a tiny buffer.
    let mut trap = vec![0u8; 64];
    trap[..4].copy_from_slice(osmium::MAGIC);
    trap[4..12].copy_from_slice(&((1u64 << 32) + 10).to_le_bytes());
    assert_eq!(osmium::decompress(&trap, |_| {}), Err(Error::Truncated));
}

#[test]
fn v3_hostile_mutations_and_truncations_rejected() {
    let data = wiki_sample(20);
    let opts = Options {
        format: Format::V3,
        level: 1,
        table_bits: None,
        block_size: 2048,
        threads: 1,
        text_transform: true,
    };
    let valid = osmium::compress_with_options(&data, &opts, |_| {}).unwrap();

    // Every strict prefix truncation must fail cleanly.
    for cut in 0..valid.len() {
        let res = osmium::decompress(&valid[..cut], |_| {});
        assert!(
            res.is_err(),
            "truncation at {cut}/{} unexpectedly succeeded",
            valid.len()
        );
    }

    // Trailing garbage byte rejected.
    let mut with_tail = valid.clone();
    with_tail.push(0x00);
    assert_eq!(
        osmium::decompress(&with_tail, |_| {}),
        Err(Error::TrailingData)
    );
    assert_eq!(
        osmium::decompress_stream(Cursor::new(&with_tail), &mut Vec::new(), |_| {}),
        Err(Error::TrailingData)
    );

    // Unsupported flag bit rejected.
    let mut bad_flags = valid.clone();
    bad_flags[4] |= 0x80;
    assert_eq!(
        osmium::decompress(&bad_flags, |_| {}),
        Err(Error::CorruptHeader)
    );

    // Invalid table_bits rejected (both too small and too large).
    for tb in [0u8, 11, 24, 255] {
        let mut bad_tb = valid.clone();
        bad_tb[5] = tb;
        assert_eq!(
            osmium::decompress(&bad_tb, |_| {}),
            Err(Error::CorruptHeader)
        );
    }

    // Non-zero reserved bytes rejected.
    let mut bad_res = valid.clone();
    bad_res[6] = 1;
    assert_eq!(
        osmium::decompress(&bad_res, |_| {}),
        Err(Error::CorruptHeader)
    );

    // Flip a bit in the stored CRC-32C of block 0 -> ChecksumMismatch.
    let mut bad_crc = valid.clone();
    bad_crc[20 + 12] ^= 0x01;
    assert_eq!(
        osmium::decompress(&bad_crc, |_| {}),
        Err(Error::ChecksumMismatch)
    );
    assert_eq!(
        osmium::decompress_stream(Cursor::new(&bad_crc), &mut Vec::new(), |_| {}),
        Err(Error::ChecksumMismatch)
    );

    // Flip a bit in the arithmetic payload of block 0 -> ChecksumMismatch / CorruptTransform / Truncated.
    let mut bad_payload = valid.clone();
    bad_payload[20 + 16 + 2] ^= 0x08;
    let err = osmium::decompress(&bad_payload, |_| {}).unwrap_err();
    assert!(
        matches!(
            err,
            Error::ChecksumMismatch | Error::CorruptTransform | Error::Truncated
        ),
        "unexpected error for payload bit-flip: {err:?}"
    );

    // Flip a bit in the final SHA-256 trailer -> ChecksumMismatch.
    let mut bad_sha = valid.clone();
    let last = bad_sha.len() - 1;
    bad_sha[last] ^= 0x01;
    assert_eq!(
        osmium::decompress(&bad_sha, |_| {}),
        Err(Error::ChecksumMismatch)
    );
    assert_eq!(
        osmium::decompress_stream(Cursor::new(&bad_sha), &mut Vec::new(), |_| {}),
        Err(Error::ChecksumMismatch)
    );

    // Corrupt total_len in trailer -> CorruptHeader.
    let mut bad_total = valid.clone();
    let total_off = bad_total.len() - 40;
    bad_total[total_off] ^= 0x01;
    assert_eq!(
        osmium::decompress(&bad_total, |_| {}),
        Err(Error::CorruptHeader)
    );

    // Fuzz-style random mutations must never panic.
    let mut rng = XorShift64(0x1234_5678_9ABC_DEF0);
    for _ in 0..150 {
        let mut mutated = valid.clone();
        let flips = 1 + (rng.next() as usize % 5);
        for _ in 0..flips {
            let idx = rng.next() as usize % mutated.len();
            mutated[idx] ^= 1u8 << (rng.next() % 8);
        }
        let _ = osmium::inspect(&mutated);
        let _ = osmium::decompress(&mutated, |_| {});
        let _ = osmium::decompress_stream(Cursor::new(&mutated), &mut Vec::new(), |_| {});
    }
}

#[test]
fn invalid_options_rejected() {
    let bad_v2_tx = Options {
        format: Format::V2,
        text_transform: true,
        ..Options::default()
    };
    assert!(matches!(
        osmium::compress_with_options(b"abc", &bad_v2_tx, |_| {}),
        Err(Error::InvalidOptions(_))
    ));

    let bad_level = Options::v3(9);
    assert!(matches!(
        osmium::compress_with_options(b"abc", &bad_level, |_| {}),
        Err(Error::InvalidOptions(_))
    ));

    let bad_tb = Options {
        format: Format::V3,
        table_bits: Some(11),
        ..Options::default()
    };
    assert!(matches!(
        osmium::compress_with_options(b"abc", &bad_tb, |_| {}),
        Err(Error::InvalidOptions(_))
    ));
}

// ------------------------------------------------------------------ CLI end-to-end tests

#[test]
fn cli_end_to_end_subcommands_and_pipes() {
    let bin = env!("CARGO_BIN_EXE_osmium");

    // --version
    let ver = Command::new(bin).arg("--version").output().unwrap();
    assert!(ver.status.success());
    let ver_out = String::from_utf8(ver.stdout).unwrap();
    assert!(ver_out.contains(osmium::VERSION));

    // Stdin -> stdout streaming roundtrip (`osmium c - - -l 1 -b 2K -x` | `osmium d - -`)
    let sample = wiki_sample(15);
    let mut c_child = Command::new(bin)
        .args(["c", "-", "-", "-l", "1", "-b", "2K", "-x"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        c_child.stdin.as_mut().unwrap().write_all(&sample).unwrap();
    }
    let c_out = c_child.wait_with_output().unwrap();
    assert!(
        c_out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&c_out.stderr)
    );
    assert_eq!(&c_out.stdout[..4], osmium::MAGIC_V3);

    let mut d_child = Command::new(bin)
        .args(["d", "-", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        use std::io::Write;
        d_child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(&c_out.stdout)
            .unwrap();
    }
    let d_out = d_child.wait_with_output().unwrap();
    assert!(d_out.status.success());
    assert_eq!(d_out.stdout, sample);

    // File-based inspect, verify, analyze, and JSON output in temp dir.
    let tmp = std::env::temp_dir().join(format!("osmium_cli_test_{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let in_path = tmp.join("sample.xml");
    let arc_path = tmp.join("sample.osm");
    let bad_path = tmp.join("corrupt.osm");
    std::fs::write(&in_path, &sample).unwrap();
    std::fs::write(&arc_path, &c_out.stdout).unwrap();

    let mut corrupt = c_out.stdout.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 0xFF;
    std::fs::write(&bad_path, &corrupt).unwrap();

    let info_out = Command::new(bin)
        .args(["i", arc_path.to_str().unwrap(), "--json"])
        .output()
        .unwrap();
    assert!(info_out.status.success());
    let info_json = String::from_utf8(info_out.stdout).unwrap();
    assert!(info_json.contains("\"format\":\"OSM3\""));
    assert!(info_json.contains("\"text_transform\":true"));

    let verify_ok = Command::new(bin)
        .args(["v", arc_path.to_str().unwrap(), "-j", "2", "--json"])
        .output()
        .unwrap();
    assert!(verify_ok.status.success());
    assert!(String::from_utf8(verify_ok.stdout)
        .unwrap()
        .contains("\"ok\":true"));

    let verify_bad = Command::new(bin)
        .args(["v", bad_path.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        !verify_bad.status.success(),
        "corrupt archive must fail verification"
    );

    let analyze_out = Command::new(bin)
        .args(["a", in_path.to_str().unwrap(), "-l", "1", "-x", "--json"])
        .output()
        .unwrap();
    assert!(analyze_out.status.success());
    let analyze_json = String::from_utf8(analyze_out.stdout).unwrap();
    assert!(analyze_json.contains("\"shannon_bpb\":"));
    assert!(analyze_json.contains("\"text_transform\":true"));

    let _ = std::fs::remove_dir_all(&tmp);
}
