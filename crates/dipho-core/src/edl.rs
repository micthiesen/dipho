//! The Edit: a program, not a timeline.
//!
//! Non-destructive EDL-as-data — an ordered list of clips (span references
//! plus transforms) with a mandatory sources manifest, compiling to two
//! targets: mpv EDL (instant zero-render preview) and a two-stage ffmpeg
//! render plan. See DESIGN.md for compilation semantics (verbatim order,
//! mandatory shared-pre-pass join elision, audio-master frame quantization).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::span::{Channel, SourceId, Span};

/// A transform applied to a clip. See the transform-semantics table in
/// DESIGN.md: Loop/Stutter preview natively in mpv EDL; Reverse/Pitch/Speed
/// are render-only (the TUI badges those clips). Parameter bounds are
/// enforced at compile time (reject-never-clamp).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Transform {
    /// Whole clip (A+V) plays `count` times (>= 1).
    Loop { count: u32 },
    /// Both streams reversed.
    Reverse,
    /// Audio-only, duration-preserving; video untouched. Semitones in [-24, 24].
    Pitch { semitones: f32 },
    /// A+V time-scale, pitch-preserving. Factor in [0.25, 4.0].
    Speed { factor: f32 },
    /// First `slice` seconds repeated `repeats` times, then the full clip
    /// once. repeats >= 1, 0 < slice <= clip length.
    Stutter { repeats: u32, slice: f64 },
}

/// The corpus unit a clip was created from. Lets later features re-derive
/// context; the clip's own span stays authoritative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "id")]
pub enum ProvenanceRef {
    Word(i64),
    Diphone(i64),
    Utterance(i64),
}

/// One entry in the edit: a span (an owned, nudgeable copy — the corpus
/// stays immutable) plus the transforms applied to it, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub span: Span,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transforms: Vec<Transform>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<ProvenanceRef>,
}

/// Entry in the edit file's mandatory sources manifest. Rebind precedence:
/// master_hash match → bind; origin_id match (duration ±0.5 s) → bind with
/// warning; neither → UnresolvedSource.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SourceRef {
    pub origin: String,
    pub origin_id: String,
    pub master_filename: String,
    pub duration: f64,
    pub master_hash: String,
}

/// The edit itself: an ordered list of clips plus the manifest that makes
/// the file self-describing. Deserialization of an edit without a manifest
/// entry for every referenced source is rejected at load.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Edl {
    pub clips: Vec<Clip>,
    pub sources: BTreeMap<SourceId, SourceRef>,
}

impl Edl {
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }
}

/// Per-source data the compilers need, resolved from the corpus by the
/// caller — keeps the compilers pure and dipho-core I/O-free.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceInfo {
    pub master_path: PathBuf,
    pub duration: f64,
    pub fps: f64,
}

pub type SourceMap = HashMap<SourceId, SourceInfo>;

#[derive(Debug, thiserror::Error)]
pub enum EdlCompileError {
    #[error("clip {clip_index}: source {source_id:?} not in the source map")]
    UnresolvedSource {
        clip_index: usize,
        source_id: SourceId,
    },
    #[error("clip {clip_index}: channel {channel:?} is not compilable in MVP (Both only)")]
    ChannelUnsupported { clip_index: usize, channel: Channel },
    #[error("clip {clip_index}: invalid span: {reason}")]
    InvalidSpan { clip_index: usize, reason: String },
    #[error("clip {clip_index}: invalid transform: {reason}")]
    InvalidTransform { clip_index: usize, reason: String },
}

/// An ordered list of complete ffmpeg invocations (argv each) implementing
/// the two-stage render: per-clip frame-exact extraction to uniform
/// intermediates, then concat into the final encode.
#[derive(Debug, Clone, PartialEq)]
pub struct FfmpegPlan {
    pub invocations: Vec<Vec<String>>,
}

/// Compile an edit to an mpv EDL playlist string for zero-render preview.
/// Clips compile verbatim, in edit order; join elision is mandatory and
/// shared with compile_ffmpeg. Milestone: flat EDL preview.
pub fn compile_mpv_edl(_edl: &Edl, _sources: &SourceMap) -> Result<String, EdlCompileError> {
    todo!("compile to mpv EDL (milestone: flat EDL preview)")
}

/// Compile an edit to the two-stage ffmpeg render plan, including the
/// audio-master frame-quantization planning pass. Milestone: render.
pub fn compile_ffmpeg(_edl: &Edl, _sources: &SourceMap) -> Result<FfmpegPlan, EdlCompileError> {
    todo!("compile to ffmpeg plan (milestone: render)")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::{Channel, SourceId, Span};

    #[test]
    fn edl_json_round_trip() {
        let edl = Edl {
            clips: vec![
                Clip {
                    span: Span {
                        source: SourceId(1),
                        t_start: 12.0,
                        t_end: 12.8,
                        channel: Channel::Both,
                    },
                    transforms: vec![Transform::Stutter {
                        repeats: 3,
                        slice: 0.06,
                    }],
                    provenance: Some(ProvenanceRef::Word(42)),
                },
                Clip {
                    span: Span {
                        source: SourceId(1),
                        t_start: 0.5,
                        t_end: 1.0,
                        channel: Channel::Both,
                    },
                    transforms: vec![],
                    provenance: None,
                },
            ],
            sources: BTreeMap::from([(
                SourceId(1),
                SourceRef {
                    origin: "https://example.com/watch?v=abc123".into(),
                    origin_id: "youtube:abc123".into(),
                    master_filename: "abc123.mkv".into(),
                    duration: 1234.5,
                    master_hash: "deadbeef".into(),
                },
            )]),
        };
        let json = edl.to_json().unwrap();
        let back = Edl::from_json(&json).unwrap();
        assert_eq!(edl, back);
    }
}
