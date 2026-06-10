"""Stage: words.json — WhisperX word alignment + text normalization.

Emits the segment tier and the normalized-token list with the token ↔
WhisperX-word mapping. WhisperX alignment is word-level only (letter-CTC —
never phonemes); the phone tier comes from MFA in the next stage.
"""

from __future__ import annotations

import importlib.metadata
from pathlib import Path

from . import transcribe
from .normalize import normalize_word
from .progress import Progress
from .stages import load_stage_json, write_stage_json

NAME = "align_words"
OUTPUT = "words.json"
VERSION = 1
INPUTS = ["audio.wav", transcribe.OUTPUT]

# wav2vec2 forced alignment on a 7 s clip measures ~0.4 s on CPU; the GPU
# stays free for transcription. Committed choice, not a fallback.
DEVICE = "cpu"


def valid(workdir: Path) -> bool:
    return load_stage_json(workdir, OUTPUT, VERSION, INPUTS) is not None


def _fill_missing_spans(words: list[dict], seg_start: float, seg_end: float) -> None:
    """WhisperX leaves words with no alignable characters (e.g. digits)
    without timestamps; interpolate between aligned neighbors."""
    for i, w in enumerate(words):
        if "start" in w and "end" in w:
            continue
        prev_end = next(
            (words[j]["end"] for j in range(i - 1, -1, -1) if "end" in words[j]), seg_start
        )
        next_start = next(
            (words[j]["start"] for j in range(i + 1, len(words)) if "start" in words[j]), seg_end
        )
        w["start"], w["end"] = prev_end, max(next_start, prev_end)


def run(workdir: Path, progress: Progress) -> None:
    import soundfile
    import whisperx

    transcript = load_stage_json(workdir, transcribe.OUTPUT, transcribe.VERSION, transcribe.INPUTS)
    assert transcript is not None
    duration = float(soundfile.info(workdir / "audio.wav").duration)

    audio = whisperx.load_audio(str(workdir / "audio.wav"))
    model, metadata = whisperx.load_align_model(language_code="en", device=DEVICE)
    progress.tick(NAME, 30)
    aligned = whisperx.align(transcript["segments"], model, metadata, audio, DEVICE)
    progress.tick(NAME, 80)

    def clamp(t: float) -> float:
        return min(max(float(t), 0.0), duration)

    segments: list[dict] = []
    tokens: list[dict] = []
    for seg in aligned["segments"]:
        seg_start, seg_end = clamp(seg["start"]), clamp(seg["end"])
        _fill_missing_spans(seg["words"], seg_start, seg_end)
        index_start = len(tokens)
        for w in seg["words"]:
            texts = normalize_word(w["word"])
            if not texts:
                continue
            start, end = clamp(w["start"]), clamp(w["end"])
            step = (end - start) / len(texts)
            for k, text in enumerate(texts):
                tokens.append(
                    {
                        "text": text,
                        "start": start + k * step,
                        "end": start + (k + 1) * step,
                        "confidence": w.get("score"),
                        "segment_index": len(segments),
                        "whisperx_word": w["word"],
                    }
                )
        if len(tokens) == index_start:
            continue  # no normalizable tokens — drop the segment
        scores = [w["score"] for w in seg["words"] if "score" in w]
        segments.append(
            {
                "text": seg["text"],
                "start": seg_start,
                "end": seg_end,
                "word_index_start": index_start,
                "word_index_end": len(tokens),
                "confidence": sum(scores) / len(scores) if scores else None,
            }
        )

    write_stage_json(
        workdir,
        OUTPUT,
        VERSION,
        INPUTS,
        {
            "whisperx_version": importlib.metadata.version("whisperx"),
            "segments": segments,
            "words": tokens,
        },
    )
