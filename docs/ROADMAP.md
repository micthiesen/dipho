# Roadmap to MVP

Small, verifiable increments. Each milestone ends with something runnable and a green `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`. Decisions in docs/research/SUMMARY.md were ratified 2026-06-10 with one veto: decision 2 (fallback aligner / `aligner_id`) is superseded — MFA is the sole aligner. DESIGN.md (revised after adversarial review round 1) is canonical.

## M1 — Corpus schema + loader

Schema migration v1 in `dipho-core::corpus` per DESIGN.md: sources (content_hash, master path), ingest_runs (tools provenance), utterances (+FTS5 content table), words, phones (stress-marked, `cut_t`, `weak_cut`), diphones (`seq` ordinal, stress-stripped label, boundary-feature columns), prosody_frames, speakers. WAL + busy_timeout + foreign_keys pragmas. A loader that consumes a fixture `manifest.json` (no real ML) and derives: utterances→words→phones with SIL/NOISE handling, per-class cut points, diphones under the adjacency rule, per-unit prosody aggregates + head/tail boundary features from fixture frames.

**Verify:** fixture round-trip into in-memory corpus; FTS5 phrase query maps back to word spans; diphone rows have class-correct cut points; the adjacency fixture (silence, speaker turn, chunk edge, single-phone word) yields exactly the expected SIL-X/X-SIL units and nothing across boundaries; a repeated `seq+1` self-join finds a contiguous run.

## M2 — Ingest pipeline (sidecar for real)

Starts with the **MFA arm64 smoke-test spike** (risk register) so failure surfaces in the first hour — MFA is the sole aligner; if it's genuinely unworkable we reassess the design rather than silently shipping softer boundaries.

`dipho ingest <url|file>`: yt-dlp → playback master + analysis wav (one ffmpeg invocation, start_time ≈ 0 asserted) → sidecar stages (mlx-whisper → WhisperX align → text normalization + `mfa g2p` OOV handling → `mfa align` per padded chunk → pyannote turns → pyin/RMS prosody npz) → manifest → loader. NDJSON progress to the CLI. Content-hash idempotency.

**Verify:** ingest a short real video end to end; spot-check word timestamps against audio (excluding reduced-confidence chunk-edge phones); **timebase integration test** — synthetic video with a beep at a known time and a deliberate 500 ms container offset: wav-time == mpv `time-pos` == rendered beep position within one video frame.

## M3 — Word search (CLI first, then TUI)

`dipho search <query>` over utterance FTS5, then the TUI shell grows the Elm-style event loop (tokio, single mpsc) with a search input (tui-input) and a results list showing each hit highlighted inside its utterance, with speaker and confidence columns.

**Verify:** word and phrase queries return every utterance with exact word spans.

## M4 — Audition via mpv

Hand-rolled IPC client (UnixStream, request_id correlation, event task), slave lifecycle (spawn flags, version probe ≥ 0.38, 0600 socket), `PlayerMode::Audition`. Three playback keys per hit: loop-exact (ab-loop), play-with-context (±500 ms), play-full-utterance.

**Verify:** arrow through hits and hear each within the latency budget — **measure the seek round-trip here**; this answers the open rodio question.

## M5 — Flat EDL + preview

Edit model: clips with owned t_start/t_end + provenance, append/reorder/remove, **trim/nudge (±5 ms / ±25 ms with instant recompile + neighborhood replay)**, undo/redo stack, autosave + recovery; save/load with the `sources` manifest. EDL compiler v1: verbatim in-order compilation, join-elision only, `%<bytes>%` quoting, named params, `title=` labels; `edl://` reload preserving output-position (`PlayerMode::Preview`).

**Verify:** golden-file tests (incl. repeated-span stutter fixture compiling to N segments and a join-elision case); build a 40-clip mix, preview gaplessly; **measured gate: 100 × 200 ms segments play without frame drops or audio gaps** (mpv stats) on the M4.

## M6 — Render (MVP complete)

`dipho render edit.json out.mp4`: two-stage ffmpeg (per-clip accurate-seek extraction to uniform intermediates → concat demuxer final encode), output profile (largest source's resolution/fps, 48 kHz stereo, `--size`/`--fps` overrides).

**Verify:** render matches preview cut-for-cut; audio cut positions match the EDL exactly; A/V sync error < half a frame at every boundary and at the end; two-source mismatched-resolution/sample-rate edit renders correctly; plays in QuickTime.

## Post-MVP (rough order)

Transforms beyond cut/concat (Stutter/Loop native in preview; Pitch/Speed/Reverse render-only with TUI badges) → in-tree waveform widget (braille Canvas, peaks cache shared with DSP) → diphone assembly search → the solver (beam over candidate lattice, contiguity bonus via `seq`, substitution matrix, boundary-feature join cost; `mash` baseline) → DSP cut refinement (zero-crossing snap then spectral flux, updating `cut_t`) → channel lanes (`!new_stream` audio dubbing) → exports (output-timeline VTT, FCP7 XML, m3u).
