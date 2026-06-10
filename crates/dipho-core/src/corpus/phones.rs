//! Phone tier processing: ARPAbet label parsing, the normative cut-point
//! table, SIL terminator insertion at chunk/turn boundaries, SIL-run
//! merging, and cut-point assignment. Pure logic — no I/O.

use super::CorpusError;
use super::manifest::Phoneme;

/// Maximum gap between two phones that still bond into a diphone. Given
/// MFA's contiguous tiers, a nonzero gap exists exactly where a NOISE row
/// was excised — this threshold decides whether a short `<spn>` blip is
/// bridgeable or a hard break.
pub const ADJACENCY_GAP_MAX: f64 = 0.020;
/// A SIL-side diphone boundary is displaced into the silence from the
/// shared speech edge by min(half the SIL duration, this cap).
pub const SIL_DISPLACEMENT_MAX: f64 = 0.200;
/// Tolerance for timestamps that should coincide exactly.
pub const T_EPS: f64 = 1e-9;
/// Stops and affricates cut this far into the phone — inside the closure,
/// before any plausible burst.
const STOP_CUT_FRACTION: f64 = 0.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CutClass {
    StopOrAffricate,
    Fricative,
    NasalOrLiquid,
    Monophthong,
    Diphthong,
    Glide,
}

impl CutClass {
    fn of(base: &str) -> Option<Self> {
        Some(match base {
            "B" | "D" | "G" | "K" | "P" | "T" | "CH" | "JH" => Self::StopOrAffricate,
            "DH" | "F" | "HH" | "S" | "SH" | "TH" | "V" | "Z" | "ZH" => Self::Fricative,
            "M" | "N" | "NG" | "L" | "R" => Self::NasalOrLiquid,
            "AA" | "AE" | "AH" | "AO" | "EH" | "ER" | "IH" | "IY" | "UH" | "UW" => {
                Self::Monophthong
            }
            "AY" | "AW" | "EY" | "OW" | "OY" => Self::Diphthong,
            "W" | "Y" => Self::Glide,
            _ => return None,
        })
    }

    pub fn weak_cut(self) -> bool {
        matches!(self, Self::Diphthong | Self::Glide)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SilOrigin {
    Chunk,
    Turn,
    /// Highest precedence when merging SIL runs: acoustic truth wins.
    Mfa,
}

impl SilOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mfa => "mfa",
            Self::Chunk => "chunk",
            Self::Turn => "turn",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Real {
        base: String,
        stress: Option<i64>,
        class: CutClass,
    },
    Sil(SilOrigin),
    Noise,
}

#[derive(Debug, Clone)]
pub struct TierPhone {
    pub t_start: f64,
    pub t_end: f64,
    pub kind: Kind,
    pub word_index: Option<usize>,
    pub confidence: Option<f64>,
    /// NULL for SIL/NOISE; assigned by `assign_cut_points`.
    pub cut_t: Option<f64>,
}

impl TierPhone {
    pub fn duration(&self) -> f64 {
        self.t_end - self.t_start
    }

    pub fn is_sil(&self) -> bool {
        matches!(self.kind, Kind::Sil(_))
    }

    pub fn is_noise(&self) -> bool {
        matches!(self.kind, Kind::Noise)
    }

    /// Zero-extent rows are inserted SIL terminators: they break adjacency
    /// but have no acoustic reality (no cut exception, no addressable unit).
    pub fn is_zero_extent(&self) -> bool {
        self.duration() <= T_EPS
    }

    pub fn sil_origin(&self) -> Option<SilOrigin> {
        match self.kind {
            Kind::Sil(origin) => Some(origin),
            _ => None,
        }
    }

    /// Stress-marked label as stored in `phones.label`.
    pub fn label(&self) -> String {
        match &self.kind {
            Kind::Real { base, stress, .. } => match stress {
                Some(s) => format!("{base}{s}"),
                None => base.clone(),
            },
            Kind::Sil(_) => "SIL".to_string(),
            Kind::Noise => "NOISE".to_string(),
        }
    }

    /// Stress-stripped label for the diphone match key.
    pub fn base(&self) -> &str {
        match &self.kind {
            Kind::Real { base, .. } => base,
            Kind::Sil(_) => "SIL",
            Kind::Noise => "NOISE",
        }
    }

    pub fn stress(&self) -> Option<i64> {
        match &self.kind {
            Kind::Real { stress, .. } => *stress,
            _ => None,
        }
    }

