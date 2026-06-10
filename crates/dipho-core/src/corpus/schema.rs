//! Schema migrations. Migration v1 creates the full corpus schema per
//! DESIGN.md — post-MVP features (solver, join cost) must be servable from
//! it without rework, which is why boundary features, the mfcc frame
//! substrate, `cut_t`, `seq`, and provenance ship here.

use rusqlite::Connection;

use super::CorpusError;

/// Current corpus schema version (SQLite `user_version`).
pub const SCHEMA_VERSION: i64 = 1;

const SQL_V1: &str = r#"
CREATE TABLE sources (
    id            INTEGER PRIMARY KEY,
    origin        TEXT NOT NULL,
    origin_id     TEXT NOT NULL UNIQUE,
    original_path TEXT,
    master_path   TEXT NOT NULL,
    master_hash   TEXT NOT NULL,
    duration      REAL NOT NULL,
    fps           REAL,
    has_video     INTEGER NOT NULL,
    start_offsets TEXT
) STRICT;

CREATE TABLE ingest_runs (
    id             INTEGER PRIMARY KEY,
    source_id      INTEGER NOT NULL REFERENCES sources(id),
    started        TEXT NOT NULL,
    finished       TEXT,
    status         TEXT NOT NULL,
    schema_version INTEGER NOT NULL,
    tools          TEXT NOT NULL
) STRICT;

CREATE TABLE speakers (
    id        INTEGER PRIMARY KEY,
    source_id INTEGER NOT NULL REFERENCES sources(id),
    label     TEXT NOT NULL,
    name      TEXT,
    stale     INTEGER NOT NULL DEFAULT 0
) STRICT;

-- Raw diarization turns from the latest run. Kept so re-ingest speaker
-- carry-forward can compute temporal overlap against the previous run.
CREATE TABLE turns (
    id            INTEGER PRIMARY KEY,
    source_id     INTEGER NOT NULL REFERENCES sources(id),
    ingest_run_id INTEGER NOT NULL REFERENCES ingest_runs(id),
    speaker_id    INTEGER NOT NULL REFERENCES speakers(id),
    t_start       REAL NOT NULL,
    t_end         REAL NOT NULL
) STRICT;
CREATE INDEX turns_by_source ON turns(source_id);

-- text is the raw WhisperX segment text (display); text_norm is the
-- segment's normalized tokens joined with spaces — the same token stream
-- as the word rows, so FTS phrase tokens and word ordinals cannot drift.
CREATE TABLE utterances (
    id            INTEGER PRIMARY KEY,
    source_id     INTEGER NOT NULL REFERENCES sources(id),
    ingest_run_id INTEGER NOT NULL REFERENCES ingest_runs(id),
    t_start       REAL NOT NULL,
    t_end         REAL NOT NULL,
    text          TEXT NOT NULL,
    text_norm     TEXT NOT NULL,
    speaker_id    INTEGER REFERENCES speakers(id),
    multi_speaker INTEGER NOT NULL DEFAULT 0,
    confidence    REAL
) STRICT;

CREATE VIRTUAL TABLE utterances_fts USING fts5(text_norm, content='utterances', content_rowid='id');

CREATE TRIGGER utterances_ai AFTER INSERT ON utterances BEGIN
    INSERT INTO utterances_fts(rowid, text_norm) VALUES (new.id, new.text_norm);
END;
CREATE TRIGGER utterances_ad AFTER DELETE ON utterances BEGIN
    INSERT INTO utterances_fts(utterances_fts, rowid, text_norm) VALUES ('delete', old.id, old.text_norm);
END;
CREATE TRIGGER utterances_au AFTER UPDATE ON utterances BEGIN
    INSERT INTO utterances_fts(utterances_fts, rowid, text_norm) VALUES ('delete', old.id, old.text_norm);
    INSERT INTO utterances_fts(rowid, text_norm) VALUES (new.id, new.text_norm);
END;

CREATE TABLE words (
    id            INTEGER PRIMARY KEY,
    source_id     INTEGER NOT NULL REFERENCES sources(id),
    ingest_run_id INTEGER NOT NULL REFERENCES ingest_runs(id),
    utterance_id  INTEGER NOT NULL REFERENCES utterances(id),
    word_ordinal  INTEGER NOT NULL,
    t_start       REAL NOT NULL,
    t_end         REAL NOT NULL,
    text          TEXT NOT NULL,
    speaker_id    INTEGER REFERENCES speakers(id),
    confidence    REAL
) STRICT;
CREATE INDEX words_by_utterance ON words(utterance_id, word_ordinal);
CREATE INDEX words_by_text ON words(text);

CREATE TABLE phones (
    id            INTEGER PRIMARY KEY,
    source_id     INTEGER NOT NULL REFERENCES sources(id),
    ingest_run_id INTEGER NOT NULL REFERENCES ingest_runs(id),
    word_id       INTEGER REFERENCES words(id),
    t_start       REAL NOT NULL,
    t_end         REAL NOT NULL,
    label         TEXT NOT NULL,
    cut_t         REAL,
    weak_cut      INTEGER NOT NULL DEFAULT 0,
    sil_origin    TEXT CHECK (sil_origin IN ('mfa', 'chunk', 'turn')),
    speaker_id    INTEGER REFERENCES speakers(id),
    confidence    REAL
) STRICT;
CREATE INDEX phones_by_time ON phones(source_id, t_start);

CREATE TABLE diphones (
    id              INTEGER PRIMARY KEY,
    source_id       INTEGER NOT NULL REFERENCES sources(id),
    ingest_run_id   INTEGER NOT NULL REFERENCES ingest_runs(id),
    seq             INTEGER NOT NULL,
    label           TEXT NOT NULL,
    stress_a        INTEGER,
    stress_b        INTEGER,
    t_start         REAL NOT NULL,
    t_end           REAL NOT NULL,
    phone_a         INTEGER NOT NULL REFERENCES phones(id),
    phone_b         INTEGER NOT NULL REFERENCES phones(id),
    speaker_id      INTEGER REFERENCES speakers(id),
    mfcc_head       BLOB,
    mfcc_tail       BLOB,
    f0_head         REAL,
    f0_tail         REAL,
    rms_head_db     REAL,
    rms_tail_db     REAL,
    f0_median       REAL,
    voiced_fraction REAL,
    f0_slope        REAL,
    rms_mean_db     REAL,
    UNIQUE (source_id, seq)
) STRICT;
CREATE INDEX diphones_by_label ON diphones(label);

CREATE TABLE prosody_frames (
    source_id     INTEGER PRIMARY KEY REFERENCES sources(id),
    ingest_run_id INTEGER NOT NULL REFERENCES ingest_runs(id),
    hop           REAL NOT NULL,
    n_frames      INTEGER NOT NULL,
    f0            BLOB NOT NULL,
    rms_db        BLOB NOT NULL,
    mfcc          BLOB NOT NULL
) STRICT;
"#;

pub fn migrate(conn: &mut Connection) -> Result<(), CorpusError> {
    let found: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if found > SCHEMA_VERSION {
        return Err(CorpusError::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    if found == SCHEMA_VERSION {
        return Ok(());
    }
    // WAL is a persistent database property, ratified to be set by migration
    // v1 (not per-connection — read-only connections must not need it). It
    // cannot change inside a transaction; in-memory databases report
    // "memory" and are unaffected.
    conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get::<_, String>(0))?;
    let tx = conn.transaction()?;
    if found < 1 {
        tx.execute_batch(SQL_V1)?;
    }
    tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    tx.commit()?;
    Ok(())
}
