# Research Summary

Synthesis of reports 01–05 in this directory (research date 2026-06-10). Each
decision below is ready to ratify or veto; sources cited by report number.

## Decisions to ratify

1. **Ingest stack: mlx-whisper (transcribe) + WhisperX align (words) + MFA 3.3.9 subprocess (phones) + pyannote 4.x on MPS (speakers) + librosa (prosody).**
   WhisperX's English alignment models are letter-CTC — it produces word/character
   timestamps only, never phonemes, so a second phoneme pass is mandatory.
   MFA 3.3.9 (actively maintained, ~10–20 ms phone boundaries) is the gold
   standard and exactly what diphone cut points need; run it as a CLI subprocess
   in its own micromamba env on per-segment chunks cut at WhisperX segment
   boundaries. faster-whisper has no Metal backend, so mlx-whisper
   large-v3-turbo does transcription on the M4 Max GPU. (Report 04)

2. **Keep a pure-pip fallback aligner and make the SQLite phone schema aligner-agnostic.**
   Fallback: phonemizer/espeak-ng → `facebook/wav2vec2-lv-60-espeak-cv-ft` →
   `torchaudio.functional.forced_align` (explicitly preserved in torchaudio
   2.10). Softer CTC boundaries are acceptable because post-MVP DSP cut
   refinement covers them. Store `aligner_id` + per-phone confidence per row so
   both backends coexist. (Report 04)

3. **Run diarization directly via pyannote 4.0.x (`speaker-diarization-community-1`, MPS), not through WhisperX's bundled integration.**
   WhisperX pins pyannote 3.x; 4.x runs on MPS and is the current open pipeline.
   The model is HF-gated (token + accepted conditions) — an ingest-setup step,
   not a code problem. Isolate environments: WhisperX's heavy pinned dependency
   set conflicts with everything else. (Report 04)

4. **Preview path: one long-lived mpv slave; recompile the edit to an `edl://` URI on every change and send `["loadfile", uri, "replace"]` over JSON IPC. No temp files.**
   Spawn with `--idle=yes --keep-open=yes --no-terminal --input-ipc-server=$TMPDIR/dipho-mpv.sock`
   (macOS sun_path ~104-byte limit; socket 0600 — IPC exposes `run`). The
   JSON-array IPC form sidesteps shell quoting entirely; the pattern is
   field-proven (occivink/mpv-music-player). Keep `.mpv.edl` file export as a
   user-facing artifact of the same compiler. (Reports 03, 02)

5. **EDL writer: one choke-point Rust module; `%<bytes>%`-quote every path unconditionally; named params; explicit float formatting; golden-file tests; probe mpv version at startup.**
   videogrep's 7-line exporter breaks on commas in filenames because it skips
   the spec's quoting rules — byte-length quoting is always valid, so apply it
   to every value. EDL v0 is explicitly unfrozen and `loadfile` gained an index
   arg in mpv 0.38.0, hence version probing. Emit per-segment `title=` (word
   label) and keep mpv's implicit one-chapter-per-segment as free cut
   navigation. `!new_stream` is the documented post-MVP dub mechanism
   (audio from A over video from B); avoid `!delay_open`/`!no_clip`/`!mp4_dash`.
   (Reports 03, 02)

6. **Lift from sentence-mixing: the 3-step cost decomposition, the contiguity principle, and the audition-loop UX. Port ideas, not code.**
   Steps 1–2 = target cost, step 3 = join cost — a 1:1 Hunt & Black mapping;
   their `parameters.py` weight ratios (amplitude-join 500 ≫ spectral-join 50;
   contiguity ~80–100/phoneme; duration caps 0.25 s consonant / 0.5 s vowel)
   are the tuning starting point. Their best idea — greedy forced following of
   contiguous source runs (`SkippedChoice`) — becomes zero join cost for
   source-adjacent diphones plus a span bonus. UX to keep: chunk-by-chunk
   (~1 word) authoring, rank-ordered candidates, stash buffer, re-edit,
   accept-and-advance, autosaved session; improve it with a visible scored
   candidate list and mpv-EDL audition. Rewrite candidate generation (add a
   phoneme substitution matrix — their exact-match-only lookup hard-fails) and
   the search (beam over a lattice; join cost depends only on a short suffix).
   Infra rot (MFA 1.1.0-beta-2, pinned yt-dlp 2022) killed the project, not the
   algorithm. (Report 01)

