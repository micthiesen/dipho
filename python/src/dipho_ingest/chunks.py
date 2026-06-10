"""MFA chunk spans — a pure function of the words.json segment tier, so a
resume that skips straight to the manifest stage recomputes them
deterministically without re-running MFA (python/README.md)."""

from __future__ import annotations

MERGE_GAP = 0.3
PAD = 0.25


def chunk_spans(segments: list[dict], duration: float) -> list[dict]:
    """Merge segments with inter-gap < 300 ms, pad 250 ms both sides clamped
    to neighbors' midpoints and [0, duration]. Segment indices are kept so
    the MFA stage can recover each chunk's tokens; chunks never overlap."""
    groups: list[dict] = []
    for i, seg in enumerate(segments):
        if groups and seg["start"] - groups[-1]["end"] < MERGE_GAP:
            groups[-1]["end"] = max(groups[-1]["end"], seg["end"])
            groups[-1]["segment_indices"].append(i)
        else:
            groups.append({"start": seg["start"], "end": seg["end"], "segment_indices": [i]})

    chunks = []
    for i, g in enumerate(groups):
        lo = g["start"] - PAD
        if i > 0:
            lo = max(lo, (groups[i - 1]["end"] + g["start"]) / 2)
        hi = g["end"] + PAD
        if i + 1 < len(groups):
            hi = min(hi, (g["end"] + groups[i + 1]["start"]) / 2)
        chunks.append(
            {
                "start": max(lo, 0.0),
                "end": min(hi, duration),
                "segment_indices": g["segment_indices"],
            }
        )
    return chunks
