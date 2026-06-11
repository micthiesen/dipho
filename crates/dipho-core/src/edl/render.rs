//! The ffmpeg render target: a two-stage plan over the shared elision
//! pre-pass. Stage 1 extracts each planned segment frame-exactly from its
//! all-intra master into a uniform ProRes 422 HQ + pcm_s24le intermediate;
//! stage 2 concatenates the intermediates into the final encode.
//!
//! Frame quantization — one algorithm, audio is master (DESIGN.md): audio
//! cuts are sample-accurate and authoritative; segment k gets
//! `n_k = round(T_k·fps) − round(T_{k−1}·fps)` video frames (cumulative
//! rounding is the error diffusion, so |video − audio| ≤ half a frame at
//! every boundary), starting at master frame `f_k = floor(t_start·fps +
//! 1e-9)`. Frames are selected by count (`trim=end_frame=n_k`), never by
//! `trim=start=<seconds>`.

use std::path::PathBuf;

use super::compile::fmt_time;
use super::{Edl, EdlCompileError, SourceMap, Transform, plan_preview};

/// Output audio: 48 kHz stereo, per the project profile.
pub const AUDIO_RATE: u32 = 48_000;

/// How far before a segment's audio cut the audio input seek lands. The
/// seek only positions the demuxer; `atrim` then cuts sample-exactly in
/// seek-shifted time, so the lead just has to out-pad cluster/interleaving
/// granularity in the master.
const AUDIO_SEEK_LEAD: f64 = 1.0;

/// Tolerance for treating a source's fps as equal to the profile's, i.e.
/// for skipping the `fps` resampling filter.
const FPS_EPS: f64 = 1e-6;

/// The render output profile: largest post-normalization display area
/// among the edit's sources, fps from that source, 48 kHz stereo audio;
/// `--size`/`--fps` override (DESIGN.md).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OutputProfile {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

/// One video-bearing source's properties, the input to profile selection.
/// Resolution is probed from the master by the caller — the corpus schema
/// stores fps but not dimensions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VideoProps {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

/// Pick the project profile: the source with the largest display area
/// wins and contributes both resolution and fps. Deterministic tie-break
/// by (area, width, fps); None iff no video-bearing source.
pub fn select_profile(videos: impl IntoIterator<Item = VideoProps>) -> Option<OutputProfile> {
    videos
        .into_iter()
        .max_by(|a, b| {
            let area = |v: &VideoProps| u64::from(v.width) * u64::from(v.height);
            (area(a), a.width)
                .cmp(&(area(b), b.width))
                .then(a.fps.total_cmp(&b.fps))
        })
        .map(|v| OutputProfile {
            width: v.width,
            height: v.height,
            fps: v.fps,
        })
}

/// Caller-resolved render parameters: the output profile plus where the
/// plan's artifacts live. The compiler stays a pure function over these.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderSpec {
    pub profile: OutputProfile,
    /// Per-edit intermediates directory (`./.dipho/render/<edit-hash>/`):
    /// deleted on success, kept on failure.
    pub intermediates_dir: PathBuf,
    pub output: PathBuf,
}

/// The two-stage render plan: complete ffmpeg invocations (argv each,
/// `ffmpeg` included) plus the concat demuxer list the runner writes
/// before stage 2.
#[derive(Debug, Clone, PartialEq)]
pub struct FfmpegPlan {
    /// One frame-exact extraction per planned segment, in output order.
    pub stage1: Vec<Vec<String>>,
    /// The intermediate each stage-1 invocation writes.
    pub intermediates: Vec<PathBuf>,
    pub concat_list_path: PathBuf,
    /// Body of the concat demuxer list (ffconcat v1.0, relative names).
    pub concat_list: String,
    /// The concat → final-encode invocation.
    pub stage2: Vec<String>,
    pub total_duration: f64,
}

