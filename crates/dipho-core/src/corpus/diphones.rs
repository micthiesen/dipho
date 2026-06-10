//! Diphone derivation: the adjacency rule and span materialization.
//!
//! Operates on a source's processed phone tier (NOISE excised, SIL runs
//! merged, cut points assigned). A and B bond iff their gap is ≤ 20 ms,
//! neither is NOISE, and not both are SIL. SIL participates but blocks
//! transitivity; zero-extent SIL terminators break adjacency without
//! forming addressable units.

use super::phones::{
    ADJACENCY_GAP_MAX, SIL_DISPLACEMENT_MAX, T_EPS, TierPhone, noise_excised_view,
};

#[derive(Debug, Clone, PartialEq)]
pub struct DerivedDiphone {
    /// Indices into the tier passed to `derive`.
    pub phone_a: usize,
    pub phone_b: usize,
    /// Per-source ordinal. Consecutive bonded diphones (sharing their
    /// middle phone) get consecutive values; every break skips one, so a
    /// `seq + 1` self-join finds exactly the source-contiguous pairs.
    pub seq: i64,
    /// Stress-stripped match key, e.g. "AA-K".
    pub label: String,
    pub stress_a: Option<i64>,
    pub stress_b: Option<i64>,
    pub t_start: f64,
    pub t_end: f64,
}

/// Boundary on the SIL side of a diphone: displaced into the silence from
/// the shared speech edge by min(half the SIL duration, 200 ms) — no units
/// carrying a second of dead air.
fn sil_displacement(sil: &TierPhone) -> f64 {
    (0.5 * sil.duration()).min(SIL_DISPLACEMENT_MAX)
}

/// A real phone's materialized boundary is its cut point — and only SIL
/// sides may lack one, so a missing cut_t is a sequencing bug, not data.
fn cut(p: &TierPhone) -> f64 {
    p.cut_t
        .expect("real phone missing cut_t: assign_cut_points must run before derive")
}

pub fn derive(tier: &[TierPhone]) -> Vec<DerivedDiphone> {
    let view = noise_excised_view(tier);
    let mut out = Vec::new();
    let mut seq: i64 = -2;
    let mut prev_b: Option<usize> = None;
    for w in view.windows(2) {
        let (ia, ib) = (w[0], w[1]);
        let (a, b) = (&tier[ia], &tier[ib]);
        let gap = b.t_start - a.t_end;
        let bonds = gap <= ADJACENCY_GAP_MAX + T_EPS && !(a.is_sil() && b.is_sil());
        let zero_sil = (a.is_sil() && a.is_zero_extent()) || (b.is_sil() && b.is_zero_extent());
        if !bonds || zero_sil {
            prev_b = None;
            continue;
        }
        seq = if prev_b == Some(ia) { seq + 1 } else { seq + 2 };
        prev_b = Some(ib);
        let t_start = if a.is_sil() {
            a.t_end - sil_displacement(a)
        } else {
            cut(a)
        };
        let t_end = if b.is_sil() {
            b.t_start + sil_displacement(b)
        } else {
            cut(b)
        };
        out.push(DerivedDiphone {
            phone_a: ia,
            phone_b: ib,
            seq,
            label: format!("{}-{}", a.base(), b.base()),
            stress_a: a.stress(),
            stress_b: b.stress(),
            t_start,
            t_end,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::manifest::Phoneme;
    use crate::corpus::phones::{assign_cut_points, build_tier};

    fn tier_of(entries: &[(&str, f64, f64)]) -> Vec<TierPhone> {
        let phonemes: Vec<Phoneme> = entries
            .iter()
            .map(|(label, start, end)| Phoneme {
                label: label.to_string(),
                start: *start,
                end: *end,
                confidence: None,
                word_index: None,
            })
            .collect();
        let mut tier = build_tier(&phonemes).unwrap();
        assign_cut_points(&mut tier);
        tier
    }

    #[test]
    fn excised_noise_bridges_under_20ms_and_breaks_over() {
        let tier = tier_of(&[
            ("AA1", 0.0, 0.2),
            ("NOISE", 0.2, 0.215),
            ("N", 0.215, 0.4),
            ("NOISE", 0.4, 0.45),
            ("M", 0.45, 0.6),
        ]);
        let labels: Vec<String> = derive(&tier).iter().map(|d| d.label.clone()).collect();
        assert_eq!(labels, vec!["AA-N"]);
    }

    #[test]
    fn sil_blocks_transitivity_and_both_sil_never_bond() {
        let tier = tier_of(&[
            ("AA1", 0.0, 0.2),
            ("SIL", 0.2, 0.5),
            ("N", 0.5, 0.7),
            ("SIL", 0.7, 0.9),
            // Within the 20 ms gap but both SIL: never a unit.
            ("SIL", 0.91, 1.0),
        ]);
        let labels: Vec<String> = derive(&tier).iter().map(|d| d.label.clone()).collect();
        assert_eq!(labels, vec!["AA-SIL", "SIL-N", "N-SIL"]);
    }

    #[test]
    fn sil_boundaries_are_displaced_with_cap() {
        // 600 ms SIL: displacement capped at 200 ms on both sides.
        let tier = tier_of(&[("AA1", 0.0, 0.2), ("SIL", 0.2, 0.8), ("N", 0.8, 1.0)]);
        let d = derive(&tier);
        assert!((d[0].t_end - 0.4).abs() < 1e-9); // AA-SIL ends 200 ms in
        assert!((d[1].t_start - 0.6).abs() < 1e-9); // SIL-N starts 200 ms back
        assert!(d[0].t_end < d[1].t_start);

        // 100 ms SIL: displacement is half the duration.
        let tier = tier_of(&[("AA1", 0.0, 0.2), ("SIL", 0.2, 0.3), ("N", 0.3, 0.5)]);
        let d = derive(&tier);
        assert!((d[0].t_end - 0.25).abs() < 1e-9);
        assert!((d[1].t_start - 0.25).abs() < 1e-9);
    }

    #[test]
    fn zero_extent_sil_terminates_without_forming_units() {
        let mut tier = tier_of(&[("AA1", 0.0, 0.2), ("N", 0.2, 0.4)]);
        super::super::phones::insert_terminators(
            &mut tier,
            &[(0.2, super::super::phones::SilOrigin::Turn)],
        );
        assert!(derive(&tier).is_empty());
    }

    #[test]
    fn seq_is_consecutive_within_chains_and_skips_across_breaks() {
        let tier = tier_of(&[
            ("AA1", 0.0, 0.2),
            ("N", 0.2, 0.4),
            ("M", 0.4, 0.6),
            // 100 ms hard gap
            ("IY1", 0.7, 0.9),
            ("Z", 0.9, 1.1),
        ]);
        let d = derive(&tier);
        let seqs: Vec<i64> = d.iter().map(|x| x.seq).collect();
        assert_eq!(seqs, vec![0, 1, 3]);
    }

    #[test]
    fn labels_strip_stress_and_keep_it_alongside() {
        let tier = tier_of(&[("K", 0.0, 0.1), ("AE1", 0.1, 0.3)]);
        let d = derive(&tier);
        assert_eq!(d[0].label, "K-AE");
        assert_eq!(d[0].stress_a, None);
        assert_eq!(d[0].stress_b, Some(1));
    }
}
