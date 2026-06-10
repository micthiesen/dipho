//! The sidecar `manifest.json` contract (python/README.md is normative).
//! Versioned; the loader rejects unknown versions. Every timestamp is
//! master-relative — chunk-time rebasing happens inside the sidecar.

use serde::Deserialize;

/// Manifest contract version this loader understands.
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub analysis: Analysis,
    /// Tool + model versions and prosody parameters; stored verbatim per
    /// ingest run so `reingest --stale` can detect parameter changes.
    pub tools: serde_json::Value,
    pub segments: Vec<Segment>,
    pub words: Vec<Word>,
    pub phonemes: Vec<Phoneme>,
    /// Raw diarization turns — the sidecar's only speaker output. All
    /// speaker labels on units are derived by the loader.
    pub turns: Vec<Turn>,
    /// MFA chunk spans. The loader inserts SIL adjacency terminators at
    /// their edges (`sil_origin = 'chunk'`).
    pub chunks: Vec<Chunk>,
    pub prosody: ProsodyMeta,
}

impl Manifest {
    pub fn from_json(json: &str) -> Result<Self, super::CorpusError> {
        Ok(serde_json::from_str(json)?)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Analysis {
    pub path: String,
    pub duration: f64,
}

/// WhisperX segment → one utterance row, the FTS5 document unit.
#[derive(Debug, Clone, Deserialize)]
pub struct Segment {
    pub text: String,
    pub start: f64,
    pub end: f64,
    /// Range into `words`, end-exclusive.
    pub word_index_start: usize,
    pub word_index_end: usize,
    #[serde(default)]
    pub confidence: Option<f64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Word {
    /// Normalized token (digits expanded etc.).
    pub text: String,
    pub start: f64,
    pub end: f64,
    #[serde(default)]
    pub confidence: Option<f64>,
    pub segment_index: usize,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Phoneme {
    /// Stress-marked ARPAbet as MFA emits it; "SIL" for silence, "NOISE"
    /// for `<spn>`.
    pub label: String,
    pub start: f64,
    pub end: f64,
    /// Nullable; reduced within 100 ms of a chunk edge.
    #[serde(default)]
    pub confidence: Option<f64>,
    /// Index into the sidecar's normalized-token mapping (`words`); null
    /// for SIL/NOISE.
    #[serde(default)]
    pub word_index: Option<usize>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Turn {
    pub speaker: String,
    pub start: f64,
    pub end: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Chunk {
    pub start: f64,
    pub end: f64,
    /// False when MFA could not align the chunk within its retry beam:
    /// the span has no phone tier (word-searchable, never
    /// phone-addressable). Defaults true for older manifests.
    #[serde(default = "default_true")]
    pub aligned: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
pub struct ProsodyMeta {
    pub path: String,
    /// Frame i is centered at t = i·hop (librosa `center=True`).
    pub hop: f64,
    /// Must equal `1 + floor(duration / hop)`; the loader rejects
    /// violations.
    pub n_frames: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_aligned_parses_and_defaults_true() {
        let chunk: Chunk = serde_json::from_str(r#"{ "start": 0.0, "end": 1.0 }"#).unwrap();
        assert!(chunk.aligned);
        let chunk: Chunk =
            serde_json::from_str(r#"{ "start": 0.0, "end": 1.0, "aligned": false }"#).unwrap();
        assert!(!chunk.aligned);
    }
}
