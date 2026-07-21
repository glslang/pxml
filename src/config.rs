//! Configuration: parallelism thresholds and record framing.

/// Tuning knobs for parsing. Start from [`Config::default`] (or
/// [`Config::new`]) and chain the `with_*` builders, then pass the result to
/// [`ParallelXml::with_config`](crate::ParallelXml::with_config).
///
/// ```
/// use pxml::{Config, ParallelXml};
///
/// let config = Config::new()
///     .with_parallel_threshold(1 << 20) // 1 MiB
///     .with_min_records(32);
///
/// let doc = ParallelXml::from_bytes(&b"<rs><r>a</r></rs>"[..]).with_config(config);
/// # assert_eq!(doc.index().unwrap().len(), 1);
/// ```
///
/// The fields are private and reachable only through the builders and the
/// matching getters. That is deliberate: it means a future release can add a
/// knob without breaking callers, which an exhaustive struct literal would not
/// allow.
///
/// # The sequential fallback
///
/// Below **either** [`parallel_threshold`](Config::parallel_threshold) bytes or
/// [`min_records`](Config::min_records) records, the drivers transparently run a
/// single sequential pass — the thread-pool and indexing overhead does not repay
/// itself on small inputs. This is a performance switch only: results are
/// identical either way. To force the parallel path (in tests, say), set both to
/// `0`.
#[derive(Debug, Clone)]
pub struct Config {
    pub(crate) parallel_threshold: usize,
    pub(crate) min_records: usize,
    pub(crate) record_path: Vec<Box<str>>,
}

impl Config {
    /// A config with the default settings — equivalent to [`Config::default`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the buffer size (in bytes) below which parsing falls back to a
    /// sequential pass, because the thread-pool + chunk-index overhead loses to
    /// a plain `quick-xml` run on small inputs.
    ///
    /// Defaults to 4 MiB.
    pub fn with_parallel_threshold(mut self, bytes: usize) -> Self {
        self.parallel_threshold = bytes;
        self
    }

    /// Set the record count below which parsing falls back to a sequential pass,
    /// for the same reason as [`with_parallel_threshold`](Self::with_parallel_threshold).
    ///
    /// Defaults to 64.
    pub fn with_min_records(mut self, n: usize) -> Self {
        self.min_records = n;
        self
    }

    /// Set the element-name path from the root to the container whose direct
    /// children are the records. Empty (the default) means the root itself, i.e.
    /// the records are the root's direct children.
    ///
    /// Each entry is a qualified element name as written in the document,
    /// including any namespace prefix. Sibling nodes that do not match the next
    /// path step are skipped. For example, `["objects"]` frames the children of
    /// `<root>…<objects><object/>…</objects></root>`, skipping siblings such as
    /// `<manifest>`; `["body", "objects"]` descends two levels.
    ///
    /// This is the only way to set a record path on the resident
    /// [`ParallelXml`](crate::ParallelXml) reader;
    /// [`StreamReader::record_path`](crate::StreamReader::record_path) is the
    /// streaming equivalent (that type takes no `Config`).
    ///
    /// ```
    /// use pxml::{Config, ParallelXml};
    ///
    /// let xml = b"<root><manifest/><objects><object/><object/></objects></root>".to_vec();
    /// let config = Config::new().with_record_path(["objects"]);
    ///
    /// let doc = ParallelXml::from_bytes(xml).with_config(config);
    /// assert_eq!(doc.index()?.len(), 2);
    /// # Ok::<(), pxml::XmlError>(())
    /// ```
    pub fn with_record_path<I, S>(mut self, path: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<Box<str>>,
    {
        self.record_path = path.into_iter().map(Into::into).collect();
        self
    }

    /// The buffer size (in bytes) below which parsing falls back to a sequential
    /// pass. See [`with_parallel_threshold`](Self::with_parallel_threshold).
    pub fn parallel_threshold(&self) -> usize {
        self.parallel_threshold
    }

    /// The record count below which parsing falls back to a sequential pass.
    /// See [`with_min_records`](Self::with_min_records).
    pub fn min_records(&self) -> usize {
        self.min_records
    }

    /// The element-name path to the record container, empty if the records are
    /// the root's direct children. See [`with_record_path`](Self::with_record_path).
    pub fn record_path(&self) -> &[Box<str>] {
        &self.record_path
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_values() {
        let c = Config::default();
        assert_eq!(c.parallel_threshold(), 4 * 1024 * 1024);
        assert_eq!(c.min_records(), 64);
        assert!(c.record_path().is_empty());
    }

    #[test]
    fn new_matches_default() {
        let (a, b) = (Config::new(), Config::default());
        assert_eq!(a.parallel_threshold(), b.parallel_threshold());
        assert_eq!(a.min_records(), b.min_records());
        assert_eq!(a.record_path(), b.record_path());
    }

    /// Builders chain in any order and only touch their own field.
    #[test]
    fn builders_are_independent_and_chainable() {
        let c = Config::new()
            .with_min_records(7)
            .with_record_path(["a", "b"])
            .with_parallel_threshold(123);

        assert_eq!(c.parallel_threshold(), 123);
        assert_eq!(c.min_records(), 7);
        assert_eq!(c.record_path(), [Box::from("a"), Box::from("b")]);
    }

    /// A later call replaces the earlier value rather than accumulating.
    #[test]
    fn builders_overwrite_on_repeat() {
        let c = Config::new()
            .with_record_path(["first"])
            .with_record_path(["second"])
            .with_min_records(1)
            .with_min_records(2);

        assert_eq!(c.record_path(), [Box::from("second")]);
        assert_eq!(c.min_records(), 2);
    }

    /// `with_record_path` accepts the string types a caller actually has.
    #[test]
    fn record_path_accepts_common_string_types() {
        let from_str = Config::new().with_record_path(["objects"]);
        let from_string = Config::new().with_record_path(vec![String::from("objects")]);
        let from_boxed = Config::new().with_record_path(vec![Box::<str>::from("objects")]);

        assert_eq!(from_str.record_path(), from_string.record_path());
        assert_eq!(from_str.record_path(), from_boxed.record_path());
    }
}
