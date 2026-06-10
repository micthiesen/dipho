//! The Edit: a program, not a timeline.
//!
//! Non-destructive EDL-as-data — an ordered list of span references plus
//! transforms. Compiles to two targets: mpv EDL (instant zero-render
//! preview) and an ffmpeg invocation (final render).

use serde::{Deserialize, Serialize};

use crate::span::Span;

/// A transform applied to a clip. Preview (mpv) may degrade some of these;
/// render (ffmpeg) compiles all of them faithfully.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Transform {
    Loop { count: u32 },
    Reverse,
    Pitch { semitones: f32 },
    Speed { factor: f32 },
    Stutter { repeats: u32 },
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

/// Compile an edit to an mpv EDL playlist string for zero-render preview.
/// Milestone: flat EDL preview.
pub fn compile_mpv_edl(_edl: &Edl) -> String {
    todo!("compile to mpv EDL (milestone: flat EDL preview)")
}

/// Compile an edit to ffmpeg arguments for final render.
/// Milestone: render.
pub fn compile_ffmpeg(_edl: &Edl) -> Vec<String> {
    todo!("compile to ffmpeg invocation (milestone: render)")
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
                    transforms: vec![Transform::Stutter { repeats: 3 }],
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
