"""Stage: phones.json — MFA forced alignment per padded chunk, rebased to
master time. MFA is the sole aligner, CLI-only in its own micromamba env
(`DIPHO_MFA_CMD` overrides the launch command). OOV tokens go through
`mfa g2p` into an augmented dictionary first.
"""

from __future__ import annotations

import os
import shlex
import shutil
import subprocess
from pathlib import Path

from . import words
from .chunks import chunk_spans
from .progress import Progress
from .stages import load_stage_json, write_stage_json

NAME = "align_phones"
OUTPUT = "phones.json"
VERSION = 2
INPUTS = ["audio.wav", words.OUTPUT]

ACOUSTIC = "english_us_arpa"
DICTIONARY = "english_us_arpa"
G2P = "english_us_arpa"
# Phones this close to a chunk edge get reduced confidence (DESIGN.md).
EDGE_S = 0.1
EDGE_CONFIDENCE = 0.5


def valid(workdir: Path) -> bool:
    return load_stage_json(workdir, OUTPUT, VERSION, INPUTS) is not None


def _mfa_cmd() -> list[str]:
    # Explicit env prefix: brew's micromamba roots its prefix in the Cellar,
    # so `-n mfa` would not resolve without MAMBA_ROOT_PREFIX.
    default = f"micromamba run -p {Path('~/micromamba/envs/mfa').expanduser()} mfa"
    return shlex.split(os.environ.get("DIPHO_MFA_CMD", default))


def _run_mfa(args: list[str]) -> None:
    cmd = _mfa_cmd() + args
    result = subprocess.run(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True)
    if result.returncode != 0:
        tail = "\n".join(result.stderr.strip().splitlines()[-8:])
        raise RuntimeError(f"{' '.join(cmd[:1] + args[:1])} failed (exit {result.returncode}):\n{tail}")


