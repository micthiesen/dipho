# Research: ratatui ecosystem (mid-2026) — architecture, 0.30.x, and supporting crates

Date: 2026-06-10. Target: dipho TUI on ratatui 0.30.1, macOS/Apple Silicon.

## Findings

### ratatui 0.30.x: what changed vs 0.29

Verified from the official highlights pages (ratatui.rs/highlights/v030 and /v0301):

- **Crate split (0.30.0).** The monolith became a workspace: `ratatui-core`
  (Buffer, Rect, Layout, Style, Widget trait — slow-moving, stability-focused),
  `ratatui-widgets` (all built-in widgets), `ratatui-crossterm` /
  `ratatui-termion` / `ratatui-termwiz` (backends), `ratatui-macros`. The
  `ratatui` facade crate re-exports everything, so **apps keep depending on
  `ratatui`**; only *widget library authors* should depend on `ratatui-core`.
  If dipho ever publishes a reusable widget (e.g. a waveform widget), depend
  on `ratatui-core` there.
- **Terminal lifecycle idioms.** `ratatui::init()` → `DefaultTerminal` and
  `ratatui::restore()` remain, plus a new **`ratatui::run(|terminal| ...)`**
  closure API that handles init/restore (incl. on error/panic) automatically.
  Recommended entrypoint:
  `fn main() -> Result<()> { ratatui::run(|term| App::new().run(term)) }`.
- **Crossterm versioning via feature flags**: `crossterm_0_28`, `crossterm_0_29`
  etc., latest is default. Style conversions changed from `From` impls to
  backend-specific traits (`FromCrossterm` / `IntoCrossterm`).
- **Breaking changes of note (0.30.0):** `Backend` trait gained an associated
  `Error` type and `clear_region()`; `Block::title` moved to `Line`-based
  titles; `WidgetRef` blanket impl reversed (implement `Widget for &MyWidget`);
  `Alignment` → `HorizontalAlignment`; layout cache is opt-in in core.
- **0.30.1 additions:** Block shadows, **filled-area rendering for Canvas and
  Chart** (relevant for waveform fill rendering), `Fill` widget,
  `Marker::Custom(char)` for Canvas/Chart, `Cell::column_span(n)` for Table,
  iterator-based buffer diffing (removes 40–50KB/frame temp allocations),
  incremental buffer apply/flush public APIs, MSRV 1.88.

### App architecture patterns in real apps

The ratatui book documents two sanctioned patterns
(ratatui.rs/concepts/application-patterns/): **Elm-style /
"The Elm Architecture"** (single Model, `Message` enum, pure-ish
`update(model, msg) -> model`, `view(model)`) and the **Component
architecture** (the `ratatui/templates` component template: per-component
`handle_events` → `Action` enum → `update` → `render`, with an async tokio +
crossterm `EventStream` runtime and an action mpsc channel). The component
template is explicitly "opinionated" and heavier (config, keybinding maps,
action dispatch loop).

How real apps actually do it (verified by reading source):

- **television** (fuzzy finder; tokio + nucleo + ratatui): spawns a tokio task
  whose loop does `tokio::select!` over a control channel, a tick interval,
  and crossterm event polling, forwarding typed events over mpsc to the app
  loop (`television/event.rs`, ~line 191). Rendering happens in a separate
  task; matching runs on nucleo's own background threadpool. Closest existing
  app to dipho's search-driven shape.
- **atuin** (interactive history search): hybrid — async fn with
  `tokio::task::spawn_blocking(|| event::poll(250ms))` inside
  `tokio::select!`, racing input readiness against background query updates;
  search engine is a `Box<dyn SearchEngine>` queried with `.await` on every
  input change (`crates/atuin/src/command/client/search/interactive.rs`).
- **gitui**: fully sync. A dedicated input thread polls crossterm and pushes
  `InputEvent` over an unbounded **crossbeam channel**; adaptive poll timeout
  (100ms after activity, 10s idle) plus condvar-gated pause for external
  editors (`src/input.rs`). Main loop selects over input/git-worker channels.
- **yazi**: tokio. Single `tokio::sync::mpsc::UnboundedReceiver<Event>` drained
  in `App::serve()`; events go through a `Dispatcher`; rendering is triggered
  by a `NEED_RENDER` atomic and **debounced ~10ms** with
  `select! { _ = sleep(t) => render, ev = drain => dispatch }`
  (`yazi-fm/src/app/app.rs`).

