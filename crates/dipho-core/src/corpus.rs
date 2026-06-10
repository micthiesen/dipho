//! The Corpus: an addressable phonetic index over immutable sources.
//!
//! One SQLite database per corpus. Planned tables (see DESIGN.md): sources,
//! words (FTS5), phonemes, diphones (+ n-gram table), prosody, speakers.
//! Schema lands in milestone 1; this module is the handle and error type.

use std::path::Path;

use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub enum CorpusError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

/// Handle to a corpus database.
pub struct Corpus {
    conn: Connection,
}

impl Corpus {
    pub fn open(path: &Path) -> Result<Self, CorpusError> {
        Ok(Self {
            conn: Connection::open(path)?,
        })
    }

    pub fn open_in_memory() -> Result<Self, CorpusError> {
        Ok(Self {
            conn: Connection::open_in_memory()?,
        })
    }

    /// Schema version of the corpus database (SQLite `user_version`).
    /// 0 means uninitialized; migrations land in milestone 1.
    pub fn schema_version(&self) -> Result<i64, CorpusError> {
        Ok(self
            .conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
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
}
