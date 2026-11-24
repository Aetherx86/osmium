use std::io::{self, BufReader, BufWriter};
use std::process::ExitCode;
use std::time::Instant;

use osmium::{Format, Options};

const USAGE: &str = "usage:
  osmium c <input> <output> [options]   compress ('-' for stdin/stdout uses v3 streaming)
  osmium d <input> <output> [options]   decompress ('-' for stdin/stdout)
  osmium t <input> [options]            compress, decompress in memory, verify, report
  osmium i <archive> [--json]           inspect archive headers and blocks without decoding
  osmium v <archive> [options]          verify archive integrity (CRC-32C + SHA-256 for v3)
  osmium a <input> [options]            entropy & per-stage predictor diagnostics

options:
  --v2                    write legacy OSM v2 format (default for plain 'c'/'t' with no v3 flags)
  --v3                    write framed OSM v3 format (CRC-32C + SHA-256)
  -l, --level <0..7>      memory preset (0=auto, 1=~7 MiB .. 7=~9 GiB; implies --v3)
  --table-bits <12..23>   explicit context table exponent (implies --v3)
  -b, --block-size <size> block size, e.g. 256K, 1M, 4M (implies --v3)
  -j, --threads <n>       worker threads for multi-block v3 (0=auto)
  -x, --transform         reversible UTF-8/case/markup preprocessor (implies --v3)
  --json                  emit machine-readable JSON summary to stdout
  -V, --version           print version
  -h, --help              print this help";

fn parse_size(s: &str) -> Result<usize, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty size value".into());
    }
    let (num_part, mult) = match s.as_bytes().last().copied() {
        Some(b'k' | b'K') => (&s[..s.len() - 1], 1024usize),
        Some(b'm' | b'M') => (&s[..s.len() - 1], 1024 * 1024),
        Some(b'g' | b'G') => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1usize),
    };
    let n: usize = num_part
        .parse()
        .map_err(|_| format!("invalid size '{s}'"))?;
    n.checked_mul(mult)
        .ok_or_else(|| format!("size overflow '{s}'"))
}

struct ParsedFlags {
    positional: Vec<String>,
    options: Options,
    explicit_v2: bool,
    json: bool,
}

fn parse_flags(args: &[String]) -> Result<ParsedFlags, String> {
    let mut positional = Vec::new();
    let mut options = Options::default();
    let mut explicit_v2 = false;
    let mut explicit_v3 = false;
    let mut json = false;
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--v2" => {
                explicit_v2 = true;
                options.format = Format::V2;
            }
            "--v3" => {
                explicit_v3 = true;
                options.format = Format::V3;
            }
            "-x" | "--transform" => {
                explicit_v3 = true;
                options.text_transform = true;
            }
            "--json" => json = true,
            "-l" | "--level" => {
                i += 1;
                let v = args.get(i).ok_or("--level requires a value (0..7)")?;
                options.level = v.parse().map_err(|_| format!("invalid level '{v}'"))?;
                explicit_v3 = true;
            }
            "--table-bits" => {
                i += 1;
                let v = args
                    .get(i)
                    .ok_or("--table-bits requires a value (12..23)")?;
                options.table_bits =
                    Some(v.parse().map_err(|_| format!("invalid table-bits '{v}'"))?);
                explicit_v3 = true;
            }
            "-b" | "--block-size" => {
                i += 1;
                let v = args.get(i).ok_or("--block-size requires a value")?;
                options.block_size = parse_size(v)?;
                explicit_v3 = true;
            }
            "-j" | "--threads" => {
                i += 1;
                let v = args.get(i).ok_or("--threads requires a value")?;
                options.threads = v.parse().map_err(|_| format!("invalid threads '{v}'"))?;
            }
            arg if arg.starts_with('-') && arg != "-" => {
                return Err(format!("unknown option '{arg}'\n{USAGE}"));
            }
            arg => positional.push(arg.to_string()),
        }
        i += 1;
    }

    if explicit_v2 && explicit_v3 {
        return Err("cannot combine --v2 with v3-only options".into());
    }
    // Preserve v0.2.0 default format when no v3 flags were given.
    if !explicit_v3 {
        options.format = Format::V2;
    }

    Ok(ParsedFlags {
        positional,
        options,
        explicit_v2,
        json,
    })
}