/// Compile an edit to the two-stage ffmpeg render plan, consuming the same
/// elision pre-pass as the mpv preview so the two targets can never
/// disagree about the output timeline.
pub fn compile_ffmpeg(
    edl: &Edl,
    sources: &SourceMap,
    spec: &RenderSpec,
) -> Result<FfmpegPlan, EdlCompileError> {
    // Loop and Stutter render through segment expansion; the render-only
    // transforms are post-MVP and are rejected, never silently rendered
    // plain (the preview plays them plain because it badges the clip — the
    // render is the reference and has no badge to show).
    for (clip_index, clip) in edl.clips.iter().enumerate() {
        for transform in &clip.transforms {
            let unsupported = match transform {
                Transform::Pitch { .. } => Some("Pitch"),
                Transform::Speed { .. } => Some("Speed"),
                Transform::Reverse => Some("Reverse"),
                Transform::Loop { .. } | Transform::Stutter { .. } => None,
            };
            if let Some(transform) = unsupported {
                return Err(EdlCompileError::TransformUnsupported {
                    clip_index,
                    transform,
                });
            }
        }
    }

    let plan = plan_preview(edl, sources)?;
    let profile = spec.profile;

    let mut stage1 = Vec::with_capacity(plan.segments.len());
    let mut intermediates = Vec::with_capacity(plan.segments.len());
    let mut concat_list = String::from("ffconcat version 1.0\n");
    let mut t_cum = 0.0;
    let mut frames_cum: i64 = 0;
    for (k, segment) in plan.segments.iter().enumerate() {
        let clip_index = segment.clips.start;
        let source_id = edl.clips[clip_index].span.source;
        let src_fps = sources[&source_id].fps.ok_or(EdlCompileError::MissingFps {
            clip_index,
            source_id,
        })?;

        // Cumulative-rounding frame budget at the profile rate.
        t_cum += segment.duration();
        let total = (t_cum * profile.fps).round() as i64;
        let n_k = total - frames_cum;
        frames_cum = total;
        // First master frame, on the source's own frame grid.
        let f_k = (segment.t_start * src_fps + 1e-9).floor() as i64;

        let name = format!("clip-{k:03}.mov");
        let out = spec.intermediates_dir.join(&name);
        stage1.push(stage1_invocation(
            segment, src_fps, f_k, n_k, &profile, &out,
        ));
        intermediates.push(out);
        concat_list.push_str(&format!("file '{name}'\n"));
    }

    let concat_list_path = spec.intermediates_dir.join("concat.txt");
    let stage2 = stage2_invocation(&concat_list_path, &spec.output);
    Ok(FfmpegPlan {
        stage1,
        intermediates,
        concat_list_path,
        concat_list,
        stage2,
        total_duration: plan.total_duration,
    })
}

