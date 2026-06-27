//! Synthetic N-record benchmark for `pxml`.
//!
//! Generates a `<trades>` document of N uniform records, then compares a
//! sequential pass against `par_for_each` across a range of thread counts,
//! reporting wall time, throughput, and speedup. Also demonstrates the
//! small-input sequential fallback.
//!
//! Run (release is important for meaningful numbers):
//!
//! ```sh
//! cargo run --release --example bench                # defaults: 200k records
//! cargo run --release --example bench -- 500000      # 500k records
//! cargo run --release --example bench -- 200000 1,4,8 # explicit thread counts
//! ```
//!
//! See `DESIGN.md` ("Verification plan"): expect sub-linear (~3–6x) scaling,
//! bounded by the sequential Phase A scan and memory bandwidth.

use std::fmt::Write as _;
use std::hint::black_box;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use pxml::{Config, Event, ParallelXml, Record};
use rayon::ThreadPoolBuilder;

fn main() {
    let mut args = std::env::args().skip(1);
    let n: usize = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(200_000);
    let threads: Vec<usize> = match args.next() {
        Some(list) => list.split(',').filter_map(|t| t.parse().ok()).collect(),
        None => default_thread_counts(),
    };

    let data = generate(n);
    println!(
        "document: {n} records, {:.1} MiB\n",
        data.len() as f64 / (1usize << 20) as f64
    );

    // Warm up caches, the allocator, and the global thread pool.
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

/// Run [`workload`] over every record, accumulating into a shared counter.
fn drive(px: &ParallelXml) -> u64 {
    let acc = AtomicU64::new(0);
    px.par_for_each(|rec| {
        acc.fetch_add(workload(rec), Ordering::Relaxed);
    })
    .expect("scan succeeds");
    acc.load(Ordering::Relaxed)
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

/// Force the parallel path regardless of input size.
fn parallel_config() -> Config {
    Config {
        parallel_threshold: 0,
        min_records: 0,
        ..Config::default()
    }
}

/// Force the sequential path (the per-record fallback loop) for the baseline.
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
    let mrec_s = records as f64 / secs / 1e6;
    let mib_s = bytes as f64 / secs / (1usize << 20) as f64;
    print!(
        "  {label:<26} {:>9.2} ms   {:>6.1} M rec/s   {:>7.0} MiB/s",
        secs * 1e3,
        mrec_s,
        mib_s,
    );
    match baseline {
        Some(b) => println!("   {:>5.2}x", b.as_secs_f64() / secs),
        None => println!(),
    }
}

/// Show that a small document transparently takes the sequential path.
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
