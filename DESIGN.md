# dipho — Design

> Canonical design doc. The architecture summary in CLAUDE.md is derived from this; when they disagree, this wins.

## Thesis

Sentence mixing is **unit-selection speech synthesis** (the Hunt & Black 1996 lineage) where the unit database is arbitrary source media instead of a studio-recorded voice corpus. Classical unit selection picks units from a database to match a target utterance, minimizing *target cost* (does this unit sound like what we want?) plus *join cost* (do adjacent units splice cleanly?). YTP sentence mixing is exactly this problem with a found-footage database — so we build the tool around that framing instead of around a video editor's timeline.

Two core abstractions follow: **the Corpus** (the unit database) and **the Edit** (the selected, transformed unit sequence). Between them, later, sits a **solver**.

## Abstraction 1: The Corpus

An addressable phonetic index over **immutable sources**. Sources are never edited, only indexed. Everything downstream is a span reference:

```
(source_id, t_start, t_end, channel)    where channel ∈ {audio, video, both}
```

Audio and video are decoupled as first-class: a mix routinely takes the audio of one span over the video of another, holds a freeze-frame under continuing speech, or loops video under a stutter. Making `channel` part of the span reference — rather than an edit-time afterthought — keeps that representable everywhere.

### Ingest pipeline

```
yt-dlp URL or local file
  → ffmpeg demux (normalized audio extraction: wav, mono, 16 kHz for analysis;
                  original media kept untouched for playback/render)
  → WhisperX: transcription + forced alignment → word AND phoneme timestamps
  → pyannote: speaker diarization → speaker label per span
  → prosody features per unit: f0 (pitch), RMS energy
  → write to corpus SQLite
```

Ingest is batch, offline, Python (`python/` sidecar). It is never in the interactive loop. Contract: media in → one JSON document out (alignment, diarization, features), which the Rust side ingests into SQLite. The JSON contract is documented in `python/README.md`.

### Why diphones, not phonemes

Coarticulation means phoneme boundaries are the *worst* place to cut: the signal there is a transition smeared across both neighbors. The stable, cuttable points are mid-phoneme — the steady state in the middle of a vowel or fricative. A **diphone** (mid-phoneme to mid-phoneme, spanning one phoneme transition) makes its boundaries land exactly on those stable points. This is the same reason diphone synthesis predated and informed unit selection.

So the index stores diphones as the primary sub-word unit: for each adjacent phoneme pair in the aligned transcript, a span from the midpoint of phoneme A to the midpoint of phoneme B. Word- and phrase-level search sits on top for the common case ("find every time he says 'gun'"); diphone n-grams serve sub-word assembly ("build the word 'mama' out of someone who never said it").

### Storage: SQLite

One SQLite database per corpus (rusqlite, bundled — no system dependency):

- `sources` — id, origin (URL/path), media path, duration, metadata
- `words` — span ref, text, speaker, confidence; **FTS5** index for word/phrase search
- `phonemes` — span ref, phoneme label, word ref
- `diphones` — span ref (midpoint-to-midpoint), label pair (e.g. `AA-K`), plus an n-gram table over diphone label sequences for assembly queries
- `prosody` — per-unit f0 mean/contour summary, RMS energy (columns on the unit tables or a sidecar table; needed by the join-cost solver later, so it ships in the schema now)
- `speakers` — diarization clusters, optional human-assigned names

Schema rule: **post-MVP features (solver, join cost) must be servable from this schema without rework.** Prosody and diphone tables exist from day one even though MVP only reads `words`.

## Abstraction 2: The Edit

A **program, not a timeline**. The edit is non-destructive EDL-as-data: an ordered structure of span references plus transforms:

```
loop, reverse, pitch, speed, stutter, ...
```

Serde-friendly format of our own (JSON or TOML — decided by what stays pleasant to hand-edit and diff). It compiles to two targets:

1. **mpv EDL** — for instant, zero-render preview. mpv plays EDL playlists natively, so auditioning an edit is just telling the already-running mpv to load a compiled EDL string. Transforms that mpv can't express in EDL (pitch, reverse) degrade gracefully in preview or use mpv property/filter commands where possible.
2. **ffmpeg render** — final export. Full-fidelity compilation of every transform to an ffmpeg filter graph / concat.

The asymmetry is deliberate: preview optimizes for latency (milliseconds, no temp files), render optimizes for correctness.

## The Solver (post-MVP)

Type a target sentence → ranked candidate assemblies. Classical unit-selection search:

- **Target cost** — does the candidate unit match the requested phoneme sequence (and eventually speaker, prosody)?
- **Join cost** — objective splice quality at each boundary: spectral discontinuity, f0 jump, energy jump. Explicitly **not** humor scoring; the human picks the funny one from a shortlist of clean ones.

Viterbi/beam search over the diphone lattice, same as the literature. The corpus schema (diphones + prosody) exists to feed this.

## DSP cut refinement (post-MVP, native)

Aligner timestamps are ±tens of ms. Two refinement passes in the Rust hot loop (symphonia for decode, rustfft for analysis):

1. Snap each cut to the nearest zero-crossing within the aligner's tolerance window — eliminates clicks.
2. Later: minimize spectral flux at the boundary within that window — picks the perceptually smoothest cut point, the local version of join cost.

## Architecture: three processes

| Process | Role | Why separate |
|---|---|---|
| **dipho** (Rust) | index, search, EDL, DSP, ratatui TUI, mpv control | The interactive loop; everything latency-sensitive |
| **mpv** | slave player: preview/audition in its own window | Best-in-class playback; controlled via JSON IPC (`--input-ipc-server`); never render video in-terminal |
| **Python sidecar** | batch ingest: WhisperX, pyannote, prosody extraction | ML stack is Python-native; offline by nature; crash/version isolation |

Workspace layout:

- `crates/dipho-core` — library: span types, corpus index, EDL types + compilation, DSP. No TUI, no process management.
- `crates/dipho` — binary: clap CLI, ratatui TUI, mpv IPC client.
- `python/` — uv project, `dipho-ingest` entry point.

## MVP: one vertical slice

In order, each step usable before the next exists:

1. **Ingest** — `dipho ingest <url|file>`: run sidecar, load JSON into corpus
2. **Index** — corpus SQLite with words (FTS5), phonemes, diphones, prosody
3. **Word search** — TUI: type a word, see every utterance ranked
4. **Audition** — select a hit, mpv plays that span instantly (ab-loop for scrubbing)
5. **Flat EDL** — append spans to an edit list, reorder, save/load the EDL file, preview via compiled mpv EDL
6. **Render** — `dipho render edit.json out.mp4` via ffmpeg

Post-MVP, in rough order: transforms beyond cut/concat, diphone assembly search, the solver, DSP cut refinement, learned splice scoring.

## Open questions

- EDL file format: JSON vs TOML (lean JSON for nested structure; revisit when the EDL type stabilizes)
- WhisperX phoneme-level output quality vs adding MFA as a second alignment pass (research pass will answer)
- Whisper backend on Apple Silicon: faster-whisper (CTranslate2, CPU int8) vs whisper.cpp / MLX (Metal) (research)
- Diphone label set: ARPAbet (what most aligners emit) vs IPA
