# dipho-ingest

Batch ingest sidecar. Analyzes one media file through staged, resumable steps and emits a manifest; the Rust side loads the manifest into the corpus SQLite and derives all units (utterances→words→phones→diphones). Never part of the interactive loop.

```bash
uv sync
uv run dipho-ingest <analysis-wav> --workdir <corpus_dir>/ingest/<content_hash>/
```

The Rust caller prepares inputs (playback master + analysis wav — mono 16 kHz pcm_s16le extracted from the master with no seek/trim, so wav t=0 == master t=0) and owns the `sources` row. **Every timestamp the sidecar emits is master-relative**; chunk-time rebasing happens inside the sidecar, never in the loader.

Planned heavy dependencies (not installed yet): mlx-whisper (transcription, Metal), WhisperX (word alignment — word timestamps ONLY, no phonemes), pyannote.audio 4.x (diarization, MPS, HF-gated), librosa (pyin f0 + RMS), num2words (text normalization). MFA (`english_us_arpa` align + g2p) is NOT a Python dep: CLI subprocess in its own micromamba env. Sole aligner — no fallback.

## Staged work dir

Each stage file is fsynced on completion; re-running skips stages whose outputs exist and validate. A crash costs one stage, not the run.

```
<workdir>/
  transcript.json    # mlx-whisper segments
  words.json         # WhisperX-aligned words + segment tier + normalized-token map
  phones.json        # MFA phone tier, rebased to master time
  diarization.json   # raw speaker turns
  prosody.npz        # float32 f0[] and rms_db[] frame arrays
  manifest.json      # the contract: everything below, referencing the files above
```

stdout is NDJSON progress: `{"stage": "diarize", "pct": 40}` per tick, terminal `{"done": true}` or `{"error": {"stage": "...", "message": "..."}}`. These feed the TUI's `Event::Job` directly.

## manifest.json contract

Versioned with `schema_version`; the loader rejects unknown versions.

```jsonc
{
  "schema_version": 1,
  "analysis": { "path": "audio.wav", "duration": 1234.56 },
  "tools": {                       // provenance, stored per ingest_run
    "dipho_ingest": "0.1.0",
    "mlx_whisper": "…", "whisperx": "…",
    "mfa": "3.3.9", "mfa_acoustic": "english_us_arpa", "mfa_g2p": "english_us_arpa",
    "pyannote": "…"
  },
  "segments": [                    // WhisperX segment tier → utterances table
    { "text": "string", "start": 1.20, "end": 4.85,
      "word_index_start": 0, "word_index_end": 11, "confidence": 0.93 }
  ],
  "words": [
    { "text": "string",            // normalized token (digits expanded etc.)
      "start": 1.23, "end": 1.56,
      "confidence": 0.97,
      "segment_index": 0,
      "speaker": "SPEAKER_00" }    // derived: maximal overlap vs turns; null if none
  ],
  "phonemes": [
    { "label": "AA1",              // stress-marked ARPAbet as MFA emits it;
                                   // "SIL" for silence, "NOISE" for <spn>
      "start": 1.23, "end": 1.31,
      "confidence": 0.92,          // nullable; reduced within 100 ms of a chunk edge
      "word_index": 0 }            // post-normalization mapping; null for SIL/NOISE
  ],
  "turns": [                       // diarization source of truth
    { "speaker": "SPEAKER_00", "start": 0.0, "end": 14.2 }
  ],
  "prosody": {
    "path": "prosody.npz",         // f0 (Hz, 0 = unvoiced) and rms_db arrays
    "hop": 0.01,                   // frame i centered at t = i*hop (librosa center=True)
    "n_frames": 123457             // must equal 1 + floor(duration/hop); loader rejects otherwise
  }
}
```

Notes:

- Word `speaker` is derived (maximal temporal overlap; ties → earlier turn; zero overlap → null); `turns` is authoritative and the loader may re-derive.
- `word_index` refers to the sidecar's own normalized-token mapping — never a positional zip of two tokenizers.
- Diphones, cut points (per-phone-class), and all per-unit prosody aggregation are loader logic in Rust — re-deriving units never re-runs Python.
- MFA chunking (segment merge < 300 ms gaps, 250 ms pads, TextGrid parse + rebase) is internal to the sidecar and invisible in the contract.
