//! The playback player: a tokio actor owning the slave mpv (spawned once
//! per session) and its event stream. The app sends it audition and
//! preview requests; it answers through the one event mpsc with status
//! updates, including the measured seek round-trip — the M4 latency number
//! that answered the open rodio question.
//!
//! Two modes mirror DESIGN.md's `PlayerMode`: Audition plays a window of a
//! playback master (ab-loop or once); Preview plays the compiled `edl://`
//! timeline, and a reload after a recompile preserves the output position
//! (the actor tracks `time-pos` itself, so the app never sees the
//! per-frame property stream).

use std::time::Instant;

use anyhow::{Result, bail};
use serde_json::json;
use tokio::sync::mpsc::{self, UnboundedSender};

use super::event::Event;
use crate::mpv::{self, MpvClient, MpvEvent};

/// One audition: play [start, end] of a playback master, looping (ab-loop)
/// or once. The span is already resolved by the app (exact match span,
/// ±context, or the full utterance).
#[derive(Debug)]
pub struct Audition {
    pub master_path: String,
    pub start: f64,
    pub end: f64,
    pub looped: bool,
    /// What's playing, for the status line.
    pub label: String,
}

/// Load (or reload) the compiled preview timeline.
#[derive(Debug)]
pub struct Preview {
    /// The compiled `edl://` URI.
    pub uri: String,
    pub seek: PreviewSeek,
}

#[derive(Debug)]
pub enum PreviewSeek {
    /// Reload after a recompile: keep the current preview output position
    /// (clamped if the edit shrank under the playhead) and the current
    /// pause state.
    Resume { max: f64 },
    /// Play from an absolute output time.
    From(f64),
    /// Play a window once, then pause — the neighborhood replay after a
    /// trim/nudge.
    Window { start: f64, end: f64 },
}

#[derive(Debug)]
pub enum PlayerCmd {
    Audition(Audition),
    Preview(Preview),
    TogglePause,
    Stop,
}

pub struct PlayerHandle {
    pub(super) tx: mpsc::UnboundedSender<PlayerCmd>,
}

impl PlayerHandle {
    pub fn audition(&self, audition: Audition) {
        // A send error means the player died; its Failed update already
        // explains why in the status line.
        let _ = self.tx.send(PlayerCmd::Audition(audition));
    }

    pub fn preview(&self, preview: Preview) {
        let _ = self.tx.send(PlayerCmd::Preview(preview));
    }

    pub fn toggle_pause(&self) {
        let _ = self.tx.send(PlayerCmd::TogglePause);
    }

    pub fn stop(&self) {
        let _ = self.tx.send(PlayerCmd::Stop);
    }
}

/// Player status for the app/status line.
#[derive(Debug, Clone)]
pub enum PlayerUpdate {
    Ready {
        version: String,
    },
    Playing {
        label: String,
        looped: bool,
        /// Seek-command-to-playback-restart round trip, once measured.
        seek_ms: Option<u64>,
    },
    /// A non-looping window reached its end and was paused.
    Done,
    Stopped,
    Failed(String),
}

/// Spawn the slave mpv and the actor task. mpv startup happens on the
/// task, so the TUI never blocks on it; failures surface as a
/// `PlayerUpdate::Failed` in the status line.
pub fn spawn(events: UnboundedSender<Event>) -> PlayerHandle {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        if let Err(e) = run(rx, &events).await {
            let _ = events.send(Event::Player(PlayerUpdate::Failed(e.to_string())));
        }
    });
    PlayerHandle { tx }
}

const TIME_POS_ID: u64 = 1;
const EOF_ID: u64 = 2;

/// One resolved playback request, audition and preview alike: seek to
/// `start`; ab-loop to `end` when looped, auto-pause there when not (None:
/// play out to EOF, where keep-open pauses).
struct Job {
    target: String,
    start: f64,
    end: Option<f64>,
    looped: bool,
    /// Restore a paused playhead instead of playing (preview resume).
    start_paused: bool,
    label: String,
}

impl Job {
    fn audition(a: Audition) -> Self {
        Self {
            start: a.start,
            end: Some(a.end),
            looped: a.looped,
            start_paused: false,
            label: a.label,
            target: a.master_path,
        }
    }

