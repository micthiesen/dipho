# Research: antiboredom/videogrep (v2.3.0)

Source: https://github.com/antiboredom/videogrep — Python supercut tool by Sam Lavigne.
All code below verified directly against `master` (videogrep/__init__.py reports
`__version__ = "2.3.0"`) on 2026-06-10.

## Findings

### 1. The internal data model: a flat "composition" list

Everything in videogrep flows through one shape — a list of dicts:

```python
[{"file": str, "start": float, "end": float, "content": str}, ...]
```

`search()` produces it; every exporter (`create_supercut`, `export_mpv_edl`,
`export_m3u`, `export_xml`, `export_individual_clips`, `vtt.render`) consumes it.
This is exactly dipho's "flat EDL" MVP concept: span references in source-file
time, with no transforms. Times are float seconds throughout; no rational/frame
representation anywhere.

### 2. mpv EDL export (`export_mpv_edl`, videogrep/videogrep.py)

The entire exporter is ~7 lines:

```python
def export_mpv_edl(composition: List[dict], outputfile: str):
    lines = []
    lines.append("# mpv EDL v0")
    for c in composition:
        lines.append(f"{os.path.abspath(c['file'])},{c['start']},{c['end']-c['start']}")
    with open(outputfile, "w") as outfile:
        outfile.write("\n".join(lines))
```

Syntax emitted: header `# mpv EDL v0`, then one positional-parameter line per
clip: `<abs-filename>,<start-seconds>,<duration-seconds>` (note: third field is
**length, not end time** — `end - start`). Floats are written with Python's
default repr (e.g. `12.34,1.5600000000000005` is possible).

**Quirks / what it does NOT handle (verified by reading the code):**
- **No escaping at all.** mpv's EDL v0 spec (mpv DOCS/edl-mpv.rst) forbids `,`,
  `;`, `!`, `#`, newline in unquoted values and requires the length-prefix
  quoting syntax `%<byte_count>%<value>` (e.g. `%18%filename,with,.mkv`).
  videogrep writes filenames raw, so any source path containing a comma (common
  in YouTube titles) silently produces a broken EDL. Triggered by the spec being
  strict: "no superfluous whitespace... UNIX line breaks" required.
- Uses positional params only; never the named form (`start=`, `length=`,
  `title=`) and none of the `!` headers (`!no_chapters`, `!track_meta`,
  `!global_tags`).
- `os.path.abspath()` on each file — sensible, since the EDL may be opened from
  any cwd.

There is also a second, inline EDL emitter for `--preview` inside `videogrep()`:

```python
if preview:
    lines = [f"{s['file']},{s['start']},{s['end']-s['start']}" for s in segments]
    edl = "edl://" + ";".join(lines)
    subprocess.run(["mpv", edl])
```

i.e. the **`edl://` URL protocol form**, segments joined with `;`, passed as an
mpv argv. No IPC, no escaping, not abspath'd here (relies on cwd). This is the
"zero-render preview" in its most primitive form: launch a throwaway mpv per
preview. dipho's persistent mpv slave + `loadfile edl://...` over JSON IPC is a
strict upgrade (no process churn, seek/pause control, no shell-quoting issues
since IPC takes the URI as a JSON string).

### 3. Transcript handling

**Discovery** (`find_transcript`): looks for a sibling file sharing the video's
stem, trying extensions in order `[".json", ".vtt", ".srt", ".transcript"]`
(`SUB_EXTS`), with an optional `prefer=` to front-load one type. Match is by
regex over the directory listing, so `video.en.vtt` etc. also match.

**Parsing** → normalized to a list of "lines":
`{"content": str, "start": float, "end": float, "words": [{word, start, end}]?}`.
The `words` key is optional — only some sources provide it:

