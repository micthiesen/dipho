//! DSP cut refinement. Post-MVP; see DESIGN.md.
//!
//! Aligner timestamps are ±tens of ms. Planned passes, both operating
//! within the aligner's tolerance window around a proposed cut:
//! 1. snap to the nearest zero-crossing (eliminates clicks)
//! 2. minimize spectral flux at the boundary (symphonia + rustfft)

/// Snap a proposed cut (in samples) to the nearest zero-crossing within
/// ±`tolerance` samples. Post-MVP.
pub fn snap_to_zero_crossing(_samples: &[f32], _cut: usize, _tolerance: usize) -> usize {
    todo!("zero-crossing snap (post-MVP)")
}
