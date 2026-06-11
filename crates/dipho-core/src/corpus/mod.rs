//! The Corpus: an addressable phonetic index over immutable sources.
//!
//! One SQLite database per corpus (rusqlite, bundled). Sources are never
//! edited, only indexed; everything downstream is a span reference. The
//! loader consumes a sidecar `manifest.json` plus prosody frames and derives
//! all units (utterances → words → phones → diphones) — loader logic, not
//! sidecar (see DESIGN.md and python/README.md for the contract).

mod diphones;
mod features;
mod loader;
pub mod manifest;
mod normalize;
mod npz;
mod phones;
mod schema;
mod search;
mod speakers;

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

pub use features::ProsodyData;
pub use loader::{LoadReport, SourceMeta};
pub use manifest::Manifest;
pub use normalize::normalize_query;
pub use npz::prosody_from_npz;
pub use schema::SCHEMA_VERSION;
pub use search::{SearchHit, WordSpan};

#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("malformed manifest: {0}")]
    ManifestJson(#[from] serde_json::Error),
    #[error("unsupported manifest schema_version {0}")]
    UnknownManifestVersion(u32),
    #[error("corpus schema version {found} is newer than this build supports ({supported})")]
    SchemaTooNew { found: i64, supported: i64 },
    #[error(
        "corpus schema version {found} is behind this build ({supported}) — run `dipho ingest` to migrate"
    )]
    SchemaStale { found: i64, supported: i64 },
    #[error("unknown phone label {label:?}")]
    UnknownPhoneLabel { label: String },
    #[error("prosody frames disagree with the manifest: {0}")]
    FrameMismatch(String),
    #[error("malformed prosody npz: {0}")]
    Npz(String),
    #[error("{what} index {index} is out of range")]
    IndexOutOfRange { what: &'static str, index: usize },
    #[error("{what} has invalid interval [{t_start}, {t_end}]")]
    InvalidInterval {
        what: &'static str,
        t_start: f64,
        t_end: f64,
    },
    #[error("manifest contract violation: {0}")]
    Contract(String),
}

/// Handle to a corpus database. All writes in a process go through one of
/// these — the single-writer-per-process topology from DESIGN.md.
pub struct Corpus {
    conn: Connection,
}

impl Corpus {
    pub fn open(path: &Path) -> Result<Self, CorpusError> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self, CorpusError> {
        Self::init(Connection::open_in_memory()?)
    }

    /// Read-only handle for search and other queries — fails (rather than
    /// creating an empty database) when the corpus doesn't exist, and can
    /// never contend for the write lock.
    pub fn open_read_only(path: &Path) -> Result<Self, CorpusError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        Self::init(Connection::open_with_flags(path, flags)?)
    }

    fn init(conn: Connection) -> Result<Self, CorpusError> {
        // Per-connection pragmas only; WAL is persistent and set by
        // migration v1, so read-only connections never need to write it.
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self { conn })
    }

    /// Schema version of the corpus database (SQLite `user_version`).
    /// 0 means uninitialized.
    pub fn schema_version(&self) -> Result<i64, CorpusError> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    /// Run pending migrations. Idempotent; errors if the database was
    /// written by a newer schema than this build knows.
    pub fn migrate(&mut self) -> Result<(), CorpusError> {
        schema::migrate(&mut self.conn)
    }

    /// Typed check that this corpus is at exactly the supported schema
    /// version — for read-only consumers, which cannot migrate.
    pub fn ensure_schema_current(&self) -> Result<(), CorpusError> {
        let found = self.schema_version()?;
        match found {
            v if v > SCHEMA_VERSION => Err(CorpusError::SchemaTooNew {
                found,
                supported: SCHEMA_VERSION,
            }),
            v if v < SCHEMA_VERSION => Err(CorpusError::SchemaStale {
                found,
                supported: SCHEMA_VERSION,
            }),
            _ => Ok(()),
        }
    }

    /// Word or phrase search over the utterance FTS5 index. The query is
    /// normalized exactly like the index ("25" finds "twenty five"); hits
    /// map back to exact word spans.
    pub fn search(&self, query: &str) -> Result<Vec<SearchHit>, CorpusError> {
        search::search(&self.conn, query)
    }

    /// Every source in the corpus, as the edit-rebind / source-map input.
    pub fn sources(&self) -> Result<Vec<crate::edl::CorpusSource>, CorpusError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, origin, origin_id, master_path, master_hash, duration, fps
             FROM sources ORDER BY id",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(crate::edl::CorpusSource {
                id: crate::span::SourceId(row.get(0)?),
                origin: row.get(1)?,
                origin_id: row.get(2)?,
                master_path: row.get(3)?,
                master_hash: row.get(4)?,
                duration: row.get(5)?,
                fps: row.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Source id for an origin_id, if it was ingested before. The
    /// pre-download idempotency check: ingest of a known origin_id is a
    /// no-op without `--force`.
    pub fn find_source(&self, origin_id: &str) -> Result<Option<i64>, CorpusError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM sources WHERE origin_id = ?1",
                [origin_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Load one source's sidecar output into the corpus and derive all
    /// units. A source with the same `origin_id` is re-ingested: derived
    /// rows are replaced in one transaction and named speakers carry
    /// forward.
    pub fn load_source(
        &mut self,
        meta: &SourceMeta,
        manifest: &Manifest,
        prosody: &ProsodyData,
    ) -> Result<LoadReport, CorpusError> {
        loader::load_source(&mut self.conn, meta, manifest, prosody)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_corpus_is_uninitialized() {
        let corpus = Corpus::open_in_memory().unwrap();
        assert_eq!(corpus.schema_version().unwrap(), 0);
    }

    #[test]
    fn migrate_reaches_current_version_and_is_idempotent() {
        let mut corpus = Corpus::open_in_memory().unwrap();
        corpus.migrate().unwrap();
        assert_eq!(corpus.schema_version().unwrap(), SCHEMA_VERSION);
        corpus.migrate().unwrap();
        assert_eq!(corpus.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn migrate_rejects_newer_schema() {
        let mut corpus = Corpus::open_in_memory().unwrap();
        corpus
            .conn
            .pragma_update(None, "user_version", 999)
            .unwrap();
        assert!(matches!(
            corpus.migrate(),
            Err(CorpusError::SchemaTooNew { found: 999, .. })
        ));
    }
}