- **.json** (videogrep's own transcription cache): loaded verbatim; produced by
  `transcribe.py` using **Vosk/Kaldi** (`KaldiRecognizer` with
  `rec.SetWords(True)` for word timestamps; audio piped from ffmpeg as 16 kHz
  mono s16le). Word-accumulation into "lines" is by character count
  (`MAX_CHARS = 36`), not by sentence boundaries. Transcript is cached as
  `<video-stem>.json` and reused on subsequent runs — the same
  transcribe-once/search-many pattern dipho's ingest formalizes in SQLite.
- **.vtt** (videogrep/vtt.py): two paths. "Cued" YouTube auto-caption VTTs
  embed per-word timing tags `<00:00:01.234>`; `parse_cued()` regex-splits on
  `r"<(\d\d:\d\d:\d\d(\.\d+)?)>"` to recover word-level timestamps (a word's
  end = the next cue; first word inherits the line start; overlapping
  line-boundary words are clipped). Plain VTTs fall back to `parse_uncued()` —
  segment-level only. HTML stripped with BeautifulSoup.
- **.srt** (videogrep/srt.py): segment-level only, no word timing. Hand-rolled
  parser (BOM strip, drop index lines, split on `-->`).
- **.transcript**: legacy pocketsphinx format (sphinx.py), word-level.

**Search** (`search()` in videogrep.py) — pure runtime regex over the parsed
transcript, three modes:

- `sentence` (default): `re.search(query, line["content"])` per line; returns
  the whole line's span. Works without word timestamps.
- `fragment`: requires `words`. Flattens all words across lines, splits the
  query on spaces into N sub-patterns, then slides an N-wide window using the
  zip idiom `zip(*[words[i:] for i in range(len(queries))])` and requires
  `re.search(q, w["word"])` per position. Span = first word's `start` to last
  word's `end`. Note: each query token is itself a regex matched per-word, so
  `fragment` is effectively a word-level regex n-gram match — conceptually the
  same query dipho answers with SQL over a word/phoneme index instead of an
  O(total_words × query_len) scan per query.
- `mash`: requires `words`; for each query token, collect exact
  (case-insensitive) word matches, pick one at random — bag-of-words sentence
  assembly. This is a degenerate version of dipho's solver (random choice
  instead of target+join cost).

`get_ngrams()` exposes corpus stats: flattens words (or regex-splits `content`
when no word timing) and zips into n-grams; CLI prints `Counter(...).most_common(100)`.

**Mapping matches → spans, padding, overlap merging** (`pad_and_sync`):
`start -= padding; end += padding` (single `--padding` float, symmetric, default
0), plus `resync` shifts both by a constant (for misaligned subs); clamps
negatives to 0. Then merges any **overlapping or touching** consecutive segments
*from the same file* (`if prev_end >= start: out[-1]["end"] = end`). Segments
are sorted by start per file, so after padding, adjacent hits fuse into one
clip — important because naive padding otherwise re-plays overlapping audio.
A separate `remove_overlaps()` exists that merges without the same-file check.
Caveat: end times are clamped to media duration only in the moviepy paths
(`create_supercut`), **not** in the EDL/m3u/xml exporters.

### 4. Rendering

**MoviePy, not raw ffmpeg.** `create_supercut()` opens each distinct source
once (`VideoFileClip`/`AudioFileClip` dict keyed by filename), takes
`.subclip(start, end)` per segment, `concatenate_videoclips(cut_clips,
method="compose")`, writes with `codec="libx264"`, `audio_codec="aac"`, and a
uniquified `temp_audiofile` name. Audio-only outputs are inferred from MIME
types of inputs/output (`plan_video_output` / `plan_audio_output`) — `.mp3`
output from video input works; audio→video is refused.

**Lessons visible in the code (moviepy pain):**
- `create_supercut_in_batches()` exists solely because moviepy holds every
  decoded clip in memory: cuts are rendered 20 at a time (`BATCH_SIZE = 20`)
  to temp files, then those are concatenated, with manual `gc.collect()` and a
  bare `except Exception` that *silently drops a whole failing batch*.
- `cleanup_log_files()` sweeps stray `*ogg.log` temp files moviepy leaves
  behind.
- `method="compose"` is needed to tolerate heterogeneous source resolutions
  (letterboxes to the largest), at extra cost.

This whole subsystem is the strongest argument for dipho's plan: render via
ffmpeg directly (per-segment `-ss/-t` trims into a concat filter, or extract +
concat demuxer), which is streaming, constant-memory, and doesn't need batching
hacks.

**Other exporters worth noting:** `.m3u` with `#EXTVLCOPT:start-time/stop-time`
per entry (VLC preview), and FCP7 XML (`fcpxml.py`, ~"FCP interchange" XML that
Premiere/Resolve import) — confirms the value of EDL-as-data compiling to
multiple targets from one composition list. `vtt.render()` writes a WebVTT for
the *output* timeline by accumulating clip durations from 0 — a cheap, useful
trick (dipho gets subtitle/caption export of an edit nearly for free since it
knows the text of every span).

### 5. CLI shape (cli.py)

argparse; flags relevant as prior art: `-s/--search` (repeatable → list of
queries), `--search-type {sentence,fragment,mash}`, `--padding`, `--resyncsubs`,
`--max-clips`, `--randomize`, `--demo` (print matches, no render), `--preview`
(mpv `edl://`), output-extension-based dispatch (`.mpv.edl`, `.m3u`, `.xml`),
`--export-clips`, `--export-vtt`, `--ngrams N`, `--transcribe` (Vosk).

