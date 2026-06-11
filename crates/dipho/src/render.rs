//! `dipho render edit.json out.mp4`: bind the edit to the corpus, resolve
//! the output profile (largest master's display area, its fps; `--size`/
//! `--fps` override), compile the two-stage ffmpeg plan, and execute it —
//! intermediates deleted on success, kept on failure, with a preflight
//! free-space check.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use dipho_core::corpus::Corpus;
use dipho_core::edl::{
    AUDIO_RATE, Edl, FfmpegPlan, OutputProfile, Rebind, RenderSpec, VideoProps, compile_ffmpeg,
    rebind, select_profile,
};
use sha2::{Digest, Sha256};

use crate::ingest::probe;

/// ProRes 422 HQ runs ~3 bits/pixel; the margin covers mux overhead and
/// content the rate control likes less than average.
const PRORES_BITS_PER_PIXEL: f64 = 3.0;
const SPACE_MARGIN: f64 = 1.25;

pub struct Options {
    pub corpus_db: PathBuf,
    /// Output resolution override (`--size WxH`).
    pub size: Option<(u32, u32)>,
    /// Output frame rate override (`--fps`).
    pub fps: Option<f64>,
    /// Persist an origin_id-only rebind back into the edit file's manifest.
    pub accept_relink: bool,
}

pub fn run(edit: &Path, output: &Path, opts: &Options) -> Result<()> {
    let json =
        fs::read_to_string(edit).with_context(|| format!("reading edit {}", edit.display()))?;
    let edl = Edl::from_json(&json).with_context(|| format!("loading {}", edit.display()))?;
    let corpus = Corpus::open_read_only(&opts.corpus_db)
        .with_context(|| format!("opening corpus {}", opts.corpus_db.display()))?;
    corpus.ensure_schema_current()?;
    let bound = rebind(&edl, &corpus.sources()?)
        .with_context(|| format!("binding {} to the corpus", edit.display()))?;
    for warning in &bound.warnings {
        eprintln!("warning: {warning}");
    }
    if !bound.warnings.is_empty() {
        if opts.accept_relink {
            let json = bound.edl.to_json()?;
            let tmp = edit.with_extension("json.tmp");
            fs::write(&tmp, &json)?;
            fs::rename(&tmp, edit)?;
            eprintln!("manifest updated: {}", edit.display());
        } else {
            eprintln!("(pass --accept-relink to update the manifest)");
        }
    }
    if bound.edl.clips.is_empty() {
        bail!("{} has no clips — nothing to render", edit.display());
    }

    let profile = resolve_profile(&bound, opts)?;
    let spec = RenderSpec {
        profile,
        intermediates_dir: intermediates_dir(&opts.corpus_db, &bound.edl, &profile)?,
        output: output.to_path_buf(),
    };
    let plan = compile_ffmpeg(&bound.edl, &bound.source_map, &spec)?;
    execute(&plan, &spec)?;
    println!(
        "rendered {} ({:.3} s, {}x{} @ {:.3} fps, {} Hz stereo)",
        output.display(),
        plan.total_duration,
        profile.width,
        profile.height,
        profile.fps,
        AUDIO_RATE,
    );
    Ok(())
}

/// The project profile: probe each video-bearing master's dimensions (the
/// schema stores fps but not size), let the largest display area win, then
/// apply the CLI overrides.
fn resolve_profile(bound: &Rebind, opts: &Options) -> Result<OutputProfile> {
    let mut videos = Vec::new();
    for info in bound.source_map.values() {
        let Some(fps) = info.fps else { continue };
        let (width, height) = probe::video_dimensions(&info.master_path)?.with_context(|| {
            format!(
                "{} has an fps in the corpus but no video stream",
                info.master_path.display()
            )
        })?;
        videos.push(VideoProps { width, height, fps });
    }
    let mut profile = select_profile(videos)
        .context("the edit references no video-bearing source (audio-only render is post-MVP)")?;
    if let Some((width, height)) = opts.size {
        profile.width = width;
        profile.height = height;
    }
    if let Some(fps) = opts.fps {
        profile.fps = fps;
    }
    Ok(profile)
}

