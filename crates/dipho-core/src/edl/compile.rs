//! EDL compilation: compile-time validation (reject-never-clamp), the
//! mandatory shared join-elision pre-pass, the preview plan (segments plus
//! per-clip output intervals), and the mpv EDL formatter.
//!
//! Clips compile verbatim, in edit order, never reordered, pad = 0. The
//! elision pre-pass is the single deterministic owner of the output
//! timeline: both compile targets consume the same plan, so preview and
//! render can never disagree about segment boundaries (DESIGN.md).

use std::ops::Range;

use crate::span::Channel;

use super::{Edl, EdlCompileError, SourceMap, Transform};

/// Join elision threshold (the one named constant from DESIGN.md):
/// consecutive same-source, empty-transform clips whose spans are
/// source-contiguous forward within this compile to one segment.
pub const JOIN_ELISION_EPS: f64 = 0.001;

/// Slack for float dust on spans that should end exactly at the source's
/// end (durations come from ffprobe, spans from f64 arithmetic).
const SPAN_EPS: f64 = 1e-9;

/// One segment of the elision-resolved output timeline, in edit order.
#[derive(Debug, Clone, PartialEq)]
pub struct PlannedSegment {
    /// Playback master to read from (resolved from the source map).
    pub master_path: String,
    pub t_start: f64,
    pub t_end: f64,
    /// Chapter title (clip labels; joined for an elided run).
    pub title: Option<String>,
    /// The original clip indices this segment realizes: one clip for a
    /// transform expansion, several for an elided run.
    pub clips: Range<usize>,
}

impl PlannedSegment {
    pub fn duration(&self) -> f64 {
        self.t_end - self.t_start
    }
}

/// The shared compilation pre-pass: validated, elided, transform-expanded
/// segments plus the output-time geometry the TUI needs (seek targets,
/// neighborhood replay windows, position clamping).
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewPlan {
    pub segments: Vec<PlannedSegment>,
    /// Per original clip: its output-time interval (a Loop's interval
    /// covers all its repeats).
    pub clip_output: Vec<(f64, f64)>,
    pub total_duration: f64,
}

impl PreviewPlan {
    /// Output-time boundary list: 0, then each segment's cumulative end.
    /// The golden contract both compile targets must satisfy.
    pub fn boundaries(&self) -> Vec<f64> {
        let mut out = Vec::with_capacity(self.segments.len() + 1);
        let mut t = 0.0;
        out.push(t);
        for segment in &self.segments {
            t += segment.duration();
            out.push(t);
        }
        out
    }
}

/// Validate every clip and compute the preview plan. Errors are typed and
/// carry the clip index; nothing is ever clamped.
pub fn plan_preview(edl: &Edl, sources: &SourceMap) -> Result<PreviewPlan, EdlCompileError> {
    validate(edl, sources)?;
    let units = elide(edl);

    let mut segments = Vec::new();
    let mut clip_output = vec![(0.0, 0.0); edl.clips.len()];
    let mut t_out = 0.0;
    for unit in units {
        // Resolution is validated; every clip in a unit shares one source.
        let first = &edl.clips[unit.clips.start];
        let master_path = sources[&first.span.source]
            .master_path
            .display()
            .to_string();
        if unit.clips.len() > 1 {
            // An elided run: one segment, contiguous in source time, so
            // each constituent clip's output interval is its source offset.
            for i in unit.clips.clone() {
                let s = &edl.clips[i].span;
                clip_output[i] = (
                    t_out + (s.t_start - unit.t_start),
                    t_out + (s.t_end - unit.t_start),
                );
            }
            let labels: Vec<&str> = unit
                .clips
                .clone()
                .filter_map(|i| edl.clips[i].label.as_deref())
                .collect();
            segments.push(PlannedSegment {
                master_path,
                t_start: unit.t_start,
                t_end: unit.t_end,
                title: (!labels.is_empty()).then(|| labels.join(" ")),
                clips: unit.clips,
            });
            t_out += segments.last().unwrap().duration();
        } else {
            let i = unit.clips.start;
            let clip = &edl.clips[i];
            let out_start = t_out;
            for piece in expand_transforms(clip) {
                t_out += piece.1 - piece.0;
                segments.push(PlannedSegment {
                    master_path: master_path.clone(),
                    t_start: piece.0,
                    t_end: piece.1,
                    title: clip.label.clone(),
                    clips: i..i + 1,
                });
            }
            clip_output[i] = (out_start, t_out);
        }
    }
    Ok(PreviewPlan {
        segments,
        clip_output,
        total_duration: t_out,
    })
}

