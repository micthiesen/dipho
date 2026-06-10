//! ffprobe wrappers: stream layout for planning the normalize invocation,
//! and the post-normalization timestamp checks (start offsets, audio packet
//! discontinuities) that DESIGN.md hard-fails on.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

/// CFR rates the master may use; the source's average rate is rounded to
/// the nearest (DESIGN.md).
const STANDARD_FPS: &[(u32, u32)] = &[
    (24000, 1001),
    (24, 1),
    (25, 1),
    (30000, 1001),
    (30, 1),
    (48, 1),
    (50, 1),
    (60000, 1001),
    (60, 1),
];

pub struct Probe {
    pub has_audio: bool,
    /// Real video (embedded thumbnails / attached pictures don't count).
    pub has_video: bool,
    /// Nearest standard rate to the source's average frame rate.
    pub fps: Option<(u32, u32)>,
    /// Per-stream start_time, the assertion trail for `sources.start_offsets`.
    pub start_offsets: Value,
}

impl Probe {
    pub fn fps_value(&self) -> Option<f64> {
        self.fps.map(|(n, d)| n as f64 / d as f64)
    }

    pub fn fps_rational(&self) -> Option<String> {
        self.fps.map(|(n, d)| format!("{n}/{d}"))
    }
}

fn ffprobe_json(path: &Path, args: &[&str]) -> Result<Value> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json"])
        .args(args)
        .arg(path)
        .output()
        .context("running ffprobe (is ffmpeg installed?)")?;
    if !out.status.success() {
        bail!(
            "ffprobe failed on {}: {}",
            path.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

pub fn probe(path: &Path) -> Result<Probe> {
    let doc = ffprobe_json(path, &["-show_streams"])?;
    let streams = doc["streams"]
        .as_array()
        .context("ffprobe: no streams array")?;

    let mut has_audio = false;
    let mut video_fps: Option<(u32, u32)> = None;
    let mut offsets = serde_json::Map::new();
    for s in streams {
        let codec_type = s["codec_type"].as_str().unwrap_or("");
        let start: f64 = s["start_time"]
            .as_str()
            .and_then(|t| t.parse().ok())
            .unwrap_or(0.0);
        offsets.insert(format!("{}:{}", codec_type, s["index"]), Value::from(start));
        match codec_type {
            "audio" => has_audio = true,
            "video" if s["disposition"]["attached_pic"].as_i64().unwrap_or(0) == 0 => {
                let avg = s["avg_frame_rate"].as_str().unwrap_or("0/0");
                if video_fps.is_none() {
                    video_fps = nearest_standard_fps(avg);
                }
            }
            _ => {}
        }
    }
    Ok(Probe {
        has_audio,
        has_video: video_fps.is_some(),
        fps: video_fps,
        start_offsets: Value::Object(offsets),
    })
}

fn nearest_standard_fps(avg_frame_rate: &str) -> Option<(u32, u32)> {
    let (num, den) = avg_frame_rate.split_once('/')?;
    let (num, den): (f64, f64) = (num.parse().ok()?, den.parse().ok()?);
    if num <= 0.0 || den <= 0.0 {
        return None;
    }
    let value = num / den;
    STANDARD_FPS.iter().copied().min_by(|a, b| {
        let da = (a.0 as f64 / a.1 as f64 - value).abs();
        let db = (b.0 as f64 / b.1 as f64 - value).abs();
        da.total_cmp(&db)
    })
}

/// Largest |start_time| across the master's streams, in seconds.
pub fn max_start_offset(path: &Path) -> Result<f64> {
    let doc = ffprobe_json(path, &["-show_streams"])?;
    let streams = doc["streams"]
        .as_array()
        .context("ffprobe: no streams array")?;
    Ok(streams
        .iter()
        .filter_map(|s| s["start_time"].as_str().and_then(|t| t.parse::<f64>().ok()))
        .map(f64::abs)
        .fold(0.0, f64::max))
}

/// Largest audio packet-timestamp discontinuity in the master: the gap
/// between each packet's end (pts + duration) and the next packet's pts.
pub fn max_audio_discontinuity(path: &Path) -> Result<f64> {
    let doc = ffprobe_json(
        path,
        &[
            "-select_streams",
            "a:0",
            "-show_entries",
            "packet=pts_time,duration_time",
        ],
    )?;
    let packets = doc["packets"]
        .as_array()
        .context("ffprobe: no packets array")?;
    let mut worst: f64 = 0.0;
    let mut prev_end: Option<f64> = None;
    for p in packets {
        let pts: f64 = match p["pts_time"].as_str().and_then(|t| t.parse().ok()) {
            Some(t) => t,
            None => continue,
        };
        let dur: f64 = p["duration_time"]
            .as_str()
            .and_then(|t| t.parse().ok())
            .unwrap_or(0.0);
        if let Some(end) = prev_end {
            worst = worst.max((pts - end).abs());
        }
        prev_end = Some(pts + dur);
    }
    Ok(worst)
}

#[cfg(test)]
mod tests {
    use super::nearest_standard_fps;

    #[test]
    fn rounds_to_standard_rates() {
        assert_eq!(nearest_standard_fps("30000/1001"), Some((30000, 1001)));
        assert_eq!(nearest_standard_fps("2997/100"), Some((30000, 1001)));
        assert_eq!(nearest_standard_fps("24/1"), Some((24, 1)));
        assert_eq!(nearest_standard_fps("23976/1000"), Some((24000, 1001)));
        assert_eq!(nearest_standard_fps("26/1"), Some((25, 1)));
        assert_eq!(nearest_standard_fps("0/0"), None);
        assert_eq!(nearest_standard_fps("nonsense"), None);
    }
}
