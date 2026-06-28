//! Configuration: parallelism thresholds.

/// Tuning knobs for parsing. Construct with [`Config::default`] and override
/// fields, then pass to [`ParallelXml::with_config`](crate::ParallelXml::with_config).
#[derive(Debug, Clone)]
pub struct Config {
    /// Below this buffer size (in bytes), parsing transparently falls back to a
    /// sequential pass — the thread-pool + chunk-index overhead loses to a plain
    /// `quick-xml` run on small inputs.
    pub parallel_threshold: usize,
    /// Below this record count, parsing transparently falls back to a sequential
    /// pass for the same reason.
    pub min_records: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            parallel_threshold: 4 * 1024 * 1024, // ~4 MiB
            min_records: 64,
        }
    }
}
