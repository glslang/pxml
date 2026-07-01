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
    /// Element-name path from the root to the container whose direct children
    /// are the records. Empty (the default) means the root itself, i.e. the
    /// records are the root's direct children — the original behaviour.
    ///
    /// Each entry is a qualified element name (as written in the document,
    /// including any namespace prefix). Sibling nodes that do not match the next
    /// path step are skipped. For example, `["objects"]` frames the children of
    /// `<root>…<objects><object/>…</objects></root>`, skipping siblings such as
    /// `<manifest>`; `["body", "objects"]` descends two levels.
    pub record_path: Vec<Box<str>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            parallel_threshold: 4 * 1024 * 1024, // ~4 MiB
            min_records: 64,
            record_path: Vec::new(),
        }
    }
}
