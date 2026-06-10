//! Speaker derivation from raw diarization turns. The loader is the single
//! owner of speaker assignment — the sidecar only emits turns.

use super::phones::T_EPS;

/// A diarization turn bound to a speaker row.
pub struct TurnRow {
    pub speaker_id: i64,
    pub t_start: f64,
    pub t_end: f64,
}

fn overlap(a_start: f64, a_end: f64, b_start: f64, b_end: f64) -> f64 {
    (a_end.min(b_end) - a_start.max(b_start)).max(0.0)
}

/// Speaker of the turn with maximal temporal overlap over the span. Ties
/// keep the earlier turn; zero overlap reads None. `turns` must be sorted
/// by start time.
pub fn max_overlap_speaker(turns: &[TurnRow], t_start: f64, t_end: f64) -> Option<i64> {
    let mut best = 0.0;
    let mut speaker = None;
    for turn in turns {
        let ov = overlap(t_start, t_end, turn.t_start, turn.t_end);
        if ov > best {
            best = ov;
            speaker = Some(turn.speaker_id);
        }
    }
    speaker
}

/// True when a speaker other than `primary` overlaps more than 20% of the
/// span.
pub fn is_multi_speaker(turns: &[TurnRow], t_start: f64, t_end: f64, primary: Option<i64>) -> bool {
    let span = t_end - t_start;
    if span <= 0.0 {
        return false;
    }
    let mut totals: Vec<(i64, f64)> = Vec::new();
    for turn in turns {
        let ov = overlap(t_start, t_end, turn.t_start, turn.t_end);
        if ov <= 0.0 {
            continue;
        }
        match totals.iter_mut().find(|(id, _)| *id == turn.speaker_id) {
            Some((_, total)) => *total += ov,
            None => totals.push((turn.speaker_id, ov)),
        }
    }
    totals
        .iter()
        .any(|&(id, total)| Some(id) != primary && total > 0.2 * span)
}

fn total_duration(set: &[(f64, f64)]) -> f64 {
    set.iter().map(|(s, e)| e - s).sum()
}

fn total_overlap(a: &[(f64, f64)], b: &[(f64, f64)]) -> f64 {
    let mut sum = 0.0;
    for &(a_start, a_end) in a {
        for &(b_start, b_end) in b {
            sum += overlap(a_start, a_end, b_start, b_end);
        }
    }
    sum
}

/// Re-ingest carry-forward: for each new turn-set, the index of the old
/// speaker it inherits, if any. A match requires temporal overlap of at
/// least 50% of the LARGER set's total speech — symmetric, so a tiny
/// diarization artifact sitting inside a named speaker's old speech can
/// never claim that identity. Each old speaker is claimed at most once,
/// best overlap first.
pub fn carry_forward(
    new_sets: &[Vec<(f64, f64)>],
    old_sets: &[Vec<(f64, f64)>],
) -> Vec<Option<usize>> {
    let mut pairs: Vec<(f64, usize, usize)> = Vec::new();
    for (ni, new) in new_sets.iter().enumerate() {
        let total_new = total_duration(new);
        if total_new <= T_EPS {
            continue;
        }
        for (oi, old) in old_sets.iter().enumerate() {
            let ov = total_overlap(new, old);
            if ov + T_EPS >= 0.5 * total_new.max(total_duration(old)) {
                pairs.push((ov, ni, oi));
            }
        }
    }
    pairs.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .expect("overlaps are finite")
            .then(a.1.cmp(&b.1))
            .then(a.2.cmp(&b.2))
    });
    let mut assigned = vec![None; new_sets.len()];
    let mut claimed = vec![false; old_sets.len()];
    for (_, ni, oi) in pairs {
        if assigned[ni].is_none() && !claimed[oi] {
            assigned[ni] = Some(oi);
            claimed[oi] = true;
        }
    }
    assigned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn turns() -> Vec<TurnRow> {
        vec![
            TurnRow {
                speaker_id: 1,
                t_start: 0.0,
                t_end: 5.0,
            },
            TurnRow {
                speaker_id: 2,
                t_start: 5.0,
                t_end: 10.0,
            },
        ]
    }

    #[test]
    fn max_overlap_picks_dominant_turn() {
        assert_eq!(max_overlap_speaker(&turns(), 4.0, 7.0), Some(2));
        assert_eq!(max_overlap_speaker(&turns(), 1.0, 2.0), Some(1));
    }

    #[test]
    fn ties_keep_the_earlier_turn_and_zero_overlap_is_none() {
        assert_eq!(max_overlap_speaker(&turns(), 4.0, 6.0), Some(1));
        assert_eq!(max_overlap_speaker(&turns(), 11.0, 12.0), None);
    }

    #[test]
    fn multi_speaker_needs_a_second_speaker_over_20_percent() {
        // Speaker 1 covers 30% of the span: multi.
        assert!(is_multi_speaker(&turns(), 3.5, 8.5, Some(2)));
        // Speaker 1 covers 10%: not multi.
        assert!(!is_multi_speaker(&turns(), 4.5, 9.5, Some(2)));
    }

    #[test]
    fn carry_forward_matches_on_majority_overlap_once_each() {
        let new = vec![vec![(0.0, 4.0)], vec![(5.0, 9.0)], vec![(20.0, 21.0)]];
        let old = vec![vec![(5.0, 10.0)], vec![(0.0, 4.5)]];
        let assigned = carry_forward(&new, &old);
        assert_eq!(assigned, vec![Some(1), Some(0), None]);
    }

    #[test]
    fn carry_forward_rejects_below_majority_overlap() {
        let new = vec![vec![(0.0, 10.0)]];
        let old = vec![vec![(0.0, 4.0)]]; // 40% of new speech
        assert_eq!(carry_forward(&new, &old), vec![None]);
    }

    #[test]
    fn carry_forward_artifact_cannot_steal_a_speaker() {
        // A 1 s artifact fully inside the old speaker's speech covers 100%
        // of its own set but ~1% of the old speaker's: no claim. The real
        // continuation matches.
        let new = vec![vec![(0.0, 1.0)], vec![(0.0, 102.0)]];
        let old = vec![vec![(0.0, 100.0)]];
        assert_eq!(carry_forward(&new, &old), vec![None, Some(0)]);
    }
}
