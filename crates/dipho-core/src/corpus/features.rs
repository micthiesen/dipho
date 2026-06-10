//! Frame-substrate aggregation: per-unit boundary features and prosody
//! summaries from the sidecar's 10 ms frame grid. Frame persistence exists
//! precisely so re-derivation (including DSP boundary-feature recompute)
//! never re-runs ingest.

use super::phones::T_EPS;

/// Dimensionality of the MFCC vectors in the frame substrate.
pub const MFCC_DIM: usize = 13;
/// Boundary features average over at most this many in-unit frames per end.
const BOUNDARY_FRAMES: usize = 3;

/// The three frame arrays on one grid: frame `i` is centered at `i · hop`.
pub struct ProsodyData {
    pub hop: f64,
    /// pyin f0 in Hz; 0 = unvoiced.
    pub f0: Vec<f32>,
    pub rms_db: Vec<f32>,
    /// Frame-major, `n_frames × MFCC_DIM`.
    pub mfcc: Vec<f32>,
}

impl ProsodyData {
    pub fn n_frames(&self) -> usize {
        self.f0.len()
    }

    fn mfcc_frame(&self, i: usize) -> &[f32] {
        &self.mfcc[i * MFCC_DIM..(i + 1) * MFCC_DIM]
    }
}

/// Aggregates for one unit span. Frame membership: a frame is in-unit iff
/// its center ∈ [t_start, t_end); if no frame qualifies, the single frame
/// nearest the unit midpoint stands in. f0 head/tail are medians of
/// *voiced* frames among the boundary frames — an unvoiced boundary reads
/// None, not a pitch from 80 ms away.
pub struct UnitFeatures {
    pub mfcc_head: [f32; MFCC_DIM],
    pub mfcc_tail: [f32; MFCC_DIM],
    pub f0_head: Option<f64>,
    pub f0_tail: Option<f64>,
    pub rms_head_db: f64,
    pub rms_tail_db: f64,
    pub f0_median: Option<f64>,
    pub voiced_fraction: f64,
    pub f0_slope: Option<f64>,
    pub rms_mean_db: f64,
}

pub fn unit_features(p: &ProsodyData, t_start: f64, t_end: f64) -> UnitFeatures {
    let n = p.n_frames();
    assert!(n > 0, "prosody substrate must have at least one frame");
    let first = ((((t_start - T_EPS) / p.hop).ceil()).max(0.0) as usize).min(n);
    let last = ((((t_end - T_EPS) / p.hop).ceil()).max(0.0) as usize).min(n);
    let in_unit: Vec<usize> = if first < last {
        (first..last).collect()
    } else {
        let mid = 0.5 * (t_start + t_end);
        vec![(((mid / p.hop).round()).max(0.0) as usize).min(n - 1)]
    };
    let k = BOUNDARY_FRAMES.min(in_unit.len());
    let head = &in_unit[..k];
    let tail = &in_unit[in_unit.len() - k..];

    let voiced: Vec<usize> = in_unit.iter().copied().filter(|&i| p.f0[i] > 0.0).collect();
    UnitFeatures {
        mfcc_head: mfcc_mean(p, head),
        mfcc_tail: mfcc_mean(p, tail),
        f0_head: voiced_f0_median(p, head),
        f0_tail: voiced_f0_median(p, tail),
        rms_head_db: rms_mean(p, head),
        rms_tail_db: rms_mean(p, tail),
        f0_median: voiced_f0_median(p, &in_unit),
        voiced_fraction: voiced.len() as f64 / in_unit.len() as f64,
        f0_slope: f0_slope(p, &voiced),
        rms_mean_db: rms_mean(p, &in_unit),
    }
}

fn mfcc_mean(p: &ProsodyData, idxs: &[usize]) -> [f32; MFCC_DIM] {
    let mut acc = [0f64; MFCC_DIM];
    for &i in idxs {
        for (d, v) in p.mfcc_frame(i).iter().enumerate() {
            acc[d] += f64::from(*v);
        }
    }
    let mut out = [0f32; MFCC_DIM];
    for d in 0..MFCC_DIM {
        out[d] = (acc[d] / idxs.len() as f64) as f32;
    }
    out
}

fn rms_mean(p: &ProsodyData, idxs: &[usize]) -> f64 {
    idxs.iter().map(|&i| f64::from(p.rms_db[i])).sum::<f64>() / idxs.len() as f64
}

fn voiced_f0_median(p: &ProsodyData, idxs: &[usize]) -> Option<f64> {
    let mut voiced: Vec<f64> = idxs
        .iter()
        .filter(|&&i| p.f0[i] > 0.0)
        .map(|&i| f64::from(p.f0[i]))
        .collect();
    if voiced.is_empty() {
        return None;
    }
    voiced.sort_by(|a, b| a.partial_cmp(b).expect("f0 values are finite"));
    let mid = voiced.len() / 2;
    Some(if voiced.len() % 2 == 1 {
        voiced[mid]
    } else {
        0.5 * (voiced[mid - 1] + voiced[mid])
    })
}