    /// Resolve a preview request. A Resume reads the current position and
    /// pause state back from mpv with request round trips — deterministic,
    /// unlike racing the property-change stream. If a prior seek is still
    /// settling, the readback can report the pre-seek position — same
    /// accepted staleness as `Phase::Seeking`'s mis-timed measurement; the
    /// steady state is clean.
    async fn preview(p: Preview, client: &MpvClient) -> Result<Self> {
        let (start, end, start_paused) = match p.seek {
            PreviewSeek::Resume { max } => {
                let pos = client
                    .get_property("time-pos")
                    .await
                    .ok()
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let paused = client
                    .get_property("pause")
                    .await
                    .ok()
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                (pos.clamp(0.0, max), None, paused)
            }
            PreviewSeek::From(t) => (t, None, false),
            PreviewSeek::Window { start, end } => (start, Some(end), false),
        };
        Ok(Self {
            target: p.uri,
            start,
            end,
            looped: false,
            start_paused,
            label: "preview".to_string(),
        })
    }
}

/// Where the active job stands.
enum Phase {
    Idle,
    /// loadfile sent; seek on file-loaded.
    Loading(Job),
    /// Seek sent; playback-restart completes it. A superseding request
    /// can leave a stale restart in flight, mis-timing one measurement —
    /// accepted, the steady state (arrowing within one source) is clean.
    Seeking {
        job: Job,
        since: Instant,
    },
    Playing(Job),
}

async fn run(
    cmds: mpsc::UnboundedReceiver<PlayerCmd>,
    events: &UnboundedSender<Event>,
) -> Result<()> {
    let (slave, mpv_events) = mpv::spawn_slave(&[]).await?;
    let version = mpv::probe_version(&slave.client).await?;
    drive(&slave.client, mpv_events, cmds, events, version).await
}

/// The actor loop, separated from slave startup so tests can drive it
/// against a scripted fake socket.
async fn drive(
    client: &MpvClient,
    mut mpv_events: mpsc::UnboundedReceiver<MpvEvent>,
    mut cmds: mpsc::UnboundedReceiver<PlayerCmd>,
    events: &UnboundedSender<Event>,
    version: String,
) -> Result<()> {
    client.observe_property(TIME_POS_ID, "time-pos").await?;
    client.observe_property(EOF_ID, "eof-reached").await?;
    let send = |update: PlayerUpdate| {
        let _ = events.send(Event::Player(update));
    };
    send(PlayerUpdate::Ready { version });

    let mut current: Option<String> = None;
    let mut phase = Phase::Idle;
    loop {
        tokio::select! {
            cmd = cmds.recv() => {
                // The app is gone; drop the slave with the task.
                let Some(mut cmd) = cmd else { return Ok(()) };
                // Only the newest playback request matters when keystrokes
                // outpace mpv — but a Stop/TogglePause never supersedes or
                // gets superseded.
                let supersedable =
                    |c: &PlayerCmd| matches!(c, PlayerCmd::Audition(_) | PlayerCmd::Preview(_));
                loop {
                    if supersedable(&cmd) {
                        match cmds.try_recv() {
                            Ok(newer) if supersedable(&newer) => {
                                cmd = newer;
                                continue;
                            }
                            Ok(other) => {
                                // Handle the playback request now, the
                                // non-supersedable one next.
                                handle_cmd(client, cmd, &mut current, &mut phase, &send)
                                    .await?;
                                cmd = other;
                                continue;
                            }
                            Err(_) => {}
                        }
                    }
                    handle_cmd(client, cmd, &mut current, &mut phase, &send).await?;
                    match cmds.try_recv() {
                        Ok(next) => cmd = next,
                        Err(_) => break,
                    }
                }
            }
            event = mpv_events.recv() => {
                let Some(event) = event else { bail!("mpv event stream closed") };
                match event {
                    MpvEvent::Disconnected => bail!("mpv exited"),
                    MpvEvent::FileLoaded => {
                        if let Phase::Loading(job) = std::mem::replace(&mut phase, Phase::Idle) {
                            phase = start_playback(client, job, &send).await?;
                        }
                    }
                    MpvEvent::PlaybackRestart => {
                        if let Phase::Seeking { job, since } =
                            std::mem::replace(&mut phase, Phase::Idle)
                        {
                            send(PlayerUpdate::Playing {
                                label: job.label.clone(),
                                looped: job.looped,
                                seek_ms: Some(since.elapsed().as_millis() as u64),
                            });
                            phase = Phase::Playing(job);
                        }
                    }
                    MpvEvent::PropertyChange { id, data } => {
                        let finished = match &phase {
                            Phase::Playing(job) if !job.looped => match (id, job.end) {
                                // Past the window's end (small slack: the
                                // observation arrives a frame-ish late anyway).
                                (TIME_POS_ID, Some(end)) => {
                                    data.as_f64().is_some_and(|t| t >= end - 0.005)
                                }
                                // keep-open pauses at EOF before time-pos
                                // can reach a window end past file end.
                                (EOF_ID, _) => data == json!(true),
                                _ => false,
                            },
                            _ => false,
                        };
                        if finished {
                            client.set_property("pause", json!(true)).await?;
                            phase = Phase::Idle;
                            send(PlayerUpdate::Done);
                        }
                    }
                }
            }
        }
    }
}

