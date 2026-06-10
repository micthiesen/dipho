# Research: mpv as dipho's preview engine

Date: 2026-06-10. Primary sources: `DOCS/edl-mpv.rst` and `DOCS/man/ipc.rst` /
`DOCS/man/input.rst` from the mpv master branch (fetched verbatim), plus
real-world usage found via GitHub code search.

## Findings

### A. The mpv EDL format (`edl-mpv.rst`)

**Concept.** An EDL concatenates ranges (segments) of source files into one
continuous *virtual file* with a single unified timeline — not a playlist.
Seeking, duration, chapters, A-B loop all operate on the virtual timeline.
This is exactly what dipho's flat EDL needs for zero-render preview.

**Header.** First line must be exactly:

```
# mpv EDL v0
```

Format version 0 is explicitly "not frozen yet and may change any time" — pin
the mpv version dipho tests against and treat the EDL writer as a single
choke-point module.

**Segment syntax.** One segment per line (or `;`-separated — line feeds and
`;` are treated identically). Comma-separated parameters, positional or named:

```
# mpv EDL v0
f1.mkv,10,20          # file, start (s), length (s)
f2.mkv                # whole file (start=0, length=remaining duration)
f1.mkv,40,10
```

Grammar (verbatim from spec):

```
segment_entry ::= <param> ( <param> ',' )*
param         ::= [ <name> '=' ] ( <value> | '%' <number> '%' <valuebytes> )
```

- Positional params map to `file`, `start`, `length` in that order; named form
  is `name=value` (e.g. `f1.mkv,10,length=20`).
- Param *names* may not contain `=%,;\n!`; *values* may not contain `,;\n!`.
- Timestamps are floating-point seconds (an alternative
  `timestamps=chapters` mode exists; irrelevant for dipho).
- Format is strict: no extra whitespace, UNIX line breaks only, comments must
  start with `#` as the first character of a line.

**`%n%` quoting for arbitrary filenames.** Any value containing disallowed
characters must be length-prefixed: `%<number-of-BYTES>%<valuebytes>`. Note:
**byte** count, not char count — compute on UTF-8 bytes. Verbatim example
from the spec:

```
# mpv EDL v0
%18%filename,with,.mkv,10,length=20,param3=%13%value,escaped,param4=value2
```

(`file` = `filename,with,.mkv`, `start` = `10`, `length` = `20`.) Real-world
confirmation (occivink/mpv-music-player, Lua):
`string.format("%%%i%%%s", string.len(file), file)` then joined with `;` into
an `edl://` URI. dipho should `%n%`-quote *every* file path unconditionally —
it is always valid and removes an entire class of escaping bugs.

**Audio from one file, video from another: yes — `!new_stream`.** Header
lines start with `!`. The `!new_stream` header partitions the EDL into track
sets that play *simultaneously* instead of being appended:

```
# mpv EDL v0
video.mkv
!new_stream
audio.mkv
```

