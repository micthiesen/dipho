//! Loading one source's sidecar output into the corpus: writes the source,
//! ingest run, speakers, and turns, then derives utterances → words →
//! phones → diphones with boundary features. One transaction; re-ingest
//! replaces derived rows bottom-up and carries named speakers forward.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::CorpusError;
use super::diphones;
use super::features::{self, ProsodyData};
use super::manifest::{MANIFEST_SCHEMA_VERSION, Manifest};
use super::phones::{self, SilOrigin, TierPhone};
use super::speakers::{self, TurnRow};
use crate::span::SourceId;

/// Source identity and playback-master facts owned by the ingest caller
/// (the sidecar never sees these — see DESIGN.md "Source identity").
pub struct SourceMeta {
    pub origin: String,
    pub origin_id: String,
    pub original_path: Option<String>,
    pub master_path: String,
    pub master_hash: String,
    /// Post-normalization CFR rate; None for audio-only sources.
    pub fps: Option<f64>,
    pub has_video: bool,
    /// Per-stream start offsets recorded as an assertion trail.
    pub start_offsets: Option<serde_json::Value>,
}

#[derive(Debug)]
pub struct LoadReport {
    pub source_id: SourceId,
    pub speakers: usize,
    pub utterances: usize,
    pub words: usize,
    pub phones: usize,
    pub diphones: usize,
}

pub fn load_source(
    conn: &mut Connection,
    meta: &SourceMeta,
    manifest: &Manifest,
    prosody: &ProsodyData,
) -> Result<LoadReport, CorpusError> {
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(CorpusError::UnknownManifestVersion(manifest.schema_version));
    }
    validate_spans(manifest)?;
    validate_frames(manifest, prosody)?;

    let tx = conn.transaction()?;

    // Capture the previous run's per-speaker turn-sets before any deletes;
    // carry-forward needs them.
    let existing: Option<i64> = tx
        .query_row(
            "SELECT id FROM sources WHERE origin_id = ?1",
            params![meta.origin_id],
            |row| row.get(0),
        )
        .optional()?;
    let old_speakers = match existing {
        Some(source_id) => read_old_speakers(&tx, source_id)?,
        None => Vec::new(),
    };

    let source_id = upsert_source(&tx, existing, meta, manifest)?;
    let run_id = insert_run(&tx, source_id, manifest)?;
    let speaker_ids = load_speakers(&tx, source_id, manifest, &old_speakers)?;
    let turn_rows = insert_turns(&tx, source_id, run_id, manifest, &speaker_ids)?;

    let utterance_ids = insert_utterances(&tx, source_id, run_id, manifest, &turn_rows)?;
    let word_ids = insert_words(&tx, source_id, run_id, manifest, &utterance_ids, &turn_rows)?;
    let loaded = insert_phones(&tx, source_id, run_id, manifest, &word_ids, &turn_rows)?;
    let diphone_count = insert_diphones(&tx, source_id, run_id, &loaded, &turn_rows, prosody)?;
    insert_prosody_frames(&tx, source_id, run_id, manifest, prosody)?;

    tx.commit()?;
    Ok(LoadReport {
        source_id: SourceId(source_id),
        speakers: speaker_ids.len(),
        utterances: utterance_ids.len(),
        words: word_ids.len(),
        phones: loaded.tier.len(),
        diphones: diphone_count,
    })
}

/// Every span the sidecar emits must be finite, ordered, and inside the
/// analysis stream — typed errors, reject-never-clamp. (The phone tier
/// additionally gets per-interval and overlap checks in `build_tier`.)
fn validate_spans(manifest: &Manifest) -> Result<(), CorpusError> {
    let duration = manifest.analysis.duration;
    if !duration.is_finite() || duration <= 0.0 {
        return Err(CorpusError::InvalidInterval {
            what: "analysis duration",
            t_start: 0.0,
            t_end: duration,
        });
    }
    let check = |what: &'static str, start: f64, end: f64| -> Result<(), CorpusError> {
        let bad = !start.is_finite()
            || !end.is_finite()
            || start < 0.0
            || end < start
            || end > duration + phones::T_EPS;
        if bad {
            return Err(CorpusError::InvalidInterval {
                what,
                t_start: start,
                t_end: end,
            });
        }
        Ok(())
    };
    for seg in &manifest.segments {
        check("segment", seg.start, seg.end)?;
    }
    for word in &manifest.words {
        check("word", word.start, word.end)?;
    }
    for phoneme in &manifest.phonemes {
        check("phoneme", phoneme.start, phoneme.end)?;
    }
    for turn in &manifest.turns {
        check("turn", turn.start, turn.end)?;
    }
    for chunk in &manifest.chunks {
        check("chunk", chunk.start, chunk.end)?;
    }
    Ok(())
}

fn validate_frames(manifest: &Manifest, prosody: &ProsodyData) -> Result<(), CorpusError> {
    let hop = manifest.prosody.hop;
    if !hop.is_finite() || hop <= 0.0 {
        return Err(CorpusError::FrameMismatch(format!("hop {hop} must be > 0")));
    }
    if (prosody.hop - hop).abs() > phones::T_EPS {
        return Err(CorpusError::FrameMismatch(format!(
            "frame hop {} != manifest hop {hop}",
            prosody.hop
        )));
    }
    // duration/hop is mathematically integral whenever the duration comes
    // from a whole sample count (e.g. 0.29 s at hop 0.01 is exactly 29), but
    // in f64 it can land just under the integer (0.29/0.01 == 28.999…) and a
    // bare floor would then reject the sidecar's integer-arithmetic frame
    // count. The epsilon absorbs that rounding; it is far below one frame.
    const FRAME_RATIO_EPS: f64 = 1e-6;
    let expected = 1 + (manifest.analysis.duration / hop + FRAME_RATIO_EPS).floor() as usize;
    let declared = manifest.prosody.n_frames;
    if declared != expected {
        return Err(CorpusError::FrameMismatch(format!(
            "n_frames {declared} != 1 + floor(duration/hop) = {expected}"
        )));
    }
    for (name, len) in [
        ("f0", prosody.f0.len()),
        ("rms_db", prosody.rms_db.len()),
        ("mfcc", prosody.mfcc.len() / features::MFCC_DIM),
    ] {
        if len != declared {
            return Err(CorpusError::FrameMismatch(format!(
                "{name} has {len} frames, manifest declares {declared}"
            )));
        }
    }
    if !prosody.mfcc.len().is_multiple_of(features::MFCC_DIM) {
        return Err(CorpusError::FrameMismatch(
            "mfcc array is not a whole number of frames".to_string(),
        ));
    }
    Ok(())
}