    pub fn weak_cut(&self) -> bool {
        match &self.kind {
            Kind::Real { class, .. } => class.weak_cut(),
            _ => false,
        }
    }
}

fn parse_kind(label: &str) -> Result<Kind, CorpusError> {
    match label {
        "SIL" => Ok(Kind::Sil(SilOrigin::Mfa)),
        "NOISE" => Ok(Kind::Noise),
        _ => {
            let (base, stress) = match label.as_bytes().last() {
                Some(d @ b'0'..=b'2') => (&label[..label.len() - 1], Some(i64::from(d - b'0'))),
                _ => (label, None),
            };
            let class = CutClass::of(base).ok_or_else(|| CorpusError::UnknownPhoneLabel {
                label: label.to_string(),
            })?;
            Ok(Kind::Real {
                base: base.to_string(),
                stress,
                class,
            })
        }
    }
}

/// Parse the manifest phoneme tier into a time-ordered working tier.
/// Rejects non-finite or inverted intervals, zero-extent real phones (zero
/// extent is reserved for inserted SIL terminators), and overlapping
/// intervals — MFA tiers are non-overlapping; overlap means a malformed
/// manifest, and a negative inter-phone gap would corrupt diphone spans.
pub fn build_tier(phonemes: &[Phoneme]) -> Result<Vec<TierPhone>, CorpusError> {
    let mut tier = Vec::with_capacity(phonemes.len());
    for p in phonemes {
        if !p.start.is_finite() || !p.end.is_finite() || p.end < p.start {
            return Err(CorpusError::InvalidInterval {
                what: "phoneme",
                t_start: p.start,
                t_end: p.end,
            });
        }
        let kind = parse_kind(&p.label)?;
        if matches!(kind, Kind::Real { .. }) && p.end - p.start <= T_EPS {
            return Err(CorpusError::InvalidInterval {
                what: "zero-extent phoneme",
                t_start: p.start,
                t_end: p.end,
            });
        }
        let word_index = match kind {
            Kind::Real { .. } => p.word_index,
            _ => None,
        };
        tier.push(TierPhone {
            t_start: p.start,
            t_end: p.end,
            kind,
            word_index,
            confidence: p.confidence,
            cut_t: None,
        });
    }
    tier.sort_by(|a, b| {
        (a.t_start, a.t_end)
            .partial_cmp(&(b.t_start, b.t_end))
            .expect("phone times are finite")
    });
    for w in tier.windows(2) {
        if w[1].t_start < w[0].t_end - T_EPS {
            return Err(CorpusError::Contract(format!(
                "phoneme intervals overlap at {:.4}s",
                w[1].t_start
            )));
        }
    }
    Ok(tier)
}

/// Indices of the adjacency view: the tier minus NOISE rows. Cut-point
/// assignment and diphone derivation must see the same view, so both build
/// it here.
pub fn noise_excised_view(tier: &[TierPhone]) -> Vec<usize> {
    (0..tier.len()).filter(|&i| !tier[i].is_noise()).collect()
}

/// Insert SIL adjacency terminators at chunk-edge and speaker-turn
/// boundaries. Each boundary time is snapped to the nearest phone-interval
/// edge; where a SIL already covers or touches the boundary, adjacency is
/// already broken and nothing is inserted; where speech abuts, a
/// zero-length SIL row goes in as a pure adjacency terminator.
pub fn insert_terminators(tier: &mut Vec<TierPhone>, boundaries: &[(f64, SilOrigin)]) {
    if tier.is_empty() {
        return;
    }
    for &(t, origin) in boundaries {
        let touches_sil = |x: f64| {
            tier.iter()
                .any(|p| p.is_sil() && x >= p.t_start - T_EPS && x <= p.t_end + T_EPS)
        };
        if touches_sil(t) {
            continue;
        }
        let mut edge = t;
        let mut best = f64::INFINITY;
        for p in tier.iter() {
            for e in [p.t_start, p.t_end] {
                let d = (e - t).abs();
                if d < best {
                    best = d;
                    edge = e;
                }
            }
        }
        // The snapped edge may itself touch a SIL even when t did not.
        if touches_sil(edge) {
            continue;
        }
        // build_tier guarantees t_end >= t_start, so ordering by t_start
        // alone places the zero-length row after [a, edge] and before
        // [edge, b].
        let idx = tier.partition_point(|p| p.t_start < edge - T_EPS);
        tier.insert(
            idx,
            TierPhone {
                t_start: edge,
                t_end: edge,
                kind: Kind::Sil(origin),
                word_index: None,
                confidence: None,
                cut_t: None,
            },
        );
    }
}

/// Merge consecutive/abutting SIL rows — SIL-SIL is never a unit. The
/// merged row keeps the highest-precedence origin (mfa > turn > chunk).
pub fn merge_sil_runs(tier: Vec<TierPhone>) -> Vec<TierPhone> {
    let mut out: Vec<TierPhone> = Vec::with_capacity(tier.len());
    for p in tier {
        if p.is_sil()
            && let Some(last) = out.last_mut()
            && last.is_sil()
            && p.t_start <= last.t_end + T_EPS
        {
            last.t_end = last.t_end.max(p.t_end);
            if p.sil_origin() > last.sil_origin() {
                last.kind = p.kind;
            }
            continue;
        }
        out.push(p);
    }
    out
}

/// Assign `cut_t` per the normative cut-point table. The SIL-preceded-stop
/// exception is evaluated on the NOISE-excised view — the same view the
/// adjacency rule sees.
pub fn assign_cut_points(tier: &mut [TierPhone]) {
    let view = noise_excised_view(tier);
    for (vi, &i) in view.iter().enumerate() {
        let Kind::Real { class, .. } = &tier[i].kind else {
            continue;
        };
        let cut = if *class == CutClass::StopOrAffricate {
            // When the preceding phone is a positive-extent SIL abutting
            // within the adjacency gap, the closure belongs to the silence:
            // cut at the phone start so the burst stays intact in the
            // following unit. (A zero-length terminator or a hard break has
            // no SIL-stop unit to own the closure, so the normal cut
            // applies — see DESIGN.md's cut-point table.)
            let sil_preceded = vi > 0 && {
                let prev = &tier[view[vi - 1]];
                prev.is_sil()
                    && !prev.is_zero_extent()
                    && tier[i].t_start - prev.t_end <= ADJACENCY_GAP_MAX + T_EPS
            };
            if sil_preceded {
                tier[i].t_start
            } else {
                tier[i].t_start + STOP_CUT_FRACTION * tier[i].duration()
            }
        } else {
            0.5 * (tier[i].t_start + tier[i].t_end)
        };
        tier[i].cut_t = Some(cut);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phoneme(label: &str, start: f64, end: f64) -> Phoneme {
        Phoneme {
            label: label.to_string(),
            start,
            end,
            confidence: None,
            word_index: None,
        }
    }

    #[test]
    fn parses_stress_marked_labels() {
        let tier = build_tier(&[phoneme("AA1", 0.0, 0.1)]).unwrap();
        assert_eq!(tier[0].label(), "AA1");
        assert_eq!(tier[0].base(), "AA");
        assert_eq!(tier[0].stress(), Some(1));
    }

    #[test]
    fn rejects_unknown_labels() {
        assert!(matches!(
            build_tier(&[phoneme("QX", 0.0, 0.1)]),
            Err(CorpusError::UnknownPhoneLabel { .. })
        ));
    }

    #[test]
    fn full_arpa_inventory_is_classified() {
        for base in [
            "B", "D", "G", "K", "P", "T", "CH", "JH", "DH", "F", "HH", "S", "SH", "TH", "V", "Z",
            "ZH", "M", "N", "NG", "L", "R", "AA", "AE", "AH", "AO", "EH", "ER", "IH", "IY", "UH",
            "UW", "AY", "AW", "EY", "OW", "OY", "W", "Y",
        ] {
            assert!(CutClass::of(base).is_some(), "unclassified: {base}");
        }
    }

    #[test]
    fn stop_cuts_twenty_percent_in_and_sil_preceded_stop_cuts_at_start() {
        let mut tier = build_tier(&[
            phoneme("SIL", 0.0, 0.5),
            phoneme("T", 0.5, 0.6),
            phoneme("AA1", 0.6, 0.8),
            phoneme("K", 0.8, 0.9),
        ])
        .unwrap();
        assign_cut_points(&mut tier);
        assert_eq!(tier[1].cut_t, Some(0.5)); // SIL-preceded: phone start
        assert_eq!(tier[2].cut_t, Some(0.7)); // vowel: midpoint
        assert!((tier[3].cut_t.unwrap() - 0.82).abs() < 1e-9); // 20% in
    }

    #[test]
    fn zero_extent_sil_does_not_trigger_the_stop_exception() {
        let mut tier = build_tier(&[phoneme("AA1", 0.0, 0.2), phoneme("T", 0.2, 0.3)]).unwrap();
        insert_terminators(&mut tier, &[(0.2, SilOrigin::Turn)]);
        assign_cut_points(&mut tier);
        let t = tier.iter().find(|p| p.base() == "T").unwrap();
        assert!((t.cut_t.unwrap() - 0.22).abs() < 1e-9);
    }

    #[test]
    fn stop_exception_sees_through_an_excised_noise_blip() {
        let mut tier = build_tier(&[
            phoneme("SIL", 0.0, 0.5),
            phoneme("NOISE", 0.5, 0.51),
            phoneme("T", 0.51, 0.6),
        ])
        .unwrap();
        assign_cut_points(&mut tier);
        assert_eq!(tier[2].cut_t, Some(0.51));
    }

    #[test]
    fn diphthongs_and_glides_are_weak_cuts() {
        let tier = build_tier(&[phoneme("OW1", 0.0, 0.2), phoneme("W", 0.2, 0.3)]).unwrap();
        assert!(tier[0].weak_cut());
        assert!(tier[1].weak_cut());
        let tier = build_tier(&[phoneme("AA1", 0.0, 0.2)]).unwrap();
        assert!(!tier[0].weak_cut());
    }

    #[test]
    fn terminator_is_inserted_where_speech_abuts() {
        let mut tier = build_tier(&[phoneme("AA1", 0.0, 0.2), phoneme("N", 0.2, 0.4)]).unwrap();
        insert_terminators(&mut tier, &[(0.205, SilOrigin::Chunk)]);
        assert_eq!(tier.len(), 3);
        // Snapped to the nearest phone edge, not the raw boundary time.
        assert_eq!(tier[1].t_start, 0.2);
        assert_eq!(tier[1].t_end, 0.2);
        assert_eq!(tier[1].sil_origin(), Some(SilOrigin::Chunk));
    }

    #[test]
    fn terminator_skipped_inside_or_touching_sil() {
        let base = build_tier(&[
            phoneme("AA1", 0.0, 0.2),
            phoneme("SIL", 0.2, 0.6),
            phoneme("N", 0.6, 0.8),
        ])
        .unwrap();
        // Inside the SIL, at its edges, and snapping onto a SIL edge: no-ops.
        for t in [0.4, 0.2, 0.6, 0.65] {
            let mut tier = base.clone();
            insert_terminators(&mut tier, &[(t, SilOrigin::Turn)]);
            assert_eq!(tier.len(), 3, "boundary at {t} should be a no-op");
        }
    }

    #[test]
    fn duplicate_boundaries_insert_one_terminator() {
        let mut tier = build_tier(&[phoneme("AA1", 0.0, 0.2), phoneme("N", 0.2, 0.4)]).unwrap();
        insert_terminators(
            &mut tier,
            &[(0.2, SilOrigin::Turn), (0.2, SilOrigin::Chunk)],
        );
        assert_eq!(tier.len(), 3);
        assert_eq!(tier[1].sil_origin(), Some(SilOrigin::Turn));
    }

    #[test]
    fn abutting_sils_merge_with_origin_precedence() {
        let mut tier = build_tier(&[
            phoneme("AA1", 0.0, 0.2),
            phoneme("SIL", 0.2, 0.4),
            phoneme("SIL", 0.4, 0.6),
            phoneme("N", 0.6, 0.8),
        ])
        .unwrap();
        tier[2].kind = Kind::Sil(SilOrigin::Chunk);
        let tier = merge_sil_runs(tier);
        assert_eq!(tier.len(), 3);
        assert_eq!(tier[1].t_start, 0.2);
        assert_eq!(tier[1].t_end, 0.6);
        assert_eq!(tier[1].sil_origin(), Some(SilOrigin::Mfa));
    }

    #[test]
    fn separated_sils_do_not_merge() {
        let tier = build_tier(&[
            phoneme("SIL", 0.0, 0.2),
            phoneme("AA1", 0.2, 0.4),
            phoneme("SIL", 0.4, 0.6),
        ])
        .unwrap();
        assert_eq!(merge_sil_runs(tier).len(), 3);
    }
}
