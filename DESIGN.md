# dipho — Design

> Canonical design doc. The architecture summary in CLAUDE.md is derived from this; when they disagree, this wins. Revised 2026-06-10 after the research pass (docs/research/SUMMARY.md) and round 1 of adversarial review.

## Thesis

Sentence mixing is **unit-selection speech synthesis** (the Hunt & Black 1996 lineage) where the unit database is arbitrary source media instead of a studio-recorded voice corpus. Classical unit selection picks units from a database to match a target utterance, minimizing *target cost* (does this unit sound like what we want?) plus *join cost* (do adjacent units splice cleanly?). YTP sentence mixing is exactly this problem with a found-footage database — so we build the tool around that framing instead of around a video editor's timeline.

Two core abstractions follow: **the Corpus** (the unit database) and **the Edit** (the selected, transformed unit sequence). Between them, later, sits a **solver**.

## Abstraction 1: The Corpus

An addressable phonetic index over **immutable sources**. Sources are never edited, only indexed. Everything downstream is a span reference:

```
(source_id, t_start, t_end, channel)    where channel ∈ {audio, video, both}
```

Audio and video are decoupled as first-class: a mix routinely takes the audio of one span over the video of another, holds a freeze-frame under continuing speech, or loops video under a stutter. Making `channel` part of the span reference — rather than an edit-time afterthought — keeps that representable everywhere.

### Timebase: the playback master

There is exactly one clock. At ingest, every source (download or local file) is normalized into a local immutable **playback master**: MKV, timestamps rebased to zero (`-avoid_negative_ts make_zero`), seek-friendly video (H.264 all-intra or GOP ≤ 12), lossless audio (FLAC). The 16 kHz mono analysis wav is extracted **from the master in the same ffmpeg invocation**. Corpus timestamps are defined as *seconds in the playback master's audio stream* — true by construction for analysis, preview, and render alike, because every consumer addresses the master:

- WhisperX/MFA/pyannote/prosody all run on the analysis wav extracted from the master
- the mpv EDL compiler targets the master (its short GOPs make hundred-segment sub-second EDLs seekable)
- the ffmpeg render reads the master (lossless audio, so audio-master timing is preserved)
- the raw download is kept only as provenance; nothing seeks it

Ingest ffprobes the master and **fails if any stream's start_time is not ≈ 0**. The `sources` table records both paths (original, master) and per-stream start offsets as an assertion trail. Integration test (M2): synthesize a video with a beep at a known time and a deliberate 500 ms container offset; ingest; assert wav-time == mpv `time-pos` == rendered beep position within one video frame.

### Ingest pipeline

> WhisperX's English alignment models are letter-CTC — word/character timestamps only, **never phonemes** (research finding). The phone tier comes from MFA, the sole aligner (ratified — no fallback backend; cut-point precision is the product).

```
yt-dlp URL or local file
  → ffmpeg: normalize to playback master + extract analysis wav
            (mono 16 kHz pcm_s16le, no seek/trim — wav t=0 == master t=0)
  → mlx-whisper (large-v3-turbo, Metal): transcription
  → WhisperX align(): word-level timestamps + segment tier
  → text normalization: expand digits/symbols to words (num2words), strip
    punctuation deterministically; the sidecar keeps the normalized-token ↔
    WhisperX-word mapping (word_index always refers to this mapping —
    never a positional zip of two tokenizers)
  → mfa g2p (english_us_arpa G2P) on the OOV list → augmented dictionary
    (no <spn> holes from out-of-vocabulary words, digits, names)
  → mfa align (english_us_arpa), per-chunk → phone tier
  → pyannote 4.x community-1 (MPS, HF token): diarization → speaker turns
  → prosody: pyin f0 + RMS on the analysis wav
  → staged work dir + manifest (below); the Rust loader writes SQLite and
    derives diphones (derivation is loader logic, not sidecar logic)
```

