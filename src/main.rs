use std::process::ExitCode;
use std::time::Instant;

const USAGE: &str = "usage:
  osmium c <input> <output>    compress
  osmium d <input> <output>    decompress
  osmium t <input>             compress, decompress in memory, verify, report";

fn progress(total: usize, t0: Instant) -> impl FnMut(usize) {
    move |done| {
        if total > (16 << 20) && done > 0 {
            let s = t0.elapsed().as_secs_f64();
            eprint!("\r  {:5.1}%  {:6.2} MB/s", 100.0 * done as f64 / total as f64, done as f64 / s / 1e6);
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let read = |p: &str| std::fs::read(p).map_err(|e| format!("{p}: {e}"));
    let write = |p: &str, d: &[u8]| std::fs::write(p, d).map_err(|e| format!("{p}: {e}"));
    match args {
        [m, i, o] if m == "c" => {
            let data = read(i)?;
            let t0 = Instant::now();
            let out = osmium::compress(&data, progress(data.len(), t0));
            write(o, &out)?;
            report(data.len(), out.len(), t0.elapsed().as_secs_f64(), "compressed");
        }
        [m, i, o] if m == "d" => {
            let data = read(i)?;
            let t0 = Instant::now();
            let out = osmium::decompress(&data, progress(data.len() * 5, t0)).map_err(|e| format!("{i}: {e}"))?;
            write(o, &out)?;
            eprintln!("\rdecompressed {} -> {} bytes in {:.1} s", data.len(), out.len(), t0.elapsed().as_secs_f64());
        }
        [m, i] if m == "t" => {
            let data = read(i)?;
            let t0 = Instant::now();
            let out = osmium::compress(&data, progress(data.len(), t0));
            report(data.len(), out.len(), t0.elapsed().as_secs_f64(), "compressed");
            let t1 = Instant::now();
            let back = osmium::decompress(&out, progress(data.len(), t1)).map_err(|e| e.to_string())?;
            if back != data { return Err("ROUND TRIP FAILED: decompressed data differs".into()); }
            eprintln!("\rround trip OK, decompressed in {:.1} s", t1.elapsed().as_secs_f64());
        }
        _ => return Err(USAGE.into()),
    }
    Ok(())
}

fn report(n: usize, m: usize, secs: f64, what: &str) {
    let bpc = if n == 0 { 0.0 } else { m as f64 * 8.0 / n as f64 };
    eprintln!("\r{what} {n} -> {m} bytes ({bpc:.4} bits/byte, ratio {:.3}) in {secs:.1} s ({:.2} MB/s)",
        n as f64 / m.max(1) as f64, n as f64 / secs / 1e6);
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => { eprintln!("{e}"); ExitCode::FAILURE }
    }
}
