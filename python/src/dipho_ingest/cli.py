"""Entry point stub. Pipeline (milestone: ingest):

media file -> WhisperX transcription + forced alignment (word + phoneme
timestamps) -> pyannote speaker diarization -> prosody features (f0, RMS)
-> JSON document on stdout (contract: python/README.md).
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