Each partition is its own segment timeline, so each partition can have its own
list of (file,start,length) segments. Effect is equivalent to loading the
second partition with `--external-file`, but with a unified cache. Caveats
from the spec: `!new_stream` must be the first header used; global metadata
comes from the first partition only; the header is "not part of the core EDL
format" and may change. This is the mechanism for dipho's eventual
audio/video-independent sentence mixing (dub one speaker's audio over
another's video).

**Other headers** (all documented, all marked implementation-specific except
`no_chapters`):
- `!no_chapters` — suppress chapter copying/generation. By default mpv
  inserts **one chapter per segment** titled with the filename (overridable
  per-segment with `title=...`). For dipho: chapters-per-segment is actually
  a *feature* — chapter navigation keys step between cuts — but emit
  `title=` per segment (the word/diphone label) for readable OSD.
- `!track_meta,lang=..,title=..,index=..` — per-partition track metadata.
- `!global_tags,key=value` — file-level tags.
- `!delay_open,media_type=video|audio|sub,codec=..,w=..,h=..,fps=..,samplerate=..,flags=default+forced`
  — defer opening the URL until the track is selected. Designed for remote
  single-track streams (ytdl DASH); spec warns multi-segment use "was never
  tested". **Not useful for dipho** (local files, instant open).
- `!mp4_dash,init=url` and `!no_clip` — internal ytdl/DASH plumbing; spec
  explicitly says `no_clip` "exists solely to support internal ytdl
  requirements" and "using no_clip with segments is not recommended and
  probably breaks". **Avoid.**
- `layout=this` per-segment option — when segments have heterogeneous track
  layouts, marks which segment defines the virtual file's track layout.
  Relevant once dipho mixes sources with/without video: put `layout=this` on
  a known-good segment.

**Loading without temp files.**
- `edl://` URI: inline EDL, same syntax minus the header line, `;` instead of
  newlines. Verbatim spec example:
  `edl://f1.mkv,length=5,start=10;f2.mkv,30,20;f3.mkv`
  Headers work inline too (e.g. `edl://!no_chapters;...` — the music-player
  example concatenates `%n%`-quoted paths this way). This is the documented,
  field-proven route and what dipho should use.
- `memory://data` protocol exists ("Use the `data` part as source data",
  mpv.rst PROTOCOLS). A `memory://# mpv EDL v0\n...` payload *should* be
  probed as an EDL file, but I did **not** verify this combination works;
  `edl://` makes it unnecessary.
- Security note in the spec: relative/absolute paths and protocol prefixes
  inside EDLs "may be prevented for security reasons" — this applies to EDLs
  reached from untrusted origins; locally-initiated `edl://` via IPC with
  absolute paths works (confirmed by real-world usage above).

### B. JSON IPC (`--input-ipc-server`)

**Transport.** Spawn mpv with `--input-ipc-server=/tmp/dipho-mpv-<pid>.sock`.
On macOS/Linux this is a unix domain socket (Windows uses named pipes —
irrelevant here). Gotcha: `sun_path` is ~104 bytes on macOS; keep the path
short (`/tmp` or `$TMPDIR`). The protocol has no auth and exposes `run`
(arbitrary command execution) — keep the socket 0600/private. Recommended
spawn flags for a slave: `--idle=yes --no-terminal --force-window=yes
--keep-open=yes` (idle keeps the process alive between loads; keep-open
prevents the window closing at EOF of a short preview).

**Wire format.** Newline-delimited UTF-8 JSON, both directions. Each message
is one line; literal `\n` must not appear inside a message (minify). Requests:

```json
{ "command": ["get_property", "time-pos"], "request_id": 100 }
```

Responses echo `request_id` (default 0 if omitted):

```json
{ "error": "success", "data": 1.468135, "request_id": 100 }
```

Optional `"async": true` runs the command without blocking the IPC queue;
reply arrives later with the same `request_id`. Named-argument form also
exists: `{ "command": {"name": "loadfile", "url": "..."} }`.

**Correlation/interleaving gotchas (all from ipc.rst):**
- Events are unsolicited lines `{ "event": "..." , ...}` and interleave
  freely with replies — the reader loop must dispatch on presence of
  `"event"` vs `"request_id"`/`"error"`.
- mpv does **not** service the socket while a synchronous command executes;
  events queued before the command are delivered before its reply. Replies to
  sync commands come back in order; async replies can reorder arbitrarily —
  always correlate by `request_id` (monotonic counter), never by order.
- Filenames/tags can contain invalid UTF-8, producing technically-invalid
  JSON from mpv; parse leniently (serde_json with lossy fallback or
  pre-filter).
- Closing the connection destroys the IPC client and **unregisters all
  observed properties** — hold one persistent connection for the lifetime of
  the preview session.

**Commands dipho needs (verified against input.rst):**

```jsonc
// Load an inline EDL (JSON array form = zero shell/input.conf quoting issues;
// commas and semicolons in the URI are fine because the URL is one array element)
{ "command": ["loadfile", "edl://%12%/path/a.mkv,1.04,0.31;%12%/path/b.mkv,9.80,0.22", "replace"], "request_id": 1 }
// Full signature: loadfile <url> [<flags> [<index> [<options>]]]
// flags: replace (default) | append | append-play | insert-next | insert-at | play (combinable: "append+play")
// NOTE mpv >= 0.38.0: an insert-at index is the THIRD arg; pass -1 there if
// you need the FOURTH arg (per-file options, "opt1=v1,opt2=v2") — e.g.
// ["loadfile", url, "replace", -1, "start=0,pause=yes"]. Version-gate this.

{ "command": ["set_property", "pause", true],  "request_id": 2 }
{ "command": ["seek", 12.5, "absolute+exact"], "request_id": 3 }
// seek flags: relative (default), absolute, absolute-percent, relative-percent,
// keyframes (fast), exact (precise hr-seek). dipho always wants absolute+exact.

{ "command": ["set_property", "ab-loop-a", 3.20], "request_id": 4 }
{ "command": ["set_property", "ab-loop-b", 3.95], "request_id": 5 }
// clear: set both to "no". (The `ab-loop` *command* just cycles A/B/clear at
// the current position — setting the properties directly is what dipho wants.)

{ "command": ["frame-step"], "request_id": 6 }       // advance 1 frame then pause
{ "command": ["frame-back-step"], "request_id": 7 }  // = frame-step -1 with `seek` flag
// frame-step [<frames>] [<flags>]; flags: play (default) | seek | mute

{ "command": ["observe_property", 1, "time-pos"], "request_id": 8 }
// -> { "event": "property-change", "id": 1, "name": "time-pos", "data": 4.31 }
// id is YOUR integer key, reused in unobserve_property.
```

**Events worth handling:** `file-loaded` (EDL parsed, duration known),
`playback-restart` (seek completed — gate UI updates on this, not on the seek
reply), `end-file`, `seek`, `property-change`. Frame-stepping in a paused
state emits `playback-restart` per step.

**Testing from a shell (macOS):**

```bash
mpv --idle --input-ipc-server=/tmp/mpvsock &
echo '{"command":["loadfile","edl://f1.mkv,10,5;f2.mkv,30,2"]}' | socat - /tmp/mpvsock
# NB: separate socat invocations each open+close a connection, which drops
# observed properties — fine for one-shot commands, wrong for observation.
```

## Code pointers

- EDL spec (read in full, verbatim): https://github.com/mpv-player/mpv/blob/master/DOCS/edl-mpv.rst
  — grammar lines 62–73; `%n%` example line 90; `!new_stream` lines 156–196;
  `!delay_open` lines 265–325; `edl://` URI lines 391–398; implicit chapters
  + `title=` lines 344–360; `layout=this` lines 362–389.
- JSON IPC spec: https://github.com/mpv-player/mpv/blob/master/DOCS/man/ipc.rst
- Command reference (loadfile/seek/frame-step/ab-loop): https://github.com/mpv-player/mpv/blob/master/DOCS/man/input.rst
- `memory://` / protocol list: https://github.com/mpv-player/mpv/blob/master/DOCS/man/mpv.rst (PROTOCOLS section, ~line 1473)
- Real-world `%n%`-quoted `edl://` built in code and sent over IPC:
  https://github.com/occivink/mpv-music-player/blob/master/scripts/music-client-main.lua (line ~460)
- EDL demuxer implementation (authoritative parser if docs are ambiguous):
  https://github.com/mpv-player/mpv/blob/master/demux/demux_edl.c
- Rust IPC crate option (not evaluated in depth): `mpvipc` /
  `mpv-client` crates exist, but the protocol is small enough that a ~200-line
  in-house client over `tokio::net::UnixStream` + serde_json is reasonable.

## Recommendation

1. **Preview path = `edl://` over JSON IPC, no temp files.** Compile dipho's
   EDL-as-data to a single-line `edl://` URI: `%n%`-quote every absolute file
   path unconditionally (byte-length prefix), join segments with `;`, send via
   `["loadfile", uri, "replace"]`. Keep one long-lived mpv process
   (`--idle=yes --keep-open=yes --no-terminal
   --input-ipc-server=$TMPDIR/dipho-mpv.sock`, short socket path, mode 0600)
   and reload the EDL on every edit — mpv re-opens local files instantly.
2. **Per-segment `title=`** with the word/diphone label; keep implicit
   chapters (free cut-point navigation + OSD). Use `!new_stream` later for
   audio-from-A/video-from-B; ignore `delay_open`/`no_clip`/`mp4_dash`.
3. **IPC client:** one persistent unix-socket connection; writer assigns
   monotonic `request_id`s and correlates replies via a pending-map; reader
   task dispatches `"event"` lines to a broadcast channel. Always seek with
   `absolute+exact`; audition a unit by setting `ab-loop-a`/`ab-loop-b`
   properties (clear with `"no"`); `frame-step`/`frame-back-step` for cut
   inspection; `observe_property` on `time-pos` and `pause`; treat
   `playback-restart` (not the seek reply) as "seek done".
4. **Pin and probe mpv version** at startup (`get_version` /
   `mpv-version` property): EDL v0 is unfrozen and `loadfile`'s argument
   order changed in 0.38.0 (index arg inserted third). Centralize EDL
   serialization in one Rust module with golden-file tests.