fn progress(total: usize, t0: Instant, quiet: bool) -> impl FnMut(usize) {
    move |done| {
        if !quiet && total > (16 << 20) && done > 0 {
            let s = t0.elapsed().as_secs_f64();
            if s > 0.0 {
                eprint!(
                    "\r  {:5.1}%  {:6.2} MB/s",
                    100.0 * done as f64 / total as f64,
                    done as f64 / s / 1e6
                );
            }
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err(USAGE.into());
    }
    match args[0].as_str() {
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            return Ok(());
        }
        "-V" | "--version" | "version" => {
            println!("osmium {}", osmium::VERSION);
            return Ok(());
        }
        _ => {}
    }

    let cmd = args[0].as_str();
    let parsed = parse_flags(&args[1..])?;
    let read = |p: &str| std::fs::read(p).map_err(|e| format!("{p}: {e}"));
    let write = |p: &str, d: &[u8]| std::fs::write(p, d).map_err(|e| format!("{p}: {e}"));

    match (cmd, parsed.positional.as_slice()) {
        ("c", [i, o]) => {
            let streaming = i == "-" || o == "-";
            let t0 = Instant::now();
            if streaming {
                if parsed.explicit_v2 {
                    return Err("streaming via stdin/stdout requires OSM v3 (omit --v2)".into());
                }
                let mut opts = parsed.options.clone();
                opts.format = Format::V3;
                let stdin = io::stdin();
                let stdout = io::stdout();
                let stats = match (i.as_str(), o.as_str()) {
                    ("-", "-") => osmium::compress_stream(
                        BufReader::new(stdin.lock()),
                        BufWriter::new(stdout.lock()),
                        &opts,
                        |_| {},
                    ),
                    ("-", path) => {
                        let f = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
                        osmium::compress_stream(
                            BufReader::new(stdin.lock()),
                            BufWriter::new(f),
                            &opts,
                            |_| {},
                        )
                    }
                    (path, "-") => {
                        let f = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
                        osmium::compress_stream(
                            BufReader::new(f),
                            BufWriter::new(stdout.lock()),
                            &opts,
                            |_| {},
                        )
                    }
                    _ => unreachable!(),
                }
                .map_err(|e| e.to_string())?;
                let secs = t0.elapsed().as_secs_f64();
                if parsed.json && o != "-" {
                    let sha = stats
                        .sha256
                        .map(|d| format!("\"{}\"", osmium::checksum::hex_digest(&d)))
                        .unwrap_or_else(|| "null".into());
                    println!(
                        "{{\"command\":\"compress\",\"format\":\"OSM3\",\"input_bytes\":{},\"output_bytes\":{},\"blocks\":{},\"seconds\":{secs:.4},\"sha256\":{sha}}}",
                        stats.input_bytes, stats.output_bytes, stats.blocks
                    );
                } else {
                    report(
                        stats.input_bytes as usize,
                        stats.output_bytes as usize,
                        secs,
                        "compressed",
                    );
                }
            } else {
                let data = read(i)?;
                let out = osmium::compress_with_options(
                    &data,
                    &parsed.options,
                    progress(data.len(), t0, parsed.json),
                )
                .map_err(|e| e.to_string())?;
                write(o, &out)?;
                let secs = t0.elapsed().as_secs_f64();
                if parsed.json {
                    let info = osmium::inspect(&out).map_err(|e| e.to_string())?;
                    println!(
                        "{{\"command\":\"compress\",\"seconds\":{secs:.4},\"archive\":{}}}",
                        info.to_json()
                    );
                } else {
                    report(data.len(), out.len(), secs, "compressed");
                }
            }
        }
        ("d", [i, o]) => {
            let streaming = i == "-" || o == "-";
            let t0 = Instant::now();
            if streaming {
                let stdin = io::stdin();
                let stdout = io::stdout();
                let stats = match (i.as_str(), o.as_str()) {
                    ("-", "-") => osmium::decompress_stream(
                        BufReader::new(stdin.lock()),
                        BufWriter::new(stdout.lock()),
                        |_| {},
                    ),
                    ("-", path) => {
                        let f = std::fs::File::create(path).map_err(|e| format!("{path}: {e}"))?;
                        osmium::decompress_stream(
                            BufReader::new(stdin.lock()),
                            BufWriter::new(f),
                            |_| {},
                        )
                    }
                    (path, "-") => {
                        let f = std::fs::File::open(path).map_err(|e| format!("{path}: {e}"))?;
                        osmium::decompress_stream(
                            BufReader::new(f),
                            BufWriter::new(stdout.lock()),
                            |_| {},
                        )
                    }
                    _ => unreachable!(),
                }
                .map_err(|e| format!("{i}: {e}"))?;
                let secs = t0.elapsed().as_secs_f64();
                if parsed.json && o != "-" {
                    println!(
                        "{{\"command\":\"decompress\",\"input_bytes\":{},\"output_bytes\":{},\"blocks\":{},\"seconds\":{secs:.4}}}",
                        stats.input_bytes, stats.output_bytes, stats.blocks
                    );
                } else {
                    eprintln!(
                        "\rdecompressed {} -> {} bytes in {:.1} s",
                        stats.input_bytes, stats.output_bytes, secs
                    );
                }
            } else {
                let data = read(i)?;
                let out = osmium::decompress_with_threads(
                    &data,
                    parsed.options.threads,
                    progress(data.len() * 5, t0, parsed.json),
                )
                .map_err(|e| format!("{i}: {e}"))?;
                write(o, &out)?;
                let secs = t0.elapsed().as_secs_f64();
                if parsed.json {
                    println!(
                        "{{\"command\":\"decompress\",\"input_bytes\":{},\"output_bytes\":{},\"seconds\":{secs:.4}}}",
                        data.len(),
                        out.len()
                    );
                } else {
                    eprintln!(
                        "\rdecompressed {} -> {} bytes in {:.1} s",
                        data.len(),
                        out.len(),
                        secs
                    );
                }
            }
        }
        ("t", [i]) => {
            let data = read(i)?;
            let t0 = Instant::now();
            let out = osmium::compress_with_options(
                &data,
                &parsed.options,
                progress(data.len(), t0, parsed.json),
            )
            .map_err(|e| e.to_string())?;
            let c_secs = t0.elapsed().as_secs_f64();
            if !parsed.json {
                report(data.len(), out.len(), c_secs, "compressed");
            }
            let t1 = Instant::now();
            let back = osmium::decompress_with_threads(
                &out,
                parsed.options.threads,
                progress(data.len(), t1, parsed.json),
            )
            .map_err(|e| e.to_string())?;
            let d_secs = t1.elapsed().as_secs_f64();
            if back != data {
                return Err("ROUND TRIP FAILED: decompressed data differs".into());
            }
            if parsed.json {
                let mut info = osmium::inspect(&out).map_err(|e| e.to_string())?;
                info.verified = true;
                println!(
                    "{{\"command\":\"test\",\"roundtrip_ok\":true,\"compress_seconds\":{c_secs:.4},\"decompress_seconds\":{d_secs:.4},\"archive\":{}}}",
                    info.to_json()
                );
            } else {
                eprintln!("\rround trip OK, decompressed in {:.1} s", d_secs);
            }
        }
        ("i" | "info", [archive]) => {
            let data = read(archive)?;
            let info = osmium::inspect(&data).map_err(|e| format!("{archive}: {e}"))?;
            if parsed.json {
                println!("{}", info.to_json());
            } else {
                let fmt = match info.format {
                    Format::V2 => "OSM v2",
                    Format::V3 => "OSM v3",
                };
                println!("format:       {fmt}");
                println!("original:     {} bytes", info.original_len);
                println!(
                    "compressed:   {} bytes ({:.4} bits/byte, ratio {:.3})",
                    info.compressed_len,
                    info.bits_per_byte(),
                    info.ratio()
                );
                println!("table_bits:   {}", info.table_bits);
                if info.format == Format::V3 {
                    println!("transform:    {}", info.text_transform);
                    println!("block_size:   {}", info.block_size);
                    println!("blocks:       {}", info.blocks.len());
                    if let Some(d) = &info.sha256 {
                        println!("sha256:       {}", osmium::checksum::hex_digest(d));
                    }
                }
            }
        }
        ("v" | "verify", [archive]) => {
            let data = read(archive)?;
            let t0 = Instant::now();
            let info = osmium::verify(&data, parsed.options.threads)
                .map_err(|e| format!("{archive}: {e}"))?;
            let secs = t0.elapsed().as_secs_f64();
            if parsed.json {
                println!(
                    "{{\"command\":\"verify\",\"ok\":true,\"seconds\":{secs:.4},\"archive\":{}}}",
                    info.to_json()
                );
            } else {
                let fmt = match info.format {
                    Format::V2 => "OSM v2",
                    Format::V3 => "OSM v3",
                };
                eprintln!(
                    "verified OK ({fmt}, {} -> {} bytes, {:.4} bits/byte) in {:.2} s",
                    info.original_len,
                    info.compressed_len,
                    info.bits_per_byte(),
                    secs
                );
            }
        }
        ("a" | "analyze", [input]) => {
            let data = read(input)?;
            let mut opts = parsed.options.clone();
            opts.format = Format::V3;
            let report = osmium::analyze(&data, &opts).map_err(|e| e.to_string())?;
            if parsed.json {
                println!("{}", report.to_json());
            } else {
                println!(
                    "input:        {} bytes (coded: {} bytes, transform: {}, table_bits: {})",
                    report.input_bytes,
                    report.coded_bytes,
                    report.text_transform,
                    report.table_bits
                );
                println!(
                    "entropy:      {:.4} bits/byte (order-0 Shannon) -> {:.4} bits/byte (coded, {} bytes)",
                    report.shannon_bpb, report.coded_bpb, report.compressed_bytes
                );
                let s = &report.stages;
                println!(
                    "mixers (bpb): m1={:.4} m2={:.4} m3={:.4} m4={:.4} -> mf={:.4}",
                    s.mixer1, s.mixer2, s.mixer3, s.mixer4, s.mixer_final
                );
                println!(
                    "sse (bpb):    a1={:.4} a2={:.4} a3={:.4} a4={:.4} ms={:.4} -> final={:.4}",
                    s.apm1, s.apm2, s.apm3, s.apm4, s.sse_mixer, s.final_pred
                );
                println!(
                    "matches:      short={:.1}% (avg {:.1} B), long={:.1}% (avg {:.1} B)",
                    report.short_match_hit_rate * 100.0,
                    report.short_match_avg_len,
                    report.long_match_hit_rate * 100.0,
                    report.long_match_avg_len
                );
                print!("ctx hit rate:");
                for (name, rate) in &report.context_hit_rates {
                    print!(" {name}={:.0}%", rate * 100.0);
                }
                println!();
            }
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

fn report(n: usize, m: usize, secs: f64, what: &str) {
    let bpc = if n == 0 {
        0.0
    } else {
        m as f64 * 8.0 / n as f64
    };
    let mb_s = if secs > 0.0 {
        n as f64 / secs / 1e6
    } else {
        0.0
    };
    eprintln!(
        "\r{what} {n} -> {m} bytes ({bpc:.4} bits/byte, ratio {:.3}) in {secs:.1} s ({mb_s:.2} MB/s)",
        n as f64 / m.max(1) as f64
    );
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