/// The compiled mpv EDL: one quoted entry per planned segment, emittable as
/// the `# mpv EDL v0` document (export) or the `edl://` URI (preview
/// reload) — the same compiler, two spellings.
#[derive(Debug, Clone, PartialEq)]
pub struct MpvEdl {
    segments: Vec<String>,
}

impl MpvEdl {
    pub fn from_plan(plan: &PreviewPlan) -> Self {
        Self {
            segments: plan.segments.iter().map(segment_entry).collect(),
        }
    }

    /// The mpv EDL file form, for `.mpv.edl` export.
    pub fn document(&self) -> String {
        let mut out = String::from("# mpv EDL v0\n");
        for segment in &self.segments {
            out.push_str(segment);
            out.push('\n');
        }
        out
    }

    /// The `edl://` URI form, loaded into the slave on every change.
    pub fn uri(&self) -> String {
        format!("edl://{}", self.segments.join(";"))
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
}

/// Compile an edit to an mpv EDL for zero-render preview. Clips compile
/// verbatim, in edit order; join elision is mandatory and shared with the
/// render target via `plan_preview`.
pub fn compile_mpv_edl(edl: &Edl, sources: &SourceMap) -> Result<MpvEdl, EdlCompileError> {
    Ok(MpvEdl::from_plan(&plan_preview(edl, sources)?))
}

/// One EDL entry: unconditional `%<utf-8-byte-count>%` quoting, named
/// `start=`/`length=` params, explicit float formatting, per-segment
/// `title=`.
fn segment_entry(segment: &PlannedSegment) -> String {
    let mut entry = format!(
        "%{}%{},start={},length={}",
        segment.master_path.len(),
        segment.master_path,
        fmt_time(segment.t_start),
        fmt_time(segment.duration()),
    );
    if let Some(title) = &segment.title {
        // Titles are display-only: keep them from breaking the line/URI
        // framing (quoting already covers `,` and `;`, not separators we
        // join with later).
        let title: String = title
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        entry.push_str(&format!(",title=%{}%{title}", title.len()));
    }
    entry
}

/// Explicit, locale-free float formatting: fixed six decimals, never
/// scientific notation.
fn fmt_time(t: f64) -> String {
    // Normalize negative zero so formatting is a pure function of value.
    format!("{:.6}", if t == 0.0 { 0.0 } else { t })
}

fn validate(edl: &Edl, sources: &SourceMap) -> Result<(), EdlCompileError> {
    for (clip_index, clip) in edl.clips.iter().enumerate() {
        let span = &clip.span;
        let info = sources
            .get(&span.source)
            .ok_or(EdlCompileError::UnresolvedSource {
                clip_index,
                source_id: span.source,
            })?;
        if span.channel != Channel::Both {
            return Err(EdlCompileError::ChannelUnsupported {
                clip_index,
                channel: span.channel,
            });
        }
        let invalid_span = |reason: String| EdlCompileError::InvalidSpan { clip_index, reason };
        if !span.t_start.is_finite() || !span.t_end.is_finite() {
            return Err(invalid_span(format!(
                "non-finite span [{}, {}]",
                span.t_start, span.t_end
            )));
        }
        if span.t_start < 0.0 {
            return Err(invalid_span(format!("t_start {} < 0", span.t_start)));
        }
        if span.t_start >= span.t_end {
            return Err(invalid_span(format!(
                "empty or inverted span [{}, {}]",
                span.t_start, span.t_end
            )));
        }
        if span.t_end > info.duration + SPAN_EPS {
            return Err(invalid_span(format!(
                "t_end {} past source duration {}",
                span.t_end, info.duration
            )));
        }
        for transform in &clip.transforms {
            validate_transform(clip_index, transform, span.duration())?;
        }
    }
    Ok(())
}

fn validate_transform(
    clip_index: usize,
    transform: &Transform,
    clip_len: f64,
) -> Result<(), EdlCompileError> {
    let invalid = |reason: String| EdlCompileError::InvalidTransform { clip_index, reason };
    match transform {
        Transform::Loop { count } => {
            if *count < 1 {
                return Err(invalid("Loop.count must be >= 1".into()));
            }
        }
        Transform::Stutter { repeats, slice } => {
            if *repeats < 1 {
                return Err(invalid("Stutter.repeats must be >= 1".into()));
            }
            if !slice.is_finite() || *slice <= 0.0 || *slice > clip_len + SPAN_EPS {
                return Err(invalid(format!(
                    "Stutter.slice {slice} outside (0, clip length {clip_len}]"
                )));
            }
        }
        Transform::Speed { factor } => {
            if !factor.is_finite() || !(0.25..=4.0).contains(factor) {
                return Err(invalid(format!(
                    "Speed.factor {factor} outside [0.25, 4.0]"
                )));
            }
        }
        Transform::Pitch { semitones } => {
            if !semitones.is_finite() || !(-24.0..=24.0).contains(semitones) {
                return Err(invalid(format!(
                    "Pitch.semitones {semitones} outside [-24, 24]"
                )));
            }
        }
        Transform::Reverse => {}
    }
    Ok(())
}

/// A maximal elidable run (or a single clip). Merged iff `clips` spans more
/// than one index — only empty-transform clips ever merge.
struct Unit {
    t_start: f64,
    t_end: f64,
    clips: Range<usize>,
}

/// The mandatory, deterministic join-elision pre-pass. A clip extends the
/// current run iff both transform lists are empty, source and channel
/// match, its start is source-contiguous forward with the run's end
/// (|gap| <= JOIN_ELISION_EPS), and it strictly extends the run — repeated
/// identical spans never merge.
fn elide(edl: &Edl) -> Vec<Unit> {
    let mut units: Vec<Unit> = Vec::new();
    for (i, clip) in edl.clips.iter().enumerate() {
        if let Some(run) = units.last_mut() {
            let prev = &edl.clips[run.clips.end - 1];
            let mergeable = clip.transforms.is_empty()
                && prev.transforms.is_empty()
                && clip.span.source == prev.span.source
                && clip.span.channel == prev.span.channel
                && (clip.span.t_start - run.t_end).abs() <= JOIN_ELISION_EPS
                && clip.span.t_end > run.t_end;
            if mergeable {
                run.t_end = clip.span.t_end;
                run.clips.end = i + 1;
                continue;
            }
        }
        units.push(Unit {
            t_start: clip.span.t_start,
            t_end: clip.span.t_end,
            clips: i..i + 1,
        });
    }
    units
}

/// Preview expansion of one clip's transform chain into source-time pieces,
/// applied in order. Loop and Stutter are native (repeated segments);
/// Pitch/Speed/Reverse are render-only, so the piece plays plain here (the
/// TUI badges those clips).
fn expand_transforms(clip: &super::Clip) -> Vec<(f64, f64)> {
    let mut pieces = vec![(clip.span.t_start, clip.span.t_end)];
    for transform in &clip.transforms {
        match transform {
            Transform::Loop { count } => {
                let one = pieces.clone();
                for _ in 1..*count {
                    pieces.extend(one.iter().copied());
                }
            }
            Transform::Stutter { repeats, slice } => {
                let prefix = take_prefix(&pieces, *slice);
                let full = std::mem::take(&mut pieces);
                for _ in 0..*repeats {
                    pieces.extend(prefix.iter().copied());
                }
                pieces.extend(full);
            }
            Transform::Pitch { .. } | Transform::Speed { .. } | Transform::Reverse => {}
        }
    }
    pieces
}

/// The first `want` seconds of a piece sequence (validated to fit).
fn take_prefix(pieces: &[(f64, f64)], mut want: f64) -> Vec<(f64, f64)> {
    let mut out = Vec::new();
    for &(t_start, t_end) in pieces {
        let len = t_end - t_start;
        if want <= len + SPAN_EPS {
            out.push((t_start, t_start + want.min(len)));
            break;
        }
        out.push((t_start, t_end));
        want -= len;
    }
    out
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::{Clip, SourceInfo, SourceRef};
    use super::*;
    use crate::span::{SourceId, Span};

    fn clip(source: i64, t_start: f64, t_end: f64) -> Clip {
        Clip {
            span: Span {
                source: SourceId(source),
                t_start,
                t_end,
                channel: Channel::Both,
            },
            transforms: vec![],
            provenance: None,
            label: None,
        }
    }

    fn labeled(source: i64, t_start: f64, t_end: f64, label: &str) -> Clip {
        Clip {
            label: Some(label.to_string()),
            ..clip(source, t_start, t_end)
        }
    }

    fn edl(clips: Vec<Clip>) -> Edl {
        let sources = clips
            .iter()
            .map(|c| {
                (
                    c.span.source,
                    SourceRef {
                        origin: format!("origin-{}", c.span.source.0),
                        origin_id: format!("id-{}", c.span.source.0),
                        master_filename: format!("{}.mkv", c.span.source.0),
                        duration: 100.0,
                        master_hash: format!("hash-{}", c.span.source.0),
                    },
                )
            })
            .collect::<BTreeMap<_, _>>();
        Edl { clips, sources }
    }

    fn sources(ids: &[i64]) -> SourceMap {
        ids.iter()
            .map(|&id| {
                (
                    SourceId(id),
                    SourceInfo {
                        master_path: format!("/masters/{id}.mkv").into(),
                        duration: 100.0,
                        fps: Some(30.0),
                    },
                )
            })
            .collect()
    }

    fn boundaries_from_entries(compiled: &MpvEdl) -> Vec<f64> {
        // Re-derive output boundaries from the compiled entry text: the
        // cross-target golden contract is over what was actually emitted.
        let mut t = 0.0;
        let mut out = vec![0.0];
        for entry in &compiled.segments {
            let length: f64 = entry
                .split(",length=")
                .nth(1)
                .unwrap()
                .split(',')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            t += length;
            out.push(t);
        }
        out
    }

    fn assert_boundaries(a: &[f64], b: &[f64]) {
        assert_eq!(a.len(), b.len(), "{a:?} vs {b:?}");
        for (x, y) in a.iter().zip(b) {
            assert!((x - y).abs() < 1e-6, "{a:?} vs {b:?}");
        }
    }

    #[test]
    fn repeated_identical_spans_compile_to_n_segments() {
        let repeated = edl(vec![
            clip(1, 5.0, 5.5),
            clip(1, 5.0, 5.5),
            clip(1, 5.0, 5.5),
        ]);
        let plan = plan_preview(&repeated, &sources(&[1])).unwrap();
        assert_eq!(plan.segments.len(), 3);
        assert_boundaries(&plan.boundaries(), &[0.0, 0.5, 1.0, 1.5]);
        // Even a sub-eps repeated span never merges (strict forward
        // extension), so a stutter of tiny grains survives compilation.
        let tiny = edl(vec![clip(1, 5.0, 5.0008), clip(1, 5.0, 5.0008)]);
        let plan = plan_preview(&tiny, &sources(&[1])).unwrap();
        assert_eq!(plan.segments.len(), 2);
    }

    #[test]
    fn contiguous_empty_transform_clips_elide_to_one_segment() {
        // 1.0 ms gap (exactly the eps) merges; 1.1 ms does not; a backward
        // jump never does.
        let edl = edl(vec![
            labeled(1, 1.0, 2.0, "twenty"),
            labeled(1, 2.001, 3.0, "five"),
            clip(1, 3.0011, 4.0),
            clip(1, 2.0, 2.5),
        ]);
        let plan = plan_preview(&edl, &sources(&[1])).unwrap();
        assert_eq!(plan.segments.len(), 3);
        let merged = &plan.segments[0];
        assert!((merged.t_start - 1.0).abs() < 1e-9 && (merged.t_end - 3.0).abs() < 1e-9);
        assert_eq!(merged.title.as_deref(), Some("twenty five"));
        assert_eq!(merged.clips, 0..2);

        // The output-time boundary list is identical between the plan (the
        // shared pre-pass, which the M6 ffmpeg compiler also consumes) and
        // the compiled mpv EDL.
        let compiled = compile_mpv_edl(&edl, &sources(&[1])).unwrap();
        assert_boundaries(&plan.boundaries(), &boundaries_from_entries(&compiled));

        // Per-clip output intervals point inside the merged segment.
        assert_eq!(plan.clip_output[0], (0.0, 1.0));
        assert!((plan.clip_output[1].0 - 1.001).abs() < 1e-9);
        assert!((plan.clip_output[1].1 - 2.0).abs() < 1e-9);
    }

    #[test]
    fn elision_never_crosses_sources_and_requires_empty_transforms() {
        let mut transformed = clip(1, 2.0, 3.0);
        transformed.transforms = vec![Transform::Pitch { semitones: 2.0 }];
        let edl = edl(vec![
            clip(1, 1.0, 2.0),
            transformed,
            clip(2, 3.0, 4.0),
            clip(2, 4.0, 5.0),
        ]);
        let plan = plan_preview(&edl, &sources(&[1, 2])).unwrap();
        // Clip 0 and 1 are source-contiguous but 1 has a transform; clips
        // 2 and 3 are a different source but contiguous with each other.
        assert_eq!(plan.segments.len(), 3);
        assert_eq!(plan.segments[2].clips, 2..4);
    }

    #[test]
    fn two_contiguous_loop2_clips_compile_to_aabb_never_elided() {
        let looped = |t_start: f64, t_end: f64| Clip {
            transforms: vec![Transform::Loop { count: 2 }],
            ..clip(1, t_start, t_end)
        };
        let edl = edl(vec![looped(1.0, 2.0), looped(2.0, 3.0)]);
        let plan = plan_preview(&edl, &sources(&[1])).unwrap();
        let spans: Vec<(f64, f64)> = plan.segments.iter().map(|s| (s.t_start, s.t_end)).collect();
        assert_eq!(
            spans,
            vec![(1.0, 2.0), (1.0, 2.0), (2.0, 3.0), (2.0, 3.0)],
            "AABB"
        );
        assert_eq!(plan.clip_output[0], (0.0, 2.0));
        assert_eq!(plan.clip_output[1], (2.0, 4.0));
        assert!((plan.total_duration - 4.0).abs() < 1e-9);
    }

    #[test]
    fn stutter_expands_to_repeated_slices_then_the_full_clip() {
        let stuttered = Clip {
            transforms: vec![Transform::Stutter {
                repeats: 3,
                slice: 0.06,
            }],
            ..labeled(1, 10.0, 10.5, "ok")
        };
        let plan = plan_preview(&edl(vec![stuttered]), &sources(&[1])).unwrap();
        let spans: Vec<(f64, f64)> = plan.segments.iter().map(|s| (s.t_start, s.t_end)).collect();
        assert_eq!(spans.len(), 4);
        for s in &spans[..3] {
            assert!((s.0 - 10.0).abs() < 1e-9 && (s.1 - 10.06).abs() < 1e-9);
        }
        assert_eq!(spans[3], (10.0, 10.5));
        assert!((plan.total_duration - 0.68).abs() < 1e-9);
        assert!(
            plan.segments
                .iter()
                .all(|s| s.title.as_deref() == Some("ok"))
        );
    }

    #[test]
    fn render_only_transforms_preview_plain() {
        let badged = Clip {
            transforms: vec![
                Transform::Reverse,
                Transform::Speed { factor: 2.0 },
                Transform::Pitch { semitones: -3.0 },
            ],
            ..clip(1, 1.0, 2.0)
        };
        let plan = plan_preview(&edl(vec![badged]), &sources(&[1])).unwrap();
        assert_eq!(plan.segments.len(), 1);
        assert_eq!(
            (plan.segments[0].t_start, plan.segments[0].t_end),
            (1.0, 2.0)
        );
    }

    #[test]
    fn mpv_edl_document_and_uri_golden() {
        let edl = edl(vec![
            labeled(1, 1.0, 2.0, "twenty"),
            labeled(1, 2.0, 3.0, "five"),
            labeled(2, 0.25, 0.5, "ok"),
        ]);
        let compiled = compile_mpv_edl(&edl, &sources(&[1, 2])).unwrap();
        assert_eq!(
            compiled.document(),
            "# mpv EDL v0\n\
             %14%/masters/1.mkv,start=1.000000,length=2.000000,title=%11%twenty five\n\
             %14%/masters/2.mkv,start=0.250000,length=0.250000,title=%2%ok\n"
        );
        assert_eq!(
            compiled.uri(),
            "edl://%14%/masters/1.mkv,start=1.000000,length=2.000000,title=%11%twenty five;\
             %14%/masters/2.mkv,start=0.250000,length=0.250000,title=%2%ok"
        );
    }

    #[test]
    fn quoting_counts_utf8_bytes_and_strips_control_chars() {
        let mut edl = edl(vec![labeled(1, 0.0, 1.0, "héllo\nwörld")]);
        edl.clips[0].span.t_start = 0.0;
        let mut map = SourceMap::new();
        map.insert(
            SourceId(1),
            SourceInfo {
                master_path: "/m/with,comma;semi.mkv".into(),
                duration: 100.0,
                fps: None,
            },
        );
        let compiled = compile_mpv_edl(&edl, &map).unwrap();
        // Path: 22 bytes incl. the comma and semicolon, safely quoted.
        // Title: "héllo wörld" = 13 utf-8 bytes, newline → space.
        assert_eq!(
            compiled.document(),
            "# mpv EDL v0\n\
             %22%/m/with,comma;semi.mkv,start=0.000000,length=1.000000,title=%13%héllo wörld\n"
        );
    }

    #[test]
    fn an_empty_edit_compiles_to_an_empty_edl() {
        let plan = plan_preview(&edl(vec![]), &SourceMap::new()).unwrap();
        assert!(plan.segments.is_empty());
        assert_eq!(plan.total_duration, 0.0);
        let compiled = MpvEdl::from_plan(&plan);
        assert!(compiled.is_empty());
        assert_eq!(compiled.document(), "# mpv EDL v0\n");
    }

    #[test]
    fn invalid_spans_are_rejected_with_the_clip_index() {
        let cases: Vec<(Clip, &str)> = vec![
            (clip(1, -0.5, 1.0), "< 0"),
            (clip(1, 2.0, 2.0), "empty or inverted"),
            (clip(1, 3.0, 2.0), "empty or inverted"),
            (clip(1, 1.0, 200.0), "past source duration"),
            (clip(1, f64::NAN, 1.0), "non-finite"),
            (clip(1, 0.0, f64::INFINITY), "non-finite"),
        ];
        for (bad, expected) in cases {
            let edl = edl(vec![clip(1, 0.0, 1.0), bad]);
            match plan_preview(&edl, &sources(&[1])) {
                Err(EdlCompileError::InvalidSpan {
                    clip_index: 1,
                    reason,
                }) => {
                    assert!(reason.contains(expected), "{reason} vs {expected}");
                }
                other => panic!("expected InvalidSpan({expected}), got {other:?}"),
            }
        }
    }

    #[test]
    fn invalid_transforms_are_rejected() {
        let cases: Vec<(Transform, &str)> = vec![
            (Transform::Loop { count: 0 }, "Loop.count"),
            (
                Transform::Stutter {
                    repeats: 0,
                    slice: 0.06,
                },
                "Stutter.repeats",
            ),
            (
                Transform::Stutter {
                    repeats: 2,
                    slice: 0.0,
                },
                "Stutter.slice",
            ),
            (
                Transform::Stutter {
                    repeats: 2,
                    slice: 1.5,
                },
                "Stutter.slice",
            ),
            (Transform::Speed { factor: 0.1 }, "Speed.factor"),
            (Transform::Speed { factor: f64::NAN }, "Speed.factor"),
            (Transform::Pitch { semitones: 25.0 }, "Pitch.semitones"),
        ];
        for (bad, expected) in cases {
            let mut c = clip(1, 0.0, 1.0);
            c.transforms = vec![bad];
            match plan_preview(&edl(vec![c]), &sources(&[1])) {
                Err(EdlCompileError::InvalidTransform {
                    clip_index: 0,
                    reason,
                }) => {
                    assert!(reason.contains(expected), "{reason} vs {expected}");
                }
                other => panic!("expected InvalidTransform({expected}), got {other:?}"),
            }
        }
    }

    #[test]
    fn unresolved_source_and_unsupported_channel_are_typed() {
        let edl1 = edl(vec![clip(9, 0.0, 1.0)]);
        assert!(matches!(
            plan_preview(&edl1, &sources(&[1])),
            Err(EdlCompileError::UnresolvedSource {
                clip_index: 0,
                source_id: SourceId(9)
            })
        ));
        let mut audio_only = clip(1, 0.0, 1.0);
        audio_only.span.channel = Channel::Audio;
        assert!(matches!(
            plan_preview(&edl(vec![audio_only]), &sources(&[1])),
            Err(EdlCompileError::ChannelUnsupported {
                clip_index: 0,
                channel: Channel::Audio
            })
        ));
    }
}
