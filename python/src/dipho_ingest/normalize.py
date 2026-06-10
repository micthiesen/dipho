"""Deterministic text normalization: digits/symbols → words (num2words),
punctuation stripping, lowercasing.

One WhisperX word expands to zero or more normalized tokens; the caller keeps
the token ↔ word mapping (`word_index` always refers to this mapping — never
a positional zip of two tokenizers). Raw-only tokens ("25") are not findable
by design; search normalizes the query, not the mapping.
"""

from __future__ import annotations

import re

from num2words import num2words

# Kept inside tokens: letters, digits (expanded below), apostrophes.
_STRIP = re.compile(r"[^a-z0-9']+")
_DIGITS = re.compile(r"\d+")
_ORDINAL = re.compile(r"^(\d+)(st|nd|rd|th)$")


def _expand_number(n: int, ordinal: bool) -> str:
    try:
        return num2words(n, to="ordinal" if ordinal else "cardinal")
    except (OverflowError, NotImplementedError):
        # Absurd magnitudes: spell digit by digit.
        return " ".join(num2words(int(d)) for d in str(n))


def normalize_word(raw: str) -> list[str]:
    """Normalized tokens for one WhisperX word. May be empty (pure
    punctuation) or several tokens ("25" → ["twenty", "five"])."""
    lowered = raw.lower()
    ordinal = _ORDINAL.match(_STRIP.sub("", lowered) or "")
    if ordinal:
        expanded = _expand_number(int(ordinal.group(1)), ordinal=True)
    else:
        # Expand each maximal digit run in place, then strip punctuation.
        expanded = _DIGITS.sub(lambda m: f" {_expand_number(int(m.group()), False)} ", lowered)
    parts = (_STRIP.sub(" ", p) for p in expanded.split())
    return [t for p in parts for t in p.split() if t.strip("'")]