7. **Lift from videogrep: the flat composition shape, pad-then-merge semantics, and multi-target export. Render with ffmpeg, never moviepy.**
   `[{file, start, end, content}]` as the universal currency is dipho's flat
   EDL; keep `content` on every span (labels, captions, demo output for free).
   Copy `pad_and_sync`: symmetric padding, clamp at 0, merge
   overlapping/touching spans per source file — in the EDL compiler, plus the
   duration clamp videogrep only applies in its render path. videogrep's
   batch-of-20 + gc.collect() + silent-batch-drop machinery is pure moviepy
   damage and validates direct ffmpeg. Cheap post-MVP exports: output-timeline
   VTT (cumulative durations) and FCP7 XML ("finish in a real NLE"). Their
   `mash` mode is the solver's baseline to demonstrably beat. (Report 02)

8. **TUI architecture: Elm-flavored single event loop on tokio; skip the component template.**
   Every serious ratatui app (television, atuin, gitui, yazi) converges on one
   App state struct, one merged Event enum, producers feeding one mpsc, a
   single consumer loop, render-on-dirty (~10 ms yazi-style debounce). dipho
   already has async I/O to multiplex (mpv socket, DB worker, ffmpeg jobs), so
   tokio fits. Module layout: `app.rs` (state+update), `event.rs`
   (`Event { Term, Tick, Mpv, Db, Job }`), `ui/` (pure draw), `mpv/`
   (spawn + client task), `db/` (rusqlite on spawn_blocking), `edl.rs`
   (data model → mpv EDL string / ffmpeg filtergraph). (Report 05)

9. **Crates — adopt now: ratatui 0.30.1 (`ratatui::run`), tokio, crossterm (event-stream), tui-input, serde_json, rusqlite. Hand-roll the mpv IPC client. Later: nucleo, ratatui-textarea, rodio. Skip: mpvipc, kira, tui-rs-era widgets.**
   The mpv protocol is ~200 lines of Rust (UnixStream + request_id→oneshot map
   + events→mpsc); existing crates are low-bus-factor and dipho needs precise
   control (ab-loops, exact seeks, EDL loads). One persistent connection only —
   closing drops `observe_property` registrations; correlate by `request_id`,
   never by message order; treat `playback-restart` as "seek done". (Reports 05, 03)

10. **Waveform display: build in-tree (Canvas + Marker::Braille min/max envelope; Sparkline stopgap); share the peaks cache with post-MVP DSP cut refinement.**
    No maintained terminal waveform widget exists (verified by search).
    0.30.1's filled-area Canvas rendering directly supports filled envelopes,
    and the per-zoom min/max/RMS bins are the same data the cut-refinement DSP
    needs. (Report 05)

## Contradictions & risks

- **WhisperX cannot deliver the "phoneme timestamps" the project context
  assumes.** Report 04 verified its output is word + character level only.
  The corpus's phone tier comes from MFA (or the CTC fallback) — this is the
  largest correction to the original plan.
- **MFA is conda-only in practice** (Kaldi/pynini binaries) — not uv/pip-clean,
  so ingest carries a micromamba env. Report 04 did not personally smoke-test
  arm64 MFA on an M4; flagged for an early spike. Report 01 shows an MFA
  version pin is exactly what killed sentence-mixing — the subprocess boundary
  + fallback aligner (decision 2) is the mitigation.
- **mpv EDL v0 is explicitly unfrozen** and the `loadfile` signature changed in
  0.38.0. Mitigation ratified in decision 5 (version probe, single serializer
  module, golden tests). `memory://` + EDL was NOT verified — don't rely on it.
- **mpv audition latency on sub-second diphone units is unmeasured** (report 05).
  If seek-to-play round trips feel bad, the fallback is rodio fed pre-decoded
  PCM — measure before adding any audio crate.
- **Pinned-dependency rot is the proven killer** in this niche (sentence-mixing:
  MFA binary pin, yt-dlp 2022.4.8, CLI/library version skew; videogrep aging
  more gracefully but moviepy-damaged). dipho's counter-policy: unpinned
  yt-dlp, subprocess boundaries around fragile tools, aligner-agnostic schema.
