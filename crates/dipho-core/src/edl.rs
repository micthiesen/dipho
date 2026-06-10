//! The Edit: a program, not a timeline.
//!
//! Non-destructive EDL-as-data — an ordered list of span references plus
//! transforms. Compiles to two targets: mpv EDL (instant zero-render
//! preview) and an ffmpeg invocation (final render).

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::span::{Channel, SourceId, Span};

/// A transform applied to a clip. See the transform-semantics table in
/// DESIGN.md: Loop/Stutter preview natively in mpv EDL; Reverse/Pitch/Speed
/// are render-only (the TUI badges those clips).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Transform {
    /// Whole clip (A+V) plays `count` times (>= 1).
    Loop { count: u32 },
    /// Both streams reversed.
    Reverse,
    /// Audio-only, duration-preserving; video untouched.
    Pitch { semitones: f32 },
    /// A+V time-scale, pitch-preserving.
    Speed { factor: f32 },
    /// First `slice` seconds repeated `repeats` times, then the full clip once.
    Stutter { repeats: u32, slice: f64 },
}

/// One entry in the edit: a span plus the transforms applied to it, in order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Clip {
    pub span: Span,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transforms: Vec<Transform>,
}

/// The edit itself: an ordered list of clips.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Edl {
    pub clips: Vec<Clip>,
}

impl Edl {
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_json(json: &str) -> serde_json::Result<Self> {
        serde_json::from_str(json)
    }
}

/// Maps each source to its playback-master path. Built from the corpus by
/// the caller — keeps the compilers pure and dipho-core I/O-free.
pub type SourceMap = HashMap<SourceId, PathBuf>;

#[derive(Debug, thiserror::Error)]
pub enum EdlCompileError {
    #[error("clip {clip_index}: source {source_id:?} not in the source map")]
    UnresolvedSource {
        clip_index: usize,
        source_id: SourceId,
    },
    #[error("clip {clip_index}: channel {channel:?} is not compilable in MVP (Both only)")]
    ChannelUnsupported { clip_index: usize, channel: Channel },
}

/// An ordered list of complete ffmpeg invocations (argv each) implementing
/// the two-stage render: per-clip extraction to uniform intermediates, then
/// concat into the final encode.
#[derive(Debug, Clone, PartialEq)]
pub struct FfmpegPlan {
    pub invocations: Vec<Vec<String>>,
}

/// Compile an edit to an mpv EDL playlist string for zero-render preview.
/// Clips compile verbatim, in edit order, never merged (join elision only).
/// Milestone: flat EDL preview.
pub fn compile_mpv_edl(_edl: &Edl, _sources: &SourceMap) -> Result<String, EdlCompileError> {
    todo!("compile to mpv EDL (milestone: flat EDL preview)")
}

/// Compile an edit to the two-stage ffmpeg render plan. Milestone: render.
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
                },
                Clip {
                    span: Span {
                        source: SourceId(2),
                        t_start: 0.5,
                        t_end: 1.0,
                        channel: Channel::Audio,
                    },
                    transforms: vec![],
                },
            ],
        };
        let json = edl.to_json().unwrap();
        let back = Edl::from_json(&json).unwrap();
        assert_eq!(edl, back);
    }
}