/// `<corpus dir>/render/<edit-hash>/`: keyed by the bound edit + profile so
/// concurrent renders never collide and a failed run's leftovers are
/// identifiable.
fn intermediates_dir(corpus_db: &Path, edl: &Edl, profile: &OutputProfile) -> Result<PathBuf> {
    let mut hasher = Sha256::new();
    hasher.update(edl.to_json()?.as_bytes());
    hasher.update(format!("{profile:?}").as_bytes());
    let hash = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    let base = corpus_db
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    Ok(base.join("render").join(&hash[..16]))
}

/// Run the plan: stage-1 extractions with per-clip progress, the concat
/// list, the final encode. Intermediates are deleted on success and kept
/// on failure (the error names where).
pub(crate) fn execute(plan: &FfmpegPlan, spec: &RenderSpec) -> Result<()> {
    fs::create_dir_all(&spec.intermediates_dir)?;
    preflight_free_space(&spec.intermediates_dir, &spec.profile, plan.total_duration)?;
    let result = (|| -> Result<()> {
        for (i, argv) in plan.stage1.iter().enumerate() {
            eprintln!("  clip {}/{}", i + 1, plan.stage1.len());
            run_ffmpeg(argv)?;
        }
        fs::write(&plan.concat_list_path, &plan.concat_list)?;
        eprintln!("  final encode");
        run_ffmpeg(&plan.stage2)?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            fs::remove_dir_all(&spec.intermediates_dir)?;
            Ok(())
        }
        Err(e) => Err(e.context(format!(
            "render failed — intermediates kept at {}",
            spec.intermediates_dir.display()
        ))),
    }
}

