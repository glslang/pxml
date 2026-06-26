//! Synthetic N-record benchmark (scaffold).
//!
//! Will generate a synthetic file of 10k–1M `<trade>` records and compare
//! `ParallelXml::sequential` vs `par_for_each` across thread counts, confirming
//! the small-input fallback and the ~3–6× ceiling. See `DESIGN.md`
//! ("Verification plan").

fn main() {
    eprintln!("pxml bench: not yet implemented");
}
