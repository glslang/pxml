//! Shared, immutable context captured once in Phase A and seeded into every
//! Phase B worker, so each record parses correctly even in isolation.

use std::collections::HashMap;

use quick_xml::escape::resolve_predefined_entity;

/// Resolved document encoding. The buffer is normalized to (or asserted as)
/// UTF-8 before slicing, so workers always see UTF-8.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Encoding {
    /// UTF-8 — the only encoding handled without transcoding in v1.
    #[default]
    Utf8,
}

/// Namespace declarations (`xmlns` / `xmlns:prefix`) in effect for the records,
/// captured from the root and from every element descended into on the way to
/// the record container (see [`Config::record_path`](crate::Config::record_path)).
/// Declarations made *inside* a record are local and are not stored here.
///
/// This is a single, flat context shared by all records (the [`Prelude`] is
/// immutable and shared by design). When `record_path` matches **multiple**
/// containers that redeclare the same prefix (or the default namespace) to
/// *different* URIs, the merge is last-writer-wins — the context cannot hold a
/// per-container scope. That is a non-issue for the uniform-records target (one
/// container, or containers that agree on their declarations); root- and
/// ancestor-declared namespaces are always correct.
#[derive(Debug, Clone, Default)]
pub struct NamespaceContext {
    /// Prefix (empty string = default namespace) -> namespace URI.
    decls: HashMap<Box<str>, Box<str>>,
}

impl NamespaceContext {
    /// An empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a declaration captured from the root/prolog.
    pub fn insert(&mut self, prefix: impl Into<Box<str>>, uri: impl Into<Box<str>>) {
        self.decls.insert(prefix.into(), uri.into());
    }

    /// Resolve a prefix to its namespace URI, if declared at the root/prolog.
    pub fn resolve(&self, prefix: &str) -> Option<&str> {
        self.decls.get(prefix).map(|s| &**s)
    }
}

/// Immutable context shared across all workers (via `Arc`). Built once in Phase A.
#[derive(Debug, Clone)]
pub struct Prelude {
    /// Resolved encoding of the source document.
    pub encoding: Encoding,
    /// The root element's qualified name.
    pub root_name: Box<str>,
    /// Namespace declarations in effect for every record.
    pub namespaces: NamespaceContext,
    /// Internal-subset `<!ENTITY>` definitions (name -> replacement text).
    pub entities: HashMap<Box<str>, Box<str>>,
}

impl Prelude {
    /// Resolve an entity reference by name for unescaping. Predefined XML
    /// entities (`lt`, `gt`, `amp`, `apos`, `quot`) take precedence; otherwise
    /// the internal-subset `<!ENTITY>` definitions captured in Phase A are used.
    ///
    /// Suitable as the resolver for `quick_xml`'s `unescape_with`.
    pub fn resolve_entity(&self, name: &str) -> Option<&str> {
        lookup_entity(&self.entities, name)
    }
}

/// Resolve an entity name against the predefined XML entities, then a custom
/// entity map. Shared by [`Prelude::resolve_entity`] and the sequential reader.
pub(crate) fn lookup_entity<'a>(
    entities: &'a HashMap<Box<str>, Box<str>>,
    name: &str,
) -> Option<&'a str> {
    if let Some(predefined) = resolve_predefined_entity(name) {
        return Some(predefined);
    }
    entities.get(name).map(|s| &**s)
}