pub(crate) fn run_ffmpeg(argv: &[String]) -> Result<()> {
    let out = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .with_context(|| format!("running {} (is ffmpeg installed?)", argv[0]))?;
    if !out.status.success() {
        bail!(
            "{} failed: {}",
            argv.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

fn preflight_free_space(dir: &Path, profile: &OutputProfile, total_duration: f64) -> Result<()> {
    let video_bps =
        f64::from(profile.width) * f64::from(profile.height) * profile.fps * PRORES_BITS_PER_PIXEL;
    let audio_bps = f64::from(AUDIO_RATE) * 2.0 * 24.0;
    let needed = ((video_bps + audio_bps) / 8.0 * total_duration * SPACE_MARGIN) as u64;
    let free = free_bytes(dir)?;
    if free < needed {
        bail!(
            "not enough disk space for intermediates: need ~{} MB, {} MB free at {}",
            needed / (1 << 20),
            free / (1 << 20),
            dir.display()
        );
    }
    Ok(())
}

fn free_bytes(path: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut vfs) } != 0 {
        bail!("statvfs({}) failed", path.display());
    }
    Ok(vfs.f_bavail as u64 * vfs.f_frsize as u64)
}

/// Render integration tests (ROADMAP M6): real masters built by the real
/// normalize stage, real ffmpeg execution, measured assertions — frame
/// counts match the quantization budget, audio cuts land where the EDL
/// says, A/V error stays under half a frame. Spawn ffmpeg, so `#[ignore]`
/// — run with `cargo test -p dipho -- --ignored`.
#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use dipho_core::edl::{Clip, SourceInfo, SourceMap, SourceRef, plan_preview};
    use dipho_core::span::{Channel, SourceId, Span};

    use super::*;
    use crate::ingest::normalize::fixtures::ffmpeg;

    const FPS: f64 = 30.0;
    /// Fixture beep: 200 ms of boosted 1 kHz starting at t = 2.0.
    const BEEP_AT: f64 = 2.0;

    /// Build a source with a beep at `BEEP_AT` and run the real normalize
    /// stage on it; returns the master and its ffprobed duration.
    fn master_with_beep(dir: &Path, name: &str, size: &str, rate: u32) -> (PathBuf, f64) {
        let beep = dir.join(format!("{name}-beep.wav"));
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:duration=0.2",
            "-af",
            "volume=12dB,adelay=2000:all=1,apad=whole_dur=6",
            "-ar",
            &rate.to_string(),
            beep.to_str().unwrap(),
        ]);
        let source = dir.join(format!("{name}.mp4"));
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=duration=6:rate=30:size={size}"),
            "-i",
            beep.to_str().unwrap(),
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "aac",
            source.to_str().unwrap(),
        ]);
        let workdir = dir.join(format!("{name}-work"));
        fs::create_dir_all(&workdir).unwrap();
        fs::copy(&source, workdir.join("original.bin")).unwrap();
        crate::ingest::normalize::create_master(&workdir).unwrap();
        let master = workdir.join("master.mkv");
        let duration: f64 = ffprobe(&master, &["-show_entries", "format=duration"])
            .parse()
            .unwrap();
        (master, duration)
    }

    fn ffprobe(path: &Path, args: &[&str]) -> String {
        let out = Command::new("ffprobe")
            .args(["-v", "error", "-of", "default=nw=1:nk=1"])
            .args(args)
            .arg(path)
            .output()
            .expect("ffprobe runs");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    fn video_frames(path: &Path) -> i64 {
        ffprobe(
            path,
            &[
                "-select_streams",
                "v:0",
                "-count_frames",
                "-show_entries",
                "stream=nb_read_frames",
            ],
        )
        .parse()
        .unwrap()
    }

    /// Audio length in samples: pcm-in-mov tracks use the sample rate as
    /// the timescale, so duration_ts is the sample count.
    fn audio_samples(path: &Path) -> i64 {
        ffprobe(
            path,
            &[
                "-select_streams",
                "a:0",
                "-show_entries",
                "stream=duration_ts",
            ],
        )
        .parse()
        .unwrap()
    }

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

    fn edl_for(clips: Vec<Clip>, masters: &[(i64, &Path, f64)]) -> (Edl, SourceMap) {
        let sources: BTreeMap<_, _> = masters
            .iter()
            .map(|&(id, _, duration)| {
                (
                    SourceId(id),
                    SourceRef {
                        origin: format!("test-{id}"),
                        origin_id: format!("test:{id}"),
                        master_filename: "master.mkv".into(),
                        duration,
                        master_hash: format!("hash-{id}"),
                    },
                )
            })
            .collect();
        let map: SourceMap = masters
            .iter()
            .map(|&(id, path, duration)| {
                (
                    SourceId(id),
                    SourceInfo {
                        master_path: path.to_path_buf(),
                        duration,
                        fps: Some(FPS),
                    },
                )
            })
            .collect();
        (Edl { clips, sources }, map)
    }

    /// Every onset (5 ms RMS crossing 0.1 with hysteresis) in a rendered
    /// file's audio, via a 16 kHz mono wav dump.
    fn beep_onsets(path: &Path, dir: &Path) -> Vec<f64> {
        let wav = dir.join("onsets.wav");
        ffmpeg(&[
            "-i",
            path.to_str().unwrap(),
            "-ar",
            "16000",
            "-ac",
            "1",
            "-c:a",
            "pcm_s16le",
            wav.to_str().unwrap(),
        ]);
        let bytes = fs::read(&wav).unwrap();
        let data_start = bytes
            .windows(4)
            .position(|w| w == b"data")
            .expect("wav data chunk")
            + 8;
        let samples: Vec<f64> = bytes[data_start..]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f64 / i16::MAX as f64)
            .collect();
        let window = 16_000 / 200; // 5 ms
        let mut onsets = Vec::new();
        let mut loud = false;
        for (i, w) in samples.chunks(window).enumerate() {
            let rms = (w.iter().map(|s| s * s).sum::<f64>() / w.len() as f64).sqrt();
            if rms > 0.1 && !loud {
                onsets.push(i as f64 * window as f64 / 16_000.0);
            }
            loud = rms > 0.05;
        }
        onsets
    }

    /// Mean absolute difference between a master frame and an
    /// intermediate's first frame, both dumped as raw rgb24.
    fn frame_mae(master: &Path, frame: i64, intermediate: &Path, dir: &Path) -> f64 {
        let a = dir.join(format!("master-f{frame}.raw"));
        ffmpeg(&[
            "-i",
            master.to_str().unwrap(),
            "-vf",
            &format!("select=eq(n\\,{frame})"),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            a.to_str().unwrap(),
        ]);
        let b = dir.join("intermediate-f0.raw");
        ffmpeg(&[
            "-i",
            intermediate.to_str().unwrap(),
            "-frames:v",
            "1",
            "-f",
            "rawvideo",
            "-pix_fmt",
            "rgb24",
            b.to_str().unwrap(),
        ]);
        let (a, b) = (fs::read(&a).unwrap(), fs::read(&b).unwrap());
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(&b)
            .map(|(&x, &y)| (f64::from(x) - f64::from(y)).abs())
            .sum::<f64>()
            / a.len() as f64
    }

    #[test]
    #[ignore = "spawns ffmpeg (M6 render integration test)"]
    fn render_is_frame_and_sample_exact_against_the_plan() {
        let dir = tempfile::tempdir().unwrap();
        let (master, duration) = master_with_beep(dir.path(), "a", "320x180", 44_100);
        // Clip 0 carries the beep 0.5 s in; clip 1 is silent; clip 2 is a
        // 200 ms grain whose beep lands 0.1 s after its output start.
        let (edl, map) = edl_for(
            vec![
                clip(1, 1.5, 2.5),
                clip(1, 0.5, 1.5),
                clip(1, BEEP_AT - 0.1, BEEP_AT + 0.1),
            ],
            &[(1, &master, duration)],
        );
        let spec = RenderSpec {
            profile: OutputProfile {
                width: 320,
                height: 180,
                fps: FPS,
            },
            intermediates_dir: dir.path().join("work"),
            output: dir.path().join("out.mp4"),
        };
        let plan = compile_ffmpeg(&edl, &map, &spec).unwrap();
        assert_eq!(plan.stage1.len(), 3);

        // Run stage 1 by hand so the intermediates survive for inspection.
        fs::create_dir_all(&spec.intermediates_dir).unwrap();
        let expected_frames = [30, 30, 6];
        let mut audio_cum = 0.0;
        let mut frames_cum = 0;
        for (k, argv) in plan.stage1.iter().enumerate() {
            run_ffmpeg(argv).unwrap();
            let inter = &plan.intermediates[k];
            // Frame counts match the cumulative-rounding budget...
            assert_eq!(video_frames(inter), expected_frames[k], "clip {k}");
            // ...audio is sample-exact (±2 for the resampler's phase)...
            let d_k = edl.clips[k].span.duration();
            let samples = audio_samples(inter);
            assert!(
                (samples - (d_k * 48_000.0).round() as i64).abs() <= 2,
                "clip {k}: {samples} samples for {d_k} s"
            );
            // ...so A/V error < half a frame at every boundary.
            audio_cum += d_k;
            frames_cum += expected_frames[k];
            let av_error = (frames_cum as f64 / FPS - audio_cum).abs();
            assert!(av_error < 0.5 / FPS, "boundary {k}: A/V error {av_error}");
        }

        // The first frame of clip 0 is master frame f_0 = floor(1.5·30) =
        // 45 — closer to it than to either neighbor.
        let mae_44 = frame_mae(&master, 44, &plan.intermediates[0], dir.path());
        let mae_45 = frame_mae(&master, 45, &plan.intermediates[0], dir.path());
        let mae_46 = frame_mae(&master, 46, &plan.intermediates[0], dir.path());
        assert!(
            mae_45 < mae_44 && mae_45 < mae_46,
            "frame identity: mae44 {mae_44}, mae45 {mae_45}, mae46 {mae_46}"
        );

        // Stage 2, then check the final output cut-for-cut.
        fs::write(&plan.concat_list_path, &plan.concat_list).unwrap();
        run_ffmpeg(&plan.stage2).unwrap();
        let out = &spec.output;
        assert_eq!(video_frames(out), 66);
        assert_eq!(
            ffprobe(
                out,
                &[
                    "-select_streams",
                    "a:0",
                    "-show_entries",
                    "stream=sample_rate"
                ]
            ),
            "48000"
        );

        // Audio cut positions match the EDL: the beep lands at output 0.5
        // (clip 0) and 2.1 (clip 2), and nowhere else.
        let plan_view = plan_preview(&edl, &map).unwrap();
        let expected = [
            plan_view.clip_output[0].0 + (BEEP_AT - 1.5),
            plan_view.clip_output[2].0 + 0.1,
        ];
        let onsets = beep_onsets(out, dir.path());
        assert_eq!(onsets.len(), 2, "onsets {onsets:?}");
        for (onset, want) in onsets.iter().zip(expected) {
            assert!(
                (onset - want).abs() < 0.020,
                "beep at {onset}, expected {want}"
            );
        }
    }

    #[test]
    #[ignore = "spawns ffmpeg (M6 render integration test)"]
    fn mismatched_resolution_and_sample_rate_sources_render_to_one_profile() {
        let dir = tempfile::tempdir().unwrap();
        let (small, small_dur) = master_with_beep(dir.path(), "small", "320x180", 44_100);
        let (large, large_dur) = master_with_beep(dir.path(), "large", "640x360", 22_050);
        let (edl, map) = edl_for(
            vec![clip(1, 1.5, 2.5), clip(2, 1.5, 2.5), clip(1, 3.0, 3.5)],
            &[(1, &small, small_dur), (2, &large, large_dur)],
        );

        // The profile comes from probed dimensions: the larger source wins.
        let videos: Vec<VideoProps> = map
            .values()
            .map(|info| {
                let (width, height) = probe::video_dimensions(&info.master_path).unwrap().unwrap();
                VideoProps {
                    width,
                    height,
                    fps: info.fps.unwrap(),
                }
            })
            .collect();
        let profile = select_profile(videos).unwrap();
        assert_eq!((profile.width, profile.height), (640, 360));

        let spec = RenderSpec {
            profile,
            intermediates_dir: dir.path().join("work"),
            output: dir.path().join("out.mp4"),
        };
        let plan = compile_ffmpeg(&edl, &map, &spec).unwrap();
        execute(&plan, &spec).unwrap();
        assert!(!spec.intermediates_dir.exists(), "intermediates deleted");

        let out = &spec.output;
        assert_eq!(
            ffprobe(
                out,
                &[
                    "-select_streams",
                    "v:0",
                    "-show_entries",
                    "stream=width,height"
                ]
            ),
            "640\n360"
        );
        assert_eq!(
            ffprobe(
                out,
                &[
                    "-select_streams",
                    "a:0",
                    "-show_entries",
                    "stream=sample_rate,channels"
                ]
            ),
            "48000\n2"
        );
        assert_eq!(video_frames(out), 75);
        // Both sources' beeps land at their planned output positions.
        let onsets = beep_onsets(out, dir.path());
        assert_eq!(onsets.len(), 2, "onsets {onsets:?}");
        assert!((onsets[0] - 0.5).abs() < 0.020, "{onsets:?}");
        assert!((onsets[1] - 1.5).abs() < 0.020, "{onsets:?}");
    }
}
