# dipho-ingest

Batch ingest sidecar. Analyzes one media file through staged, resumable steps and emits a manifest; the Rust side loads the manifest into the corpus SQLite and derives all units (utterances→words→phones→diphones). Never part of the interactive loop.

```bash
uv sync
uv run dipho-ingest --workdir <corpus_dir>/ingest/<origin_id>/
```

The Rust caller creates the workdir (keyed by **origin_id**), runs download + normalization as the first stages (playback master + analysis wav — mono 16 kHz pcm_s16le sharing the master's `aresample=async=1:first_pts=0` audio chain, so wav time == master time by construction), hard-links the wav into the workdir, and owns the `sources` row. The sidecar errors if the workdir is missing. **Every timestamp the sidecar emits is master-relative**; chunk-time rebasing happens inside the sidecar, never in the loader.

Dependencies (one uv env — whisperx ≥ 3.8 depends on pyannote 4.x directly, so no env split is needed): mlx-whisper (transcription, Metal), WhisperX (word alignment — word timestamps ONLY, no phonemes), pyannote.audio 4.x (diarization, MPS, HF-gated), librosa (pyin f0, RMS, MFCC), num2words (text normalization), praatio (TextGrid parsing). MFA (`english_us_arpa` align + g2p) is NOT a Python dep: CLI subprocess in its own micromamba env. Sole aligner — no fallback.

## One-time setup

```bash
brew install micromamba
micromamba create -y -n mfa -c conda-forge montreal-forced-aligner
micromamba run -p ~/micromamba/envs/mfa mfa model download acoustic english_us_arpa
micromamba run -p ~/micromamba/envs/mfa mfa model download dictionary english_us_arpa
micromamba run -p ~/micromamba/envs/mfa mfa model download g2p english_us_arpa
```

The sidecar invokes MFA as `micromamba run -p ~/micromamba/envs/mfa mfa` (explicit prefix — brew's micromamba roots its env prefix inside the Cellar); override with `DIPHO_MFA_CMD`.

Diarization is HF-gated and **not optional**: accept the license for `pyannote/speaker-diarization-community-1` on Hugging Face, then `uv run hf auth login` (or set `HF_TOKEN`).

## Staged work dir

```
<workdir>/
  original.bin       # raw download / copied local file (Rust stage)
  master.mkv         # playback master (Rust stage)
  audio.wav          # analysis wav (Rust stage)
  normalize.json     # Rust normalize-stage record: master_hash (computed once), fps, has_video, start_offsets
  ingest.log         # sidecar stderr from the most recent run (Rust truncates per run)
  transcript.json    # mlx-whisper segments
  words.json         # WhisperX-aligned words + segment tier + normalized-token map
  phones.json        # MFA phone tier, rebased to master time
  diarization.json   # raw speaker turns
  prosody.npz        # float32 arrays: f0[], rms_db[], mfcc[n_frames, 13]
  manifest.json      # the contract — written last; the commit record
  mfa/               # MFA scratch (chunk corpus, dict, temp) — deleted on stage success
```

**Integrity protocol:** every stage writes `<name>.tmp`, fsyncs the file, renames, fsyncs the directory. Each JSON stage embeds `{"stage_schema_version": 1, "input_fingerprint": "<sha256>"}` over the upstream stage files it consumed (`prosody.npz` embeds the same two fields, plus its parameters, as 0-d string arrays). "Valid" = parses + version known + fingerprint chain matches; a mismatch invalidates that stage and everything downstream. Re-running skips valid stages — a pyannote crash 90 minutes in costs one stage. A workdir without `manifest.json` is incomplete by definition. All paths in the manifest are workdir-relative.

stdout is NDJSON progress: `{"stage": "diarize", "pct": 40}` per tick, `{"stage": "...", "warning": "..."}` for non-fatal degradation (e.g. an unalignable chunk), terminal `{"done": true}` or `{"error": {"stage": "...", "message": "..."}}` — these feed the TUI's `Event::Job`.

## manifest.json contract

Versioned with `schema_version`; the loader rejects unknown versions.

```jsonc
{
  "schema_version": 1,
  "analysis": { "path": "audio.wav", "duration": 1234.56 },
  "tools": {                       // provenance, stored per ingest_run;
    "dipho_ingest": "0.1.0",       // a change in ANY of these marks the
    "mlx_whisper": "…", "whisperx": "…",   // source stale for `reingest --stale`
    "mfa": "3.3.9", "mfa_acoustic": "english_us_arpa", "mfa_g2p": "english_us_arpa",
    "pyannote": "…",
    "prosody_params": { "fmin": 50, "fmax": 650, "hop": 0.01, "frame_length": 1024,
                        "mfcc_n": 13, "mfcc_window": 0.025, "rms_window": 0.025 }
  },
  "segments": [                    // WhisperX segment tier → utterances table
    { "text": "string", "start": 1.20, "end": 4.85,
      "word_index_start": 0, "word_index_end": 11, "confidence": 0.93 }
  ],
  "words": [
    { "text": "string",            // normalized token (digits expanded etc.)
      "start": 1.23, "end": 1.56,
      "confidence": 0.97,
      "segment_index": 0 }         // NO speaker here: the loader is the sole
  ],                               // owner of speaker derivation, from turns
  "phonemes": [
    { "label": "AA1",              // stress-marked ARPAbet as MFA emits it;
                                   // "SIL" for silence, "NOISE" for <spn>
      "start": 1.23, "end": 1.31,
      "confidence": 0.92,          // nullable; reduced within 100 ms of a chunk edge
      "word_index": 0 }            // post-normalization mapping; null for SIL/NOISE
  ],
  "turns": [                       // the ONLY speaker data the sidecar emits
    { "speaker": "SPEAKER_00", "start": 0.0, "end": 14.2 }
  ],
  "chunks": [                      // MFA chunk spans, master-relative — the loader
    { "start": 0.0, "end": 30.0,   // inserts SIL adjacency terminators at their edges
      "aligned": true }            // false: MFA gave up within its retry beam; the
  ],                               // span has no phones (word-searchable only)
  "prosody": {
    "path": "prosody.npz",         // f0 (Hz, 0 = unvoiced), rms_db, mfcc[n,13]
    "hop": 0.01,                   // frame i centered at t = i*hop (librosa center=True)
    "n_frames": 123457             // all three arrays; must equal 1 + floor(duration/hop)
  }
}
```

Notes:

- Speaker labels on units are **loader-derived** from `turns` (maximal temporal overlap; ties → earlier; zero → null). The sidecar never assigns speakers to words.
- `word_index` refers to the sidecar's own normalized-token mapping — never a positional zip of two tokenizers.
- `segments[].text` is the raw WhisperX text (kept for display); the search index document (`utterances.text_norm`) is loader-derived by joining the segment's normalized words, so FTS phrase tokens and word ordinals are the same stream by construction.
- Diphones, cut points, SIL insertion/merging, adjacency, and all per-unit aggregation (prosody summaries, MFCC/f0/RMS boundary features) are loader logic in Rust — re-deriving units never re-runs Python.
- MFA chunking (segment merge < 300 ms gaps, 250 ms pads, TextGrid parse + rebase) is internal to the sidecar; only the resulting chunk spans appear in the contract — the loader needs their edges to insert chunk-origin SIL terminators (`phones.sil_origin = 'chunk'`). Chunk spans are a pure function of `words.json`'s segment tier, so a resume that skips straight to the manifest stage recomputes them deterministically without re-running MFA.
- The loader validates before writing (typed errors, reject-never-clamp): every timestamp must be finite and lie within `[0, analysis.duration]` with `end >= start`; phone intervals must not overlap and real phones must have positive extent; the prosody arrays must match `1 + floor(duration/hop)`. The sidecar must clamp emitted times to the analysis duration (MFA TextGrid edges can exceed it by rounding).
