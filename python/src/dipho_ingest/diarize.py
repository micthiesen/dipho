"""Stage: diarization.json — pyannote speaker turns (MPS, HF-gated).

Emits raw turns only; all speaker labels on units are loader-derived
(single owner). The HF token + license acceptance for
pyannote/speaker-diarization-community-1 is a hard documented prerequisite.
"""

from __future__ import annotations

import importlib.metadata
from pathlib import Path

from .progress import Progress
from .stages import load_stage_json, write_stage_json

NAME = "diarize"
OUTPUT = "diarization.json"
VERSION = 1
INPUTS = ["audio.wav"]
MODEL = "pyannote/speaker-diarization-community-1"


def valid(workdir: Path) -> bool:
    return load_stage_json(workdir, OUTPUT, VERSION, INPUTS) is not None


def run(workdir: Path, progress: Progress) -> None:
    import numpy as np
    import soundfile
    import torch
    from pyannote.audio import Pipeline

    # Decode the wav ourselves: pyannote 4.x's built-in decoding goes
    # through torchcodec, whose bundled dylibs only link FFmpeg 4–7 and
    # fail against brew's FFmpeg 8.
    samples, sr = soundfile.read(workdir / "audio.wav", dtype="float32")
    duration = len(samples) / sr
    waveform = torch.from_numpy(np.atleast_2d(samples))
    try:
        pipeline = Pipeline.from_pretrained(MODEL)
    except Exception as e:
        raise RuntimeError(
            f"cannot load {MODEL} — accept its license on Hugging Face and "
            "log in (`uv run hf auth login`) or set HF_TOKEN; "
            f"see python/README.md. Underlying error: {e}"
        ) from e
    pipeline.to(torch.device("mps" if torch.backends.mps.is_available() else "cpu"))
    progress.tick(NAME, 20)

    output = pipeline({"waveform": waveform, "sample_rate": sr})
    # pyannote 4.x returns a DiarizeOutput; the raw (possibly overlapping)
    # turns are its speaker_diarization Annotation.
    turns = [
        {
            "speaker": str(label),
            "start": min(max(float(segment.start), 0.0), duration),
            "end": min(max(float(segment.end), 0.0), duration),
        }
        for segment, _, label in output.speaker_diarization.itertracks(yield_label=True)
    ]
    turns = [t for t in turns if t["end"] > t["start"]]

    write_stage_json(
        workdir,
        OUTPUT,
        VERSION,
        INPUTS,
        {
            "pyannote_version": importlib.metadata.version("pyannote.audio"),
            "model": MODEL,
            "turns": turns,
        },
    )
