//! The Edit: a program, not a timeline.
//!
//! Non-destructive EDL-as-data — an ordered list of clips (span references
//! plus transforms) with a mandatory sources manifest, compiling to two
//! targets: mpv EDL (instant zero-render preview) and a two-stage ffmpeg
//! render plan. See DESIGN.md for compilation semantics (verbatim order,
//! mandatory shared-pre-pass join elision, audio-master frame quantization).

mod compile;
mod rebind;
mod render;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::span::{Channel, SourceId, Span};

pub use compile::{
    JOIN_ELISION_EPS, MpvEdl, PlannedSegment, PreviewPlan, compile_mpv_edl, plan_preview,
};
pub use rebind::{CorpusSource, REBIND_DURATION_TOLERANCE, Rebind, RebindError, rebind};
pub use render::{
    AUDIO_RATE, FfmpegPlan, OutputProfile, RenderSpec, VideoProps, compile_ffmpeg, select_profile,
};

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
    Speed { factor: f64 },
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
    /// Display label (the matched phrase at append time): the EDL segment
    /// `title=`, giving free chapter-per-cut navigation in mpv.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
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

#[derive(Debug, thiserror::Error)]
pub enum EdlLoadError {
    #[error("malformed edit file: {0}")]
    Json(#[from] serde_json::Error),
    #[error("clip {clip_index} references source {source_id:?} missing from the sources manifest")]
    MissingManifestEntry {
        clip_index: usize,
        source_id: SourceId,
    },
}

impl Edl {
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    /// Parse an edit file, rejecting any edit whose mandatory sources
    /// manifest doesn't cover every referenced source.
    pub fn from_json(json: &str) -> Result<Self, EdlLoadError> {
        let edl: Edl = serde_json::from_str(json)?;
        for (clip_index, clip) in edl.clips.iter().enumerate() {
            if !edl.sources.contains_key(&clip.span.source) {
                return Err(EdlLoadError::MissingManifestEntry {
                    clip_index,
                    source_id: clip.span.source,
                });
            }
        }
        Ok(edl)
    }
}

/// Per-source data the compilers need, resolved from the corpus by the
/// caller — keeps the compilers pure and dipho-core I/O-free.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceInfo {
    pub master_path: PathBuf,
    pub duration: f64,
    /// Post-normalization CFR rate; None for audio-only sources. The mpv
    /// preview compiler never needs it; ffmpeg frame quantization (M6)
    /// requires it for any video-bearing clip.
    pub fps: Option<f64>,
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
    #[error(
        "clip {clip_index}: transform {transform} is render-only and not implemented yet (post-MVP)"
    )]
    TransformUnsupported {
        clip_index: usize,
        transform: &'static str,
    },
    #[error(
        "clip {clip_index}: source {source_id:?} has no video frame rate — frame quantization needs fps"
    )]
    MissingFps {
        clip_index: usize,
        source_id: SourceId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::{Channel, SourceId, Span};

    fn span(source: i64, t_start: f64, t_end: f64) -> Span {
        Span {
            source: SourceId(source),
            t_start,
            t_end,
            channel: Channel::Both,
        }
    }

    fn source_ref(name: &str) -> SourceRef {
        SourceRef {
            origin: format!("https://example.com/watch?v={name}"),
            origin_id: format!("youtube:{name}"),
            master_filename: format!("{name}.mkv"),
            duration: 1234.5,
            master_hash: format!("hash-{name}"),
        }
    }

    #[test]
    fn edl_json_round_trip() {
        let edl = Edl {
            clips: vec![
                Clip {
                    span: span(1, 12.0, 12.8),
                    transforms: vec![Transform::Stutter {
                        repeats: 3,
                        slice: 0.06,
                    }],
                    provenance: Some(ProvenanceRef::Word(42)),
                    label: Some("hello".into()),
                },
                Clip {
                    span: span(1, 0.5, 1.0),
                    transforms: vec![],
                    provenance: None,
                    label: None,
                },
            ],
            sources: BTreeMap::from([(SourceId(1), source_ref("abc123"))]),
        };
        let json = edl.to_json().unwrap();
        let back = Edl::from_json(&json).unwrap();
        assert_eq!(edl, back);
    }

    #[test]
    fn load_rejects_a_clip_without_a_manifest_entry() {
        let edl = Edl {
            clips: vec![Clip {
                span: span(7, 0.0, 1.0),
                transforms: vec![],
                provenance: None,
                label: None,
            }],
            sources: BTreeMap::from([(SourceId(1), source_ref("abc123"))]),
        };
        let json = edl.to_json().unwrap();
        match Edl::from_json(&json) {
            Err(EdlLoadError::MissingManifestEntry {
                clip_index: 0,
                source_id: SourceId(7),
            }) => {}
            other => panic!("expected MissingManifestEntry, got {other:?}"),
        }
    }
}
