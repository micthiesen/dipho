# Roadmap to MVP

Small, verifiable increments. Each milestone ends with something runnable and a green `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`. Decisions referenced here are in docs/research/SUMMARY.md (pending ratification).

## M1 — Corpus schema + loader

Schema migration v1 in `dipho-core::corpus`: sources, words (+FTS5), phonemes (aligner-agnostic: `aligner_id`, confidence), diphones (+ n-gram table), prosody, speakers. A loader that ingests the sidecar's JSON contract (fixture-driven — no real ML yet) and derives diphones from adjacent phone midpoints.

**Verify:** `cargo test` round-trips a fixture JSON into an in-memory corpus; FTS5 finds a word; diphone rows have midpoint boundaries.

## M2 — Ingest pipeline (sidecar for real)

`dipho ingest <url|file>`: yt-dlp/ffmpeg demux in Rust, then the sidecar with the real stack: mlx-whisper → WhisperX align → MFA subprocess → pyannote → librosa. Starts with the **MFA arm64 smoke-test spike** (risk register) — if MFA fights back, ship M2 on the CTC fallback aligner and revisit.

**Verify:** ingest a short real video end to end; spot-check word timestamps in the DB against the audio.

## M3 — Word search (CLI first, then TUI)

`dipho search <query>` against FTS5, then the TUI shell grows the Elm-style event loop (tokio, single mpsc) with a search input (tui-input) + results list.

**Verify:** type a word, see every utterance with source + timestamps.

## M4 — Audition via mpv

Hand-rolled IPC client (UnixStream, request_id correlation, event task), mpv slave lifecycle (spawn flags, version probe, 0600 socket). Select a search hit → exact seek + ab-loop of that span.

**Verify:** arrow through search results, hear each one in the mpv window with <~200 ms perceived latency (measure — informs the rodio question).

## M5 — Flat EDL + preview

EDL append/reorder/remove from the TUI; save/load the edit file (JSON). EDL compiler v1: pad-then-merge semantics, `%<bytes>%` quoting, named params, golden-file tests; recompile to `edl://` and `loadfile ... replace` on every change.

**Verify:** build a 5-clip mix from search hits and play it gaplessly in mpv without temp files; golden tests pin the EDL output.

## M6 — Render (MVP complete)

`dipho render edit.json out.mp4`: ffmpeg filter-graph/concat compilation of the same padded-merged span list. Re-encode correctness over speed.

**Verify:** rendered file matches the preview cut-for-cut; plays in QuickTime.

## Post-MVP (rough order)

Transforms beyond cut/concat (stutter, loop, reverse, pitch, speed) in preview-degraded + render-faithful pairs → in-tree waveform widget (braille Canvas, shared peaks cache) → diphone assembly search → the solver (beam search, contiguity bonus, substitution matrix; `mash` as baseline) → DSP cut refinement (zero-crossing snap, then spectral flux) → exports (VTT, FCP7 XML) → `!new_stream` audio-dub channel decoupling in mpv preview.