Takeaway: every serious app converges on the same skeleton regardless of
sync/async: **one owned `App` state struct, one merged `Event` enum, one
consumer loop, render-on-demand (dirty flag/debounce), and producers (input,
workers, IPC) feeding a single mpsc channel.** Async (tokio) is chosen when
the app already has async I/O to multiplex — which dipho does (mpv unix
socket, SQLite queries off-thread, ffmpeg child processes).

### mpv IPC from Rust

- **mpvipc** (crates.io, maintained at pvv.ntnu.no gitea): current versions are
  tokio-based; the `Mpv` handle is cheaply clonable and wraps an mpsc channel
  to a tokio task owning the `UnixStream`. Supports `get_property`,
  `set_property`, commands, and event/property-observation streams. There are
  also `mpv-ipc` (spawns mpv + IPC, tokio) and `mpvrc`. All are small/low-bus-
  factor crates.
- The protocol itself is trivial: newline-delimited JSON over
  `--input-ipc-server=<socket>`; requests carry `request_id`, asynchronous
  `event` objects (`property-change`, `playback-restart`, `end-file`, ...) are
  interleaved on the same stream. A hand-rolled client is ~150–250 lines with
  `tokio::net::UnixStream` + `tokio::io::BufReader::lines()` + `serde_json`,
  with a `HashMap<u64, oneshot::Sender>` for request/response correlation and
  an mpsc for events. Given dipho needs precise control (ab-loops, EDL
  loading, exact-seek), owning this code is low-cost and avoids dependency
  risk. Not verified: mpvipc's handling of mpv ≥0.38 event edge cases.

### Fuzzy finding / input widgets

