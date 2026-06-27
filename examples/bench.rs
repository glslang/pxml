//! Synthetic N-record benchmark for `pxml`.
//!
//! Three modes:
//!
//! ```sh
//! # In-memory: sequential baseline vs par_for_each across thread counts.
//! cargo run --release --example bench                 # 200k records
//! cargo run --release --example bench -- 500000 1,4,8 # N records, explicit threads
//!
//! # Write a zstd-compressed file of N records.
//! cargo run --release --example bench -- gen 1000000 trades.xml.zst
//!
//! # Benchmark parsing a file: resident path (from_path, decompress-whole) vs
//! # the bounded-memory streaming path (from_zstd_reader) for zstd inputs.
//! cargo run --release --example bench -- file trades.xml.zst
//! ```
//!
//! See `DESIGN.md` ("Verification plan"): expect sub-linear (~3–6x) scaling,
//! bounded by the sequential Phase A scan / decompression and memory bandwidth.

use std::fmt::Write as _;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pxml::{Config, Event, ParallelXml, Record};
use rayon::ThreadPoolBuilder;

const MIB: f64 = (1u64 << 20) as f64;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("gen") => gen_mode(&args[1..]),
        Some("file") => file_mode(&args[1..]),
        _ => synthetic_mode(&args),
    }
}

// --- Shared workload ------------------------------------------------------

/// Parse one record end-to-end and fold its content into a checksum, so the work
/// (name handling, attribute entity-decode, text) is not optimized away.
fn workload(rec: &Record) -> u64 {
    let mut reader = rec.events();
    let mut acc: u64 = 0;
    while let Some(ev) = reader.next_event().expect("well-formed record") {
        match ev {
            Event::Start { name, attrs } => {
                acc = acc.wrapping_add(name.as_ref().len() as u64);
                for attr in attrs.iter() {
                    let attr = attr.expect("valid attribute");
                    acc = acc.wrapping_add(attr.value.len() as u64);
                }
            }
            Event::Text(t) => acc = acc.wrapping_add(t.len() as u64),
            Event::Cdata(c) => acc = acc.wrapping_add(c.len() as u64),
            Event::End { .. } => {}
        }
    }
    acc
}

fn generate(n: usize) -> Vec<u8> {
    let mut s = String::with_capacity(n * 96 + 32);
    s.push_str("<trades>\n");
    for i in 0..n {
        let _ = write!(
            s,
            "<trade id=\"{i}\" sym=\"AAPL\">\
             <px>{}.{:02}</px><qty>{}</qty><note>fill &amp; done</note></trade>\n",
            100 + i % 900,
            i % 100,
            1 + i % 1000,
        );
    }
    s.push_str("</trades>\n");
    s.into_bytes()
}

// --- In-memory synthetic mode ---------------------------------------------

fn synthetic_mode(args: &[String]) {
    let n: usize = args.first().and_then(|a| a.parse().ok()).unwrap_or(200_000);
    let threads: Vec<usize> = match args.get(1) {
        Some(list) => list.split(',').filter_map(|t| t.parse().ok()).collect(),
        None => default_thread_counts(),
    };

    let data = generate(n);
    println!("document: {n} records, {:.1} MiB\n", data.len() as f64 / MIB);

    black_box(drive(&build(&data, parallel_config())));

    let baseline = time(|| drive(&build(&data, sequential_config())));
    report("sequential (fallback path)", baseline, n, data.len(), None);

    for &t in &threads {
        let px = build(&data, parallel_config());
        let pool = ThreadPoolBuilder::new()
            .num_threads(t)
            .build()
            .expect("thread pool");
        let elapsed = time(|| black_box(pool.install(|| drive(&px))));
        let label = format!("parallel ({t} thread{})", if t == 1 { "" } else { "s" });
        report(&label, elapsed, n, data.len(), Some(baseline));
    }

    fallback_demo();
}

fn drive(px: &ParallelXml) -> u64 {
    let acc = AtomicU64::new(0);
    px.par_for_each(|rec| {
        acc.fetch_add(workload(rec), Ordering::Relaxed);
    })
    .expect("scan succeeds");
    acc.load(Ordering::Relaxed)
}

fn parallel_config() -> Config {
    Config {
        parallel_threshold: 0,
        min_records: 0,
        ..Config::default()
    }
}

fn sequential_config() -> Config {
    Config {
        parallel_threshold: usize::MAX,
        min_records: usize::MAX,
        ..Config::default()
    }
}

fn build(data: &[u8], config: Config) -> ParallelXml {
    ParallelXml::from_bytes(data.to_vec()).with_config(config)
}

fn time<T>(f: impl FnOnce() -> T) -> Duration {
    let start = Instant::now();
    black_box(f());
    start.elapsed()
}