struct OldSpeaker {
    id: i64,
    named: bool,
    turns: Vec<(f64, f64)>,
}

fn read_old_speakers(tx: &Transaction, source_id: i64) -> Result<Vec<OldSpeaker>, CorpusError> {
    let mut speakers: Vec<OldSpeaker> = {
        let mut stmt = tx.prepare(
            "SELECT id, name IS NOT NULL FROM speakers WHERE source_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map(params![source_id], |row| {
            Ok(OldSpeaker {
                id: row.get(0)?,
                named: row.get(1)?,
                turns: Vec::new(),
            })
        })?;
        rows.collect::<Result<_, _>>()?
    };
    let mut stmt =
        tx.prepare("SELECT speaker_id, t_start, t_end FROM turns WHERE source_id = ?1")?;
    let rows = stmt.query_map(params![source_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get(1)?, row.get(2)?))
    })?;
    for row in rows {
        let (speaker_id, t_start, t_end) = row?;
        if let Some(s) = speakers.iter_mut().find(|s| s.id == speaker_id) {
            s.turns.push((t_start, t_end));
        }
    }
    Ok(speakers)
}

fn upsert_source(
    tx: &Transaction,
    existing: Option<i64>,
    meta: &SourceMeta,
    manifest: &Manifest,
) -> Result<i64, CorpusError> {
    let start_offsets = meta.start_offsets.as_ref().map(|v| v.to_string());
    match existing {
        Some(source_id) => {
            // Re-ingest: replace derived rows bottom-up. Utterance deletes
            // fire the FTS sync triggers. Turns are NOT deleted here:
            // load_speakers replaces them per speaker, preserving a stale
            // named speaker's last-known turns so a later run can re-claim
            // it by overlap.
            for sql in [
                "DELETE FROM diphones WHERE source_id = ?1",
                "DELETE FROM phones WHERE source_id = ?1",
                "DELETE FROM words WHERE source_id = ?1",
                "DELETE FROM utterances WHERE source_id = ?1",
                "DELETE FROM prosody_frames WHERE source_id = ?1",
            ] {
                tx.execute(sql, params![source_id])?;
            }
            tx.execute(
                "UPDATE sources SET origin = ?2, original_path = ?3, master_path = ?4,
                        master_hash = ?5, duration = ?6, fps = ?7, has_video = ?8,
                        start_offsets = ?9
                 WHERE id = ?1",
                params![
                    source_id,
                    meta.origin,
                    meta.original_path,
                    meta.master_path,
                    meta.master_hash,
                    manifest.analysis.duration,
                    meta.fps,
                    meta.has_video,
                    start_offsets,
                ],
            )?;
            Ok(source_id)
        }
        None => {
            tx.execute(
                "INSERT INTO sources (origin, origin_id, original_path, master_path,
                                      master_hash, duration, fps, has_video, start_offsets)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    meta.origin,
                    meta.origin_id,
                    meta.original_path,
                    meta.master_path,
                    meta.master_hash,
                    manifest.analysis.duration,
                    meta.fps,
                    meta.has_video,
                    start_offsets,
                ],
            )?;
            Ok(tx.last_insert_rowid())
        }
    }
}

fn insert_run(tx: &Transaction, source_id: i64, manifest: &Manifest) -> Result<i64, CorpusError> {
    tx.execute(
        "INSERT INTO ingest_runs (source_id, started, finished, status, schema_version, tools)
         VALUES (?1, datetime('now'), datetime('now'), 'loaded', ?2, ?3)",
        params![
            source_id,
            manifest.schema_version,
            manifest.tools.to_string()
        ],
    )?;
    Ok(tx.last_insert_rowid())
}

/// Returns speaker row ids parallel to the manifest's turn labels (first
/// appearance order).
fn load_speakers(
    tx: &Transaction,
    source_id: i64,
    manifest: &Manifest,
    old: &[OldSpeaker],
) -> Result<Vec<(String, i64)>, CorpusError> {
    let mut labels: Vec<String> = Vec::new();
    let mut sets: Vec<Vec<(f64, f64)>> = Vec::new();
    for turn in &manifest.turns {
        match labels.iter().position(|l| *l == turn.speaker) {
            Some(i) => sets[i].push((turn.start, turn.end)),
            None => {
                labels.push(turn.speaker.clone());
                sets.push(vec![(turn.start, turn.end)]);
            }
        }
    }
    let old_sets: Vec<Vec<(f64, f64)>> = old.iter().map(|s| s.turns.clone()).collect();
    let matches = speakers::carry_forward(&sets, &old_sets);

    let mut ids: Vec<(String, i64)> = Vec::new();
    let mut claimed: Vec<i64> = Vec::new();
    for (label, matched) in labels.iter().zip(&matches) {
        let id = match matched {
            Some(oi) => {
                let id = old[*oi].id;
                tx.execute(
                    "UPDATE speakers SET label = ?2, stale = 0 WHERE id = ?1",
                    params![id, label],
                )?;
                // The claimed speaker's turns are replaced by this run's.
                tx.execute("DELETE FROM turns WHERE speaker_id = ?1", params![id])?;
                claimed.push(id);
                id
            }
            None => {
                tx.execute(
                    "INSERT INTO speakers (source_id, label) VALUES (?1, ?2)",
                    params![source_id, label],
                )?;
                tx.last_insert_rowid()
            }
        };
        ids.push((label.clone(), id));
    }
    // Orphaned named speakers are kept flagged stale, never deleted — and
    // they keep their last-known turns so a later run can re-claim them by
    // overlap. Unnamed orphans go, turns first (FK).
    for s in old {
        if claimed.contains(&s.id) {
            continue;
        }
        if s.named {
            tx.execute("UPDATE speakers SET stale = 1 WHERE id = ?1", params![s.id])?;
        } else {
            tx.execute("DELETE FROM turns WHERE speaker_id = ?1", params![s.id])?;
            tx.execute("DELETE FROM speakers WHERE id = ?1", params![s.id])?;
        }
    }
    Ok(ids)
}

