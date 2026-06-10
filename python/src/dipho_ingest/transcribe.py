"""Stage: transcript.json — mlx-whisper transcription (Metal).

`word_timestamps` stays off (risk register: memory growth on long chunked
audio); word times come from WhisperX alignment in the next stage.
"""

from __future__ import annotations

import importlib.metadata
from pathlib import Path

from .progress import Progress
from .stages import load_stage_json, write_stage_json

NAME = "transcribe"
OUTPUT = "transcript.json"
VERSION = 1
INPUTS = ["audio.wav"]
MODEL = "mlx-community/whisper-large-v3-turbo"


def valid(workdir: Path) -> bool:
    return load_stage_json(workdir, OUTPUT, VERSION, INPUTS) is not None


def run(workdir: Path, progress: Progress) -> None:
    import mlx_whisper

    result = mlx_whisper.transcribe(str(workdir / "audio.wav"), path_or_hf_repo=MODEL)
    if result["language"] != "en":
        raise RuntimeError(
            f"detected language {result['language']!r}; only English sources are supported"
        )
    segments = [
        {
            "text": seg["text"],
            "start": float(seg["start"]),
            "end": float(seg["end"]),
            "avg_logprob": float(seg["avg_logprob"]),
            "no_speech_prob": float(seg["no_speech_prob"]),
        }
        for seg in result["segments"]
        if seg["text"].strip()
    ]
    write_stage_json(
        workdir,
        OUTPUT,
        VERSION,
        INPUTS,
        {
            "model": MODEL,
            "mlx_whisper_version": importlib.metadata.version("mlx-whisper"),
            "language": result["language"],
            "segments": segments,
        },
    )
