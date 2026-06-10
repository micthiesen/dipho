//! Word and phrase search over the utterance FTS5 index, mapping hits back
//! to exact word spans via per-word ordinals (the token stream and the word
//! rows are the same sequence by construction — see DESIGN.md).

use rusqlite::{Connection, params};

use super::CorpusError;
use super::normalize::normalize_query;
use crate::span::SourceId;

/// One utterance matching the query, with its full token stream and the
/// positions of every phrase occurrence.
#[derive(Debug, Clone)]
pub struct SearchHit {
    pub utterance_id: i64,
    pub source_id: SourceId,
    /// The source's origin (URL or path), for display.
    pub origin: String,
    pub t_start: f64,
    pub t_end: f64,
    /// Raw ASR text, for display.
    pub text: String,
    pub speaker: Option<String>,
    pub multi_speaker: bool,
    pub confidence: Option<f64>,
    /// The utterance's normalized tokens with spans, in ordinal order.
    pub words: Vec<WordSpan>,
    /// Each phrase occurrence as a range into `words` (end-exclusive).
    pub matches: Vec<std::ops::Range<usize>>,
}

impl SearchHit {
    /// Exact source-time span of one phrase occurrence.
    pub fn match_span(&self, m: &std::ops::Range<usize>) -> (f64, f64) {
        (self.words[m.start].t_start, self.words[m.end - 1].t_end)
    }
}

#[derive(Debug, Clone)]
pub struct WordSpan {
    pub text: String,
    pub t_start: f64,
    pub t_end: f64,
    pub confidence: Option<f64>,
}