/// Least-squares f0 slope (Hz/s) over the unit's voiced frames; None below
/// two voiced frames.
fn f0_slope(p: &ProsodyData, voiced: &[usize]) -> Option<f64> {
    if voiced.len() < 2 {
        return None;
    }
    let n = voiced.len() as f64;
    let t_mean = voiced.iter().map(|&i| i as f64 * p.hop).sum::<f64>() / n;
    let y_mean = voiced.iter().map(|&i| f64::from(p.f0[i])).sum::<f64>() / n;
    let mut cov = 0.0;
    let mut var = 0.0;
    for &i in voiced {
        let dt = i as f64 * p.hop - t_mean;
        cov += dt * (f64::from(p.f0[i]) - y_mean);
        var += dt * dt;
    }
    if var <= T_EPS {
        return None;
    }
    Some(cov / var)
}

/// Little-endian float32 BLOB encoding for SQLite feature columns.
pub fn f32_blob(xs: &[f32]) -> Vec<u8> {
    xs.iter().flat_map(|x| x.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// hop 10 ms, 10 frames; f0 voiced (and rising) from frame 5 on;
    /// mfcc[i][d] = i + 100·d; rms[i] = -20 - i.
    fn fixture() -> ProsodyData {
        let n = 10;
        ProsodyData {
            hop: 0.01,
            f0: (0..n)
                .map(|i| if i < 5 { 0.0 } else { 100.0 + 10.0 * i as f32 })
                .collect(),
            rms_db: (0..n).map(|i| -20.0 - i as f32).collect(),
            mfcc: (0..n)
                .flat_map(|i| (0..MFCC_DIM).map(move |d| i as f32 + 100.0 * d as f32))
                .collect(),
        }
    }

    #[test]
    fn membership_is_center_in_half_open_span() {
        let p = fixture();
        // Centers 0.02, 0.03, 0.04 are in; 0.05 is excluded (half-open).
        let f = unit_features(&p, 0.02, 0.05);
        assert_eq!(f.mfcc_head[0], 3.0); // mean of frames 2,3,4
        assert_eq!(f.mfcc_head, f.mfcc_tail); // n < 3 collapses head/tail
        assert!((f.rms_mean_db - (-23.0)).abs() < 1e-9);
    }

    #[test]
    fn head_and_tail_average_three_frames() {
        let p = fixture();
        let f = unit_features(&p, 0.0, 0.10); // frames 0..=9
        assert_eq!(f.mfcc_head[0], 1.0); // mean(0,1,2)
        assert_eq!(f.mfcc_tail[0], 8.0); // mean(7,8,9)
        assert_eq!(f.mfcc_head[2], 201.0); // dimension offset intact
        assert!((f.rms_head_db - (-21.0)).abs() < 1e-9);
        assert!((f.rms_tail_db - (-28.0)).abs() < 1e-9);
    }

    #[test]
    fn empty_span_falls_back_to_nearest_midpoint_frame() {
        let p = fixture();
        // No frame center in [0.041, 0.047); midpoint 0.044 → frame 4.
        let f = unit_features(&p, 0.041, 0.047);
        assert_eq!(f.mfcc_head[0], 4.0);
        assert_eq!(f.mfcc_tail[0], 4.0);
        assert!((f.rms_mean_db - (-24.0)).abs() < 1e-9);
    }

    #[test]
    fn unvoiced_boundary_reads_none() {
        let p = fixture();
        let f = unit_features(&p, 0.0, 0.10);
        assert_eq!(f.f0_head, None); // frames 0,1,2 unvoiced
        assert_eq!(f.f0_tail, Some(180.0)); // median of 170,180,190
        assert!((f.voiced_fraction - 0.5).abs() < 1e-9);
        assert_eq!(f.f0_median, Some(170.0)); // median of 150..=190
    }

    #[test]
    fn fully_unvoiced_unit_has_no_pitch_summaries() {
        let p = fixture();
        let f = unit_features(&p, 0.0, 0.05);
        assert_eq!(f.f0_median, None);
        assert_eq!(f.f0_slope, None);
        assert!((f.voiced_fraction - 0.0).abs() < 1e-9);
    }

    #[test]
    fn slope_recovers_the_linear_ramp() {
        let p = fixture();
        // f0 rises 10 Hz per frame = 1000 Hz/s.
        let f = unit_features(&p, 0.05, 0.10);
        assert!((f.f0_slope.unwrap() - 1000.0).abs() < 1e-6);
    }

    #[test]
    fn even_count_median_averages_the_middle_pair() {
        let p = fixture();
        let f = unit_features(&p, 0.05, 0.09); // voiced 150,160,170,180
        assert_eq!(f.f0_median, Some(165.0));
    }

    #[test]
    fn blob_encoding_is_little_endian_f32() {
        assert_eq!(f32_blob(&[1.0]), 1.0f32.to_le_bytes().to_vec());
        assert_eq!(f32_blob(&[1.0, -2.5]).len(), 8);
    }
}
