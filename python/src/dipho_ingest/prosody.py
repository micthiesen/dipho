"""Stage: prosody.npz — f0 (pyin), RMS dB, and 13-dim MFCC frames on one
10 ms grid (hop 160 samples at 16 kHz, librosa `center=True`; frame i
centered at t = i·hop). All three arrays have length 1 + floor(duration/hop);
the Rust loader rejects violations.

The parameters live in PARAMS and ship in the manifest tools block so
`dipho reingest --stale` detects parameter changes like tool-version
changes — changing them must bump VERSION here too.
"""

from __future__ import annotations

import io
import json
from pathlib import Path

from .progress import Progress
from .stages import atomic_write_bytes, input_fingerprint

NAME = "prosody"
OUTPUT = "prosody.npz"
VERSION = 1
INPUTS = ["audio.wav"]

SR = 16000
HOP = 160
PARAMS = {
    "fmin": 50,
    "fmax": 650,
    "hop": HOP / SR,
    "frame_length": 1024,
    "mfcc_n": 13,
    "mfcc_window": 0.025,
    "rms_window": 0.025,
}
_WINDOW = round(PARAMS["mfcc_window"] * SR)


def _read_meta(workdir: Path) -> dict | None:
    import numpy as np

    path = workdir / OUTPUT
    if not path.exists():
        return None
    try:
        with np.load(path) as npz:
            return {key: str(npz[key]) for key in ("stage_schema_version", "input_fingerprint", "params")}
    except Exception:
        return None


def valid(workdir: Path) -> bool:
    meta = _read_meta(workdir)
    return (
        meta is not None
        and meta["stage_schema_version"] == str(VERSION)
        and all((workdir / i).exists() for i in INPUTS)
        and meta["input_fingerprint"] == input_fingerprint(workdir, INPUTS)
    )


def run(workdir: Path, progress: Progress) -> None:
    import librosa
    import numpy as np
    import soundfile

    y, sr = soundfile.read(workdir / "audio.wav", dtype="float32")
    if sr != SR or y.ndim != 1:
        raise RuntimeError(f"analysis wav must be {SR} Hz mono, got {sr} Hz ndim={y.ndim}")
    n_frames = 1 + len(y) // HOP

    f0, _, _ = librosa.pyin(
        y,
        fmin=PARAMS["fmin"],
        fmax=PARAMS["fmax"],
        sr=SR,
        frame_length=PARAMS["frame_length"],
        hop_length=HOP,
        center=True,
    )
    f0 = np.nan_to_num(f0, nan=0.0).astype(np.float32)
    progress.tick(NAME, 60)

    rms = librosa.feature.rms(y=y, frame_length=_WINDOW, hop_length=HOP, center=True)[0]
    rms_db = (20.0 * np.log10(np.maximum(rms, 1e-10))).astype(np.float32)
    # order="C": the transposed view is Fortran-order, which np.save would
    # record and the Rust npz reader rejects.
    mfcc = librosa.feature.mfcc(
        y=y, sr=SR, n_mfcc=PARAMS["mfcc_n"], n_fft=_WINDOW, hop_length=HOP, center=True
    ).T.astype(np.float32, order="C")
    progress.tick(NAME, 90)

    for name, arr in (("f0", f0), ("rms_db", rms_db), ("mfcc", mfcc)):
        if len(arr) != n_frames:
            raise RuntimeError(f"{name} has {len(arr)} frames, expected {n_frames}")

    buf = io.BytesIO()
    np.savez(
        buf,
        f0=f0,
        rms_db=rms_db,
        mfcc=mfcc,
        stage_schema_version=np.array(str(VERSION)),
        input_fingerprint=np.array(input_fingerprint(workdir, INPUTS)),
        params=np.array(json.dumps(PARAMS)),
    )
    atomic_write_bytes(workdir / OUTPUT, buf.getvalue())
