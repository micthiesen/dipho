//! The playback master + analysis wav, in one ffmpeg invocation so both
//! share one audio stream by construction: `aresample=async=1:first_pts=0`
//! makes the stream contiguous from 0 as an invariant, then splits into the
//! FLAC master encode and the 16 kHz mono analysis wav. Video gets rotation
//! baked in (ffmpeg autorotation), square pixels, and CFR at the nearest
//! standard rate via the `fps` filter — all-intra x264 for frame-exact
//! seeking.
//!
//! After encoding, the master is ffprobed and ingest **hard-fails** if any
//! |start_time| > 10 ms or audio packet discontinuity > 10 ms remains —
//! given the aresample chain, that's a dipho pipeline bug, not a source
//! property.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::origin::sha256_file;
use super::probe;

const MAX_TIMESTAMP_ERROR: f64 = 0.010;
const AUDIO_CHAIN: &str = "aresample=async=1:first_pts=0,asplit=2[am][ana];\
                           [ana]aresample=16000,aformat=sample_fmts=s16:channel_layouts=mono[wav]";

/// FLAC frame size for the master's audio, in samples. mpv never starts
/// audio mid-packet after a seek or at an EDL segment boundary — it snaps
/// forward to the next packet — so the packet duration bounds every cut's
/// audio precision (measured against mpv 0.41 during the M5 preview gate;
/// the encoder default of 4608 samples ≈ 104 ms ate whole phones per cut).
/// 576 samples is ~13 ms at 44.1 kHz / 12 ms at 48 kHz: under one video
/// frame at any common rate, keeping cuts within the documented ≤ 1 frame
/// preview tolerance.
const FLAC_FRAME_SIZE: &str = "576";

/// The normalize stage's record (`normalize.json` in the workdir):
/// `master_hash` is computed exactly once when the master is created, never
/// recomputed; fps/has_video/start_offsets ride along for resume.
#[derive(Serialize, Deserialize)]
pub struct MasterInfo {
    pub master_hash: String,
    pub fps: Option<f64>,
    pub has_video: bool,
    pub start_offsets: serde_json::Value,
}

pub fn create_master(workdir: &Path) -> Result<MasterInfo> {
    let original = workdir.join("original.bin");
    let source = probe::probe(&original)?;
    if !source.has_audio {
        bail!("source has no audio stream — nothing to index");
    }
    let master_tmp = workdir.join("master.mkv.tmp");
    let wav_tmp = workdir.join("audio.wav.tmp");

    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-v", "error", "-i"]).arg(&original);
    if let Some(rate) = source.fps_rational() {
        // Autorotation is ffmpeg's default on re-encode: the display matrix
        // is applied and the side data stripped.
        cmd.arg("-filter_complex").arg(format!(
            "[0:v]scale=iw*sar:ih,setsar=1,fps={rate}[v];[0:a]{AUDIO_CHAIN}"
        ));
        cmd.args(["-map", "[v]", "-map", "[am]"]);
        cmd.args([
            "-c:v", "libx264", "-g", "1", "-crf", "14", "-preset", "fast",
        ]);
        cmd.args(["-pix_fmt", "yuv420p"]);
    } else {
        cmd.arg("-filter_complex")
            .arg(format!("[0:a]{AUDIO_CHAIN}"));
        cmd.args(["-map", "[am]"]);
    }
    cmd.args([
        "-c:a",
        "flac",
        "-frame_size",
        FLAC_FRAME_SIZE,
        "-f",
        "matroska",
    ])
    .arg(&master_tmp);
    cmd.args(["-map", "[wav]", "-c:a", "pcm_s16le", "-f", "wav"])
        .arg(&wav_tmp);

    let status = cmd.status().context("running ffmpeg (is it installed?)")?;
    if !status.success() {
        bail!("ffmpeg normalization failed for {}", original.display());
    }

    let start_offset = probe::max_start_offset(&master_tmp)?;
    if start_offset > MAX_TIMESTAMP_ERROR {
        bail!(
            "master has a {:.1} ms stream start offset — dipho pipeline bug, not a source property",
            start_offset * 1e3
        );
    }
    let discontinuity = probe::max_audio_discontinuity(&master_tmp)?;
    if discontinuity > MAX_TIMESTAMP_ERROR {
        bail!(
            "master has a {:.1} ms audio timestamp discontinuity — dipho pipeline bug, not a source property",
            discontinuity * 1e3
        );
    }

    let info = MasterInfo {
        master_hash: sha256_file(&master_tmp)?,
        fps: source.fps_value(),
        has_video: source.has_video,
        start_offsets: probe::probe(&master_tmp)?.start_offsets,
    };
    fs::rename(&master_tmp, workdir.join("master.mkv"))?;
    fs::rename(&wav_tmp, workdir.join("audio.wav"))?;
    let json = serde_json::to_vec_pretty(&info)?;
    fs::write(workdir.join("normalize.json.tmp"), &json)?;
    fs::rename(
        workdir.join("normalize.json.tmp"),
        workdir.join("normalize.json"),
    )?;
    Ok(info)
}

/// Resume path: trust the recorded stage output rather than rehashing the
/// master (the hash is computed exactly once, at creation).
pub fn load_master_info(workdir: &Path) -> Result<Option<MasterInfo>> {
    if !workdir.join("master.mkv").exists() || !workdir.join("audio.wav").exists() {
        return Ok(None);
    }
    let path = workdir.join("normalize.json");
    if !path.exists() {
        return Ok(None);
    }
    let info = serde_json::from_slice(&fs::read(&path)?)
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(info))
}

