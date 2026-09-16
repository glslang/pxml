//! Configuration: parallelism thresholds, record framing, and streaming
//! pipeline sizing.

/// Tuning knobs for parsing. Start from [`Config::default`] (or
/// [`Config::new`]) and chain the `with_*` builders, then pass the result to
/// [`ParallelXml::with_config`](crate::ParallelXml::with_config) — or, for the
/// `stream_*` knobs, to
/// [`StreamReader::with_config`](crate::StreamReader::with_config).
///
/// The knobs split by execution path: `parallel_threshold` / `min_records` gate
/// the resident path's sequential fallback and are ignored by the streaming
/// pipeline, while the `stream_*` knobs size that pipeline and are ignored by
/// the resident path. [`record_path`](Config::record_path) applies to both.
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
    pub(crate) stream_batch_records: usize,
    pub(crate) stream_batch_bytes: usize,
    pub(crate) stream_queue_capacity: usize,
    pub(crate) stream_workers: usize,
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
    /// streaming shorthand for the same setting.
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

    /// Cap the number of records a [`StreamReader`](crate::StreamReader) packs
    /// into one pipeline message. Batching amortizes the channel send and the
    /// receiver mutex over many records; a batch is dispatched as soon as
    /// *either* this cap or [`with_stream_batch_bytes`](Self::with_stream_batch_bytes)
    /// is reached.
    ///
    /// Defaults to 256. `0` is treated as 1 (one record per message).
    pub fn with_stream_batch_records(mut self, n: usize) -> Self {
        self.stream_batch_records = n;
        self
    }

    /// Cap a [`StreamReader`](crate::StreamReader) batch by the bytes of record
    /// data it carries, so that large records do not turn a
    /// [record cap](Self::with_stream_batch_records) into a large allocation.
    ///
    /// A single record larger than the cap is still dispatched on its own — the
    /// pipeline never stalls on it.
    ///
    /// Defaults to 1 MiB. `0` is treated as 1 (one record per message).
    pub fn with_stream_batch_bytes(mut self, bytes: usize) -> Self {
        self.stream_batch_bytes = bytes;
        self
    }

    /// Set how many worker threads a [`StreamReader`](crate::StreamReader)
    /// runs alongside its framing producer.
    ///
    /// Defaults to `0`, meaning "derive from the pool": `rayon`'s current thread
    /// count, so `pool.install(|| reader.par_for_each(f))` sizes the pipeline
    /// from that pool. Set it explicitly to size the pipeline independently of
    /// `rayon` — the workers are plain threads, not pool tasks.
    pub fn with_stream_workers(mut self, n: usize) -> Self {
        self.stream_workers = n;
        self
    }

    /// Set the capacity of the [`StreamReader`](crate::StreamReader) channel —
    /// how many framed batches may be in flight before the producer blocks. This
    /// is the pipeline's backpressure knob, so it also bounds resident memory:
    /// roughly `capacity × batch size` on top of the chunk being framed.
    ///
    /// Defaults to `0`, meaning "derive from the pool": twice rayon's current
    /// thread count.
    pub fn with_stream_queue_capacity(mut self, batches: usize) -> Self {
        self.stream_queue_capacity = batches;
        self
    }

    /// The element-name path to the record container, empty if the records are
    /// the root's direct children. See [`with_record_path`](Self::with_record_path).
    pub fn record_path(&self) -> &[Box<str>] {
        &self.record_path
    }

    /// The per-batch record cap for streaming. See
    /// [`with_stream_batch_records`](Self::with_stream_batch_records).
    pub fn stream_batch_records(&self) -> usize {
        self.stream_batch_records
    }

    /// The per-batch byte cap for streaming. See
    /// [`with_stream_batch_bytes`](Self::with_stream_batch_bytes).
    pub fn stream_batch_bytes(&self) -> usize {
        self.stream_batch_bytes
    }

    /// The streaming channel capacity in batches, `0` meaning "derive from the
    /// pool". See [`with_stream_queue_capacity`](Self::with_stream_queue_capacity).
    pub fn stream_queue_capacity(&self) -> usize {
        self.stream_queue_capacity
    }

    /// The streaming worker-thread count, `0` meaning "derive from the pool".
    /// See [`with_stream_workers`](Self::with_stream_workers).
    pub fn stream_workers(&self) -> usize {
        self.stream_workers
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            parallel_threshold: 4 * 1024 * 1024, // ~4 MiB
            min_records: 64,
            record_path: Vec::new(),
            stream_batch_records: 256,
            stream_batch_bytes: 1024 * 1024, // 1 MiB
            stream_queue_capacity: 0,        // derive from the rayon pool
            stream_workers: 0,               // derive from the rayon pool
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
        assert_eq!(c.stream_batch_records(), 256);
        assert_eq!(c.stream_batch_bytes(), 1024 * 1024);
        assert_eq!(c.stream_queue_capacity(), 0);
        assert_eq!(c.stream_workers(), 0);
    }

    #[test]
    fn stream_builders_round_trip_through_the_getters() {
        let c = Config::new()
            .with_stream_batch_records(8)
            .with_stream_batch_bytes(4096)
            .with_stream_queue_capacity(3)
            .with_stream_workers(2);

        assert_eq!(c.stream_batch_records(), 8);
        assert_eq!(c.stream_batch_bytes(), 4096);
        assert_eq!(c.stream_queue_capacity(), 3);
        assert_eq!(c.stream_workers(), 2);
        // ... and leave the resident-path knobs alone.
        assert_eq!(
            c.parallel_threshold(),
            Config::default().parallel_threshold()
        );
        assert_eq!(c.min_records(), Config::default().min_records());
    }

    #[test]
    fn new_matches_default() {
        let (a, b) = (Config::new(), Config::default());
        assert_eq!(a.parallel_threshold(), b.parallel_threshold());
        assert_eq!(a.min_records(), b.min_records());
        assert_eq!(a.record_path(), b.record_path());
        assert_eq!(a.stream_batch_records(), b.stream_batch_records());
        assert_eq!(a.stream_batch_bytes(), b.stream_batch_bytes());
        assert_eq!(a.stream_queue_capacity(), b.stream_queue_capacity());
        assert_eq!(a.stream_workers(), b.stream_workers());
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
