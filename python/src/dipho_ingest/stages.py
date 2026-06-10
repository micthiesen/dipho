"""Stage framework: atomic writes, input fingerprints, validity checks.

Integrity protocol (python/README.md): every stage writes `<name>.tmp`,
fsyncs the file, renames, fsyncs the directory. Each JSON stage embeds
`{"stage_schema_version": N, "input_fingerprint": "<sha256>"}` over the
upstream stage files it consumed. "Valid" = parses + version known +
fingerprint chain matches; a mismatch invalidates that stage and everything
downstream.
"""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path


def file_sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        while chunk := f.read(1 << 20):
            h.update(chunk)
    return h.hexdigest()


def input_fingerprint(workdir: Path, inputs: list[str]) -> str:
    """SHA-256 over the named upstream stage files, order-independent of
    caller: one `name\\0sha256\\n` line per file, sorted by name."""
    lines = sorted(f"{name}\0{file_sha256(workdir / name)}\n" for name in inputs)
    return hashlib.sha256("".join(lines).encode()).hexdigest()


def atomic_write_bytes(path: Path, data: bytes) -> None:
    tmp = path.with_name(path.name + ".tmp")
    with open(tmp, "wb") as f:
        f.write(data)
        f.flush()
        os.fsync(f.fileno())
    os.replace(tmp, path)
    dir_fd = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(dir_fd)
    finally:
        os.close(dir_fd)


def write_stage_json(workdir: Path, name: str, version: int, inputs: list[str], body: dict) -> None:
    doc = {
        "stage_schema_version": version,
        "input_fingerprint": input_fingerprint(workdir, inputs),
        **body,
    }
    atomic_write_bytes(workdir / name, json.dumps(doc, indent=1).encode())


def load_stage_json(workdir: Path, name: str, version: int, inputs: list[str]) -> dict | None:
    """The stage's output if valid, else None (missing, unparseable, unknown
    version, or broken fingerprint chain)."""
    path = workdir / name
    if not path.exists():
        return None
    try:
        doc = json.loads(path.read_text())
    except (json.JSONDecodeError, UnicodeDecodeError):
        return None
    if doc.get("stage_schema_version") != version:
        return None
    if not all((workdir / i).exists() for i in inputs):
        return None
    if doc.get("input_fingerprint") != input_fingerprint(workdir, inputs):
        return None
    return doc
