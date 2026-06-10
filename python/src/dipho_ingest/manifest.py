"""Stage: manifest.json — the contract (python/README.md is normative),
written last; the commit record. Assembled purely from the upstream stage
files; chunk spans are recomputed from the words.json segment tier, never
read back from MFA state.
"""

from __future__ import annotations

import importlib.metadata
import json
from pathlib import Path

from . import diarize, mfa, prosody, transcribe, words
from .chunks import chunk_spans
from .progress import Progress
from .stages import load_stage_json, write_stage_json

NAME = "manifest"
OUTPUT = "manifest.json"
VERSION = 1
INPUTS = ["audio.wav", words.OUTPUT, mfa.OUTPUT, diarize.OUTPUT, prosody.OUTPUT]

MANIFEST_SCHEMA_VERSION = 1


def valid(workdir: Path) -> bool:
    return load_stage_json(workdir, OUTPUT, MANIFEST_SCHEMA_VERSION, INPUTS) is not None


def run(workdir: Path, progress: Progress) -> None:
    import numpy as np
    import soundfile

    transcript = load_stage_json(workdir, transcribe.OUTPUT, transcribe.VERSION, transcribe.INPUTS)
    words_doc = load_stage_json(workdir, words.OUTPUT, words.VERSION, words.INPUTS)
    phones_doc = load_stage_json(workdir, mfa.OUTPUT, mfa.VERSION, mfa.INPUTS)
    turns_doc = load_stage_json(workdir, diarize.OUTPUT, diarize.VERSION, diarize.INPUTS)
    assert transcript and words_doc and phones_doc and turns_doc

    duration = float(soundfile.info(workdir / "audio.wav").duration)
    with np.load(workdir / prosody.OUTPUT) as npz:
        n_frames = len(npz["f0"])
        prosody_params = json.loads(str(npz["params"]))

    write_stage_json(
        workdir,
        OUTPUT,
        MANIFEST_SCHEMA_VERSION,
        INPUTS,
        {
            "schema_version": MANIFEST_SCHEMA_VERSION,
            "analysis": {"path": "audio.wav", "duration": duration},
            "tools": {
                "dipho_ingest": importlib.metadata.version("dipho-ingest"),
                "mlx_whisper": transcript["mlx_whisper_version"],
                "whisper_model": transcript["model"],
                "whisperx": words_doc["whisperx_version"],
                "mfa": phones_doc["mfa_version"],
                "mfa_acoustic": phones_doc["acoustic_model"],
                "mfa_dictionary": phones_doc["dictionary"],
                "mfa_g2p": phones_doc["g2p_model"],
                "pyannote": turns_doc["pyannote_version"],
                "pyannote_model": turns_doc["model"],
                "prosody_params": prosody_params,
            },
            "segments": words_doc["segments"],
            "words": [
                {key: tok[key] for key in ("text", "start", "end", "confidence", "segment_index")}
                for tok in words_doc["words"]
            ],
            "phonemes": phones_doc["phonemes"],
            "turns": turns_doc["turns"],
            "chunks": [
                {"start": c["start"], "end": c["end"]}
                for c in chunk_spans(words_doc["segments"], duration)
            ],
            "prosody": {
                "path": prosody.OUTPUT,
                "hop": prosody_params["hop"],
                "n_frames": n_frames,
            },
        },
    )