## Code pointers

- mpv EDL exporter: https://github.com/antiboredom/videogrep/blob/master/videogrep/videogrep.py — `export_mpv_edl()` (~line 583) and the `edl://` preview branch inside `videogrep()` (~line 672)
- Search modes + window-zip fragment matching: same file, `search()` (~line 191)
- Padding/resync + same-file overlap merge: `pad_and_sync()` (~line 145); `remove_overlaps()` (~line 120)
- Transcript discovery/dispatch: `find_transcript()` / `parse_transcript()` (~lines 25–88); `SUB_EXTS = [".json", ".vtt", ".srt", ".transcript"]`
- YouTube cued-VTT word-timestamp recovery: https://github.com/antiboredom/videogrep/blob/master/videogrep/vtt.py — `parse_cued()`; output-timeline VTT: `render()`
- SRT parser (segment-level only): https://github.com/antiboredom/videogrep/blob/master/videogrep/srt.py
- Vosk transcription + JSON cache + ffmpeg s16le pipe: https://github.com/antiboredom/videogrep/blob/master/videogrep/transcribe.py
- MoviePy rendering + batching workaround: videogrep.py `create_supercut()` (~line 388), `create_supercut_in_batches()` (~line 449), `BATCH_SIZE = 20`
- FCP XML exporter: https://github.com/antiboredom/videogrep/blob/master/videogrep/fcpxml.py
- CLI: https://github.com/antiboredom/videogrep/blob/master/videogrep/cli.py
- mpv EDL spec (for the escaping rules videogrep skips): https://github.com/mpv-player/mpv/blob/master/DOCS/edl-mpv.rst

## Recommendation

**Copy:**
1. **The composition shape.** `[{file, start, end, content}]` as the universal
   currency between search → pad/merge → export is exactly dipho's flat EDL.
   Keep `content` (matched text) on every span — it powers demo output,
   captions, and UI labels for free.
2. **EDL file + `edl://` as two compile targets** of the same composition. For
   the persistent mpv slave, prefer sending `loadfile "edl://..."` over JSON
   IPC rather than writing temp `.mpv.edl` files; keep file export as a
   user-facing artifact.
3. **`pad_and_sync` semantics**: symmetric padding, clamp at 0, then merge
   overlapping/touching spans *per source file* before compiling. Do this in
   the EDL compiler, not just in render.
4. **Output-timeline VTT generation** (cumulative durations from 0) — trivial
   and great for review.
5. **Multi-target export** (mpv EDL, m3u, FCP XML) from one EDL — validates
   dipho's EDL-as-data design; FCP XML is a cheap, high-leverage post-MVP add
   for "finish in a real NLE".

**Do differently:**
1. **Escape EDL values.** Implement mpv's `%<byte-count>%<value>` quoting for
   every filename (and any `title=`); videogrep breaks on commas in paths.
   Prefer named params (`start=`, `length=`) and emit `!no_chapters` (or
   conversely use auto-chapters as free per-clip nav markers in preview).
   Format floats explicitly (e.g. `{:.6f}`), don't rely on repr. Note the third
   positional field is **length**, not end.
2. **Render with ffmpeg, never moviepy.** Videogrep's batch-of-20 +
   gc.collect() + silent batch-drop machinery is pure moviepy damage. dipho's
   ffmpeg plan avoids the entire class of problems. Do copy the duration-clamp
   (`end = min(end, source_duration)`) into the compiler so EDL/render targets
   agree.
3. **Index, don't grep.** Videogrep re-parses transcripts and does O(n) regex
   window scans per query, per run; word-level timing is *optional* and absent
   for .srt sources, killing fragment search. dipho's SQLite corpus (FTS5
   words, phoneme/diphone tables, mandatory word- and phoneme-level alignment
   from WhisperX) makes fragment-equivalent queries indexed lookups and makes
   the `mash` mode's failure cases (exact-string-only matching, no homophone /
   pronunciation awareness) solvable properly.
4. **Better cut points.** Videogrep cuts at raw word timestamps and offers only
   a global symmetric padding knob — audible clicks and clipped consonants are
   the known result. dipho's diphone (mid-phoneme) cut points plus planned
   zero-crossing/spectral-flux refinement is precisely the fix; keep padding as
   a fallback knob but default to alignment-derived boundaries.
5. **`mash` mode is the solver's floor.** Random word selection with no join
   cost is what dipho's target+join-cost solver should demonstrably beat;
   useful as a baseline/`--random` mode and as a test fixture.
