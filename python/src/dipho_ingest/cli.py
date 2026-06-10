"""Stage orchestrator. The Rust caller creates the workdir (keyed by
origin_id) with `original.bin`, `master.mkv`, and `audio.wav` already
present; this CLI runs the analysis stages in order, skipping any whose
output is still valid (parses + version known + fingerprint chain matches),
and writes `manifest.json` last. Contract: python/README.md.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from . import diarize, manifest, mfa, prosody, transcribe, words
from .progress import Progress, claim_stdout

STAGES = [transcribe, words, mfa, diarize, prosody, manifest]


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="dipho-ingest",
        description="Analyze a staged ingest workdir and emit manifest.json.",
    )
    parser.add_argument("--workdir", required=True, type=Path, help="staged ingest work dir")
    args = parser.parse_args()

    progress = Progress(claim_stdout())
    workdir: Path = args.workdir
    if not (workdir / "audio.wav").exists():
        progress.error("setup", f"{workdir}/audio.wav missing — the Rust caller stages it")
        sys.exit(1)

    for stage in STAGES:
        if stage.valid(workdir):
            progress.tick(stage.NAME, 100, skipped=True)
            continue
        progress.tick(stage.NAME, 0)
        try:
            stage.run(workdir, progress)
        except Exception as e:
            progress.error(stage.NAME, str(e))
            sys.exit(1)
        progress.tick(stage.NAME, 100)
    progress.done()