fn report(label: &str, elapsed: Duration, records: usize, bytes: usize, baseline: Option<Duration>) {
    let secs = elapsed.as_secs_f64();
    print!(
        "  {label:<28} {:>9.2} ms   {:>6.1} M rec/s   {:>7.0} MiB/s",
        secs * 1e3,
        records as f64 / secs / 1e6,
        bytes as f64 / secs / MIB,
    );
    match baseline {
        Some(b) => println!("   {:>5.2}x", b.as_secs_f64() / secs),
        None => println!(),
    }
}

fn fallback_demo() {
    let small = generate(8);
    let cfg = Config::default();
    let to_sequential = small.len() < cfg.parallel_threshold || 8 < cfg.min_records;
    println!(
        "\nsmall-input fallback: 8 records / {} bytes vs defaults \
         (parallel_threshold = {} bytes, min_records = {}) -> sequential = {to_sequential}",
        small.len(),
        cfg.parallel_threshold,
        cfg.min_records,
    );
    black_box(drive(&ParallelXml::from_bytes(small)));
}

fn default_thread_counts() -> Vec<usize> {
    let max = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(8);
    let mut counts: Vec<usize> = [1, 2, 4, 8].into_iter().filter(|&t| t < max).collect();
    counts.push(max);
    counts
}

// --- File modes (zstd) ----------------------------------------------------

#[cfg(feature = "zstd")]
fn gen_mode(args: &[String]) {
    let n: usize = args[0].parse().expect("usage: gen <N> <path.zst>");
    let path = &args[1];
    let xml = generate(n);
    let compressed = zstd::encode_all(xml.as_slice(), 3).expect("compress");
    std::fs::write(path, &compressed).expect("write file");
    println!(
        "wrote {path}: {n} records, {:.1} MiB raw -> {:.1} MiB zstd ({:.1}x)",
        xml.len() as f64 / MIB,
        compressed.len() as f64 / MIB,
        xml.len() as f64 / compressed.len() as f64,
    );
}

#[cfg(feature = "zstd")]
fn file_mode(args: &[String]) {
    use pxml::StreamReader;
    use std::path::Path;

    let path = Path::new(&args[0]);
    let raw = std::fs::read(path).expect("read file");
    let is_zstd = raw.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]);
    let decompressed_len = if is_zstd {
        zstd::decode_all(raw.as_slice()).expect("decode").len()
    } else {
        raw.len()
    };
    println!(
        "file: {} — {:.1} MiB on disk, {:.1} MiB decompressed\n",
        path.display(),
        raw.len() as f64 / MIB,
        decompressed_len as f64 / MIB,
    );

    // Resident: from_path decompresses the whole document up front, then parses
    // it in parallel. Timing includes decompression.
    let count = AtomicU64::new(0);
    let resident = time(|| {
        let acc = AtomicU64::new(0);
        let doc = ParallelXml::from_path(path)
            .expect("open")
            .with_config(parallel_config());
        doc.par_for_each(|rec| {
            acc.fetch_add(workload(rec), Ordering::Relaxed);
            count.fetch_add(1, Ordering::Relaxed);
        })
        .expect("parse");
        black_box(acc.load(Ordering::Relaxed));
    });
    let records = count.load(Ordering::Relaxed) as usize;
    report_file("resident (from_path)", resident, records, decompressed_len);

    if is_zstd {
        let streaming = time(|| {
            let acc = AtomicU64::new(0);
            let reader = StreamReader::from_zstd_reader(std::fs::File::open(path).expect("open"))
                .expect("zstd reader");
            reader
                .par_for_each(|rec| {
                    acc.fetch_add(workload(rec), Ordering::Relaxed);
                })
                .expect("stream");
            black_box(acc.load(Ordering::Relaxed));
        });
        report_file("streaming (from_zstd_reader)", streaming, records, decompressed_len);
    }
}

#[cfg(feature = "zstd")]
fn report_file(label: &str, elapsed: Duration, records: usize, decompressed: usize) {
    let secs = elapsed.as_secs_f64();
    println!(
        "  {label:<30} {:>9.2} ms   {:>6.2} M rec/s   {:>7.0} MiB/s (decompressed)",
        secs * 1e3,
        records as f64 / secs / 1e6,
        decompressed as f64 / secs / MIB,
    );
}

#[cfg(not(feature = "zstd"))]
fn gen_mode(_: &[String]) {
    eprintln!("`gen` mode requires the `zstd` feature (enabled by default)");
}

#[cfg(not(feature = "zstd"))]
fn file_mode(_: &[String]) {
    eprintln!("`file` mode requires the `zstd` feature (enabled by default)");
}