def _mfa_version() -> str:
    cmd = _mfa_cmd() + ["version"]
    result = subprocess.run(cmd, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(
            f"cannot run MFA via {' '.join(cmd)!r} — install it with "
            "`micromamba create -n mfa -c conda-forge montreal-forced-aligner` "
            "or set DIPHO_MFA_CMD (see python/README.md)"
        )
    return result.stdout.strip().splitlines()[-1]


def _dictionary_path() -> Path:
    root = Path(os.environ.get("MFA_ROOT_DIR", "~/Documents/MFA")).expanduser()
    path = root / "pretrained_models" / "dictionary" / f"{DICTIONARY}.dict"
    if not path.exists():
        raise RuntimeError(
            f"MFA dictionary not found at {path} — run "
            f"`mfa model download acoustic {ACOUSTIC}`, "
            f"`mfa model download dictionary {DICTIONARY}`, and "
            f"`mfa model download g2p {G2P}` (see python/README.md)"
        )
    return path


def _augmented_dictionary(tokens: set[str], scratch: Path) -> Path:
    """The pretrained dictionary plus G2P pronunciations for OOV tokens."""
    base = _dictionary_path()
    known = {line.split(maxsplit=1)[0] for line in base.read_text().splitlines() if line.strip()}
    oov = sorted(tokens - known)
    if not oov:
        return base
    (scratch / "oov.txt").write_text("".join(w + "\n" for w in oov))
    _run_mfa(
        ["g2p", str(scratch / "oov.txt"), G2P, str(scratch / "g2p.dict"), "--clean",
         "--temporary_directory", str(scratch / "tmp")]
    )
    augmented = scratch / "dict.txt"
    augmented.write_text(base.read_text() + (scratch / "g2p.dict").read_text())
    return augmented


def _parse_chunk(tg_path: Path, chunk: dict, chunk_tokens: list[tuple[int, dict]],
                 duration: float) -> list[dict]:
    from praatio import textgrid

    tg = textgrid.openTextgrid(str(tg_path), includeEmptyIntervals=True)
    word_iv = [e for e in tg.getTier("words").entries if e.label.strip()]
    if len(word_iv) != len(chunk_tokens):
        raise RuntimeError(
            f"{tg_path.name}: MFA aligned {len(word_iv)} words, expected {len(chunk_tokens)}"
        )

    def to_master(t: float) -> float:
        # Clamped to the chunk span so TextGrid rounding can never overlap a
        # neighboring chunk's phones (the loader rejects overlaps).
        return min(max(chunk["start"] + t, 0.0), chunk["end"], duration)

    phones = []
    for e in tg.getTier("phones").entries:
        if e.end <= e.start:
            continue
        label = e.label.strip()
        if not label or label in ("sil", "sp"):
            label, word_index = "SIL", None
        elif label == "spn":
            label, word_index = "NOISE", None
        else:
            # word_iv[k] is the aligned interval of chunk_tokens[k]; a real
            # phone belongs to the interval containing its midpoint.
            mid = (e.start + e.end) / 2
            word_index = next(
                (chunk_tokens[k][0] for k, iv in enumerate(word_iv)
                 if iv.start <= mid <= iv.end),
                None,
            )
        start, end = to_master(e.start), to_master(e.end)
        if end <= start:
            continue
        near_edge = e.start < EDGE_S or (chunk["end"] - chunk["start"]) - e.end < EDGE_S
        phones.append(
            {
                "label": label,
                "start": start,
                "end": end,
                "confidence": EDGE_CONFIDENCE if near_edge else 1.0,
                "word_index": word_index,
            }
        )
    return phones


def run(workdir: Path, progress: Progress) -> None:
    import soundfile

    words_doc = load_stage_json(workdir, words.OUTPUT, words.VERSION, words.INPUTS)
    assert words_doc is not None
    info = soundfile.info(workdir / "audio.wav")
    duration = float(info.duration)
    chunks = chunk_spans(words_doc["segments"], duration)

    scratch = workdir / "mfa"
    if scratch.exists():
        shutil.rmtree(scratch)
    corpus = scratch / "corpus"
    corpus.mkdir(parents=True)

    # One wav + .lab per chunk; chunk tokens are a contiguous global range
    # because tokens are ordered by segment.
    audio, sr = soundfile.read(workdir / "audio.wav", dtype="int16")
    per_chunk_tokens: list[list[tuple[int, dict]]] = []
    for i, chunk in enumerate(chunks):
        tokens = [
            (gi, tok)
            for si in chunk["segment_indices"]
            for gi, tok in enumerate(words_doc["words"])
            if tok["segment_index"] == si
        ]
        per_chunk_tokens.append(tokens)
        lo, hi = int(round(chunk["start"] * sr)), int(round(chunk["end"] * sr))
        soundfile.write(corpus / f"chunk-{i:04d}.wav", audio[lo:hi], sr)
        (corpus / f"chunk-{i:04d}.lab").write_text(" ".join(t["text"] for _, t in tokens))
    progress.tick(NAME, 10)

    all_tokens = {t["text"] for t in words_doc["words"]}
    dictionary = _augmented_dictionary(all_tokens, scratch)
    progress.tick(NAME, 25)

    mfa_version = _mfa_version()
    out = scratch / "out"
    _run_mfa(
        ["align", str(corpus), str(dictionary), ACOUSTIC, str(out), "--clean", "--overwrite",
         "-j", str(min(8, os.cpu_count() or 4)), "--temporary_directory", str(scratch / "tmp")]
    )
    progress.tick(NAME, 80)

    phones: list[dict] = []
    chunk_meta: list[dict] = []
    for i, chunk in enumerate(chunks):
        tg_path = out / f"chunk-{i:04d}.TextGrid"
        # MFA emits no TextGrid for an utterance it cannot align within its
        # retry beam (hallucinated transcripts, music, crosstalk — routine
        # in found footage). Degrade gracefully: no phones for that span, so
        # it stays word-searchable but never phone-addressable. A garbage
        # forced alignment would be worse than an honest gap.
        aligned = tg_path.exists()
        if aligned:
            phones.extend(_parse_chunk(tg_path, chunk, per_chunk_tokens[i], duration))
        else:
            progress.warn(
                NAME,
                f"chunk {i} ({chunk['start']:.2f}-{chunk['end']:.2f}s) could not be "
                f"aligned - no phones for this span",
            )
        chunk_meta.append({"start": chunk["start"], "end": chunk["end"], "aligned": aligned})

    write_stage_json(
        workdir,
        OUTPUT,
        VERSION,
        INPUTS,
        {
            "mfa_version": mfa_version,
            "acoustic_model": ACOUSTIC,
            "dictionary": DICTIONARY,
            "g2p_model": G2P,
            "chunks": chunk_meta,
            "phonemes": phones,
        },
    )
    shutil.rmtree(scratch)