**MFA chunking semantics.** MFA runs per WhisperX segment. Segments whose inter-gap is under 300 ms are merged before chunking; each chunk is padded 250 ms on both sides (clamped to neighbors' midpoints when segments abut). The sidecar parses each chunk's TextGrid and rebases all times by the chunk offset before emitting — **every timestamp in the contract is master-relative**, rebasing is owned by the sidecar. Phones lying within 100 ms of a chunk edge get reduced confidence so the solver deprioritizes them.

**Diarization.** The sidecar emits raw speaker turns `[{speaker, start, end}]` as the source of truth. Word/phone/diphone speaker labels are *derived* by the loader: maximal temporal overlap (ties → earlier turn; zero overlap → null).

**Prosody.** pyin on the analysis wav: hop 160 samples (10 ms), frame_length 1024, fmin 50 Hz, fmax 450 Hz; RMS at the same hop, stored in dB. Frame i is centered at t = i·hop (librosa `center=True`); f0 and rms arrays must both have length `1 + floor(duration/hop)` — the loader rejects violations. Frames ship as binary npz, not JSON. Per-unit aggregation (median voiced f0, voiced fraction, f0 slope, RMS mean, and the head/tail boundary features below) happens in the Rust loader — re-deriving units never re-runs Python.

**Staged, resumable work dir.** Ingest is not one fragile document. The sidecar writes `<corpus_dir>/ingest/<content_hash>/`: `transcript.json` → `words.json` → `phones.json` → `diarization.json` → `prosody.npz`, each fsynced on stage completion, plus a final `manifest.json` referencing the stage files (the contract, minus bulk data). Re-running skips stages whose outputs exist and validate — a pyannote crash 90 minutes in costs one stage, not the run. stdout is NDJSON progress events (`{"stage": "diarize", "pct": 40}`, terminal `{"error": {stage, message}}`) feeding the TUI's `Event::Job`. Full contract in `python/README.md`.

**Environment strategy** — learned from why sentence-mixing died (pinned MFA 1.1.0-beta binary, yt-dlp 2022): fragile tools live behind subprocess boundaries (MFA is CLI-only in its own micromamba env; WhisperX's heavily-pinned deps isolated), yt-dlp stays unpinned, and pyannote's HF token + one-time license acceptance is a documented hard setup prerequisite (diarization is not optional).

### Why diphones, not phonemes

Coarticulation means phoneme boundaries are the *worst* place to cut: the signal there is a transition smeared across both neighbors. The stable, cuttable points are inside phones — the steady state of a vowel or fricative. A **diphone** (cut-point to cut-point, spanning one phoneme transition) puts its boundaries on those stable points. Word- and phrase-level search sits on top for the common case; diphone assembly serves sub-word synthesis ("build 'mama' from someone who never said it").

**Cut points are per-phone-class, stored explicitly** (`phones.cut_t`), not recomputed:

| Phone class | Cut point |
|---|---|
| vowels, fricatives, nasals, liquids | temporal midpoint |
| stops, affricates | 20% into the phone (inside the closure, before any plausible burst) |
| diphthongs | midpoint, flagged `weak_cut` (solver penalizes joins there) |

Post-MVP DSP refinement updates `cut_t` in place (zero-crossing snap, then spectral flux) without re-deriving the diphone table.

**Silence is a first-class phone.** MFA's silence intervals become `SIL` phone rows; MFA's `<spn>` becomes `NOISE`, which never participates in diphones. A `SIL` row is also inserted at every chunk boundary and every speaker-turn boundary. The SIL-side extent of a SIL-adjacent diphone is clamped to min(half the silence, 200 ms) — no units carrying a second of dead air.

**Adjacency rule.** Two phones are adjacent — and yield a diphone — iff they are consecutive in master time within one utterance, neither is `NOISE`, and the gap between A's end and B's start is ≤ 20 ms. Silence rows, chunk edges, and speaker turns terminate adjacency: no diphone ever spans two speakers or a pause. This makes utterance edges and single-phone words addressable (`SIL-AH`, `AH-SIL`) — without SIL diphones, half of every utterance-initial and -final phone would be unreachable and the solver could never start or end a sentence.

**Stress.** `phones.label` keeps the full stress-marked ARPAbet exactly as MFA emits it (`AA1`). `diphones.label` is stress-stripped (`AA-K`) — the canonical match key — with `stress_a`/`stress_b` (0/1/2/NULL) alongside; target cost adds a stress-mismatch penalty, and the substitution matrix is defined over stripped labels.

### Storage: SQLite

One SQLite database per corpus (rusqlite, bundled). Migration v1 sets `PRAGMA journal_mode=WAL` (persistent); every connection sets `busy_timeout=5000` and `foreign_keys=ON`. Discipline: one write connection per process; ingest loads each source in a single transaction (readers see it appear atomically); TUI connections are `SQLITE_OPEN_READ_ONLY` except one dedicated writer for user data. WAL means the corpus must live on a local filesystem — fine for a single-machine tool.

- `sources` — id, origin (URL/path), original path, **master path**, duration, per-stream start offsets, **content_hash (SHA-256, UNIQUE)** — re-ingest of a known hash is a no-op without `--force`
- `ingest_runs` — id, source_id, started/finished, status, schema_version, `tools` JSON (versions of mlx-whisper, whisperx, MFA + model names, pyannote, dipho-ingest — emitted by the sidecar in the manifest). Every derived row carries `ingest_run_id`. Re-ingest = one transaction: delete the source's derived rows, insert the new run's. `dipho reingest --stale` re-runs sources whose recorded tool versions differ from the current environment.
- `utterances` — span ref, full text, speaker, ASR confidence; populated from WhisperX segments. **This is the FTS5 document unit** (`fts5(text, content='utterances')`) — phrase search works because the phrase lives in one row; hits map back to word spans via per-word ordinals. It is also what the TUI shows around a hit.
- `words` — span ref, text, `utterance_id` FK, `word_ordinal`, speaker (derived from turns), confidence
- `phones` — span ref, stress-marked label, `cut_t`, `weak_cut`, confidence (nullable), word FK
- `diphones` — id, source_id, **`seq`** (per-source ordinal), stress-stripped label, stress_a, stress_b, t_start, t_end (= cut_t of A and B), phone_a/phone_b FKs, speaker, and **join-cost boundary features**: `mfcc_head`/`mfcc_tail` (13-dim BLOBs; 25 ms window, 10 ms hop, mean of first/last 3 in-unit frames), `f0_head`/`f0_tail` (median of first/last 3 voiced frames, NULL if unvoiced), `rms_head_db`/`rms_tail_db`, plus per-unit summaries (median f0, voiced fraction, f0 slope, RMS mean) for target cost. Join cost is then a pure SQLite read — no audio decode in the search loop.
  - **There is no n-gram table.** Source-adjacency is `seq_b = seq_a + 1` — a self-join on `(source_id, seq)`; any n-gram query is n−1 such joins, and the solver's per-position candidate fetch is an index hit on `label`. Contiguity (the zero-join-cost case) falls out of `seq` arithmetic.
- `prosody_frames` — source_id, hop, f0 BLOB, rms BLOB (float32, ~6 MB/hour): unit redefinition and future join-cost changes re-derive from frames without re-running ingest
- `speakers` — id, source_id, label, optional human-assigned name

Schema rule (kept, now honest): **post-MVP features (solver, join cost) must be servable from this schema without rework** — which is precisely why boundary MFCCs, `cut_t`, `seq`, and frame persistence ship in v1.

## Abstraction 2: The Edit

A **program, not a timeline**. Non-destructive EDL-as-data: an ordered list of clips plus transforms, serialized as JSON.

**A clip owns its own `t_start`/`t_end`** — a mutable copy taken at append time — plus an optional provenance reference (the corpus hit it came from). The corpus stays immutable; trimming and nudging a clip edits the clip, and provenance lets later features re-derive context. This is load-bearing UX: without nudge, every slightly-off alignment is fatal until DSP refinement exists.

**Project binding.** The corpus lives at `./.dipho/corpus.db` (project = working directory; `--corpus` overrides). An edit file carries a mandatory `sources` manifest: every referenced source_id → {origin, master filename, duration, content_hash}. Preview/render resolve media through the manifest against the corpus, relinking by hash when paths moved; a shared edit file is self-describing — the recipient re-ingests the listed origins and dipho rebinds by hash.

**Channels.** The data model keeps `channel` on every span. The full model is two parallel lanes (audio, video) sharing one output clock — `both` appends to both lanes; a single-lane span requires the other lane be explicitly filled (span, freeze, or silence) so lane durations always match. **MVP compiles `Channel::Both` only**: both compilers return a typed `ChannelUnsupported { clip_index }` error otherwise (surfaced in the TUI — no silent coercion). Post-MVP, audio-lane spans compile via `!new_stream` (mpv) and stream-mapped graphs (ffmpeg).

### Transform semantics

| Transform | Meaning | Render (ffmpeg) | Preview (mpv EDL) |
|---|---|---|---|
| `Loop { count }` | whole clip (A+V) plays `count` times (≥1) | repeated trim segments | native: repeated EDL segments (full fidelity) |
| `Stutter { repeats, slice }` | first `slice` seconds repeated `repeats` times, then the full clip once (classic YTP stutter) | repeated trim segments | native: repeated EDL segments |
| `Pitch { semitones }` | audio-only, duration-preserving; video untouched | rubberband (fallback asetrate+atempo) | render-only: untransformed span plays, clip badged |
| `Speed { factor }` | A+V time-scale, pitch-preserving | setpts + atempo chain | render-only, badged |
| `Reverse` | both streams | reverse + areverse (buffers whole clip — fine for short clips) | render-only, badged |

Chipmunk = `Speed` + `Pitch` composed. Preview fidelity is per-transform and *decided*: Loop/Stutter are exact in preview; Reverse/Pitch/Speed are render-only and the TUI badges those clips.

### Compilation

Both compilers are pure functions in `dipho-core::edl` taking a resolver, keeping the core I/O-free:

```rust
compile_mpv_edl(&Edl, &SourceMap) -> Result<String, EdlCompileError>
compile_ffmpeg(&Edl, &SourceMap) -> Result<FfmpegPlan, EdlCompileError>
// SourceMap: source_id -> master path (built from the corpus by the caller)
// unresolved id, channel != Both, bad spans => typed errors
// FfmpegPlan: an ordered list of complete ffmpeg invocations
```

**Clips compile verbatim, in edit order, never merged, pad = 0.** (Round-1 review: the earlier "pad-then-merge before both targets" rule would have collapsed stutters into one segment and reordered edits — supercut semantics, not sentence-mixing semantics.) The only merge permitted is *join elision*: consecutive entries sharing (source, channel), with identical transform lists, whose spans are source-contiguous forward (next.t_start within ~1 ms of prev.t_end) may compile to one segment. Identical repeated spans never merge. Context padding exists only in audition playback, never in compilation. Golden test: a repeated span compiles to N segments, not 1.

1. **mpv EDL (preview)** — recompiled to an `edl://` URI on every change, sent to the long-lived slave via `["loadfile", uri, "replace"]`; no temp files. `.mpv.edl` file export is the same compiler. Every value quoted unconditionally with the spec's `%<utf-8-byte-count>%` form; named params (`start=`, `length=`); floats formatted explicitly; per-segment `title=` (word label — mpv's implicit chapter-per-segment gives free cut navigation). mpv EDL v0 is unfrozen → startup version probe (floor ≥ 0.38, also gates the `loadfile` 4th arg). Golden-file tests.
2. **ffmpeg render (export)** — **two-stage**: stage 1 extracts each clip with accurate seek (`-ss` before `-i`, then `trim`/`atrim` + `setpts=PTS-STARTPTS`/`asetpts`) and applies that clip's transform chain, re-encoding to a uniform intermediate (project fps/SAR/yuv420p, 48 kHz stereo, intra-friendly codec); stage 2 concatenates intermediates (concat demuxer) into the final encode. Never stream-copy cuts. A single-process `filter_complex` path is permitted as an optimization for small edits only if golden-tested timing-identical. **Output profile**: resolution/fps of the largest-area source in the edit (others scaled/padded), audio 48 kHz stereo via aresample; overridable (`dipho render --size --fps`). Integration test: two-source edit with mismatched resolution and sample rate.

**Frame quantization: audio is master.** Audio cuts are sample-accurate and authoritative. Per segment, the video frame count is round-to-nearest(audio duration × fps) with a running error accumulator (telecine-style error diffusion) so |cumulative video − cumulative audio| < half a frame across the edit; a segment's first frame is the one whose PTS interval contains the audio cut time. mpv preview may differ by ≤ 1 frame per boundary — acceptable. M6 verifies: waveform cut positions match the EDL exactly; A/V error < half a frame at every boundary and at the end.

### mpv slave lifecycle

Spawn once: `mpv --idle=yes --keep-open=yes --no-terminal --input-ipc-server=$TMPDIR/dipho-mpv.sock` (short path — macOS `sun_path` ~104 bytes; socket 0600, IPC exposes `run`). One persistent JSON IPC connection for the session (closing drops `observe_property` registrations). Correlate by `request_id`, never message order; `playback-restart` = seek-done. Unit audition = exact seek + `ab-loop-a`/`ab-loop-b` (clear with `"no"`).

**Player modes.** App state carries `PlayerMode { Audition(hit), Preview(output_pos) }` — one slave, no contention ambiguity. Audition offers three keys: loop-exact, play-with-context (±500 ms, no loop), play-full-utterance. In Preview, dipho owns the compiled segment durations, so on recompile it computes the current output-time, reloads, and seeks back to the equivalent position (clamped if the edit changed under the playhead), clearing any ab-loop.

## The Solver (post-MVP)

Type a target sentence → ranked candidate assemblies. Classical unit-selection search:

- **Target cost** — phone-sequence match (via the substitution matrix — no hard OOV failures), stress mismatch penalty, speaker constraint, duration plausibility, alignment confidence, `weak_cut` penalty
- **Join cost** — splice quality at each boundary, read straight from the diphone boundary columns: Euclidean MFCC distance + |Δlog f0| + |Δ dB|. Explicitly **not** humor scoring; the human picks the funny one from a shortlist of clean ones.

Beam search over a per-position candidate lattice (join cost depends only on a short suffix — last unit's tail features — keeping state small). Source-adjacent diphones (`seq + 1`) get zero join cost plus a contiguous-run bonus — long natural runs beat technically-clean Frankenstein joins (sentence-mixing's best idea). Their weight ratios (amplitude ≫ spectral; contiguity bonus per phone; per-class duration caps) seed tuning. Punctuation becomes `SIL` targets scored against real silence units. Seeded noise diversifies alternative rankings. videogrep-style random `mash` is the baseline the solver must demonstrably beat. UX: chunk-by-chunk authoring, rank-ordered scored candidates, stash buffer, re-edit, accept-and-advance, autosaved sessions.

## DSP cut refinement (post-MVP, native)

Aligner timestamps are ±tens of ms. Two passes in the Rust hot loop (symphonia decode of the master's FLAC, rustfft), both updating `phones.cut_t` in place within the aligner's tolerance window: (1) snap to nearest zero-crossing — eliminates clicks; (2) minimize spectral flux at the boundary — the local version of join cost. The waveform widget's peaks cache shares this decode path.

## Architecture: three processes

| Process | Role | Why separate |
|---|---|---|
| **dipho** (Rust) | index, search, EDL, DSP, ratatui TUI, mpv control | the interactive loop; everything latency-sensitive |
| **mpv** | slave player: preview/audition in its own window | best playback engine; JSON IPC; never render video in-terminal |
| **Python sidecar** | batch ingest (staged work dir, NDJSON progress) | ML stack is Python-native; offline; crash/version isolation |

Workspace: `crates/dipho-core` (library: spans, corpus, EDL + compilers, DSP — no TUI, no process management), `crates/dipho` (binary: clap CLI, ratatui TUI, mpv IPC client), `python/` (uv project, `dipho-ingest`).

### TUI architecture

Elm-flavored single event loop on tokio (the convergent pattern across television, atuin, gitui, yazi): one `App` state struct, one merged `Event` enum (`Term`, `Tick`, `Mpv`, `Db`, `Job`), all producers into one mpsc, single consumer loop, render-on-dirty (~10 ms debounce). Modules: `app.rs` (state + update), `event.rs`, `ui/`, `mpv/`, `db/` (rusqlite on `spawn_blocking`, read-only pool + one writer).

**Every EDL mutation is a reversible message**: bounded undo/redo stack (inverse ops or snapshots — EDL-as-data makes this nearly free), and the edit autosaves to `<edit>.json.autosave` after every mutation with recovery on open. Undo scope is the EDL only.

mpv IPC client is hand-rolled (~200 lines: UnixStream + request_id→oneshot map + events→mpsc). Crates: now — ratatui, tokio, crossterm (event-stream), tui-input, serde_json, rusqlite; later — nucleo, ratatui-textarea, rodio (only if audition latency disappoints); skip — mpvipc, kira. Waveform widget in-tree (Canvas + braille min/max envelope; Sparkline stopgap); its peaks cache is shared with DSP refinement.

## MVP: one vertical slice

1. **Ingest** — `dipho ingest <url|file>`: master + analysis wav, sidecar stages, load into corpus
2. **Index** — schema v1 as above
3. **Word search** — FTS5 over utterances; hits shown in utterance context with speaker + confidence
4. **Audition** — mpv ab-loop / context / full-utterance playback per hit
5. **Flat EDL** — append, reorder, **trim/nudge (±5 ms fine, ±25 ms coarse, instant recompile + neighborhood replay)**, undo/redo, autosave; save/load; `edl://` preview
6. **Render** — `dipho render edit.json out.mp4`, two-stage ffmpeg

Post-MVP, rough order: transforms beyond cut/concat → waveform widget → diphone assembly search → the solver → DSP cut refinement → channel lanes (`!new_stream` dubbing) → exports (output-timeline VTT, FCP7 XML, m3u).

## Risk register

- **mpv EDL v0 unfrozen** — version probe, single serializer, golden tests
- **MFA conda-only, arm64-on-M4 unverified** — first task of M2 is the smoke test; MFA is behind a subprocess boundary (swappable without schema changes). MFA is the sole aligner: if it's truly unworkable we reassess the design, not silently degrade.
- **Hundred-segment sub-second EDL preview is unproven** — the all-intra master is the mitigation; M5 has a measured gate: 100 × 200 ms segments must play without drops/gaps (mpv frame-drop + cache stats) before M5 is done
- **mpv audition latency unmeasured** — measure in M4; rodio + pre-decoded PCM is the fallback if it disappoints
- **pyannote HF-gated** — token + license acceptance is a hard documented setup step
- **Dependency rot killed the prior art** — yt-dlp unpinned, subprocess boundaries, staged re-runnable ingest, `tools` provenance per run
- mlx-whisper `word_timestamps` memory growth on long audio — don't enable it; word times come from WhisperX align

## Open questions

Ratified 2026-06-10: MFA sole aligner; ARPAbet (`english_us_arpa`); HF token hard prerequisite; mpv ≥ 0.38 probe; playback-master timebase; audio-master frame quantization; two-stage render; verbatim compilation (no pad-then-merge); SIL/NOISE phone model; per-class cut points; boundary MFCCs in schema v1; no n-gram table (`seq` adjacency); utterance tier + FTS5; staged ingest with provenance; WAL.

Still open:

- Audition latency budget via mpv before investing in a rodio PCM path (measure in M4)
- Exact MFCC config sanity-check against real joins once M2 produces corpora (parameters above are standard, not yet validated on found footage)
- Stutter `slice` default (fixed 60 ms? first-phone length? decide when the transform lands, post-MVP)