/// Timebase fixtures shared by the M2 wav-leg tests below and the M4 mpv
/// leg (`crate::mpv` integration tests): synthetic sources with a beep at
/// a known presentation time and a deliberate container-timestamp
/// pathology.
#[cfg(test)]
pub(crate) mod fixtures {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    pub const BEEP_AT: f64 = 2.0;
    /// One video frame at the fixtures' 30 fps.
    pub const TOLERANCE: f64 = 1.0 / 30.0;

    pub fn ffmpeg(args: &[&str]) {
        let status = Command::new("ffmpeg")
            .args(["-y", "-v", "error"])
            .args(args)
            .status()
            .expect("ffmpeg runs");
        assert!(status.success(), "ffmpeg {args:?} failed");
    }

    /// 4 s of silence with a 200 ms 1 kHz beep starting at `BEEP_AT`.
    pub fn beep_wav(dir: &Path) -> PathBuf {
        let path = dir.join("beep.wav");
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=1000:duration=0.2",
            "-af",
            // lavfi sine peaks at -18 dBFS; boost so onset detection has a
            // clear margin over the encoded noise floor.
            "volume=12dB,adelay=2000:all=1,apad=whole_dur=4",
            path.to_str().unwrap(),
        ]);
        path
    }

    /// A deliberate 500 ms container offset on the audio stream: the
    /// beep's presentation time becomes `BEEP_AT + 0.5`. Returns the
    /// source and that expected onset.
    pub fn offset_source(dir: &Path) -> (PathBuf, f64) {
        let beep = beep_wav(dir);
        let source = dir.join("offset.mp4");
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=4.5:rate=30:size=320x180",
            "-itsoffset",
            "0.5",
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
        (source, BEEP_AT + 0.5)
    }

    /// A 300 ms pts jump at t = 1.5 (before the beep): every later frame
    /// shifts +0.3, so the beep's presentation time becomes `BEEP_AT +
    /// 0.3`. pcm in matroska keeps the gapped packet timestamps verbatim.
    pub fn gapped_source(dir: &Path) -> (PathBuf, f64) {
        let beep = beep_wav(dir);
        let source = dir.join("gapped.mkv");
        ffmpeg(&[
            "-f",
            "lavfi",
            "-i",
            "testsrc2=duration=4.5:rate=30:size=320x180",
            "-i",
            beep.to_str().unwrap(),
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-af",
            "asetpts=PTS+gte(T\\,1.5)*0.3/TB",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-c:a",
            "pcm_s16le",
            source.to_str().unwrap(),
        ]);
        (source, BEEP_AT + 0.3)
    }

    /// First time the wav's 5 ms-window RMS exceeds a tenth of full scale
    /// — the beep onset. Expects 16 kHz mono s16le.
    pub fn beep_onset(wav: &Path) -> f64 {
        let bytes = fs::read(wav).unwrap();
        let data_start = bytes
            .windows(4)
            .position(|w| w == b"data")
            .expect("wav data chunk")
            + 8;
        let samples: Vec<f64> = bytes[data_start..]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f64 / i16::MAX as f64)
            .collect();
        let window = 16_000 / 200; // 5 ms at the analysis rate
        samples
            .chunks(window)
            .position(|w| (w.iter().map(|s| s * s).sum::<f64>() / w.len() as f64).sqrt() > 0.1)
            .map(|i| i as f64 * window as f64 / 16_000.0)
            .expect("no beep found in analysis wav")
    }

    /// Copy `original` into `dir` as the staged download and run the
    /// normalize stage there; returns the analysis wav.
    pub fn run_normalize(dir: &Path, original: &Path) -> PathBuf {
        fs::copy(original, dir.join("original.bin")).unwrap();
        super::create_master(dir).unwrap();
        dir.join("audio.wav")
    }
}

/// Timebase integration tests (ROADMAP M2), wav leg: a beep at a known
/// presentation time must land at that exact time in the analysis wav,
/// whatever container timestamp pathology the source carries. Spawn
/// ffmpeg, so `#[ignore]` — run with `cargo test -p dipho -- --ignored`.
#[cfg(test)]
mod tests {
    use super::fixtures::*;

    #[test]
    #[ignore = "spawns ffmpeg (M2 timebase integration test)"]
    fn container_offset_is_neutralized() {
        let dir = tempfile::tempdir().unwrap();
        let (source, expected) = offset_source(dir.path());
        let wav = run_normalize(dir.path(), &source);
        let onset = beep_onset(&wav);
        assert!(
            (onset - expected).abs() <= TOLERANCE,
            "beep at {onset}, expected {expected}"
        );
    }

    #[test]
    #[ignore = "spawns ffmpeg (M2 timebase integration test)"]
    fn midstream_timestamp_gap_is_filled_with_silence() {
        let dir = tempfile::tempdir().unwrap();
        let (source, expected) = gapped_source(dir.path());
        let wav = run_normalize(dir.path(), &source);
        let onset = beep_onset(&wav);
        assert!(
            (onset - expected).abs() <= TOLERANCE,
            "beep at {onset}, expected {expected}"
        );
    }
}
