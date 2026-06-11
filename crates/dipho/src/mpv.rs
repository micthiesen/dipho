//! mpv JSON IPC client and slave lifecycle. dipho never renders video
//! itself: mpv runs as an external slave player spawned with
//! `--input-ipc-server=<socket>`, driven over one persistent UnixStream
//! connection per session (closing it would drop `observe_property`).
//! Replies are correlated by `request_id`, never message order; events are
//! forwarded to the owner's mpsc; `playback-restart` is the seek-done
//! signal (DESIGN.md).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{mpsc, oneshot};

/// mpv's EDL v0 format is unfrozen upstream; 0.38 is the ratified floor
/// (DESIGN.md), probed at startup so M5's EDL preview can rely on it.
const VERSION_FLOOR: (u64, u64) = (0, 38);

/// The mpv events dipho consumes. Everything else is dropped at the
/// reader, not forwarded.
#[derive(Debug)]
pub enum MpvEvent {
    FileLoaded,
    /// Playback (re)started after a seek or file load — the seek-done
    /// signal used for the audition latency measurement.
    PlaybackRestart,
    /// An observed property changed; `id` is the observe_property id.
    PropertyChange {
        id: u64,
        data: Value,
    },
    /// The IPC connection closed: mpv exited or crashed.
    Disconnected,
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// One persistent JSON IPC connection to a slave mpv.
pub struct MpvClient {
    next_id: AtomicU64,
    pending: Pending,
    writer: tokio::sync::Mutex<OwnedWriteHalf>,
}

impl MpvClient {
    /// Connect to an mpv IPC socket. Returns the client plus the stream of
    /// forwarded events; a reader task runs until the socket closes.
    pub async fn connect(
        socket: &Path,
    ) -> std::io::Result<(Self, mpsc::UnboundedReceiver<MpvEvent>)> {
        let stream = UnixStream::connect(socket).await?;
        let (read, write) = stream.into_split();
        let pending = Pending::default();
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        tokio::spawn(read_loop(read, pending.clone(), events_tx));
        let client = Self {
            next_id: AtomicU64::new(1),
            pending,
            writer: tokio::sync::Mutex::new(write),
        };
        Ok((client, events_rx))
    }

    /// Send one command (a JSON array, e.g. `["seek", 2.0, "absolute"]`)
    /// and await its reply, correlated by request_id.
    pub async fn command(&self, args: Value) -> Result<Value> {
        let what = args.to_string();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = json!({ "command": args, "request_id": id }).to_string();
        let written = {
            let mut writer = self.writer.lock().await;
            match writer.write_all(line.as_bytes()).await {
                Ok(()) => writer.write_all(b"\n").await,
                Err(e) => Err(e),
            }
        };
        if let Err(e) = written {
            // Never delivered: drop the pending entry instead of leaking it
            // until disconnect.
            self.pending.lock().unwrap().remove(&id);
            return Err(e.into());
        }
        match rx.await {
            Ok(Ok(data)) => Ok(data),
            Ok(Err(error)) => Err(anyhow!("mpv rejected {what}: {error}")),
            Err(_) => Err(anyhow!("mpv connection closed")),
        }
    }

    pub async fn get_property(&self, name: &str) -> Result<Value> {
        self.command(json!(["get_property", name])).await
    }

    pub async fn set_property(&self, name: &str, value: Value) -> Result<()> {
        self.command(json!(["set_property", name, value]))
            .await
            .map(|_| ())
    }

    pub async fn observe_property(&self, id: u64, name: &str) -> Result<()> {
        self.command(json!(["observe_property", id, name]))
            .await
            .map(|_| ())
    }
}

async fn read_loop(read: OwnedReadHalf, pending: Pending, events: mpsc::UnboundedSender<MpvEvent>) {
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(id) = msg.get("request_id").and_then(Value::as_u64) {
            let Some(tx) = pending.lock().unwrap().remove(&id) else {
                continue;
            };
            let error = msg.get("error").and_then(Value::as_str).unwrap_or("");
            let result = if error == "success" {
                Ok(msg.get("data").cloned().unwrap_or(Value::Null))
            } else {
                Err(error.to_string())
            };
            let _ = tx.send(result);
        } else if let Some(event) = msg.get("event").and_then(Value::as_str) {
            let forwarded = match event {
                "file-loaded" => Some(MpvEvent::FileLoaded),
                "playback-restart" => Some(MpvEvent::PlaybackRestart),
                "property-change" => {
                    msg.get("id")
                        .and_then(Value::as_u64)
                        .map(|id| MpvEvent::PropertyChange {
                            id,
                            data: msg.get("data").cloned().unwrap_or(Value::Null),
                        })
                }
                _ => None,
            };
            if let Some(event) = forwarded
                && events.send(event).is_err()
            {
                break;
            }
        }
    }
    // Socket closed: fail every in-flight command (their oneshot senders
    // drop with the map) and tell the consumer.
    pending.lock().unwrap().clear();
    let _ = events.send(MpvEvent::Disconnected);
}