/// One frame-exact extraction. The video input seeks half a source frame
/// before frame `f_k` — accurate input seeking discards decoded frames
/// before the requested time, and the half-frame margin absorbs the
/// master's millisecond container-timestamp rounding — then takes exactly
/// `n_k` frames by count. The audio input seeks `AUDIO_SEEK_LEAD` early
/// and `atrim` cuts sample-exactly in seek-shifted time (the shift is
/// exactly the requested seek, whatever packet the demuxer landed on).
fn stage1_invocation(
    segment: &super::compile::PlannedSegment,
    src_fps: f64,
    f_k: i64,
    n_k: i64,
    profile: &OutputProfile,
    out: &std::path::Path,
) -> Vec<String> {
    let video_seek = ((f_k as f64 - 0.5) / src_fps).max(0.0);
    let audio_seek = (segment.t_start - AUDIO_SEEK_LEAD).max(0.0);

    let mut video_chain = if (src_fps - profile.fps).abs() <= FPS_EPS {
        format!("[0:v]trim=end_frame={n_k},setpts=PTS-STARTPTS")
    } else {
        // Resample to the profile rate, then trim to the exact budget.
        // Take enough source frames to cover n_k output frames plus slack
        // for the resampler's phase.
        let m_k = ((n_k as f64) * src_fps / profile.fps).ceil() as i64 + 2;
        format!(
            "[0:v]trim=end_frame={m_k},setpts=PTS-STARTPTS,fps={fps},\
             trim=end_frame={n_k},setpts=PTS-STARTPTS",
            fps = fmt_time(profile.fps)
        )
    };
    video_chain.push_str(&format!(
        ",scale={w}:{h}:force_original_aspect_ratio=decrease:force_divisible_by=2,\
         pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1[v]",
        w = profile.width,
        h = profile.height
    ));
    let audio_chain = format!(
        "[1:a]atrim=start={start}:end={end},asetpts=PTS-STARTPTS,\
         aresample={AUDIO_RATE},aformat=sample_fmts=s32:channel_layouts=stereo[a]",
        start = fmt_time(segment.t_start - audio_seek),
        end = fmt_time(segment.t_end - audio_seek),
    );

    [
        "ffmpeg",
        "-hide_banner",
        "-nostdin",
        "-v",
        "error",
        "-y",
        "-ss",
        &fmt_time(video_seek),
        "-i",
        &segment.master_path,
        "-ss",
        &fmt_time(audio_seek),
        "-i",
        &segment.master_path,
        "-filter_complex",
        &format!("{video_chain};{audio_chain}"),
        "-map",
        "[v]",
        "-map",
        "[a]",
        "-c:v",
        "prores_ks",
        "-profile:v",
        "3",
        "-vendor",
        "apl0",
        "-pix_fmt",
        "yuv422p10le",
        "-fps_mode",
        "cfr",
        "-r",
        &fmt_time(profile.fps),
        "-c:a",
        "pcm_s24le",
        &out.display().to_string(),
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Concat demuxer over the uniform intermediates → the final encode.
/// h.264 high + yuv420p + aac + faststart: the QuickTime-safe default.
fn stage2_invocation(concat_list: &std::path::Path, output: &std::path::Path) -> Vec<String> {
    let mut argv: Vec<String> = [
        "ffmpeg",
        "-hide_banner",
        "-nostdin",
        "-v",
        "error",
        "-y",
        "-f",
        "concat",
        "-i",
        &concat_list.display().to_string(),
        "-map",
        "0:v:0",
        "-map",
        "0:a:0",
        "-c:v",
        "libx264",
        "-crf",
        "16",
        "-preset",
        "medium",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-b:a",
        "256k",
    ]
    .into_iter()
    .map(String::from)
    .collect();
    let ext = output
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase());
    if matches!(ext.as_deref(), Some("mp4" | "mov" | "m4v")) {
        argv.extend(["-movflags".into(), "+faststart".into()]);
    }
    argv.push(output.display().to_string());
    argv
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::super::{Clip, SourceInfo, SourceRef, compile_mpv_edl};
    use super::*;
    use crate::span::{Channel, SourceId, Span};

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

    fn sources(specs: &[(i64, Option<f64>)]) -> SourceMap {
        specs
            .iter()
            .map(|&(id, fps)| {
                (
                    SourceId(id),
                    SourceInfo {
                        master_path: format!("/masters/{id}.mkv").into(),
                        duration: 100.0,
                        fps,
                    },
                )
            })
            .collect()
    }

    fn spec() -> RenderSpec {
        RenderSpec {
            profile: OutputProfile {
                width: 640,
                height: 360,
                fps: 30.0,
            },
            intermediates_dir: "/work".into(),
            output: "/out/mix.mp4".into(),
        }
    }

    fn arg_after<'a>(argv: &'a [String], flag: &str) -> &'a str {
        let i = argv.iter().position(|a| a == flag).unwrap();
        &argv[i + 1]
    }

    /// Frame budget per segment, recovered from the emitted filtergraphs:
    /// the last `trim=end_frame=` in the video chain carries it.
    fn frame_counts(plan: &FfmpegPlan) -> Vec<i64> {
        plan.stage1
            .iter()
            .map(|argv| {
                let fc = arg_after(argv, "-filter_complex");
                let vchain = fc.split(';').next().unwrap();
                vchain
                    .rsplit("trim=end_frame=")
                    .next()
                    .unwrap()
                    .split(',')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect()
    }

    /// Output-time boundaries recovered from the emitted atrim windows.
    fn boundaries_from_argv(plan: &FfmpegPlan) -> Vec<f64> {
        let mut t = 0.0;
        let mut out = vec![0.0];
        for argv in &plan.stage1 {
            let fc = arg_after(argv, "-filter_complex");
            let atrim = fc.split("atrim=start=").nth(1).unwrap();
            let start: f64 = atrim.split(":end=").next().unwrap().parse().unwrap();
            let end: f64 = atrim
                .split(":end=")
                .nth(1)
                .unwrap()
                .split(',')
                .next()
                .unwrap()
                .parse()
                .unwrap();
            t += end - start;
            out.push(t);
        }
        out
    }

    #[test]
    fn cumulative_rounding_keeps_video_within_half_a_frame_of_audio() {
        // Irregular sub-frame durations: per-segment rounding would drift,
        // cumulative rounding must not.
        let edl = edl(vec![
            clip(1, 10.0, 10.13),
            clip(1, 20.0, 20.27),
            clip(1, 30.0, 30.011),
            clip(1, 40.0, 40.49),
            clip(1, 50.0, 51.005),
        ]);
        let plan = compile_ffmpeg(&edl, &sources(&[(1, Some(30.0))]), &spec()).unwrap();
        let counts = frame_counts(&plan);
        let mut t = 0.0;
        let mut frames = 0;
        for (n, c) in counts.iter().zip(&edl.clips) {
            t += c.span.duration();
            frames += n;
            let err = (frames as f64 / 30.0 - t).abs();
            assert!(err <= 0.5 / 30.0 + 1e-9, "boundary error {err}");
        }
        // The 11 ms segment legitimately rounds to zero frames.
        assert_eq!(counts[2], 0);
        assert_eq!(counts.iter().sum::<i64>(), (t * 30.0).round() as i64);
    }

    #[test]
    fn ffmpeg_and_mpv_emit_identical_boundary_lists_for_an_elision_fixture() {
        let edl = edl(vec![
            clip(1, 1.0, 2.0),
            clip(1, 2.0, 3.0), // elides with the previous
            clip(1, 5.0, 5.25),
            clip(1, 5.0, 5.25), // repeated span: never merges
        ]);
        let map = sources(&[(1, Some(30.0))]);
        let plan = compile_ffmpeg(&edl, &map, &spec()).unwrap();
        assert_eq!(plan.stage1.len(), 3);

        let mpv = compile_mpv_edl(&edl, &map).unwrap();
        let mpv_bounds: Vec<f64> = {
            let mut t = 0.0;
            let mut out = vec![0.0];
            for entry in mpv.document().lines().skip(1) {
                let len: f64 = entry
                    .split(",length=")
                    .nth(1)
                    .unwrap()
                    .split(',')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap();
                t += len;
                out.push(t);
            }
            out
        };
        let ffmpeg_bounds = boundaries_from_argv(&plan);
        assert_eq!(mpv_bounds.len(), ffmpeg_bounds.len());
        for (a, b) in mpv_bounds.iter().zip(&ffmpeg_bounds) {
            assert!((a - b).abs() < 1e-6, "{mpv_bounds:?} vs {ffmpeg_bounds:?}");
        }
    }

    #[test]
    fn stage1_golden() {
        let edl = edl(vec![clip(1, 1.5, 2.5)]);
        let plan = compile_ffmpeg(&edl, &sources(&[(1, Some(30.0))]), &spec()).unwrap();
        // f_0 = floor(1.5 * 30) = 45; seek half a frame early = 44.5/30.
        assert_eq!(
            plan.stage1[0],
            vec![
                "ffmpeg",
                "-hide_banner",
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-ss",
                "1.483333",
                "-i",
                "/masters/1.mkv",
                "-ss",
                "0.500000",
                "-i",
                "/masters/1.mkv",
                "-filter_complex",
                "[0:v]trim=end_frame=30,setpts=PTS-STARTPTS,\
                 scale=640:360:force_original_aspect_ratio=decrease:force_divisible_by=2,\
                 pad=640:360:(ow-iw)/2:(oh-ih)/2,setsar=1[v];\
                 [1:a]atrim=start=1.000000:end=2.000000,asetpts=PTS-STARTPTS,\
                 aresample=48000,aformat=sample_fmts=s32:channel_layouts=stereo[a]",
                "-map",
                "[v]",
                "-map",
                "[a]",
                "-c:v",
                "prores_ks",
                "-profile:v",
                "3",
                "-vendor",
                "apl0",
                "-pix_fmt",
                "yuv422p10le",
                "-fps_mode",
                "cfr",
                "-r",
                "30.000000",
                "-c:a",
                "pcm_s24le",
                "/work/clip-000.mov",
            ]
        );
        assert_eq!(
            plan.intermediates,
            vec![PathBuf::from("/work/clip-000.mov")]
        );
        assert_eq!(
            plan.concat_list,
            "ffconcat version 1.0\nfile 'clip-000.mov'\n"
        );
        assert_eq!(plan.concat_list_path, PathBuf::from("/work/concat.txt"));
        assert_eq!(
            plan.stage2,
            vec![
                "ffmpeg",
                "-hide_banner",
                "-nostdin",
                "-v",
                "error",
                "-y",
                "-f",
                "concat",
                "-i",
                "/work/concat.txt",
                "-map",
                "0:v:0",
                "-map",
                "0:a:0",
                "-c:v",
                "libx264",
                "-crf",
                "16",
                "-preset",
                "medium",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-b:a",
                "256k",
                "-movflags",
                "+faststart",
                "/out/mix.mp4",
            ]
        );
        assert!((plan.total_duration - 1.0).abs() < 1e-9);
    }

    #[test]
    fn fps_mismatch_inserts_the_resampling_chain() {
        let edl = edl(vec![clip(1, 0.0, 1.0)]);
        let plan = compile_ffmpeg(&edl, &sources(&[(1, Some(25.0))]), &spec()).unwrap();
        let fc = arg_after(&plan.stage1[0], "-filter_complex");
        // 30 output frames need ceil(30 * 25/30) + 2 = 27 source frames.
        assert!(
            fc.starts_with(
                "[0:v]trim=end_frame=27,setpts=PTS-STARTPTS,fps=30.000000,\
                 trim=end_frame=30,setpts=PTS-STARTPTS,"
            ),
            "{fc}"
        );
    }

    #[test]
    fn loop_and_stutter_render_via_segment_expansion() {
        let mut looped = clip(1, 1.0, 2.0);
        looped.transforms = vec![Transform::Loop { count: 2 }];
        let plan =
            compile_ffmpeg(&edl(vec![looped]), &sources(&[(1, Some(30.0))]), &spec()).unwrap();
        assert_eq!(plan.stage1.len(), 2);
        assert!((plan.total_duration - 2.0).abs() < 1e-9);
    }

    #[test]
    fn render_only_transforms_are_rejected_typed() {
        for (transform, name) in [
            (Transform::Pitch { semitones: 2.0 }, "Pitch"),
            (Transform::Speed { factor: 2.0 }, "Speed"),
            (Transform::Reverse, "Reverse"),
        ] {
            let mut c = clip(1, 1.0, 2.0);
            c.transforms = vec![transform];
            match compile_ffmpeg(&edl(vec![c]), &sources(&[(1, Some(30.0))]), &spec()) {
                Err(EdlCompileError::TransformUnsupported {
                    clip_index: 0,
                    transform,
                }) => assert_eq!(transform, name),
                other => panic!("expected TransformUnsupported({name}), got {other:?}"),
            }
        }
    }

    #[test]
    fn a_video_clip_on_an_fps_less_source_is_rejected_typed() {
        let edl = edl(vec![clip(1, 0.0, 1.0)]);
        assert!(matches!(
            compile_ffmpeg(&edl, &sources(&[(1, None)]), &spec()),
            Err(EdlCompileError::MissingFps {
                clip_index: 0,
                source_id: SourceId(1)
            })
        ));
    }

    #[test]
    fn select_profile_takes_the_largest_area_and_its_fps() {
        let chosen = select_profile([
            VideoProps {
                width: 1920,
                height: 1080,
                fps: 30.0,
            },
            VideoProps {
                width: 1280,
                height: 720,
                fps: 60.0,
            },
        ])
        .unwrap();
        assert_eq!((chosen.width, chosen.height), (1920, 1080));
        assert_eq!(chosen.fps, 30.0);
        assert_eq!(select_profile([]), None);
    }

    #[test]
    fn faststart_only_for_quicktime_containers() {
        let edl = edl(vec![clip(1, 0.0, 1.0)]);
        let map = sources(&[(1, Some(30.0))]);
        let mut mkv_spec = spec();
        mkv_spec.output = "/out/mix.mkv".into();
        let plan = compile_ffmpeg(&edl, &map, &mkv_spec).unwrap();
        assert!(!plan.stage2.iter().any(|a| a == "-movflags"));
    }
}
