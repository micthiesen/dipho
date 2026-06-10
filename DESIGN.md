# dipho — Design

> Canonical design doc. The architecture summary in CLAUDE.md is derived from this; when they disagree, this wins. Revised 2026-06-10 after the research pass (docs/research/SUMMARY.md) and two rounds of adversarial review.

## Thesis

Sentence mixing is **unit-selection speech synthesis** (the Hunt & Black 1996 lineage) where the unit database is arbitrary source media instead of a studio-recorded voice corpus. Classical unit selection picks units from a database to match a target utterance, minimizing *target cost* (does this unit sound like what we want?) plus *join cost* (do adjacent units splice cleanly?). YTP sentence mixing is exactly this problem with a found-footage database — so we build the tool around that framing instead of around a video editor's timeline.

Two core abstractions follow: **the Corpus** (the unit database) and **the Edit** (the selected, transformed unit sequence). Between them, later, sits a **solver**.

## Abstraction 1: The Corpus

An addressable phonetic index over **immutable sources**. Sources are never edited, only indexed. Everything downstream is a span reference:

```
(source_id, t_start, t_end, channel)    where channel ∈ {audio, video, both}
```

Audio and video are decoupled as first-class: a mix routinely takes the audio of one span over the video of another, holds a freeze-frame under continuing speech, or loops video under a stutter. Making `channel` part of the span reference — rather than an edit-time afterthought — keeps that representable everywhere.

### Source identity

Two identifiers with distinct jobs (never conflated):