async fn handle_cmd(
    client: &MpvClient,
    cmd: PlayerCmd,
    current: &mut Option<String>,
    phase: &mut Phase,
    send: &impl Fn(PlayerUpdate),
) -> Result<()> {
    let job = match cmd {
        PlayerCmd::Stop => {
            client.set_property("pause", json!(true)).await?;
            clear_ab_loop(client).await?;
            *phase = Phase::Idle;
            send(PlayerUpdate::Stopped);
            return Ok(());
        }
        PlayerCmd::TogglePause => {
            if current.is_some() {
                client.command(json!(["cycle", "pause"])).await?;
            }
            return Ok(());
        }
        PlayerCmd::Audition(audition) => Job::audition(audition),
        PlayerCmd::Preview(preview) => Job::preview(preview, client).await?,
    };
    client.set_property("pause", json!(true)).await?;
    if current.as_deref() == Some(job.target.as_str()) {
        *phase = start_playback(client, job, send).await?;
    } else {
        client
            .command(json!(["loadfile", job.target, "replace"]))
            .await?;
        *current = Some(job.target.clone());
        *phase = Phase::Loading(job);
    }
    Ok(())
}

/// The file is loaded: set or clear the ab-loop, exact-seek to the window
/// start, restore the pause state. `playback-restart` will complete the
/// latency measurement.
async fn start_playback(
    client: &MpvClient,
    job: Job,
    send: &impl Fn(PlayerUpdate),
) -> Result<Phase> {
    if job.looped {
        client.set_property("ab-loop-a", json!(job.start)).await?;
        client
            .set_property(
                "ab-loop-b",
                json!(job.end.expect("looped jobs carry an end")),
            )
            .await?;
    } else {
        clear_ab_loop(client).await?;
    }
    client
        .command(json!(["seek", job.start, "absolute+exact"]))
        .await?;
    let since = Instant::now();
    client
        .set_property("pause", json!(job.start_paused))
        .await?;
    send(PlayerUpdate::Playing {
        label: job.label.clone(),
        looped: job.looped,
        seek_ms: None,
    });
    Ok(Phase::Seeking { job, since })
}