pub fn search(conn: &Connection, query: &str) -> Result<Vec<SearchHit>, CorpusError> {
    let tokens = normalize_query(query);
    if tokens.is_empty() {
        return Ok(Vec::new());
    }
    // Tokens contain only [a-z0-9'] — safe inside an FTS5 string. The
    // quoted form is a phrase query; a single token degenerates to a term.
    let fts_query = format!("\"{}\"", tokens.join(" "));

    let mut hits = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT u.id, u.source_id, src.origin, u.t_start, u.t_end, u.text,
                coalesce(s.name, s.label), u.multi_speaker, u.confidence
         FROM utterances_fts f
         JOIN utterances u ON u.id = f.rowid
         JOIN sources src ON src.id = u.source_id
         LEFT JOIN speakers s ON s.id = u.speaker_id
         WHERE utterances_fts MATCH ?1
         ORDER BY u.source_id, u.t_start",
    )?;
    let mut words_stmt = conn.prepare(
        "SELECT text, t_start, t_end, confidence FROM words
         WHERE utterance_id = ?1 ORDER BY word_ordinal",
    )?;
    let mut rows = stmt.query(params![fts_query])?;
    while let Some(row) = rows.next()? {
        let utterance_id: i64 = row.get(0)?;
        let words: Vec<WordSpan> = words_stmt
            .query_map(params![utterance_id], |r| {
                Ok(WordSpan {
                    text: r.get(0)?,
                    t_start: r.get(1)?,
                    t_end: r.get(2)?,
                    confidence: r.get(3)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        let matches = phrase_occurrences(&words, &tokens);
        if matches.is_empty() {
            // FTS5's tokenizer can split differently around apostrophes
            // (it indexes "don't" as don + t); only word-row-exact phrase
            // occurrences are addressable hits.
            continue;
        }
        hits.push(SearchHit {
            utterance_id,
            source_id: SourceId(row.get(1)?),
            origin: row.get(2)?,
            t_start: row.get(3)?,
            t_end: row.get(4)?,
            text: row.get(5)?,
            speaker: row.get(6)?,
            multi_speaker: row.get(7)?,
            confidence: row.get(8)?,
            words,
            matches,
        });
    }
    Ok(hits)
}

/// Every (possibly overlapping) occurrence of the token sequence in the
/// utterance's word rows. Comparison ignores leading/trailing apostrophes,
/// matching how FTS5's unicode61 tokenizer treats them at token edges.
fn phrase_occurrences(words: &[WordSpan], tokens: &[String]) -> Vec<std::ops::Range<usize>> {
    let eq = |a: &str, b: &str| a.trim_matches('\'') == b.trim_matches('\'');
    (0..(words.len() + 1).saturating_sub(tokens.len()))
        .filter(|&i| tokens.iter().zip(&words[i..]).all(|(t, w)| eq(t, &w.text)))
        .map(|i| i..i + tokens.len())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{Corpus, ProsodyData, features::MFCC_DIM, manifest::Manifest};

    /// Two sources; "twenty five" appears in three utterances (twice in
    /// one of them) across both, plus an utterance without it.
    fn corpus_with_numbers() -> Corpus {
        let mut corpus = Corpus::open_in_memory().unwrap();
        corpus.migrate().unwrap();
        for (origin_id, manifest) in [
            ("youtube:srcA", source_a_manifest()),
            ("youtube:srcB", source_b_manifest()),
        ] {
            let manifest = Manifest::from_json(&manifest).unwrap();
            let prosody = ProsodyData {
                hop: 0.01,
                f0: vec![0.0; 1001],
                rms_db: vec![0.0; 1001],
                mfcc: vec![0.0; 1001 * MFCC_DIM],
            };
            let meta = crate::corpus::SourceMeta {
                origin: format!("https://example.test/{origin_id}"),
                origin_id: origin_id.to_string(),
                original_path: None,
                master_path: "master.mkv".to_string(),
                master_hash: origin_id.to_string(),
                fps: Some(30.0),
                has_video: true,
                start_offsets: None,
            };
            corpus.load_source(&meta, &manifest, &prosody).unwrap();
        }
        corpus
    }

    fn source_a_manifest() -> String {
        r#"{
            "schema_version": 1,
            "analysis": { "path": "audio.wav", "duration": 10.0 },
            "tools": {},
            "segments": [
                { "text": "I have 25 cats", "start": 0.1, "end": 1.9,
                  "word_index_start": 0, "word_index_end": 5, "confidence": 0.9 },
                { "text": "25 plus 25", "start": 2.0, "end": 4.0,
                  "word_index_start": 5, "word_index_end": 10, "confidence": 0.8 },
                { "text": "no numbers here", "start": 5.0, "end": 6.0,
                  "word_index_start": 10, "word_index_end": 13, "confidence": 0.7 }
            ],
            "words": [
                { "text": "i",      "start": 0.1, "end": 0.3, "segment_index": 0 },
                { "text": "have",   "start": 0.3, "end": 0.6, "segment_index": 0 },
                { "text": "twenty", "start": 0.6, "end": 1.0, "segment_index": 0 },
                { "text": "five",   "start": 1.0, "end": 1.4, "segment_index": 0 },
                { "text": "cats",   "start": 1.4, "end": 1.9, "segment_index": 0 },
                { "text": "twenty", "start": 2.0, "end": 2.4, "segment_index": 1 },
                { "text": "five",   "start": 2.4, "end": 2.8, "segment_index": 1 },
                { "text": "plus",   "start": 2.8, "end": 3.2, "segment_index": 1 },
                { "text": "twenty", "start": 3.2, "end": 3.6, "segment_index": 1 },
                { "text": "five",   "start": 3.6, "end": 4.0, "segment_index": 1 },
                { "text": "no",      "start": 5.0, "end": 5.3, "segment_index": 2 },
                { "text": "numbers", "start": 5.3, "end": 5.7, "segment_index": 2 },
                { "text": "here",    "start": 5.7, "end": 6.0, "segment_index": 2 }
            ],
            "phonemes": [],
            "turns": [ { "speaker": "SPEAKER_00", "start": 0.0, "end": 6.0 } ],
            "chunks": [],
            "prosody": { "path": "prosody.npz", "hop": 0.01, "n_frames": 1001 }
        }"#
        .to_string()
    }

    fn source_b_manifest() -> String {
        r#"{
            "schema_version": 1,
            "analysis": { "path": "audio.wav", "duration": 10.0 },
            "tools": {},
            "segments": [
                { "text": "twenty five again", "start": 1.0, "end": 3.0,
                  "word_index_start": 0, "word_index_end": 3, "confidence": null },
                { "text": "don't stop", "start": 4.0, "end": 5.0,
                  "word_index_start": 3, "word_index_end": 5, "confidence": 0.95 }
            ],
            "words": [
                { "text": "twenty", "start": 1.0, "end": 1.5, "segment_index": 0 },
                { "text": "five",   "start": 1.5, "end": 2.0, "segment_index": 0 },
                { "text": "again",  "start": 2.0, "end": 3.0, "segment_index": 0 },
                { "text": "don't",  "start": 4.0, "end": 4.5, "segment_index": 1 },
                { "text": "stop",   "start": 4.5, "end": 5.0, "segment_index": 1 }
            ],
            "phonemes": [],
            "turns": [],
            "chunks": [],
            "prosody": { "path": "prosody.npz", "hop": 0.01, "n_frames": 1001 }
        }"#
        .to_string()
    }

    fn run(corpus: &Corpus, query: &str) -> Vec<SearchHit> {
        corpus.search(query).unwrap()
    }

    #[test]
    fn word_query_returns_every_utterance_with_exact_spans() {
        let corpus = corpus_with_numbers();
        let hits = run(&corpus, "twenty");
        assert_eq!(hits.len(), 3);
        // First hit: "I have 25 cats", token "twenty" at ordinal 2.
        let hit = &hits[0];
        assert_eq!(hit.text, "I have 25 cats");
        assert_eq!(hit.matches, vec![2..3]);
        let (start, end) = hit.match_span(&hit.matches[0]);
        assert!((start - 0.6).abs() < 1e-9 && (end - 1.0).abs() < 1e-9);
        // Second hit has two occurrences.
        assert_eq!(hits[1].matches, vec![0..1, 3..4]);
    }

    #[test]
    fn phrase_query_finds_consecutive_words_only() {
        let corpus = corpus_with_numbers();
        let hits = run(&corpus, "five plus");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matches, vec![1..3]);
        let (start, end) = hits[0].match_span(&hits[0].matches[0]);
        assert!((start - 2.4).abs() < 1e-9 && (end - 3.2).abs() < 1e-9);
        // "cats twenty" spans an utterance boundary: no hit.
        assert!(run(&corpus, "cats twenty").is_empty());
    }

    #[test]
    fn digits_in_the_query_find_expanded_words() {
        let corpus = corpus_with_numbers();
        // "25" must find "twenty five" (ROADMAP M3 verify).
        let hits = run(&corpus, "25");
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[1].matches, vec![0..2, 3..5]);
        let (start, end) = hits[1].match_span(&hits[1].matches[1]);
        assert!((start - 3.2).abs() < 1e-9 && (end - 4.0).abs() < 1e-9);
        // Mixed digits and words, with punctuation and case noise.
        let hits = run(&corpus, "Have 25 CATS!");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matches, vec![1..5]);
    }

    #[test]
    fn speaker_and_confidence_ride_along() {
        let corpus = corpus_with_numbers();
        let hits = run(&corpus, "cats");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].speaker.as_deref(), Some("SPEAKER_00"));
        assert_eq!(hits[0].confidence, Some(0.9));
        // Source B has no turns: no speaker; null confidence passes through.
        let hits = run(&corpus, "again");
        assert_eq!(hits[0].speaker, None);
        assert_eq!(hits[0].confidence, None);
        // A named speaker displays by name.
        corpus
            .conn
            .execute("UPDATE speakers SET name = 'Liquid'", [])
            .unwrap();
        let hits = run(&corpus, "cats");
        assert_eq!(hits[0].speaker.as_deref(), Some("Liquid"));
    }

    #[test]
    fn apostrophe_words_match_exactly() {
        let corpus = corpus_with_numbers();
        let hits = run(&corpus, "don't stop");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].matches, vec![0..2]);
    }

    #[test]
    fn empty_and_unmatched_queries_return_nothing() {
        let corpus = corpus_with_numbers();
        assert!(run(&corpus, "").is_empty());
        assert!(run(&corpus, "...").is_empty());
        assert!(run(&corpus, "zebra").is_empty());
        assert!(run(&corpus, "twenty zebra").is_empty());
    }
}