fn insert_turns(
    tx: &Transaction,
    source_id: i64,
    run_id: i64,
    manifest: &Manifest,
    speaker_ids: &[(String, i64)],
) -> Result<Vec<TurnRow>, CorpusError> {
    let mut stmt = tx.prepare(
        "INSERT INTO turns (source_id, ingest_run_id, speaker_id, t_start, t_end)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut rows = Vec::with_capacity(manifest.turns.len());
    for turn in &manifest.turns {
        let speaker_id = speaker_ids
            .iter()
            .find(|(label, _)| *label == turn.speaker)
            .expect("every turn label was registered")
            .1;
        stmt.execute(params![source_id, run_id, speaker_id, turn.start, turn.end])?;
        rows.push(TurnRow {
            speaker_id,
            t_start: turn.start,
            t_end: turn.end,
        });
    }
    rows.sort_by(|a, b| {
        a.t_start
            .partial_cmp(&b.t_start)
            .expect("turn times are finite")
    });
    Ok(rows)
}

fn insert_utterances(
    tx: &Transaction,
    source_id: i64,
    run_id: i64,
    manifest: &Manifest,
    turns: &[TurnRow],
) -> Result<Vec<i64>, CorpusError> {
    // The FTS document is the segment's normalized tokens joined — the same
    // token stream as the word rows, so phrase tokens and word ordinals
    // can't drift apart. The raw segment text is kept for display.
    let mut norm_texts = vec![String::new(); manifest.segments.len()];
    for word in &manifest.words {
        if let Some(norm) = norm_texts.get_mut(word.segment_index) {
            if !norm.is_empty() {
                norm.push(' ');
            }
            norm.push_str(&word.text);
        }
    }
    let mut stmt = tx.prepare(
        "INSERT INTO utterances (source_id, ingest_run_id, t_start, t_end, text, text_norm,
                                 speaker_id, multi_speaker, confidence)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let mut ids = Vec::with_capacity(manifest.segments.len());
    for (seg, norm) in manifest.segments.iter().zip(&norm_texts) {
        let speaker = speakers::max_overlap_speaker(turns, seg.start, seg.end);
        let multi = speakers::is_multi_speaker(turns, seg.start, seg.end, speaker);
        let id = stmt.insert(params![
            source_id,
            run_id,
            seg.start,
            seg.end,
            seg.text,
            norm,
            speaker,
            multi,
            seg.confidence
        ])?;
        ids.push(id);
    }
    Ok(ids)
}

fn insert_words(
    tx: &Transaction,
    source_id: i64,
    run_id: i64,
    manifest: &Manifest,
    utterance_ids: &[i64],
    turns: &[TurnRow],
) -> Result<Vec<i64>, CorpusError> {
    let mut stmt = tx.prepare(
        "INSERT INTO words (source_id, ingest_run_id, utterance_id, word_ordinal,
                            t_start, t_end, text, speaker_id, confidence)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let mut ordinals = vec![0i64; manifest.segments.len()];
    let mut ids = Vec::with_capacity(manifest.words.len());
    for (wi, word) in manifest.words.iter().enumerate() {
        let seg = word.segment_index;
        if seg >= manifest.segments.len() {
            return Err(CorpusError::IndexOutOfRange {
                what: "word segment_index",
                index: seg,
            });
        }
        let segment = &manifest.segments[seg];
        if wi < segment.word_index_start || wi >= segment.word_index_end {
            return Err(CorpusError::Contract(format!(
                "word {wi} lies outside its segment's word_index range"
            )));
        }
        let ordinal = ordinals[seg];
        ordinals[seg] += 1;
        let speaker = speakers::max_overlap_speaker(turns, word.start, word.end);
        let id = stmt.insert(params![
            source_id,
            run_id,
            utterance_ids[seg],
            ordinal,
            word.start,
            word.end,
            word.text,
            speaker,
            word.confidence
        ])?;
        ids.push(id);
    }
    Ok(ids)
}

/// The processed phone tier with its database row ids and derived speakers,
/// index-aligned by construction (one push per tier row).
struct LoadedTier {
    tier: Vec<TierPhone>,
    row_ids: Vec<i64>,
    speakers: Vec<Option<i64>>,
}

fn insert_phones(
    tx: &Transaction,
    source_id: i64,
    run_id: i64,
    manifest: &Manifest,
    word_ids: &[i64],
    turns: &[TurnRow],
) -> Result<LoadedTier, CorpusError> {
    let mut tier = phones::build_tier(&manifest.phonemes)?;
    for p in &tier {
        if let Some(wi) = p.word_index
            && wi >= word_ids.len()
        {
            return Err(CorpusError::IndexOutOfRange {
                what: "phoneme word_index",
                index: wi,
            });
        }
    }
    // Turn boundaries before chunk edges: where they coincide, the higher-
    // precedence origin wins the inserted terminator.
    let mut boundaries: Vec<(f64, SilOrigin)> = Vec::new();
    for turn in &manifest.turns {
        boundaries.push((turn.start, SilOrigin::Turn));
        boundaries.push((turn.end, SilOrigin::Turn));
    }
    for chunk in &manifest.chunks {
        boundaries.push((chunk.start, SilOrigin::Chunk));
        boundaries.push((chunk.end, SilOrigin::Chunk));
    }
    phones::insert_terminators(&mut tier, &boundaries);
    let mut tier = phones::merge_sil_runs(tier);
    phones::assign_cut_points(&mut tier);

    let mut stmt = tx.prepare(
        "INSERT INTO phones (source_id, ingest_run_id, word_id, t_start, t_end, label,
                             cut_t, weak_cut, sil_origin, speaker_id, confidence)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    let mut row_ids = Vec::with_capacity(tier.len());
    let mut phone_speakers = Vec::with_capacity(tier.len());
    for p in &tier {
        let speaker = speakers::max_overlap_speaker(turns, p.t_start, p.t_end);
        let id = stmt.insert(params![
            source_id,
            run_id,
            p.word_index.map(|wi| word_ids[wi]),
            p.t_start,
            p.t_end,
            p.label(),
            p.cut_t,
            p.weak_cut(),
            p.sil_origin().map(SilOrigin::as_str),
            speaker,
            p.confidence
        ])?;
        row_ids.push(id);
        phone_speakers.push(speaker);
    }
    drop(stmt);
    Ok(LoadedTier {
        tier,
        row_ids,
        speakers: phone_speakers,
    })
}

fn insert_diphones(
    tx: &Transaction,
    source_id: i64,
    run_id: i64,
    loaded: &LoadedTier,
    turns: &[TurnRow],
    prosody: &ProsodyData,
) -> Result<usize, CorpusError> {
    let derived = diphones::derive(&loaded.tier);
    let mut stmt = tx.prepare(
        "INSERT INTO diphones (source_id, ingest_run_id, seq, label, stress_a, stress_b,
                               t_start, t_end, phone_a, phone_b, speaker_id,
                               mfcc_head, mfcc_tail, f0_head, f0_tail,
                               rms_head_db, rms_tail_db,
                               f0_median, voiced_fraction, f0_slope, rms_mean_db)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                 ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
    )?;
    for d in &derived {
        let f = features::unit_features(prosody, d.t_start, d.t_end);
        // A SIL diphone inherits speaker context from its real-phone member.
        let speaker = if loaded.tier[d.phone_a].is_sil() {
            loaded.speakers[d.phone_b]
        } else if loaded.tier[d.phone_b].is_sil() {
            loaded.speakers[d.phone_a]
        } else {
            speakers::max_overlap_speaker(turns, d.t_start, d.t_end)
        };
        stmt.execute(params![
            source_id,
            run_id,
            d.seq,
            d.label,
            d.stress_a,
            d.stress_b,
            d.t_start,
            d.t_end,
            loaded.row_ids[d.phone_a],
            loaded.row_ids[d.phone_b],
            speaker,
            features::f32_blob(&f.mfcc_head),
            features::f32_blob(&f.mfcc_tail),
            f.f0_head,
            f.f0_tail,
            f.rms_head_db,
            f.rms_tail_db,
            f.f0_median,
            f.voiced_fraction,
            f.f0_slope,
            f.rms_mean_db,
        ])?;
    }
    Ok(derived.len())
}

