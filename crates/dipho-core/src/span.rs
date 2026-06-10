//! Span references: the universal address into immutable sources.

use serde::{Deserialize, Serialize};

/// Identifier of an immutable source in the corpus (SQLite rowid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SourceId(pub i64);

/// Which channels of the source a span addresses. Audio and video are
/// decoupled as first-class: a mix routinely takes the audio of one span
/// over the video of another.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    Audio,
    Video,
    Both,
}

/// A reference into an immutable source. Sources are never edited, only
/// indexed; everything downstream points back into them via spans.
/// Times are in seconds from the start of the source.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Span {
    pub source: SourceId,
    pub t_start: f64,
    pub t_end: f64,
    pub channel: Channel,
}

impl Span {
    pub fn duration(&self) -> f64 {
        self.t_end - self.t_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_is_end_minus_start() {
        let span = Span {
            source: SourceId(1),
            t_start: 1.5,
            t_end: 4.0,
            channel: Channel::Both,
        };
        assert!((span.duration() - 2.5).abs() < f64::EPSILON);
    }

    #[test]
    fn serde_round_trip() {
        let span = Span {
            source: SourceId(42),
            t_start: 0.25,
            t_end: 1.0,
            channel: Channel::Audio,
        };
        let json = serde_json::to_string(&span).unwrap();
        let back: Span = serde_json::from_str(&json).unwrap();
        assert_eq!(span, back);
    }
}