- **`origin_id`** (UNIQUE) — the *pre-download idempotency key*: for URLs, the normalized yt-dlp extractor + video id (extractor key lowercased, video id verbatim — YouTube ids are case-sensitive); for local files, SHA-256 of the original file. Checked before any bytes move; ingest of a known origin_id is a no-op without `--force`. Also keys the ingest work dir, so it exists from the first byte of a download. (Hashing downloaded bytes can't provide idempotency — a re-download of the same video yields different bytes.)
- **`master_hash`** — SHA-256 of the playback master, computed exactly once when the master is created, never recomputed. Serves local relink (rehash candidate files when paths moved) and edit-file integrity on the same machine. Cross-machine it never matches (masters are local re-encodes), so cross-machine rebind uses origin_id.

### Timebase: the playback master

There is exactly one clock. At ingest, every source is normalized into a local immutable **playback master**, and the analysis wav is extracted **in the same ffmpeg invocation** so both share one audio stream by construction:

- **Audio**: one shared filter chain `aresample=async=1:first_pts=0` (fills timestamp gaps with silence, trims overlaps — the stream is contiguous from 0 as an *invariant*, not an assumption) feeds both the FLAC encode (master) and the 16 kHz mono pcm_s16le wav (analysis). Corpus timestamps are defined as *seconds in this contiguous audio stream*.
- **Video**: rotation baked in (decode with autorotation, strip side-data), square pixels, CFR forced via the `fps` filter at the source's average frame rate rounded to the nearest standard rate — that fps is recorded in `sources` and is what frame quantization and the compilers read. Codec: libx264 all-intra, `-g 1 -crf 14 -preset fast -pix_fmt yuv420p` — visually lossless for downstream re-encoding and frame-exact seekable. Honest cost: ~15–25 GB per hour of 1080p30; accepted (masters are per-project and prunable).
- **Audio-only sources** are first-class: FLAC-only MKV master, `sources.has_video = 0`; their spans are `Channel::Audio` and the MVP compilers reject them with the existing typed error until channel lanes land.

Ingest ffprobes the muxed master and **hard-fails if any |start_time| > 10 ms or packet-timestamp discontinuity > 10 ms remains** — given the aresample chain, such a failure indicates a dipho pipeline bug, not a source property. The raw download is kept only as provenance; nothing seeks it.

Integration tests (M2): (a) a synthetic video with a beep at a known time and a deliberate 500 ms container offset; (b) one with a 300 ms mid-stream audio timestamp gap — in both, wav-time == mpv `time-pos` == rendered beep position within one video frame.

### Ingest pipeline

> WhisperX's English alignment models are letter-CTC — word/character timestamps only, **never phonemes** (research finding). The phone tier comes from MFA, the sole aligner (ratified — no fallback backend; cut-point precision is the product).

```
yt-dlp URL or local file                     (staged: original.bin)
  → ffmpeg: playback master + analysis wav   (staged: master.mkv, audio.wav)
  → mlx-whisper (large-v3-turbo, Metal): transcription
  → WhisperX align(): word timestamps + segment tier
  → text normalization: digits/symbols → words (num2words), deterministic
    punctuation stripping; the sidecar keeps the normalized-token ↔
    WhisperX-word mapping (word_index always refers to this mapping —
    never a positional zip of two tokenizers)
  → mfa g2p (english_us_arpa) on the OOV list → augmented dictionary
  → mfa align (english_us_arpa), per-chunk → phone tier (rebased)
  → pyannote 4.x community-1 (MPS, HF token): speaker turns
  → prosody + spectral frames: pyin f0, RMS, 13-dim MFCC
  → manifest; the Rust loader writes SQLite and derives all units
    (utterances → words → phones → diphones — loader logic, not sidecar)
```

**MFA chunking.** Per WhisperX segment; segments with inter-gap < 300 ms are merged first; each chunk padded 250 ms both sides (clamped to neighbors' midpoints). The sidecar parses each chunk's TextGrid and rebases to master time — **every timestamp in the contract is master-relative**. Phones within 100 ms of a chunk edge get reduced confidence. The resulting chunk spans are part of the manifest contract (`chunks`): the loader needs their edges to insert chunk-origin SIL terminators. **Unalignable chunks degrade gracefully**: MFA emits no TextGrid for an utterance it cannot align within its retry beam (hallucinated transcripts, music, crosstalk — routine in found footage); the sidecar warns over NDJSON, marks the chunk `aligned: false` in the contract, and emits no phones for that span — it stays word-searchable but is never phone-addressable. A garbage forced alignment would be worse than an honest gap; the beam is deliberately not widened.

**Diarization.** The sidecar emits raw turns `[{speaker, start, end}]` — its *only* speaker output. All speaker labels on units are derived by the loader (single owner): words/phones/diphones get the turn with maximal temporal overlap (ties → earlier turn; zero overlap → null); `utterances.speaker` is the max-overlap turn over the segment span, with a `multi_speaker` flag when a second speaker overlaps > 20% of it.

**Frame substrate.** Three arrays on one 10 ms grid (hop 160 samples at 16 kHz; frame i centered at t = i·hop, librosa `center=True`; all arrays length `1 + floor(duration/hop)` — loader rejects violations): `f0` (pyin, fmin 50 Hz, **fmax 650 Hz** — found footage is full of shouts and high-pitched voices; the lag budget is governed by fmin so the wider range costs nothing), `rms_db`, and `mfcc` (13-dim, 25 ms window). Shipped as binary npz. The pyin/MFCC parameters are recorded in the manifest's tools block so `dipho reingest --stale` detects parameter changes like tool-version changes. Per-unit aggregation happens in the Rust loader — re-deriving units never re-runs Python.

**Loader validation.** The loader is the gate between sidecar output and the corpus: reject-never-clamp, all failures typed. It rejects unknown manifest versions; non-finite, inverted, or beyond-duration spans on every tier (segments, words, phonemes, turns, chunks); overlapping phone intervals and zero-extent real phones (zero extent is reserved for inserted SIL terminators); and frame arrays disagreeing with `1 + floor(duration/hop)` — computed with an epsilon-tolerant floor, since duration/hop is integral up to f64 rounding whenever the duration comes from a whole sample count.

**Staged, resumable work dir** — keyed by **origin_id**, created by the Rust caller. Stages: `original.bin` → `master.mkv` + `audio.wav` → `transcript.json` → `words.json` → `phones.json` → `diarization.json` → `prosody.npz` → `manifest.json`. Integrity protocol: every stage writes `<name>.tmp`, fsyncs the file, renames, fsyncs the directory; each JSON stage embeds `{stage_schema_version, input_fingerprint}` where the fingerprint is SHA-256 over the upstream stage files it consumed — "validates" means parses + version known + fingerprint chain matches; a mismatch invalidates that stage and everything downstream. `manifest.json` is written last and is the commit record: a workdir without one is incomplete by definition. All manifest paths are workdir-relative; the workdir is self-contained. Sidecar stdout is NDJSON progress (`{"stage": "diarize", "pct": 40}`, terminal `{"done"}` / `{"error": {stage, message}}`) feeding `Event::Job`. Full contract in `python/README.md`.

**Environment strategy** — learned from why sentence-mixing died (pinned MFA 1.1.0-beta, yt-dlp 2022): fragile tools behind subprocess boundaries (MFA is CLI-only in its own micromamba env), yt-dlp unpinned, pyannote's HF token + license acceptance a documented hard prerequisite (diarization is not optional). The WhisperX/pyannote env split the research pass anticipated is moot: whisperx ≥ 3.8 depends on pyannote 4.x directly, so one uv env holds the whole sidecar. pyannote 4.x gotchas (verified 2026-06-10): the diarize stage feeds a soundfile-decoded waveform dict — pyannote's built-in torchcodec decode links FFmpeg 4–7 dylibs only and fails against brew's FFmpeg 8 — and reads turns from `DiarizeOutput.speaker_diarization` (4.x returns a dataclass, not an Annotation).

### Why diphones, not phonemes

Coarticulation means phoneme boundaries are the *worst* place to cut: the signal there is a transition smeared across both neighbors. The stable, cuttable points are inside phones. A **diphone** (cut-point to cut-point, spanning one transition) puts its boundaries on those stable points. Word/phrase search sits on top for the common case; diphone assembly serves sub-word synthesis.

**Cut points are per-phone-class, stored explicitly** (`phones.cut_t`). The normative table over the full `english_us_arpa` inventory:

| Class | Labels | cut_t |
|---|---|---|
| stops, affricates | B D G K P T, CH JH | 20% into the phone (inside the closure, before any plausible burst); **exception:** when the preceding phone is a positive-extent SIL abutting within the 20 ms adjacency gap, cut_t = phone start — the closure belongs to the silence, keeping the burst intact in the following unit. A zero-length terminator or a hard break upstream gets the normal 20% cut: no SIL-stop unit exists to own the closure |
| fricatives | DH F HH S SH TH V Z ZH | temporal midpoint |
| nasals, liquids | M N NG, L R | temporal midpoint |
| monophthong vowels | AA AE AH AO EH ER IH IY UH UW | temporal midpoint |
| diphthongs | AY AW EY OW OY | midpoint, flagged `weak_cut` |
| glides | W Y | midpoint, flagged `weak_cut` (pure transition, no steady state) |
| SIL, NOISE | — | `cut_t` is NULL; SIL boundaries are role-dependent and materialized per diphone (below) |

**Silence model.** MFA's silence intervals become `SIL` phone rows; `<spn>` becomes `NOISE`. At every chunk edge and speaker-turn boundary, the boundary time is snapped to the nearest phone-interval edge; if it falls inside an existing SIL, that SIL is split (or simply terminates adjacency); if speech abuts the boundary, a zero-length SIL row is inserted as a pure adjacency terminator. `phones.sil_origin ∈ {mfa, chunk, turn}` records provenance. After insertion, consecutive/abutting SIL rows are merged — **SIL-SIL is never a unit**. Only SILs with positive acoustic extent form addressable SIL diphones; zero-length terminators do not.

**Adjacency rule** (defines which phone pairs yield diphones). Operate on each source's phone tier, time-ordered, **after deleting NOISE rows and merging SIL runs**. A and B bond iff:

- gap(A.end, B.start) ≤ 20 ms — given MFA's contiguous tiers, a nonzero gap exists exactly where a NOISE row was excised, so this threshold decides whether a short `<spn>` blip is bridgeable (≤ 20 ms) or a hard break (load-bearing, not decorative)
- neither is NOISE; not both are SIL
- SIL participates but blocks transitivity: A-SIL and SIL-B exist; A-B across a SIL never does

There is **no utterance-membership condition** (round-2 fix: SIL belongs to no utterance, so such a condition would forbid every SIL diphone the model exists to create — sentence starts/ends would be unsynthesizable). Chunk edges and speaker turns break non-SIL adjacency via the inserted SIL terminators; a SIL diphone inherits word/utterance/speaker context from its real-phone member. Two real phones with zero gap across a WhisperX *segment* boundary inside one chunk DO bond — audio truth beats ASR segmentation.

**Diphone spans are materialized values** (`diphones.t_start/t_end`), equal to cut_t of A and B *for real phones only*. A SIL side has no cut_t; its boundary is displaced into the silence from the shared speech edge by min(half the SIL duration, 200 ms) — no units carrying a second of dead air. When DSP refinement updates a `cut_t`, the (at most two) diphones touching that phone get their spans **and all six boundary-feature columns** recomputed in the same transaction (re-aggregated from `prosody_frames`) — materialized values never go stale.

**Stress.** `phones.label` keeps full stress-marked ARPAbet as MFA emits it (`AA1`). `diphones.label` is stress-stripped (`AA-K`) — the canonical match key — with `stress_a`/`stress_b` (0/1/2/NULL) alongside; target cost penalizes stress mismatch; the substitution matrix is over stripped labels.

### Storage: SQLite

One SQLite database per corpus (rusqlite, bundled). Migration v1 sets `PRAGMA journal_mode=WAL` (persistent); every connection sets `busy_timeout=5000`, `foreign_keys=ON`. **Write topology:** the loader always runs in-process in whichever dipho process initiated the ingest (only the ML sidecar is a subprocess); each process serializes ALL writes — source loads and user data alike — through one writer task. Cross-process collision (standalone `dipho ingest` vs a TUI on the same corpus): the write-lock loser retries with backoff and surfaces a visible "waiting for corpus writer" state — never a raw SQLITE_BUSY error. Local filesystem only (WAL constraint).

- `sources` — id, origin (URL/path), **origin_id (UNIQUE)**, original path, master path, **master_hash**, duration, **fps** (post-normalization), has_video, per-stream start offsets (assertion trail)
- `ingest_runs` — id, source_id, started/finished, status, schema_version, `tools` JSON (tool + model versions and prosody parameters, from the manifest). Every derived row carries `ingest_run_id`. **Re-ingest** is one transaction: delete derived rows bottom-up (diphones → phones → words → utterances, firing FTS triggers) → insert. `dipho reingest --stale` re-runs sources whose recorded tools/parameters differ.
- `utterances` — span ref, full text (raw WhisperX, for display), **`text_norm`** (the segment's normalized tokens joined — loader-derived from the word rows), speaker FK, `multi_speaker`, ASR confidence; from WhisperX segments. **FTS5 document unit**: external-content `fts5(text_norm, content='utterances')` kept in sync by the three AFTER INSERT/DELETE/UPDATE triggers, shipped in migration v1. Indexing `text_norm` (not raw text) means the FTS token stream and the word rows are the same sequence by construction, so phrase hits map back to word spans via per-word ordinals without drift — raw text disagrees with the word tier whenever normalization changes token count ("25" → "twenty five"). Consequence, accepted: raw-only tokens ("25") are not findable; search normalizes the query, not the mapping.
- `words` — span ref, text, utterance FK, word_ordinal, speaker FK (derived), confidence
- `phones` — span ref, stress-marked label, `cut_t` (NULL for SIL/NOISE), `weak_cut`, `sil_origin`, confidence (nullable), word FK (NULL for SIL/NOISE)
- `diphones` — id, source_id, **`seq`** (per-source ordinal), stress-stripped label, stress_a/stress_b, materialized t_start/t_end, phone_a/phone_b FKs, speaker FK, and **join-cost boundary features**: `mfcc_head`/`mfcc_tail` (13-dim float32 BLOBs), `f0_head`/`f0_tail`, `rms_head_db`/`rms_tail_db`, plus per-unit summaries (median voiced f0, voiced fraction, f0 slope, RMS mean) for target cost. **Frame membership for boundary features:** a frame is in-unit iff its center ∈ [t_start, t_end); head/tail = mean over the first/last min(3, n) in-unit frames; if n = 0, the single frame nearest the unit midpoint; f0_head/tail = median of *voiced* frames among those same frames, NULL if none — an unvoiced boundary reads NULL, not a pitch from 80 ms away. Join cost is a pure SQLite read.
  - **No n-gram table.** Source-adjacency is `seq_b = seq_a + 1` — a self-join on `(source_id, seq)`; n-gram queries are n−1 joins; the solver's candidate fetch is an index hit on `label`. Contiguity falls out of `seq` arithmetic.
- `prosody_frames` — source_id, hop, f0 BLOB, rms_db BLOB, **mfcc BLOB** (13 × n float32, ~19 MB/hour — accepted; frame persistence exists precisely so re-derivation, including DSP boundary-feature recompute, never re-runs ingest)
- `speakers` — id, source_id, label, optional human name, `stale` flag. All unit speaker columns are INTEGER FKs here. **Re-ingest carries speakers forward**: a new run's turn-set matching an existing speaker by temporal overlap ≥ 50% of the *larger* set's total speech inherits its id (and name) — the symmetric threshold means a tiny diarization artifact inside a named speaker's old speech can never claim that identity. Otherwise a new row; orphaned *named* speakers are kept flagged stale, never deleted.
- `turns` — raw diarization turns (speaker FK, span). Kept so re-ingest carry-forward can compute temporal overlap against the previous run — without them, the 50%-of-total-speech test has nothing to be measured against. A claimed speaker's turns are replaced by each run; a stale named speaker keeps its *last-known* turns so a later run can still re-claim it after diarization missed the voice for a run.

Schema rule (kept, now honest): **post-MVP features (solver, join cost) must be servable from this schema without rework** — which is why boundary MFCCs, the mfcc frame substrate, `cut_t`, `seq`, and provenance ship in v1. (Round-2 lesson: a schema column without a specified producer is rework deferred, not avoided — the sidecar emits the MFCC frames that populate the boundary features.)

## Abstraction 2: The Edit

A **program, not a timeline**. Non-destructive EDL-as-data: an ordered list of clips plus transforms, serialized as JSON.

**A clip owns its own `t_start`/`t_end`** — a mutable copy taken at append time — plus optional provenance (the corpus unit kind + id it came from). The corpus stays immutable; trim/nudge edits the clip. This is load-bearing UX: without nudge, every slightly-off alignment is fatal until DSP refinement exists.

**Project binding & the sources manifest.** The corpus lives at `./.dipho/corpus.db` (project = working directory; `--corpus` overrides). An edit file carries a mandatory `sources` manifest: source_id → {origin, origin_id, master filename, duration, master_hash}; deserialization rejects edits without it. **Rebind precedence** when resolving: (1) master_hash match in the corpus → bind; (2) origin_id match with duration sanity (±0.5 s) → bind with a surfaced warning, manifest updated only on explicit save (TUI) or `--accept-relink` (CLI); (3) neither → typed `UnresolvedSource`. A shared edit is self-describing: the recipient ingests the listed origins and dipho rebinds by origin_id.

**Channels.** The data model keeps `channel` on every span. The full model is two parallel lanes (audio, video) sharing one output clock — `both` feeds both lanes; a single-lane span requires the other lane be explicitly filled (span, freeze, or silence) so lane durations always match. **MVP compiles `Channel::Both` only**: typed `ChannelUnsupported { clip_index }` otherwise, surfaced in the TUI — no silent coercion. Post-MVP: `!new_stream` (mpv) and stream-mapped graphs (ffmpeg).

### Transform semantics

| Transform | Meaning | Render (ffmpeg) | Preview (mpv EDL) |
|---|---|---|---|
| `Loop { count }` | whole clip (A+V) plays `count` times | repeated segments | native: repeated EDL segments |
| `Stutter { repeats, slice }` | first `slice` seconds × `repeats`, then the full clip once | repeated segments | native |
| `Pitch { semitones }` | audio-only, duration-preserving | rubberband (fallback asetrate+atempo) | render-only, clip badged |
| `Speed { factor }` | A+V time-scale, pitch-preserving | setpts + chained atempo | render-only, badged |
| `Reverse` | both streams | reverse + areverse (buffers the clip; fine for short clips) | render-only, badged |

**Validation is reject-never-clamp, at compile time**, with typed errors `InvalidSpan` / `InvalidTransform`: spans need 0 ≤ t_start < t_end ≤ source duration; `Loop.count ≥ 1`; `Stutter.repeats ≥ 1`, `0 < slice ≤ clip length`; `Speed.factor ∈ [0.25, 4.0]`; `Pitch.semitones ∈ [−24, +24]`. Golden error-case tests alongside the compile goldens.

### Compilation

Both compilers are pure functions in `dipho-core::edl` over a resolver, keeping the core I/O-free:

```rust
compile_mpv_edl(&Edl, &SourceMap) -> Result<String, EdlCompileError>
compile_ffmpeg(&Edl, &SourceMap) -> Result<FfmpegPlan, EdlCompileError>
// SourceMap: source_id -> { master_path, duration, fps }  (from the corpus, by the caller)
// FfmpegPlan: ordered list of complete ffmpeg invocations (two-stage render)
```

**Clips compile verbatim, in edit order, never reordered, pad = 0.** (Round 1: "pad-then-merge" would have collapsed stutters and reordered edits — supercut semantics, not sentence-mixing.) **Join elision is mandatory and deterministic, computed once in a shared pre-pass both targets consume** (round 2: "may elide" would let preview and render disagree about the output timeline): consecutive clips sharing (source, channel), **both with empty transform lists**, whose spans are source-contiguous forward (|next.t_start − prev.t_end| ≤ 1.0 ms, one named constant) compile to one segment [prev.t_start, next.t_end]. Repeated identical spans never merge. Context padding exists only in audition playback. Goldens: a repeated span compiles to N segments; both compilers emit the identical output-time boundary list for an elision fixture; two contiguous clips each with `Loop{2}` compile to four segments AABB, never elided.

1. **mpv EDL (preview)** — recompiled to an `edl://` URI on every change, `["loadfile", uri, "replace"]` to the long-lived slave; no temp files. `.mpv.edl` export is the same compiler. Unconditional `%<utf-8-byte-count>%` quoting; named params (`start=`, `length=`); explicit float formatting; per-segment `title=` (word label; mpv's chapter-per-segment gives free cut navigation). EDL v0 is unfrozen → startup version probe (floor ≥ 0.38). Golden-file tests.
2. **ffmpeg render (export)** — **two-stage**. Stage 1, per clip: accurate seek on the all-intra master and **frame-exact extraction** (below), apply the clip's transform chain, encode to a uniform intermediate: **ProRes 422 HQ + pcm_s24le in .mov** (intra, effectively transparent — the final encode is the only quality-determining step after the master) at the project profile (largest post-normalization display area among edit sources; fps from `sources`; audio 48 kHz stereo; `--size`/`--fps` override). Intermediates live in `./.dipho/render/<edit-hash>/clip-NNN.mov`, deleted on success, kept on failure, with a preflight free-space check. Stage 2: concat demuxer over intermediates → final encode. Never stream-copy cuts. A single-process `filter_complex` path is permitted for small edits only if golden-tested timing-identical.

**Frame quantization — one algorithm, audio is master.** Audio cuts are sample-accurate and authoritative. One planning pass in `compile_ffmpeg` computes cumulative audio time T_k after each output segment; segment k gets `n_k = round(T_k·fps) − round(T_{k−1}·fps)` video frames (cumulative rounding *is* the error diffusion; |video − audio| ≤ half a frame everywhere, provably), starting at master frame `f_k = floor(t_start·fps + 1e-9)`. Stage 1 selects frames exactly (`-ss` keyframe-exact on the all-intra master, then `trim=end_frame=n_k` — never `trim=start=<seconds>`). mpv preview uses seconds (`length=`) and may differ by ≤ 1 frame per boundary — acceptable, preview ≠ reference. M6 asserts exactly this formula: audio cut positions match the EDL; A/V error < half a frame at every boundary and at the end.

### mpv slave lifecycle

Spawn once: `mpv --idle=yes --keep-open=yes --no-terminal --input-ipc-server=<socket>`, where the socket lives in a fresh per-process 0700 temp directory (short path — macOS `sun_path` ~104 bytes; private because IPC exposes `run`), removed when the slave is dropped. One persistent JSON IPC connection per session (closing drops `observe_property`). Correlate by `request_id`, never message order; `playback-restart` = seek-done. Audition = exact seek + `ab-loop-a`/`ab-loop-b` (clear with `"no"`).

**Player modes.** `PlayerMode { Audition(hit), Preview(output_pos) }`. Audition keys: loop-exact, play-with-context (±500 ms, no loop), play-full-utterance. In Preview, dipho owns compiled segment durations, so on recompile it computes current output-time, reloads, seeks back (clamped if the edit changed under the playhead), clearing ab-loops.

## The Solver (post-MVP)

Type a target sentence → ranked candidate assemblies.

- **Target cost** — phone-sequence match via the substitution matrix (no hard OOV failures), stress mismatch, speaker constraint, duration plausibility, alignment confidence, `weak_cut` penalty
- **Join cost** — read straight from diphone boundary columns: Euclidean MFCC distance + |Δlog f0| (skipped when either side is NULL/unvoiced) + |Δ dB|. Explicitly **not** humor scoring; the human picks the funny one from a shortlist of clean ones.

Beam search over a per-position candidate lattice (join cost depends only on the previous unit's tail features — small state). Source-adjacent diphones (`seq + 1`) get zero join cost plus a contiguous-run bonus (sentence-mixing's best idea; their weight ratios seed tuning). Punctuation becomes SIL targets scored against real positive-extent silence units. Seeded noise diversifies rankings. Random `mash` (videogrep) is the baseline to beat. UX: chunk-by-chunk authoring, rank-ordered scored candidates, stash buffer, re-edit, accept-and-advance, autosaved sessions.

## DSP cut refinement (post-MVP, native)

Aligner timestamps are ±tens of ms. Two passes in the Rust hot loop (symphonia decode of the master FLAC, rustfft), updating `phones.cut_t` within the aligner tolerance window: (1) nearest zero-crossing snap; (2) spectral-flux minimization. Each `cut_t` update recomputes, in the same transaction, the materialized spans and all six boundary-feature columns of the ≤ 2 affected diphones (re-aggregated from `prosody_frames`). The waveform widget's peaks cache shares the decode path.

## Architecture: three processes

| Process | Role | Why separate |
|---|---|---|
| **dipho** (Rust) | index, search, EDL, DSP, ratatui TUI, mpv control, **corpus loader** | the interactive loop; single writer per process |
| **mpv** | slave player in its own window | best playback engine; JSON IPC; never in-terminal video |
| **Python sidecar** | batch ML ingest (staged work dir, NDJSON progress) | Python-native ML; offline; crash/version isolation |

Workspace: `crates/dipho-core` (library: spans, corpus, EDL + compilers, DSP — no TUI, no process management), `crates/dipho` (binary: clap CLI, ratatui TUI, mpv IPC client), `python/` (uv project, `dipho-ingest`).

### TUI architecture

Elm-flavored single event loop on tokio (the convergent pattern across television, atuin, gitui, yazi): one `App` state struct, one merged `Event` enum (`Term`, `Tick`, `Mpv`, `Db`, `Job`), all producers into one mpsc, single consumer, render-on-dirty (~10 ms debounce). Modules: `app.rs`, `event.rs`, `ui/`, `mpv/`, `db/` (rusqlite on `spawn_blocking`; read-only pool + the process's single writer task).

**Every EDL mutation is a reversible message**: bounded undo/redo stack, autosave to `<edit>.json.autosave` after every mutation, recovery on open. Undo scope is the EDL only.

mpv IPC client is hand-rolled (~200 lines: UnixStream + request_id→oneshot map + events→mpsc). Crates: now — ratatui, tokio, crossterm (event-stream), tui-input, serde_json, rusqlite; later — nucleo, ratatui-textarea, rodio (only if audition latency disappoints); skip — mpvipc, kira. Waveform widget in-tree (Canvas + braille min/max envelope; Sparkline stopgap); peaks cache shared with DSP.

## MVP: one vertical slice

1. **Ingest** — `dipho ingest <url|file>`: staged workdir (origin_id-keyed), master + wav, sidecar stages, in-process load
2. **Index** — schema v1 as above
3. **Word search** — FTS5 over utterances; hits in utterance context with speaker + confidence
4. **Audition** — mpv ab-loop / context / full-utterance per hit
5. **Flat EDL** — append, reorder, trim/nudge (±5 ms / ±25 ms, instant recompile + neighborhood replay), undo/redo, autosave; save/load with sources manifest; `edl://` preview
6. **Render** — `dipho render edit.json out.mp4`, two-stage ffmpeg

Post-MVP, rough order: transforms → waveform widget → diphone assembly search → solver → DSP cut refinement → channel lanes → exports (VTT, FCP7 XML, m3u).

## Risk register

- **mpv EDL v0 unfrozen** — version probe, single serializer, golden tests
- **MFA conda-only** — arm64-on-M4 verified 2026-06-10 (micromamba + conda-forge MFA 3.3.9: align + g2p both work; ~30 s startup overhead per `mfa align` run). Subprocess boundary keeps it swappable. Sole aligner: if it degrades on real footage, reassess openly, never silently.
- **Master disk cost** (~15–25 GB/hour 1080p30 all-intra) — accepted for seek quality; masters are per-project and prunable; revisit codec only with measurements in hand
- **Hundred-segment sub-second EDL preview unproven** — all-intra master is the mitigation; M5 gate: 100 × 200 ms segments play without drops/gaps (mpv stats)
- **mpv audition latency** — measured in M4 (M4 Max, real 1080p30 all-intra master, paused exact seeks): ~20 ms median seek → `playback-restart` round trip; no rodio path needed
- **pyannote HF-gated** — token + license is a hard documented setup step
- **Dependency rot killed the prior art** — yt-dlp unpinned, subprocess boundaries, staged re-runnable ingest, tools+parameters provenance per run
- mlx-whisper `word_timestamps` memory growth — don't enable it; word times come from WhisperX align

## Open questions

Ratified 2026-06-10 (rounds 1–2): MFA sole aligner; ARPAbet `english_us_arpa`; HF token hard prerequisite; mpv ≥ 0.38 probe; playback-master timebase with aresample-continuous audio; all-intra x264 master + ProRes intermediates; audio-master cumulative-rounding frame quantization; verbatim compilation with mandatory shared-pre-pass elision (empty-transform clips only); SIL/NOISE model with role-dependent materialized SIL boundaries and the normative cut-point table; origin_id/master_hash identity split; boundary MFCCs fed by a sidecar MFCC frame array; no n-gram table; utterance FTS5 tier with sync triggers; staged tmp+rename+fingerprint ingest; in-process loader with single-writer-per-process WAL.

Still open:

- MFCC/pyin parameter validation against real found-footage joins (M2 corpora; parameters are standard but unvalidated on this domain)
- Stutter `slice` default (fixed 60 ms? first-phone length? decide when the transform lands, post-MVP)

Deferred from the M1 review (do when their milestone lands, not before):

- Loader hot-loop costs (terminator insertion and per-unit speaker assignment are O(units × turns/boundaries); frame/feature blobs are encoded with a per-element copy) — fine at fixture scale and dwarfed by the ML stages; optimize only with profiles from a real long-source ingest in hand (M2 only ingested short fixtures)
- `ingest_runs.started/finished/status` are stamped at load time by the loader. M2 shipped `dipho ingest` without per-stage lifecycle provenance — re-deferred: do it when the TUI ingest job UX lands (`Event::Job` already carries per-stage progress) or alongside `dipho reingest --stale`, whichever comes first
- EDL `SourceInfo.fps` is non-optional while `sources.fps` is NULL for audio-only sources — reconcile when the binary wires corpus → SourceMap (M5)

Deferred from M2 (recorded 2026-06-10):

- `phones.confidence` is a placeholder heuristic, not an acoustic score: 1.0, reduced to 0.5 within 100 ms of a chunk edge. Replace with real per-phone alignment scores (MFA exposes them via alignment analysis) when the solver's target cost starts consuming confidence — before tuning, not after
- ~~The mpv `time-pos` leg of the timebase integration tests needs the IPC client → M4~~ — closed in M4: both beep fixtures assert wav-time == mpv playback time (`--ao=pcm` dump position) == `time-pos` after exact seek, within one frame (`crates/dipho/src/mpv.rs` integration tests; the wav legs remain in `crates/dipho/src/ingest/normalize.rs`)
- ~~The yt-dlp URL download path is implemented but unexercised~~ — verified in M3 (real YouTube ingest end to end, origin_id idempotency re-checked)

Deferred from M3 (recorded 2026-06-10):

- Query normalization is a token-level Rust port of the sidecar's num2words English expansion (`corpus/normalize.rs`), parity-pinned by tests. Accepted divergences, both unreachable for English ASR tokens: numbers beyond u128 fall back to digit-by-digit earlier than Python, and non-ASCII unicode digits are stripped rather than expanded. If the sidecar's normalization ever changes, the Rust port and its pinned cases must change with it
- Words containing apostrophes must be queried with the same spelling ("don't" finds "don't"; "don t" does not) — FTS5's unicode61 tokenizer splits on apostrophes, so such utterances FTS-match more loosely, but only word-row-exact phrase occurrences are returned as hits
- Search results are corpus-ordered (source_id, t_start); relevance ranking is a solver-era concern
