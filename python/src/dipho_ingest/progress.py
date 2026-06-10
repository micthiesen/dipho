"""NDJSON progress on stdout (python/README.md): `{"stage": ..., "pct": ...}`
per tick, terminal `{"done": true}` or `{"error": {"stage", "message"}}`.

Stdout is reserved for this protocol; `claim_stdout()` rebinds `sys.stdout`
to stderr so stray library prints can't corrupt the stream.
"""

from __future__ import annotations

import json
import sys
from typing import IO


def claim_stdout() -> IO[str]:
    out = sys.stdout
    sys.stdout = sys.stderr
    return out


class Progress:
    def __init__(self, out: IO[str]):
        self._out = out

    def _emit(self, obj: dict) -> None:
        self._out.write(json.dumps(obj) + "\n")
        self._out.flush()

    def tick(self, stage: str, pct: int, *, skipped: bool = False) -> None:
        obj: dict = {"stage": stage, "pct": pct}
        if skipped:
            obj["skipped"] = True
        self._emit(obj)

    def warn(self, stage: str, message: str) -> None:
        """Non-fatal degradation the user should see (e.g. an unalignable
        chunk). The stage still completes."""
        self._emit({"stage": stage, "warning": message})

    def done(self) -> None:
        self._emit({"done": True})

    def error(self, stage: str, message: str) -> None:
        self._emit({"error": {"stage": stage, "message": message}})