async fn clear_ab_loop(client: &MpvClient) -> Result<()> {
    client.set_property("ab-loop-a", json!("no")).await?;
    client.set_property("ab-loop-b", json!("no")).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    /// A scripted mpv: replies success to every command (logging the
    /// command array, with reply data from `replies`) and writes injected
    /// event lines verbatim.
    fn fake_mpv_server(
        listener: UnixListener,
        replies: impl Fn(&Value) -> Value + Send + 'static,
    ) -> (mpsc::UnboundedSender<Value>, mpsc::UnboundedReceiver<Value>) {
        let (inject_tx, mut inject_rx) = mpsc::unbounded_channel::<Value>();
        let (log_tx, log_rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, mut write) = stream.into_split();
            let mut lines = BufReader::new(read).lines();
            loop {
                tokio::select! {
                    line = lines.next_line() => {
                        let Ok(Some(line)) = line else { break };
                        let msg: Value = serde_json::from_str(&line).unwrap();
                        let _ = log_tx.send(msg["command"].clone());
                        let reply = json!({
                            "request_id": msg["request_id"], "error": "success",
                            "data": replies(&msg["command"]),
                        });
                        if write.write_all(format!("{reply}\n").as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    event = inject_rx.recv() => {
                        let Some(event) = event else { break };
                        if write.write_all(format!("{event}\n").as_bytes()).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        (inject_tx, log_rx)
    }

    async fn next_cmd(log: &mut mpsc::UnboundedReceiver<Value>) -> Value {
        tokio::time::timeout(Duration::from_secs(5), log.recv())
            .await
            .expect("timed out waiting for a command")
            .expect("command log open")
    }

    async fn next_update(events: &mut mpsc::UnboundedReceiver<Event>) -> PlayerUpdate {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("timed out waiting for a player update")
            .expect("event channel open");
        match event {
            Event::Player(update) => update,
            _ => panic!("expected a player event"),
        }
    }

    fn audition(path: &str, start: f64, end: f64, looped: bool) -> PlayerCmd {
        PlayerCmd::Audition(Audition {
            master_path: path.to_string(),
            start,
            end,
            looped,
            label: "x".to_string(),
        })
    }

    struct Rig {
        inject: mpsc::UnboundedSender<Value>,
        log: mpsc::UnboundedReceiver<Value>,
        cmd_tx: mpsc::UnboundedSender<PlayerCmd>,
        events: mpsc::UnboundedReceiver<Event>,
    }

    async fn rig(
        dir: &std::path::Path,
        replies: impl Fn(&Value) -> Value + Send + 'static,
    ) -> (Rig, tokio::task::JoinHandle<Result<()>>) {
        let socket = dir.join("mpv.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (inject, log) = fake_mpv_server(listener, replies);
        let (client, mpv_events) = MpvClient::connect(&socket).await.unwrap();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, events) = mpsc::unbounded_channel();
        let driver = tokio::spawn(async move {
            drive(
                &client,
                mpv_events,
                cmd_rx,
                &event_tx,
                "mpv v0.41.0".to_string(),
            )
            .await
        });
        (
            Rig {
                inject,
                log,
                cmd_tx,
                events,
            },
            driver,
        )
    }

    async fn expect_startup(r: &mut Rig) {
        assert_eq!(
            next_cmd(&mut r.log).await,
            json!(["observe_property", TIME_POS_ID, "time-pos"])
        );
        assert_eq!(
            next_cmd(&mut r.log).await,
            json!(["observe_property", EOF_ID, "eof-reached"])
        );
        assert!(matches!(
            next_update(&mut r.events).await,
            PlayerUpdate::Ready { .. }
        ));
    }

    fn time_pos(t: f64) -> Value {
        json!({ "event": "property-change", "id": TIME_POS_ID, "name": "time-pos", "data": t })
    }

    #[tokio::test]
    async fn phase_machine_loads_seeks_plays_and_autopauses() {
        let dir = tempfile::tempdir().unwrap();
        let (mut r, driver) = rig(dir.path(), |_| Value::Null).await;
        let script = async {
            expect_startup(&mut r).await;

            // New file: pause, loadfile; seek waits for file-loaded.
            r.cmd_tx.send(audition("A", 1.0, 2.0, false)).unwrap();
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "pause", true])
            );
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["loadfile", "A", "replace"])
            );
            r.inject.send(json!({ "event": "file-loaded" })).unwrap();
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "ab-loop-a", "no"])
            );
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "ab-loop-b", "no"])
            );
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["seek", 1.0, "absolute+exact"])
            );
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "pause", false])
            );
            assert!(matches!(
                next_update(&mut r.events).await,
                PlayerUpdate::Playing { seek_ms: None, .. }
            ));

            // playback-restart completes the latency measurement.
            r.inject
                .send(json!({ "event": "playback-restart" }))
                .unwrap();
            assert!(matches!(
                next_update(&mut r.events).await,
                PlayerUpdate::Playing {
                    seek_ms: Some(_),
                    looped: false,
                    ..
                }
            ));

            // Past the window end: auto-pause, Done.
            r.inject.send(time_pos(2.5)).unwrap();
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "pause", true])
            );
            assert!(matches!(
                next_update(&mut r.events).await,
                PlayerUpdate::Done
            ));

            // Same file again, looped: ab-loop set, seek, no loadfile.
            r.cmd_tx.send(audition("A", 0.5, 0.8, true)).unwrap();
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "pause", true])
            );
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "ab-loop-a", 0.5])
            );
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "ab-loop-b", 0.8])
            );
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["seek", 0.5, "absolute+exact"])
            );
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "pause", false])
            );
            assert!(matches!(
                next_update(&mut r.events).await,
                PlayerUpdate::Playing { looped: true, .. }
            ));

            // A looped play never auto-pauses on time-pos.
            r.inject.send(time_pos(5.0)).unwrap();

            // Stop pauses and clears the loop.
            r.cmd_tx.send(PlayerCmd::Stop).unwrap();
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "pause", true])
            );
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "ab-loop-a", "no"])
            );
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "ab-loop-b", "no"])
            );
            assert!(matches!(
                next_update(&mut r.events).await,
                PlayerUpdate::Stopped
            ));
            // Closing the command channel ends the actor.
            drop(r.cmd_tx);
        };

        tokio::time::timeout(Duration::from_secs(10), script)
            .await
            .expect("test timed out");
        driver.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn preview_reload_preserves_position_and_pause_state() {
        let dir = tempfile::tempdir().unwrap();
        let (mut r, driver) = rig(dir.path(), |cmd| {
            if cmd == &json!(["get_property", "time-pos"]) {
                json!(3.25)
            } else if cmd == &json!(["get_property", "pause"]) {
                json!(true)
            } else {
                Value::Null
            }
        })
        .await;
        let script = async {
            expect_startup(&mut r).await;

            // First preview load plays from an absolute position.
            r.cmd_tx
                .send(PlayerCmd::Preview(Preview {
                    uri: "edl://v1".to_string(),
                    seek: PreviewSeek::From(0.0),
                }))
                .unwrap();
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "pause", true])
            );
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["loadfile", "edl://v1", "replace"])
            );
            r.inject.send(json!({ "event": "file-loaded" })).unwrap();
            for expected in [
                json!(["set_property", "ab-loop-a", "no"]),
                json!(["set_property", "ab-loop-b", "no"]),
                json!(["seek", 0.0, "absolute+exact"]),
                json!(["set_property", "pause", false]),
            ] {
                assert_eq!(next_cmd(&mut r.log).await, expected);
            }
            assert!(matches!(
                next_update(&mut r.events).await,
                PlayerUpdate::Playing { looped: false, .. }
            ));

            // The edit changed under the playhead: the reload reads the
            // position (the fake reports 3.25, the user paused) and
            // resumes there, clamped to the new (shorter) duration, still
            // paused.
            r.cmd_tx
                .send(PlayerCmd::Preview(Preview {
                    uri: "edl://v2".to_string(),
                    seek: PreviewSeek::Resume { max: 2.0 },
                }))
                .unwrap();
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["get_property", "time-pos"])
            );
            assert_eq!(next_cmd(&mut r.log).await, json!(["get_property", "pause"]));
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "pause", true])
            );
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["loadfile", "edl://v2", "replace"])
            );
            r.inject.send(json!({ "event": "file-loaded" })).unwrap();
            for expected in [
                json!(["set_property", "ab-loop-a", "no"]),
                json!(["set_property", "ab-loop-b", "no"]),
                json!(["seek", 2.0, "absolute+exact"]),
                json!(["set_property", "pause", true]),
            ] {
                assert_eq!(next_cmd(&mut r.log).await, expected);
            }
            assert!(matches!(
                next_update(&mut r.events).await,
                PlayerUpdate::Playing { .. }
            ));

            // A neighborhood-replay window on the same URI skips loadfile
            // and auto-pauses past its end.
            r.cmd_tx
                .send(PlayerCmd::Preview(Preview {
                    uri: "edl://v2".to_string(),
                    seek: PreviewSeek::Window {
                        start: 0.5,
                        end: 1.5,
                    },
                }))
                .unwrap();
            for expected in [
                json!(["set_property", "pause", true]),
                json!(["set_property", "ab-loop-a", "no"]),
                json!(["set_property", "ab-loop-b", "no"]),
                json!(["seek", 0.5, "absolute+exact"]),
                json!(["set_property", "pause", false]),
            ] {
                assert_eq!(next_cmd(&mut r.log).await, expected);
            }
            assert!(matches!(
                next_update(&mut r.events).await,
                PlayerUpdate::Playing { .. }
            ));
            // Seek completes; only then does the window end auto-pause.
            r.inject
                .send(json!({ "event": "playback-restart" }))
                .unwrap();
            assert!(matches!(
                next_update(&mut r.events).await,
                PlayerUpdate::Playing {
                    seek_ms: Some(_),
                    ..
                }
            ));
            r.inject.send(time_pos(1.6)).unwrap();
            assert_eq!(
                next_cmd(&mut r.log).await,
                json!(["set_property", "pause", true])
            );
            assert!(matches!(
                next_update(&mut r.events).await,
                PlayerUpdate::Done
            ));

            // TogglePause cycles when something is loaded.
            r.cmd_tx.send(PlayerCmd::TogglePause).unwrap();
            assert_eq!(next_cmd(&mut r.log).await, json!(["cycle", "pause"]));
            drop(r.cmd_tx);
        };

        tokio::time::timeout(Duration::from_secs(10), script)
            .await
            .expect("test timed out");
        driver.await.unwrap().unwrap();
    }
}