- **mlx-whisper word_timestamps has a reported memory-growth issue on long
  audio** (mlx-examples#1254) — minor, since word times come from WhisperX
  align anyway; don't enable word_timestamps in mlx-whisper.
- **pyannote community-1 is HF-gated**: ingest requires an HF token and
  one-time license acceptance — document as a setup prerequisite.

## Changes to DESIGN.md

- Replace "WhisperX transcription + forced alignment (word AND phoneme
  timestamps)" with the four-stage pipeline: mlx-whisper (transcribe, Metal) →
  WhisperX `align()` (word timestamps) → MFA 3.3.9 subprocess on per-segment
  chunks (phone tier → diphones at phone midpoints) → pyannote 4.x community-1
  on MPS (diarization, run directly, HF token). WhisperX does not emit phonemes.
- Add the fallback aligner path (phonemizer/espeak → wav2vec2-espeak CTC →
  torchaudio `forced_align`) and an aligner-agnostic phones schema:
  `aligner_id` + per-phone confidence columns.
- Specify Python sidecar environment strategy: WhisperX env and MFA
  micromamba env isolated; MFA invoked only as CLI subprocess; yt-dlp unpinned.
- Specify the EDL compiler contract: single Rust module, two targets (`.mpv.edl`
  file + `edl://` URI), unconditional `%<bytes>%` path quoting (UTF-8 byte
  count), named params (`start=`, `length=`), explicit `{:.6f}` floats,
  per-segment `title=`, golden-file tests.
- Add EDL compiler semantics from videogrep: symmetric padding → clamp at 0 →
  merge overlapping/touching spans per source file → clamp end to source
  duration, applied before BOTH preview and render targets.
- Specify mpv slave lifecycle: spawn flags (`--idle=yes --keep-open=yes
  --no-terminal --input-ipc-server=$TMPDIR/dipho-mpv.sock`), short 0600 socket,
  startup version probe (gate `loadfile` 4th-arg usage on ≥0.38), one
  persistent IPC connection, request_id correlation, `playback-restart` =
  seek-done, ab-loop-a/b properties for unit audition (clear with `"no"`).
- Note `!new_stream` as the post-MVP audio-dub mechanism; explicitly mark
  `!delay_open`/`!no_clip`/`!mp4_dash` and `memory://` as out of scope.
- Add solver section (post-MVP): 3-step cost decomposition with
  sentence-mixing's weight ratios as tuning seeds; zero join cost for
  source-adjacent diphones + contiguous-span bonus; phoneme substitution
  matrix for candidate generation (no hard OOV failures); beam search over a
  per-position candidate lattice (join cost depends only on last vowel +
  4-unit RMS suffix + contiguity); seeded noise for alternative rankings;
  punctuation-as-pause `<BLANK>` targets scored by silence-RMS + duration
  window; videogrep-style `mash` as the random baseline.
- Add TUI architecture section: tokio Elm-style single loop, module layout
  (app/event/ui/mpv/db/edl), one mpsc Event channel, dirty-flag render with
  ~10 ms debounce; crate list (adopt now / later / skip); in-tree braille
  waveform widget with shared peaks cache.
- Add EDL export targets roadmap: MVP = mpv EDL + ffmpeg render; post-MVP =
  output-timeline VTT, FCP7 XML, m3u.
- Add risk register: mpv EDL v0 unfrozen; MFA conda/arm64 smoke test pending;
  mpv audition latency unmeasured (rodio fallback); pyannote HF gating;
  dependency-rot policy.

## Open questions for the owner

1. **Conda tolerance:** is a micromamba env inside ingest acceptable for MVP,
   or should dipho start on the pure-pip CTC fallback aligner (softer
   boundaries) and adopt MFA later?
2. **mpv version floor:** pin/bundle a known mpv version (homebrew formula
   dependency?) or support a range with the startup probe?
3. **English-only MVP?** MFA `english_mfa` model set vs ARPAbet
   (`english_us_arpa`, CMUdict-compatible) affects the canonical phone set in
   the schema — pick one before the phones table lands.
4. **HF account/token as a hard ingest prerequisite** (pyannote gating):
   acceptable, or should diarization be optional at ingest?
5. **Latency budget for audition:** what's "good enough" for sub-second unit
   preview via mpv before investing in a rodio PCM path?