/// A slave mpv process plus its IPC client. Dropping it kills the process
/// and removes the private socket directory.
pub struct MpvSlave {
    child: Child,
    sock_dir: PathBuf,
    pub client: MpvClient,
}

impl Drop for MpvSlave {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.sock_dir);
    }
}

static SLAVE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Spawn a slave mpv (idle, keep-open, no terminal) and connect to its IPC
/// socket. The socket lives in a fresh 0700 directory — IPC exposes `run`,
/// so it must not be reachable by other users — under a path short enough
/// for `sun_path` (~104 bytes on macOS).
pub async fn spawn_slave(
    extra_args: &[String],
) -> Result<(MpvSlave, mpsc::UnboundedReceiver<MpvEvent>)> {
    let name = format!(
        "dipho-mpv-{}-{}",
        std::process::id(),
        SLAVE_SEQ.fetch_add(1, Ordering::Relaxed)
    );
    let mut sock_dir = std::env::temp_dir().join(&name);
    // Budget the full socket path against sun_path, with headroom.
    if sock_dir.join("mpv.sock").as_os_str().len() > 100 {
        sock_dir = PathBuf::from("/tmp").join(&name);
    }
    std::fs::create_dir_all(&sock_dir)?;
    set_permissions(&sock_dir, 0o700)?;
    let socket = sock_dir.join("mpv.sock");

    let child = Command::new("mpv")
        .args(["--idle=yes", "--keep-open=yes", "--no-terminal"])
        .arg(format!("--input-ipc-server={}", socket.display()))
        .args(extra_args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawning mpv (is it installed?)");
    let mut child = match child {
        Ok(child) => child,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&sock_dir);
            return Err(e);
        }
    };

    // mpv creates the socket during startup; retry until it accepts.
    let deadline = Instant::now() + Duration::from_secs(10);
    let (client, events) = loop {
        match MpvClient::connect(&socket).await {
            Ok(pair) => break pair,
            Err(e) => {
                let died = child.try_wait().ok().flatten();
                if died.is_some() || Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = std::fs::remove_dir_all(&sock_dir);
                    match died {
                        Some(status) => bail!("mpv exited during startup ({status})"),
                        None => bail!("connecting to mpv socket: {e}"),
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    };
    let _ = set_permissions(&socket, 0o600);

    Ok((
        MpvSlave {
            child,
            sock_dir,
            client,
        },
        events,
    ))
}

fn set_permissions(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Probe the slave's version and enforce the ≥ 0.38 floor. Returns the
/// version string for display. An unparseable (e.g. git) build passes —
/// it cannot be older than the floor.
pub async fn probe_version(client: &MpvClient) -> Result<String> {
    let raw = client.get_property("mpv-version").await?;
    let version = raw.as_str().unwrap_or("unknown").to_string();
    if let Some((major, minor)) = parse_version(&version)
        && (major, minor) < VERSION_FLOOR
    {
        bail!(
            "{version} is older than the supported floor {}.{}",
            VERSION_FLOOR.0,
            VERSION_FLOOR.1
        );
    }
    Ok(version)
}

/// (major, minor) out of strings like "mpv 0.40.0-dirty" / "mpv v0.41.0".
fn parse_version(s: &str) -> Option<(u64, u64)> {
    let rest = s.strip_prefix("mpv").unwrap_or(s).trim_start();
    let rest = rest.strip_prefix('v').unwrap_or(rest);
    let mut parts = rest.split('.');
    let numeric = |part: &str| {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        digits.parse::<u64>().ok()
    };
    Some((numeric(parts.next()?)?, numeric(parts.next()?)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[test]
    fn version_strings_parse() {
        assert_eq!(parse_version("mpv 0.40.0-dirty"), Some((0, 40)));
        assert_eq!(parse_version("mpv v0.41.0"), Some((0, 41)));
        assert_eq!(parse_version("mpv 0.38.0"), Some((0, 38)));
        assert_eq!(parse_version("mpv git-2026"), None);
        assert_eq!(parse_version(""), None);
    }

    async fn fake_mpv() -> (PathBuf, UnixListener, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mpv.sock");
        let listener = UnixListener::bind(&path).unwrap();
        (path, listener, dir)
    }

    #[tokio::test]
    async fn replies_correlate_by_request_id_not_arrival_order() {
        let (path, listener, _dir) = fake_mpv().await;
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let first = lines.next_line().await.unwrap().unwrap();
            let second = lines.next_line().await.unwrap().unwrap();
            let id = |line: &str| {
                serde_json::from_str::<Value>(line).unwrap()["request_id"]
                    .as_u64()
                    .unwrap()
            };
            // Reply to the second command first.
            for (req, data) in [(id(&second), "second"), (id(&first), "first")] {
                let reply = json!({ "request_id": req, "error": "success", "data": data });
                write
                    .write_all(format!("{reply}\n").as_bytes())
                    .await
                    .unwrap();
            }
            // Keep the connection open until the client is done.
            lines.next_line().await.ok();
        });

        let (client, _events) = MpvClient::connect(&path).await.unwrap();
        let (a, b) = tokio::join!(
            client.command(json!(["get_property", "a"])),
            client.command(json!(["get_property", "b"])),
        );
        assert_eq!(a.unwrap(), json!("first"));
        assert_eq!(b.unwrap(), json!("second"));
        server.abort();
    }

    #[tokio::test]
    async fn error_replies_surface_the_mpv_error_string() {
        let (path, listener, _dir) = fake_mpv().await;
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            let line = lines.next_line().await.unwrap().unwrap();
            let id = serde_json::from_str::<Value>(&line).unwrap()["request_id"].clone();
            let reply = json!({ "request_id": id, "error": "invalid parameter" });
            write
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
            lines.next_line().await.ok();
        });

        let (client, _events) = MpvClient::connect(&path).await.unwrap();
        let err = client
            .command(json!(["seek", "nowhere"]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid parameter"), "{err}");
    }

    #[tokio::test]
    async fn events_are_forwarded_and_disconnect_is_signalled() {
        let (path, listener, _dir) = fake_mpv().await;
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (_read, mut write) = stream.into_split();
            for event in [
                json!({ "event": "file-loaded" }),
                json!({ "event": "playback-restart" }),
                json!({ "event": "property-change", "id": 7, "name": "time-pos", "data": 1.5 }),
                json!({ "event": "idle" }), // not forwarded
            ] {
                write
                    .write_all(format!("{event}\n").as_bytes())
                    .await
                    .unwrap();
            }
            // Dropping the stream closes the connection.
        });

        let (_client, mut events) = MpvClient::connect(&path).await.unwrap();
        assert!(matches!(events.recv().await, Some(MpvEvent::FileLoaded)));
        assert!(matches!(
            events.recv().await,
            Some(MpvEvent::PlaybackRestart)
        ));
        match events.recv().await {
            Some(MpvEvent::PropertyChange { id: 7, data }) => assert_eq!(data, json!(1.5)),
            other => panic!("expected property-change, got {other:?}"),
        }
        assert!(matches!(events.recv().await, Some(MpvEvent::Disconnected)));
    }

    /// Integration tests against a real mpv (and ffmpeg for fixtures) —
    /// run with `cargo test -p dipho -- --ignored`.
    mod integration {
        use super::*;
        use crate::ingest::normalize::fixtures;
        use std::fs;

        async fn wait_for(
            events: &mut mpsc::UnboundedReceiver<MpvEvent>,
            mut pred: impl FnMut(&MpvEvent) -> bool,
        ) -> MpvEvent {
            tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    let event = events.recv().await.expect("mpv event stream open");
                    if matches!(event, MpvEvent::Disconnected) || pred(&event) {
                        return event;
                    }
                }
            })
            .await
            .expect("timed out waiting for mpv event")
        }

        /// Build a fixture source, normalize it into a master + analysis
        /// wav, and return (master, wav-time of the beep onset).
        fn master_with_beep(
            dir: &Path,
            source: fn(&Path) -> (PathBuf, f64),
        ) -> (PathBuf, f64, f64) {
            let (source, expected) = source(dir);
            let wav = fixtures::run_normalize(dir, &source);
            let onset = fixtures::beep_onset(&wav);
            assert!(
                (onset - expected).abs() <= fixtures::TOLERANCE,
                "wav leg regressed: beep at {onset}, expected {expected}"
            );
            (dir.join("master.mkv"), onset, expected)
        }

        /// The M2-deferred mpv leg of the timebase integration tests:
        /// wav-time == mpv `time-pos` at the beep. mpv plays the master
        /// through `--ao=pcm`, whose output starts at time-pos 0, so the
        /// beep's position in the dump *is* its mpv time.
        async fn assert_mpv_clock_matches_wav(source: fn(&Path) -> (PathBuf, f64)) {
            let dir = tempfile::tempdir().unwrap();
            let (master, wav_onset, _) = master_with_beep(dir.path(), source);
            let dump = dir.path().join("mpv-dump.wav");
            let extra: Vec<String> = [
                "--vo=null",
                "--untimed",
                "--ao=pcm",
                &format!("--ao-pcm-file={}", dump.display()),
                "--audio-format=s16",
                "--audio-samplerate=16000",
                "--audio-channels=mono",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            let (slave, mut events) = spawn_slave(&extra).await.unwrap();
            probe_version(&slave.client).await.unwrap();
            slave
                .client
                .observe_property(1, "eof-reached")
                .await
                .unwrap();
            slave
                .client
                .command(json!(["loadfile", master.to_str().unwrap(), "replace"]))
                .await
                .unwrap();
            let eof = wait_for(
                &mut events,
                |e| matches!(e, MpvEvent::PropertyChange { id: 1, data } if data == &json!(true)),
            )
            .await;
            assert!(!matches!(eof, MpvEvent::Disconnected), "mpv died");

            let mpv_onset = fixtures::beep_onset(&dump);
            assert!(
                (mpv_onset - wav_onset).abs() <= fixtures::TOLERANCE,
                "mpv plays the beep at {mpv_onset}, wav time is {wav_onset}"
            );

            // Seek leg: an exact seek to the wav onset must report the
            // same clock back.
            slave
                .client
                .set_property("pause", json!(true))
                .await
                .unwrap();
            slave
                .client
                .command(json!(["seek", wav_onset, "absolute+exact"]))
                .await
                .unwrap();
            wait_for(&mut events, |e| matches!(e, MpvEvent::PlaybackRestart)).await;
            let pos = slave
                .client
                .get_property("time-pos")
                .await
                .unwrap()
                .as_f64()
                .unwrap();
            assert!(
                (pos - wav_onset).abs() <= fixtures::TOLERANCE,
                "time-pos {pos} after exact seek to {wav_onset}"
            );
        }

        #[tokio::test]
        #[ignore = "spawns mpv and ffmpeg (M2/M4 timebase integration test)"]
        async fn mpv_time_matches_wav_time_offset_fixture() {
            assert_mpv_clock_matches_wav(fixtures::offset_source).await;
        }

        #[tokio::test]
        #[ignore = "spawns mpv and ffmpeg (M2/M4 timebase integration test)"]
        async fn mpv_time_matches_wav_time_gapped_fixture() {
            assert_mpv_clock_matches_wav(fixtures::gapped_source).await;
        }

        /// The M4 latency measurement (ROADMAP): seek round-trip on an
        /// all-intra master, command write → playback-restart. Prints the
        /// distribution; the budget assertion is deliberately loose — the
        /// number itself answers the open rodio question. Set
        /// `DIPHO_LATENCY_MASTER=<path>` to measure a real corpus master
        /// instead of the tiny synthetic fixture.
        #[tokio::test]
        #[ignore = "spawns mpv and ffmpeg (M4 audition latency measurement)"]
        async fn seek_round_trip_latency() {
            let dir = tempfile::tempdir().unwrap();
            let master = match std::env::var("DIPHO_LATENCY_MASTER") {
                Ok(path) => PathBuf::from(path),
                Err(_) => master_with_beep(dir.path(), fixtures::offset_source).0,
            };
            let extra: Vec<String> = ["--vo=null", "--ao=null"]
                .iter()
                .map(|s| s.to_string())
                .collect();
            let (slave, mut events) = spawn_slave(&extra).await.unwrap();
            slave
                .client
                .command(json!(["loadfile", master.to_str().unwrap(), "replace"]))
                .await
                .unwrap();
            wait_for(&mut events, |e| matches!(e, MpvEvent::FileLoaded)).await;
            slave
                .client
                .set_property("pause", json!(true))
                .await
                .unwrap();
            let duration = slave
                .client
                .get_property("duration")
                .await
                .unwrap()
                .as_f64()
                .unwrap();

            let mut latencies_ms = Vec::new();
            let positions = [
                0.7, 0.1, 0.55, 0.22, 0.9, 0.04, 0.82, 0.4, 0.64, 0.17, 0.93, 0.3,
            ];
            for (i, frac) in positions.iter().enumerate() {
                let start = Instant::now();
                slave
                    .client
                    .command(json!(["seek", frac * duration, "absolute+exact"]))
                    .await
                    .unwrap();
                wait_for(&mut events, |e| matches!(e, MpvEvent::PlaybackRestart)).await;
                if i >= 2 {
                    // First seeks warm caches; measure the steady state.
                    latencies_ms.push(start.elapsed().as_millis() as u64);
                }
            }
            latencies_ms.sort_unstable();
            let median = latencies_ms[latencies_ms.len() / 2];
            println!(
                "seek round-trips on {} (ms): {latencies_ms:?}, median {median}",
                master.display()
            );
            assert!(
                median < 250,
                "median seek round-trip {median} ms — audition needs a rodio path"
            );
            let _ = fs::remove_dir_all(dir.path());
        }

        /// The M5 preview gate (ROADMAP): 100 × 200 ms EDL segments play
        /// without frame drops or audio gaps. Two passes over the same
        /// compiler-built EDL: an untimed `--ao=pcm` dump asserts how much
        /// audio mpv actually cuts per boundary (it snaps each segment's
        /// audio start forward to the next packet/frame boundary — this
        /// pass caught FLAC's 104 ms default frame size), then a realtime
        /// playback (a window opens) asserts mpv's drop counters and that
        /// the wall clock matches the dumped audio (a stall or gap would
        /// stretch it). Set `DIPHO_GATE_MASTER=<path>` to run against a
        /// real corpus master instead of the tiny synthetic fixture.
        #[tokio::test]
        #[ignore = "spawns mpv and ffmpeg, opens a window (M5 preview gate)"]
        async fn preview_gate_100_segments_play_gaplessly() {
            use dipho_core::edl::{Clip, Edl, SourceInfo, compile_mpv_edl};
            use dipho_core::span::{Channel, SourceId, Span};

            let dir = tempfile::tempdir().unwrap();
            let master = match std::env::var("DIPHO_GATE_MASTER") {
                Ok(path) => PathBuf::from(path),
                Err(_) => master_with_beep(dir.path(), fixtures::offset_source).0,
            };

            // Probe the master's duration through mpv itself.
            let (slave, mut events) = spawn_slave(&[]).await.unwrap();
            probe_version(&slave.client).await.unwrap();
            slave
                .client
                .set_property("pause", json!(true))
                .await
                .unwrap();
            slave
                .client
                .command(json!(["loadfile", master.to_str().unwrap(), "replace"]))
                .await
                .unwrap();
            wait_for(&mut events, |e| matches!(e, MpvEvent::FileLoaded)).await;
            let duration = slave
                .client
                .get_property("duration")
                .await
                .unwrap()
                .as_f64()
                .unwrap();
            let fps = slave
                .client
                .get_property("container-fps")
                .await
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(30.0);

            // 100 × 200 ms clips scattered through the master with a
            // golden-ratio stride, so no two are source-contiguous (the
            // compiler must emit all 100 segments — assert it).
            const N: usize = 100;
            const SLICE: f64 = 0.2;
            let source = SourceId(1);
            let clips: Vec<Clip> = (0..N)
                .map(|i| {
                    let t_start = (i as f64 * 0.618_034 * duration) % (duration - SLICE);
                    Clip {
                        span: Span {
                            source,
                            t_start,
                            t_end: t_start + SLICE,
                            channel: Channel::Both,
                        },
                        transforms: vec![],
                        provenance: None,
                        label: Some(format!("seg{i}")),
                    }
                })
                .collect();
            let edl = Edl {
                clips,
                sources: Default::default(),
            };
            let mut sources = dipho_core::edl::SourceMap::new();
            sources.insert(
                source,
                SourceInfo {
                    master_path: master.clone(),
                    duration,
                    fps: None,
                },
            );
            let compiled = compile_mpv_edl(&edl, &sources).unwrap();
            let plan = dipho_core::edl::plan_preview(&edl, &sources).unwrap();
            assert_eq!(
                plan.segments.len(),
                N,
                "a contiguity accident elided segments"
            );
            let timeline = plan.total_duration;
            drop(slave);
            drop(events);

            // Pass 1: untimed audio dump. mpv quantizes each segment's
            // audio start forward (≤ one video frame by DESIGN's preview
            // tolerance, now that master FLAC frames are smaller than a
            // frame), so the dump may run short by at most that per cut.
            let dump = dir.path().join("gate-dump.wav");
            let extra: Vec<String> = [
                "--vo=null",
                "--untimed",
                "--ao=pcm",
                &format!("--ao-pcm-file={}", dump.display()),
                "--audio-format=s16",
                "--audio-samplerate=16000",
                "--audio-channels=mono",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect();
            let (dumper, mut dump_events) = spawn_slave(&extra).await.unwrap();
            dumper
                .client
                .observe_property(1, "eof-reached")
                .await
                .unwrap();
            dumper
                .client
                .command(json!(["loadfile", compiled.uri(), "replace"]))
                .await
                .unwrap();
            let eof = wait_for(
                &mut dump_events,
                |e| matches!(e, MpvEvent::PropertyChange { id: 1, data } if data == &json!(true)),
            )
            .await;
            assert!(!matches!(eof, MpvEvent::Disconnected), "dump mpv died");
            drop(dumper);
            drop(dump_events);
            let audio = wav_duration(&dump);
            let cut_budget = N as f64 / fps;
            assert!(
                audio >= timeline - cut_budget && audio <= timeline + 0.1,
                "EDL audio {audio:.2} s vs timeline {timeline:.2} s —                  cuts lose more than a frame each (budget {cut_budget:.2} s)"
            );

            // Pass 2: realtime playback, default vo/ao.
            let (slave, mut events) = spawn_slave(&[]).await.unwrap();
            slave
                .client
                .observe_property(1, "eof-reached")
                .await
                .unwrap();
            slave
                .client
                .command(json!(["loadfile", compiled.uri(), "replace"]))
                .await
                .unwrap();
            wait_for(&mut events, |e| matches!(e, MpvEvent::FileLoaded)).await;
            slave
                .client
                .set_property("pause", json!(false))
                .await
                .unwrap();
            wait_for(&mut events, |e| matches!(e, MpvEvent::PlaybackRestart)).await;
            let started = Instant::now();
            let eof = wait_for(
                &mut events,
                |e| matches!(e, MpvEvent::PropertyChange { id: 1, data } if data == &json!(true)),
            )
            .await;
            assert!(!matches!(eof, MpvEvent::Disconnected), "mpv died");
            let wall = started.elapsed().as_secs_f64();

            let counter = |name: &str| {
                let client = &slave.client;
                let name = name.to_string();
                async move {
                    client
                        .get_property(&name)
                        .await
                        .ok()
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0)
                }
            };
            let vo_drops = counter("frame-drop-count").await;
            let decoder_drops = counter("decoder-frame-drop-count").await;
            println!(
                "preview gate on {}: timeline {timeline:.2} s, EDL audio {audio:.2} s, \
                 wall {wall:.2} s, vo drops {vo_drops}, decoder drops {decoder_drops}",
                master.display()
            );
            assert_eq!(vo_drops, 0, "vo dropped frames");
            assert_eq!(decoder_drops, 0, "decoder dropped frames");
            // Gapless pacing: realtime playback takes exactly as long as
            // the audio mpv decodes for this EDL — a stall, underrun, or
            // skipped segment shows up as a mismatch. The slack absorbs
            // EOF signalling and ao drain.
            assert!(
                (wall - audio).abs() < 1.0,
                "wall {wall:.2} s vs EDL audio {audio:.2} s — playback stalled or skipped"
            );
        }

        /// Duration of a 16 kHz mono s16le wav, from its data chunk.
        fn wav_duration(wav: &Path) -> f64 {
            let bytes = fs::read(wav).unwrap();
            let data_start = bytes
                .windows(4)
                .position(|w| w == b"data")
                .expect("wav data chunk")
                + 8;
            (bytes.len() - data_start) as f64 / 2.0 / 16_000.0
        }
    }
}