- **nucleo 0.5.0** (helix project, MPL-2.0; last release Apr 2024 — stable,
  not abandoned: it's the matcher inside helix and television). High-level
  `Nucleo<T>` API: push items, set the pattern, matching runs on its own
  threadpool, UI reads lock-free **snapshots** — never blocks the render loop.
  Also `nucleo-matcher` for just the algorithm. `nucleo-picker` exists as a
  full picker TUI library if you want a canned UI (less applicable; dipho's
  picker is custom). Note: dipho's primary word search is **SQLite FTS5**, so
  nucleo's role is secondary — fuzzy-filtering already-fetched candidate lists
  (sources, speakers, EDL clips) client-side.
- **tui-input 0.15.3** (Apr 2026): single-line input state machine, backend-
  agnostic, actively maintained. Right size for a search box.
- **tui-textarea**: original rhysd crate stalled at 0.7.0 (Oct 2024,
  pre-0.30). The maintained successor is **`ratatui-textarea`**
  (github.com/ratatui/ratatui-textarea, a fork maintained in the ratatui org;
  v0.9.1, Apr 2026, ratatui 0.30-compatible). Only needed if dipho grows
  multi-line editing (it likely won't for MVP).
- `ratatui/tui-widgets` collection (tui-popup, tui-scrollview, tui-prompts,
  tui-big-text) — grab-bag, check per-widget 0.30 compatibility before use.

### Waveform display in-terminal

- **No maintained, dedicated waveform widget crate was found** (searched
  crates.io/lib.rs/GitHub topics). What exists: `scope-tui`
  (oscilloscope/vectorscope app, tui-rs era, Linux/pulseaudio-oriented — a
  code reference, not a dependency), spectrum visualizer apps
  (`terminal-vibes`, `lookas`), and `audio-visualizer` (renders to images/GUI,
  not terminal).
- Practical approach is built-in: a **custom widget over `Canvas` with
  `Marker::Braille`** (2×4 dots per cell = 8× vertical resolution) drawing
  min/max envelope columns from downsampled PCM; 0.30.1's filled-area Canvas
  rendering helps draw a filled envelope. `Sparkline` is a cheap v0 (unsigned
  u64 bins, no negative axis — fine for RMS envelope). `Chart` works but is
  axis/dataset-oriented overkill. Plan: compute per-column min/max/RMS bins
  from decoded PCM (ffmpeg → f32 samples) keyed by zoom level; this is also
  exactly the data the post-MVP cut-refinement DSP needs, so share the
  peaks-cache module.

### Audio playback crates (flag only)

- **rodio 0.22.2** (Mar 2026, RustAudio, cpal/CoreAudio underneath) is healthy.
  **kira** targets game audio (precise scheduling, tweens). Neither is needed:
  mpv over IPC already gives dipho seek-accurate audition with zero render,
  and audio-only audition is just mpv with `--no-video` / `lavfi` EDL. Only
  revisit rodio if mpv round-trip latency for sub-200ms diphone auditions
  proves annoying — then a rodio `Sink` fed pre-decoded PCM slices from the
  corpus would be the compelling case. Not verified: actual mpv seek-to-play
  latency on M4 Max; measure before adding any audio crate.

## Code pointers

- ratatui 0.30.0 highlights (crate split, `ratatui::run`, breaking changes):
  https://ratatui.rs/highlights/v030/
- ratatui 0.30.1 highlights (Canvas/Chart fill, buffer-diff perf):
  https://ratatui.rs/highlights/v0301/ ; full list:
  https://github.com/ratatui/ratatui/blob/main/BREAKING-CHANGES.md
- App patterns: https://ratatui.rs/concepts/application-patterns/component-architecture/ ,
  component template: https://github.com/ratatui/templates (component/),
  async template structure: https://ratatui.github.io/async-template/02-structure.html
- television event loop: https://github.com/alexpasmantier/television/blob/main/television/event.rs
  (`tokio::spawn` + `tokio::select!` over control channel / tick / poll, ~L191)
- atuin interactive loop: https://github.com/atuinsh/atuin/blob/main/crates/atuin/src/command/client/search/interactive.rs
  (`spawn_blocking(event::poll)` inside `tokio::select!`)
- gitui input thread: https://github.com/gitui-org/gitui/blob/master/src/input.rs
  (crossbeam channel, adaptive 100ms/10s polling)
- yazi render debounce: `yazi-fm/src/app/app.rs` in https://github.com/sxyazi/yazi
  (`NEED_RENDER` atomic, 10ms debounce select)
- nucleo: https://docs.rs/nucleo (0.5.0); picker lib: https://github.com/autobib/nucleo-picker
- tui-input: https://crates.io/crates/tui-input (0.15.3, Apr 2026)
- ratatui-textarea (maintained fork): https://github.com/ratatui/ratatui-textarea (0.9.1, Apr 2026)
- mpv IPC crates: https://crates.io/crates/mpvipc , https://crates.io/crates/mpv-ipc ;
  protocol: mpv docs `--input-ipc-server` JSON IPC
- waveform reference app: https://github.com/alemidev/scope-tui
- rodio: https://crates.io/crates/rodio (0.22.2, Mar 2026)

## Recommendation

**Architecture: Elm-flavored single-loop on tokio — not the component
template.** dipho is one screen-graph with a few panes (search, results,
timeline/EDL, waveform, status), not a plugin-style multi-screen app; the
component template's per-component action dispatch is more indirection than
the problem needs. Concretely:

```
src/
  main.rs        # ratatui::run(|term| tokio rt block_on(app::run(term)))
  app.rs         # App state (Mode, SearchState, EdlState, PlayerState), update(Event)
  event.rs       # enum Event { Term(crossterm::Event), Tick,
                 #   Mpv(MpvEvent), Db(DbResult), Job(JobUpdate) }
  ui/            # pure render fns: fn draw(frame, &App); widgets/waveform.rs
  mpv/           # child spawn (--input-ipc-server=/tmp/dipho-$pid.sock),
                 #   client task: UnixStream + serde_json,
                 #   request_id->oneshot map, events -> mpsc<Event::Mpv>
  db/            # rusqlite on spawn_blocking / dedicated thread,
                 #   queries -> mpsc<Event::Db>
  edl.rs         # EDL data model -> mpv EDL string / ffmpeg filtergraph
```

Event flow: producers (crossterm `EventStream` via `ratatui-crossterm`/
`crossterm` `event-stream` feature; mpv reader task; DB worker; ffmpeg job
watcher) all send into **one `tokio::sync::mpsc::UnboundedSender<Event>`**;
the single consumer loop does `update()` then renders behind a dirty flag with
yazi-style ~10ms debounce. mpv `property-change` observers (time-pos,
eof-reached) become `Event::Mpv` and drive the playhead cursor on the
waveform.

**Adopt now:** ratatui 0.30.1 (`ratatui::run` entrypoint), tokio, crossterm
(event-stream feature), tui-input (search box), serde_json + tokio
UnixStream (hand-rolled mpv client — skip mpvipc, the protocol is too simple
to take a low-bus-factor dependency for), rusqlite. Build the waveform as an
in-tree Canvas/braille widget (Sparkline as a half-day stopgap).

**Adopt later:** nucleo (client-side fuzzy filter over candidate lists once
FTS5 search works), ratatui-textarea (only if multi-line editing appears),
rodio (only if measured mpv audition latency for sub-second units is bad).
**Skip:** the component template, kira, any tui-rs-era widget crates.