fn insert_prosody_frames(
    tx: &Transaction,
    source_id: i64,
    run_id: i64,
    manifest: &Manifest,
    prosody: &ProsodyData,
) -> Result<(), CorpusError> {
    tx.execute(
        "INSERT INTO prosody_frames (source_id, ingest_run_id, hop, n_frames, f0, rms_db, mfcc)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            source_id,
            run_id,
            manifest.prosody.hop,
            manifest.prosody.n_frames as i64,
            features::f32_blob(&prosody.f0),
            features::f32_blob(&prosody.rms_db),
            features::f32_blob(&prosody.mfcc),
        ],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::Corpus;
    use crate::corpus::features::MFCC_DIM;

    /// The M1 adjacency fixture (see ROADMAP M1 "Verify"). 9 s source,
    /// two speakers (turn boundary at 5.0, mid-word), three MFA chunks
    /// (speech abutting the 3.0 edge), a 500 ms mid silence, NOISE blips of
    /// 15 ms (bridgeable) and 60 ms (hard break), a single-phone word "a",
    /// and SIL-preceded stops G (0.5) and D (7.2).
    const FIXTURE_MANIFEST: &str = r#"{
        "schema_version": 1,
        "analysis": { "path": "audio.wav", "duration": 9.0 },
        "tools": { "dipho_ingest": "0.1.0", "mfa": "3.3.9" },
        "segments": [
            { "text": "go a sun", "start": 0.5, "end": 1.5,
              "word_index_start": 0, "word_index_end": 3, "confidence": 0.95 },
            { "text": "if it cat", "start": 2.0, "end": 3.0,
              "word_index_start": 3, "word_index_end": 6, "confidence": 0.9 },
            { "text": "yes", "start": 3.0, "end": 3.5,
              "word_index_start": 6, "word_index_end": 7, "confidence": 0.92 },
            { "text": "we no", "start": 4.85, "end": 5.55,
              "word_index_start": 7, "word_index_end": 9, "confidence": 0.88 },
            { "text": "dog", "start": 7.2, "end": 7.6,
              "word_index_start": 9, "word_index_end": 10, "confidence": 0.97 }
        ],
        "words": [
            { "text": "go",  "start": 0.5,   "end": 0.8,  "confidence": 0.99, "segment_index": 0 },
            { "text": "a",   "start": 0.8,   "end": 0.9,  "confidence": 0.98, "segment_index": 0 },
            { "text": "sun", "start": 0.9,   "end": 1.5,  "confidence": 0.97, "segment_index": 0 },
            { "text": "if",  "start": 2.0,   "end": 2.3,  "confidence": 0.96, "segment_index": 1 },
            { "text": "it",  "start": 2.315, "end": 2.5,  "confidence": 0.95, "segment_index": 1 },
            { "text": "cat", "start": 2.56,  "end": 3.0,  "confidence": 0.94, "segment_index": 1 },
            { "text": "yes", "start": 3.0,   "end": 3.5,  "confidence": 0.93, "segment_index": 2 },
            { "text": "we",  "start": 4.9,   "end": 5.2,  "confidence": 0.92, "segment_index": 3 },
            { "text": "no",  "start": 5.2,   "end": 5.55, "confidence": 0.91, "segment_index": 3 },
            { "text": "dog", "start": 7.2,   "end": 7.6,  "confidence": 0.90, "segment_index": 4 }
        ],
        "phonemes": [
            { "label": "SIL",   "start": 0.0,   "end": 0.5,   "confidence": null, "word_index": null },
            { "label": "G",     "start": 0.5,   "end": 0.6,   "confidence": 0.9,  "word_index": 0 },
            { "label": "OW1",   "start": 0.6,   "end": 0.8,   "confidence": 0.9,  "word_index": 0 },
            { "label": "AH0",   "start": 0.8,   "end": 0.9,   "confidence": 0.9,  "word_index": 1 },
            { "label": "S",     "start": 0.9,   "end": 1.1,   "confidence": 0.9,  "word_index": 2 },
            { "label": "AH1",   "start": 1.1,   "end": 1.3,   "confidence": 0.9,  "word_index": 2 },
            { "label": "N",     "start": 1.3,   "end": 1.5,   "confidence": 0.9,  "word_index": 2 },
            { "label": "SIL",   "start": 1.5,   "end": 2.0,   "confidence": null, "word_index": null },
            { "label": "IH1",   "start": 2.0,   "end": 2.1,   "confidence": 0.9,  "word_index": 3 },
            { "label": "F",     "start": 2.1,   "end": 2.3,   "confidence": 0.9,  "word_index": 3 },
            { "label": "NOISE", "start": 2.3,   "end": 2.315, "confidence": null, "word_index": null },
            { "label": "IH0",   "start": 2.315, "end": 2.4,   "confidence": 0.9,  "word_index": 4 },
            { "label": "T",     "start": 2.4,   "end": 2.5,   "confidence": 0.9,  "word_index": 4 },
            { "label": "NOISE", "start": 2.5,   "end": 2.56,  "confidence": null, "word_index": null },
            { "label": "K",     "start": 2.56,  "end": 2.66,  "confidence": 0.9,  "word_index": 5 },
            { "label": "AE1",   "start": 2.66,  "end": 2.86,  "confidence": 0.9,  "word_index": 5 },
            { "label": "T",     "start": 2.86,  "end": 3.0,   "confidence": 0.85, "word_index": 5 },
            { "label": "Y",     "start": 3.0,   "end": 3.1,   "confidence": 0.85, "word_index": 6 },
            { "label": "EH1",   "start": 3.1,   "end": 3.3,   "confidence": 0.9,  "word_index": 6 },
            { "label": "S",     "start": 3.3,   "end": 3.5,   "confidence": 0.9,  "word_index": 6 },
            { "label": "SIL",   "start": 3.5,   "end": 4.9,   "confidence": null, "word_index": null },
            { "label": "W",     "start": 4.9,   "end": 5.0,   "confidence": 0.9,  "word_index": 7 },
            { "label": "IY1",   "start": 5.0,   "end": 5.2,   "confidence": 0.9,  "word_index": 7 },
            { "label": "N",     "start": 5.2,   "end": 5.35,  "confidence": 0.9,  "word_index": 8 },
            { "label": "OW1",   "start": 5.35,  "end": 5.55,  "confidence": 0.9,  "word_index": 8 },
            { "label": "SIL",   "start": 5.55,  "end": 6.0,   "confidence": null, "word_index": null },
            { "label": "SIL",   "start": 7.0,   "end": 7.2,   "confidence": null, "word_index": null },
            { "label": "D",     "start": 7.2,   "end": 7.3,   "confidence": 0.9,  "word_index": 9 },
            { "label": "AO1",   "start": 7.3,   "end": 7.5,   "confidence": 0.9,  "word_index": 9 },
            { "label": "G",     "start": 7.5,   "end": 7.6,   "confidence": 0.9,  "word_index": 9 },
            { "label": "SIL",   "start": 7.6,   "end": 8.0,   "confidence": null, "word_index": null }
        ],
        "turns": [
            { "speaker": "SPEAKER_00", "start": 0.0, "end": 5.0 },
            { "speaker": "SPEAKER_01", "start": 5.0, "end": 8.5 }
        ],
        "chunks": [
            { "start": 0.0, "end": 3.0 },
            { "start": 3.0, "end": 6.0 },
            { "start": 7.0, "end": 8.5 }
        ],
        "prosody": { "path": "prosody.npz", "hop": 0.01, "n_frames": 901 }
    }"#;

    fn fixture_manifest() -> Manifest {
        Manifest::from_json(FIXTURE_MANIFEST).unwrap()
    }

    /// Unvoiced before 0.6 s, then flat 120 Hz; rms −20 dB everywhere;
    /// mfcc[i][d] = i + 1000·d so frame means are recognizable.
    fn fixture_prosody() -> ProsodyData {
        let n = 901;
        ProsodyData {
            hop: 0.01,
            f0: (0..n)
                .map(|i| if (i as f64) * 0.01 < 0.6 { 0.0 } else { 120.0 })
                .collect(),
            rms_db: vec![-20.0; n],
            mfcc: (0..n)
                .flat_map(|i| (0..MFCC_DIM).map(move |d| i as f32 + 1000.0 * d as f32))
                .collect(),
        }
    }

    fn fixture_meta() -> SourceMeta {
        SourceMeta {
            origin: "https://example.test/watch?v=abc123".to_string(),
            origin_id: "youtube:abc123".to_string(),
            original_path: Some("original.bin".to_string()),
            master_path: "master.mkv".to_string(),
            master_hash: "deadbeef".to_string(),
            fps: Some(30.0),
            has_video: true,
            start_offsets: None,
        }
    }

    fn loaded_corpus() -> (Corpus, LoadReport) {
        let mut corpus = Corpus::open_in_memory().unwrap();
        corpus.migrate().unwrap();
        let report = corpus
            .load_source(&fixture_meta(), &fixture_manifest(), &fixture_prosody())
            .unwrap();
        (corpus, report)
    }

    fn blob_f32(blob: &[u8]) -> Vec<f32> {
        blob.chunks_exact(4)
            .map(|c| f32::from_le_bytes(c.try_into().unwrap()))
            .collect()
    }

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn fixture_round_trips_into_an_in_memory_corpus() {
        let (corpus, report) = loaded_corpus();
        assert_eq!(report.speakers, 2);
        assert_eq!(report.utterances, 5);
        assert_eq!(report.words, 10);
        // 31 manifest phonemes + 2 inserted zero-length terminators.
        assert_eq!(report.phones, 33);
        assert_eq!(report.diphones, 24);
        let count = |sql: &str| -> i64 { corpus.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT count(*) FROM sources"), 1);
        assert_eq!(count("SELECT count(*) FROM ingest_runs"), 1);
        assert_eq!(count("SELECT count(*) FROM turns"), 2);
        assert_eq!(count("SELECT count(*) FROM diphones"), 24);
        assert_eq!(count("SELECT count(*) FROM prosody_frames"), 1);
        // NOISE rows are stored (only excised from the adjacency view).
        assert_eq!(
            count("SELECT count(*) FROM phones WHERE label = 'NOISE' AND cut_t IS NULL"),
            2
        );
    }

    #[test]
    fn fts_phrase_query_maps_back_to_word_spans() {
        let (corpus, _) = loaded_corpus();
        let utterance_id: i64 = corpus
            .conn
            .query_row(
                "SELECT rowid FROM utterances_fts WHERE utterances_fts MATCH '\"a sun\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let words: Vec<(i64, String, f64, f64)> = {
            let mut stmt = corpus
                .conn
                .prepare(
                    "SELECT word_ordinal, text, t_start, t_end FROM words
                     WHERE utterance_id = ?1 ORDER BY word_ordinal",
                )
                .unwrap();
            stmt.query_map(params![utterance_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
        };
        // Phrase tokens located by per-word ordinals within the utterance.
        let phrase = ["a", "sun"];
        let at = words
            .windows(2)
            .position(|w| w[0].1 == phrase[0] && w[1].1 == phrase[1])
            .expect("phrase tokens found as consecutive words");
        assert_eq!(words[at].0, 1);
        assert!(approx(words[at].2, 0.8) && approx(words[at].3, 0.9));
        assert!(approx(words[at + 1].2, 0.9) && approx(words[at + 1].3, 1.5));
    }

    #[test]
    fn cut_points_are_class_correct() {
        let (corpus, _) = loaded_corpus();
        let cut = |label: &str, t_start: f64| -> (f64, bool) {
            corpus
                .conn
                .query_row(
                    "SELECT cut_t, weak_cut FROM phones WHERE label = ?1 AND t_start = ?2",
                    params![label, t_start],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap()
        };
        // SIL-preceded stops cut at phone start.
        assert!(approx(cut("G", 0.5).0, 0.5));
        assert!(approx(cut("D", 7.2).0, 7.2));
        // Plain stop: 20% into the closure.
        assert!(approx(cut("K", 2.56).0, 2.58));
        assert!(approx(cut("T", 2.4).0, 2.42));
        // Fricative and nasal: midpoint.
        assert!(approx(cut("F", 2.1).0, 2.2));
        assert!(approx(cut("N", 1.3).0, 1.4));
        // Diphthong and glide: midpoint, flagged weak.
        let (ow, ow_weak) = cut("OW1", 0.6);
        assert!(approx(ow, 0.7) && ow_weak);
        let (y, y_weak) = cut("Y", 3.0);
        assert!(approx(y, 3.05) && y_weak);
        assert!(!cut("AH1", 1.1).1);
    }

    #[test]
    fn adjacency_yields_exactly_the_expected_units() {
        let (corpus, _) = loaded_corpus();
        let got: Vec<(i64, String, f64, f64)> = {
            let mut stmt = corpus
                .conn
                .prepare("SELECT seq, label, t_start, t_end FROM diphones ORDER BY seq")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        #[rustfmt::skip]
        let expected: Vec<(i64, &str, f64, f64)> = vec![
            (0,  "SIL-G",  0.30,   0.50),
            (1,  "G-OW",   0.50,   0.70),
            (2,  "OW-AH",  0.70,   0.85),
            (3,  "AH-S",   0.85,   1.00),
            (4,  "S-AH",   1.00,   1.20),
            (5,  "AH-N",   1.20,   1.40),
            (6,  "N-SIL",  1.40,   1.70),
            (7,  "SIL-IH", 1.80,   2.05),
            (8,  "IH-F",   2.05,   2.20),
            (9,  "F-IH",   2.20,   2.3575),  // bridged 15 ms NOISE blip
            (10, "IH-T",   2.3575, 2.42),
            // 60 ms NOISE: hard break (no T-K), seq skips.
            (12, "K-AE",   2.58,   2.76),
            (13, "AE-T",   2.76,   2.888),
            // chunk-edge terminator at 3.0: no T-Y.
            (15, "Y-EH",   3.05,   3.20),
            (16, "EH-S",   3.20,   3.40),
            (17, "S-SIL",  3.40,   3.70),
            (18, "SIL-W",  4.70,   4.95),
            // turn terminator at 5.0: no W-IY, even mid-word.
            (20, "IY-N",   5.10,   5.275),
            (21, "N-OW",   5.275,  5.45),
            (22, "OW-SIL", 5.45,   5.75),
            // SIL-SIL never bonds across the 6.0–7.0 gap.
            (24, "SIL-D",  7.10,   7.20),    // half-duration displacement
            (25, "D-AO",   7.20,   7.40),
            (26, "AO-G",   7.40,   7.52),
            (27, "G-SIL",  7.52,   7.80),
        ];
        assert_eq!(got.len(), expected.len());
        for (g, e) in got.iter().zip(&expected) {
            assert_eq!(g.0, e.0, "seq for {}", e.1);
            assert_eq!(g.1, e.1);
            assert!(approx(g.2, e.2), "{}: t_start {} != {}", e.1, g.2, e.2);
            assert!(approx(g.3, e.3), "{}: t_end {} != {}", e.1, g.3, e.3);
        }
        // The >400 ms silence: X-SIL.t_end != SIL-Y.t_start by construction.
        assert!(approx(got[6].3, 1.70) && approx(got[7].2, 1.80));
    }

    #[test]
    fn zero_extent_terminators_are_recorded_with_provenance() {
        let (corpus, _) = loaded_corpus();
        let origins: Vec<(f64, String)> = {
            let mut stmt = corpus
                .conn
                .prepare(
                    "SELECT t_start, sil_origin FROM phones
                     WHERE t_start = t_end ORDER BY t_start",
                )
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(origins.len(), 2);
        assert!(approx(origins[0].0, 3.0) && origins[0].1 == "chunk");
        assert!(approx(origins[1].0, 5.0) && origins[1].1 == "turn");
        // MFA silences keep their own provenance.
        let mfa: i64 = corpus
            .conn
            .query_row(
                "SELECT count(*) FROM phones WHERE sil_origin = 'mfa'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mfa, 6);
    }

    #[test]
    fn seq_self_join_finds_contiguous_runs() {
        let (corpus, _) = loaded_corpus();
        let contiguous: i64 = corpus
            .conn
            .query_row(
                "SELECT count(*) FROM diphones a
                 JOIN diphones b ON b.source_id = a.source_id AND b.seq = a.seq + 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // 24 diphones in 5 chains: 10 + 1 + 3 + 2 + 3 internal adjacencies.
        assert_eq!(contiguous, 19);
        let next: String = corpus
            .conn
            .query_row(
                "SELECT b.label FROM diphones a
                 JOIN diphones b ON b.source_id = a.source_id AND b.seq = a.seq + 1
                 WHERE a.label = 'G-OW'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(next, "OW-AH");
    }

    #[test]
    fn boundary_features_come_from_the_frame_substrate() {
        let (corpus, _) = loaded_corpus();
        let row = |label: &str| -> (Vec<u8>, Option<f64>, Option<f64>, f64, f64) {
            corpus
                .conn
                .query_row(
                    "SELECT mfcc_head, f0_head, f0_tail, rms_head_db, voiced_fraction
                     FROM diphones WHERE label = ?1",
                    params![label],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .unwrap()
        };
        // SIL-G spans [0.30, 0.50): all frames unvoiced → f0 reads NULL,
        // not a pitch from elsewhere; mfcc head = mean of frames 30,31,32.
        let (mfcc_head, f0_head, f0_tail, rms_head, voiced) = row("SIL-G");
        let mfcc_head = blob_f32(&mfcc_head);
        assert_eq!(mfcc_head.len(), MFCC_DIM);
        for (d, v) in mfcc_head.iter().enumerate() {
            assert!((v - (31.0 + 1000.0 * d as f32)).abs() < 1e-3);
        }
        assert_eq!(f0_head, None);
        assert_eq!(f0_tail, None);
        assert!(approx(rms_head, -20.0));
        assert!(approx(voiced, 0.0));
        // G-OW spans [0.50, 0.70): unvoiced head, voiced tail, half voiced.
        let (_, f0_head, f0_tail, _, voiced) = row("G-OW");
        assert_eq!(f0_head, None);
        assert_eq!(f0_tail, Some(120.0));
        assert!(approx(voiced, 0.5));
    }

    #[test]
    fn speakers_are_loader_derived_from_turns() {
        let (corpus, _) = loaded_corpus();
        let speaker_of = |label: &str| -> i64 {
            corpus
                .conn
                .query_row(
                    "SELECT s.id FROM speakers s WHERE s.label = ?1",
                    params![label],
                    |r| r.get(0),
                )
                .unwrap()
        };
        let (spk0, spk1) = (speaker_of("SPEAKER_00"), speaker_of("SPEAKER_01"));
        // Utterances: max-overlap turn; "we no" straddles the boundary with
        // the second speaker over 20% → multi_speaker.
        let utts: Vec<(String, Option<i64>, bool)> = {
            let mut stmt = corpus
                .conn
                .prepare("SELECT text, speaker_id, multi_speaker FROM utterances ORDER BY t_start")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        assert_eq!(utts[0], ("go a sun".to_string(), Some(spk0), false));
        assert_eq!(utts[3], ("we no".to_string(), Some(spk1), true));
        assert_eq!(utts[4], ("dog".to_string(), Some(spk1), false));
        // Phones split exactly at the turn boundary.
        let phone_speaker = |label: &str, t_start: f64| -> Option<i64> {
            corpus
                .conn
                .query_row(
                    "SELECT speaker_id FROM phones WHERE label = ?1 AND t_start = ?2",
                    params![label, t_start],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(phone_speaker("W", 4.9), Some(spk0));
        assert_eq!(phone_speaker("IY1", 5.0), Some(spk1));
        // Zero-extent terminators overlap nothing.
        assert_eq!(phone_speaker("SIL", 3.0), None);
        // A SIL diphone inherits its real-phone member's speaker.
        let sil_g: Option<i64> = corpus
            .conn
            .query_row(
                "SELECT speaker_id FROM diphones WHERE label = 'SIL-G'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(sil_g, Some(spk0));
    }

    #[test]
    fn reingest_carries_named_speakers_forward_and_keeps_fts_in_sync() {
        let (mut corpus, first) = loaded_corpus();
        let liquid_id: i64 = corpus
            .conn
            .query_row(
                "SELECT id FROM speakers WHERE label = 'SPEAKER_00'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        corpus
            .conn
            .execute(
                "UPDATE speakers SET name = 'Liquid' WHERE id = ?1",
                params![liquid_id],
            )
            .unwrap();

        let second = corpus
            .load_source(&fixture_meta(), &fixture_manifest(), &fixture_prosody())
            .unwrap();
        assert_eq!(second.source_id, first.source_id);
        assert_eq!(second.diphones, first.diphones);

        let count = |sql: &str| -> i64 { corpus.conn.query_row(sql, [], |r| r.get(0)).unwrap() };
        assert_eq!(count("SELECT count(*) FROM sources"), 1);
        assert_eq!(count("SELECT count(*) FROM ingest_runs"), 2);
        assert_eq!(count("SELECT count(*) FROM speakers"), 2);
        assert_eq!(count("SELECT count(*) FROM phones"), 33);

        // Identical turn-set → ≥ 50% overlap → same row, name kept, live.
        let (name, stale): (Option<String>, bool) = corpus
            .conn
            .query_row(
                "SELECT name, stale FROM speakers WHERE id = ?1",
                params![liquid_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(name.as_deref(), Some("Liquid"));
        assert!(!stale);
        // Derived rows point at the carried-forward speaker row.
        let speaker_for_go: Option<i64> = corpus
            .conn
            .query_row(
                "SELECT speaker_id FROM utterances WHERE text = 'go a sun'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(speaker_for_go, Some(liquid_id));

        // The delete/insert cycle went through the FTS sync triggers.
        assert_eq!(
            count("SELECT count(*) FROM utterances_fts WHERE utterances_fts MATCH 'sun'"),
            1
        );
    }

    #[test]
    fn unknown_manifest_version_is_rejected() {
        let mut corpus = Corpus::open_in_memory().unwrap();
        corpus.migrate().unwrap();
        let mut manifest = fixture_manifest();
        manifest.schema_version = 99;
        assert!(matches!(
            corpus.load_source(&fixture_meta(), &manifest, &fixture_prosody()),
            Err(CorpusError::UnknownManifestVersion(99))
        ));
    }

    #[test]
    fn frame_count_violations_are_rejected() {
        let mut corpus = Corpus::open_in_memory().unwrap();
        corpus.migrate().unwrap();
        let mut prosody = fixture_prosody();
        prosody.f0.truncate(900);
        assert!(matches!(
            corpus.load_source(&fixture_meta(), &fixture_manifest(), &prosody),
            Err(CorpusError::FrameMismatch(_))
        ));
        let mut manifest = fixture_manifest();
        manifest.prosody.n_frames = 900; // != 1 + floor(duration/hop)
        assert!(matches!(
            corpus.load_source(&fixture_meta(), &manifest, &fixture_prosody()),
            Err(CorpusError::FrameMismatch(_))
        ));
    }

    #[test]
    fn frame_count_tolerates_f64_floor_rounding() {
        // 4640 samples @ 16 kHz: duration 0.29, and 0.29/0.01 in f64 is
        // 28.999…; the sidecar's integer arithmetic emits 30 frames. A bare
        // floor would reject this valid manifest.
        let mut manifest = fixture_manifest();
        manifest.analysis.duration = 0.29;
        manifest.prosody.n_frames = 30;
        let prosody = ProsodyData {
            hop: 0.01,
            f0: vec![0.0; 30],
            rms_db: vec![0.0; 30],
            mfcc: vec![0.0; 30 * MFCC_DIM],
        };
        assert!(validate_frames(&manifest, &prosody).is_ok());
    }

    #[test]
    fn out_of_range_spans_are_rejected() {
        let load = |m: &Manifest| {
            let mut corpus = Corpus::open_in_memory().unwrap();
            corpus.migrate().unwrap();
            corpus
                .load_source(&fixture_meta(), m, &fixture_prosody())
                .map(|_| ())
        };
        let mut inverted = fixture_manifest();
        inverted.words[0].end = inverted.words[0].start - 0.1;
        assert!(matches!(
            load(&inverted),
            Err(CorpusError::InvalidInterval { what: "word", .. })
        ));
        let mut beyond = fixture_manifest();
        beyond.turns[0].end = 99.0;
        assert!(matches!(
            load(&beyond),
            Err(CorpusError::InvalidInterval { what: "turn", .. })
        ));
        let mut negative = fixture_manifest();
        negative.analysis.duration = -3.0;
        negative.prosody.n_frames = 1;
        assert!(matches!(
            load(&negative),
            Err(CorpusError::InvalidInterval {
                what: "analysis duration",
                ..
            })
        ));
    }

    #[test]
    fn overlapping_or_zero_extent_phonemes_are_rejected() {
        let mut corpus = Corpus::open_in_memory().unwrap();
        corpus.migrate().unwrap();
        let mut overlapping = fixture_manifest();
        overlapping.phonemes[1].start = 0.4; // G now overlaps the initial SIL
        assert!(matches!(
            corpus.load_source(&fixture_meta(), &overlapping, &fixture_prosody()),
            Err(CorpusError::Contract(_))
        ));
        let mut collapsed = fixture_manifest();
        collapsed.phonemes[1].end = collapsed.phonemes[1].start;
        assert!(matches!(
            corpus.load_source(&fixture_meta(), &collapsed, &fixture_prosody()),
            Err(CorpusError::InvalidInterval {
                what: "zero-extent phoneme",
                ..
            })
        ));
    }

    #[test]
    fn stale_named_speaker_is_reclaimed_by_a_later_run() {
        use crate::corpus::manifest::Turn;
        let (mut corpus, _) = loaded_corpus();
        let liquid_id: i64 = corpus
            .conn
            .query_row(
                "SELECT id FROM speakers WHERE label = 'SPEAKER_00'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        corpus
            .conn
            .execute(
                "UPDATE speakers SET name = 'Liquid' WHERE id = ?1",
                params![liquid_id],
            )
            .unwrap();

        // Run 2: diarization misses Liquid entirely; one speaker matching
        // the other voice. Liquid goes stale but keeps its last-known turns.
        let mut second = fixture_manifest();
        second.turns = vec![Turn {
            speaker: "SPEAKER_X".to_string(),
            start: 5.0,
            end: 8.5,
        }];
        corpus
            .load_source(&fixture_meta(), &second, &fixture_prosody())
            .unwrap();
        let (stale, kept_turns): (bool, i64) = corpus
            .conn
            .query_row(
                "SELECT stale, (SELECT count(*) FROM turns WHERE speaker_id = ?1)
                 FROM speakers WHERE id = ?1",
                params![liquid_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(stale);
        assert_eq!(kept_turns, 1);

        // Run 3: the voice is back; overlap with the preserved turns
        // re-claims the named row.
        corpus
            .load_source(&fixture_meta(), &fixture_manifest(), &fixture_prosody())
            .unwrap();
        let (id, name, stale): (i64, Option<String>, bool) = corpus
            .conn
            .query_row(
                "SELECT id, name, stale FROM speakers WHERE label = 'SPEAKER_00'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(id, liquid_id);
        assert_eq!(name.as_deref(), Some("Liquid"));
        assert!(!stale);
    }

    #[test]
    fn fts_indexes_normalized_tokens_so_ordinals_cannot_drift() {
        const NORMALIZED: &str = r#"{
            "schema_version": 1,
            "analysis": { "path": "audio.wav", "duration": 2.0 },
            "tools": {},
            "segments": [
                { "text": "I have 25 cats", "start": 0.1, "end": 1.9,
                  "word_index_start": 0, "word_index_end": 5, "confidence": 0.9 }
            ],
            "words": [
                { "text": "i",      "start": 0.1, "end": 0.3, "segment_index": 0 },
                { "text": "have",   "start": 0.3, "end": 0.6, "segment_index": 0 },
                { "text": "twenty", "start": 0.6, "end": 1.0, "segment_index": 0 },
                { "text": "five",   "start": 1.0, "end": 1.4, "segment_index": 0 },
                { "text": "cats",   "start": 1.4, "end": 1.9, "segment_index": 0 }
            ],
            "phonemes": [],
            "turns": [],
            "chunks": [],
            "prosody": { "path": "prosody.npz", "hop": 0.01, "n_frames": 201 }
        }"#;
        let manifest = Manifest::from_json(NORMALIZED).unwrap();
        let prosody = ProsodyData {
            hop: 0.01,
            f0: vec![0.0; 201],
            rms_db: vec![0.0; 201],
            mfcc: vec![0.0; 201 * MFCC_DIM],
        };
        let mut meta = fixture_meta();
        meta.origin_id = "youtube:normtest".to_string();
        let mut corpus = Corpus::open_in_memory().unwrap();
        corpus.migrate().unwrap();
        corpus.load_source(&meta, &manifest, &prosody).unwrap();

        let (text, text_norm): (String, String) = corpus
            .conn
            .query_row("SELECT text, text_norm FROM utterances", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(text, "I have 25 cats");
        assert_eq!(text_norm, "i have twenty five cats");

        // Phrase tokens land on the word rows' ordinals exactly.
        let count = |query: &str| -> i64 {
            corpus
                .conn
                .query_row(
                    "SELECT count(*) FROM utterances_fts WHERE utterances_fts MATCH ?1",
                    params![query],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(count("\"twenty five\""), 1);
        // Raw-only tokens are not findable — consistent with what the word
        // rows can address, rather than wrongly mapped.
        assert_eq!(count("25"), 0);
        let span: (f64, f64) = corpus
            .conn
            .query_row(
                "SELECT t_start, t_end FROM words WHERE word_ordinal = 2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(approx(span.0, 0.6) && approx(span.1, 1.0));
    }
}
