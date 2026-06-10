//! `dipho search <query>`: word/phrase search over the corpus, printing
//! each hit inside its utterance with speaker, confidence, and the exact
//! word spans of every occurrence.

use std::path::Path;

use anyhow::{Context, Result, bail};
use dipho_core::corpus::{Corpus, SearchHit, normalize_query};

/// Open an existing corpus read-only, with a friendly error when there is
/// none and a typed one when its schema doesn't match this build.
pub fn open_corpus(corpus_db: &Path) -> Result<Corpus> {
    if !corpus_db.exists() {
        bail!(
            "no corpus at {} — run `dipho ingest <url|file>` first",
            corpus_db.display()
        );
    }
    let corpus = Corpus::open_read_only(corpus_db)
        .with_context(|| format!("opening corpus {}", corpus_db.display()))?;
    corpus.ensure_schema_current()?;
    Ok(corpus)
}

pub fn run(query: &str, corpus_db: &Path) -> Result<()> {
    let corpus = open_corpus(corpus_db)?;
    let hits = corpus.search(query)?;
    let normalized = normalize_query(query).join(" ");
    if hits.is_empty() {
        println!("no hits for \"{normalized}\"");
        return Ok(());
    }
    let occurrences: usize = hits.iter().map(|h| h.matches.len()).sum();
    println!(
        "{occurrences} match{} in {} utterance{} for \"{normalized}\"",
        if occurrences == 1 { "" } else { "es" },
        hits.len(),
        if hits.len() == 1 { "" } else { "s" },
    );
    for hit in &hits {
        println!();
        println!(
            "source {}  {:.2}–{:.2}s  {}  conf {}",
            hit.source_id.0,
            hit.t_start,
            hit.t_end,
            hit.speaker.as_deref().unwrap_or("(unknown speaker)"),
            hit.confidence
                .map(|c| format!("{c:.2}"))
                .as_deref()
                .unwrap_or("—"),
        );
        println!("  {}", highlighted(hit));
        for m in &hit.matches {
            let (start, end) = hit.match_span(m);
            println!("    match {start:.2}–{end:.2}s");
        }
    }
    Ok(())
}

/// The utterance's normalized token stream with each occurrence bracketed.
fn highlighted(hit: &SearchHit) -> String {
    let mut out = String::new();
    for (i, word) in hit.words.iter().enumerate() {
        if !out.is_empty() {
            out.push(' ');
        }
        if hit.matches.iter().any(|m| m.start == i) {
            out.push('[');
        }
        out.push_str(&word.text);
        if hit.matches.iter().any(|m| m.end == i + 1) {
            out.push(']');
        }
    }
    out
}
