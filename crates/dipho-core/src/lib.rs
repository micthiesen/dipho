//! dipho-core: corpus index, span references, EDL types and compilation, DSP.
//!
//! Library only — no TUI, no process management. See DESIGN.md at the repo
//! root for the two core abstractions (the Corpus and the Edit).

pub mod corpus;
pub mod dsp;
pub mod edl;
pub mod span;
