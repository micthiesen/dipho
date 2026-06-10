"""Entry point stub. Pipeline (milestone: ingest):

analysis wav -> mlx-whisper transcription -> WhisperX word alignment ->
text normalization + MFA g2p/align subprocess (phone tier, rebased) ->
pyannote speaker turns -> pyin/RMS prosody -> staged work dir +
manifest.json, NDJSON progress on stdout (contract: python/README.md).
"""

import argparse
import sys


def main() -> None:
    parser = argparse.ArgumentParser(
        prog="dipho-ingest",
        description="Analyze a media file and emit alignment/diarization/features JSON.",
    )
    parser.add_argument("media", help="path to the media file to analyze")
    parser.parse_args()
    sys.exit("dipho-ingest: not implemented yet (milestone: ingest)")
