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

/// Namespace declarations (`xmlns` / `xmlns:prefix`) captured from the root or
/// prolog and applied to every record. Declarations made *inside* a record are
/// local and are not stored here.
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
#[derive(Debug)]
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
        if let Some(predefined) = resolve_predefined_entity(name) {
            return Some(predefined);
        }
        self.entities.get(name).map(|s| &**s)
    }
}
