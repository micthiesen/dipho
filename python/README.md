# dipho-ingest

Batch ingest sidecar. Analyzes one media file and emits a single JSON document on stdout; the Rust side loads it into the corpus SQLite. Never part of the interactive loop.

```bash
uv sync
uv run dipho-ingest <media-file> > analysis.json
```

Planned heavy dependencies (not installed yet): mlx-whisper (transcription, Metal), WhisperX (word alignment), Montreal Forced Aligner (phone alignment — CLI subprocess in its own micromamba env, not a Python dep), pyannote.audio 4.x (diarization, HF-gated), librosa (prosody). See `docs/research/04-alignment-stack.md` and `docs/research/SUMMARY.md`.

## JSON output contract

Versioned with `schema_version`; the Rust loader rejects versions it doesn't know.

```jsonc
{
  "schema_version": 1,
  "source": {
    "path": "string",          // media file analyzed
    "duration": 123.45         // seconds
  },
  "words": [
    {
      "text": "string",
      "start": 1.23,           // seconds, all timestamps source-relative
      "end": 1.56,
      "confidence": 0.97,      // aligner confidence, 0..1
      "speaker": "SPEAKER_00"  // diarization label, null if unknown
    }
  ],
  "phonemes": [
    {
      "label": "AA",           // ARPAbet (ratified: MFA english_us_arpa)
      "start": 1.23,
      "end": 1.31,
      "word_index": 0          // index into words[], null for non-speech
    }
  ],
  "speakers": [
    { "label": "SPEAKER_00" }  // one entry per diarization cluster
  ],
  "prosody": {
    "hop": 0.01,               // seconds between frames
    "f0": [123.4, 0.0],        // Hz per frame, 0 = unvoiced
    "rms": [0.012, 0.011]      // linear RMS energy per frame
  }
}
```

Notes:

- Prosody is frame-level; the Rust side aggregates per-unit statistics (mean f0, contour summary, energy) at index time. This keeps the sidecar dumb and lets unit definitions evolve without re-running ingest.
- Diphones are NOT emitted here — the Rust indexer derives them from adjacent phoneme pairs (midpoint to midpoint).
- The sidecar receives an already-demuxed audio file (wav, mono, 16 kHz) prepared by the Rust side with ffmpeg.
