//! The audition player: a tokio actor owning the slave mpv (spawned once
//! per session) and its event stream. The app sends it audition requests;
//! it answers through the one event mpsc with status updates, including
//! the measured seek round-trip — the M4 latency number that answers the
//! open rodio question.

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

#[derive(Debug)]
pub enum PlayerCmd {
    Audition(Audition),
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
    /// A non-looping audition reached its end and was paused.
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

/// Where the active audition stands. Mirrors DESIGN.md's audition mode of
/// `PlayerMode` — the preview half arrives with the EDL (M5).
enum Phase {
    Idle,
    /// loadfile sent; seek on file-loaded.
    Loading(Audition),
    /// Seek sent; playback-restart completes it. A superseding request
    /// can leave a stale restart in flight, mis-timing one measurement —
    /// accepted, the steady state (arrowing within one source) is clean.
    Seeking {
        audition: Audition,
        since: Instant,
    },
    Playing(Audition),
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
                // Only the newest request matters when keystrokes outpace mpv.
                while let Ok(newer) = cmds.try_recv() {
                    cmd = newer;
                }
                match cmd {
                    PlayerCmd::Stop => {
                        client.set_property("pause", json!(true)).await?;
                        clear_ab_loop(client).await?;
                        phase = Phase::Idle;
                        send(PlayerUpdate::Stopped);
                    }
                    PlayerCmd::Audition(audition) => {
                        client.set_property("pause", json!(true)).await?;
                        if current.as_deref() == Some(audition.master_path.as_str()) {
                            phase = start_playback(client, audition, &send).await?;
                        } else {
                            client
                                .command(json!(["loadfile", audition.master_path, "replace"]))
                                .await?;
                            current = Some(audition.master_path.clone());
                            phase = Phase::Loading(audition);
                        }
                    }
                }
            }
            event = mpv_events.recv() => {
                let Some(event) = event else { bail!("mpv event stream closed") };
                match event {
                    MpvEvent::Disconnected => bail!("mpv exited"),
                    MpvEvent::FileLoaded => {
                        if let Phase::Loading(audition) = std::mem::replace(&mut phase, Phase::Idle) {
                            phase = start_playback(client, audition, &send).await?;
                        }
                    }
                    MpvEvent::PlaybackRestart => {
                        if let Phase::Seeking { audition, since } =
                            std::mem::replace(&mut phase, Phase::Idle)
                        {
                            send(PlayerUpdate::Playing {
                                label: audition.label.clone(),
                                looped: audition.looped,
                                seek_ms: Some(since.elapsed().as_millis() as u64),
                            });
                            phase = Phase::Playing(audition);
                        }
                    }
                    MpvEvent::PropertyChange { id, data } => {
                        let finished = match &phase {
                            Phase::Playing(a) if !a.looped => match id {
                                // Past the window's end (small slack: the
                                // observation arrives a frame-ish late anyway).
                                TIME_POS_ID => {
                                    data.as_f64().is_some_and(|t| t >= a.end - 0.005)
                                }
                                // keep-open pauses at EOF before time-pos
                                // can reach a window end past file end.
                                EOF_ID => data == json!(true),
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

/// The file is loaded: set or clear the ab-loop, exact-seek to the window
/// start, unpause. `playback-restart` will complete the latency
/// measurement.
async fn start_playback(
    client: &MpvClient,
    audition: Audition,
    send: &impl Fn(PlayerUpdate),
) -> Result<Phase> {
    if audition.looped {
        client
            .set_property("ab-loop-a", json!(audition.start))
            .await?;
        client
            .set_property("ab-loop-b", json!(audition.end))
            .await?;
    } else {
        clear_ab_loop(client).await?;
    }
    client
        .command(json!(["seek", audition.start, "absolute+exact"]))
        .await?;
    let since = Instant::now();
    client.set_property("pause", json!(false)).await?;
    send(PlayerUpdate::Playing {
        label: audition.label.clone(),
        looped: audition.looped,
        seek_ms: None,
    });
    Ok(Phase::Seeking { audition, since })
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
    /// command array) and writes injected event lines verbatim.
    fn fake_mpv_server(
        listener: UnixListener,
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
                            "request_id": msg["request_id"], "error": "success", "data": null,
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

    #[tokio::test]
    async fn phase_machine_loads_seeks_plays_and_autopauses() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("mpv.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let (inject, mut log) = fake_mpv_server(listener);
        let (client, mpv_events) = MpvClient::connect(&socket).await.unwrap();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (event_tx, mut events) = mpsc::unbounded_channel();

        let driver = drive(
            &client,
            mpv_events,
            cmd_rx,
            &event_tx,
            "mpv v0.41.0".to_string(),
        );
        let script = async move {
            // Startup: both observes, then Ready.
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["observe_property", TIME_POS_ID, "time-pos"])
            );
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["observe_property", EOF_ID, "eof-reached"])
            );
            assert!(matches!(
                next_update(&mut events).await,
                PlayerUpdate::Ready { .. }
            ));

            // New file: pause, loadfile; seek waits for file-loaded.
            cmd_tx.send(audition("A", 1.0, 2.0, false)).unwrap();
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["set_property", "pause", true])
            );
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["loadfile", "A", "replace"])
            );
            inject.send(json!({ "event": "file-loaded" })).unwrap();
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["set_property", "ab-loop-a", "no"])
            );
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["set_property", "ab-loop-b", "no"])
            );
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["seek", 1.0, "absolute+exact"])
            );
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["set_property", "pause", false])
            );
            assert!(matches!(
                next_update(&mut events).await,
                PlayerUpdate::Playing { seek_ms: None, .. }
            ));

            // playback-restart completes the latency measurement.
            inject.send(json!({ "event": "playback-restart" })).unwrap();
            assert!(matches!(
                next_update(&mut events).await,
                PlayerUpdate::Playing {
                    seek_ms: Some(_),
                    looped: false,
                    ..
                }
            ));

            // Past the window end: auto-pause, Done.
            inject
                .send(json!({
                    "event": "property-change", "id": TIME_POS_ID, "name": "time-pos", "data": 2.5,
                }))
                .unwrap();
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["set_property", "pause", true])
            );
            assert!(matches!(next_update(&mut events).await, PlayerUpdate::Done));

            // Same file again, looped: ab-loop set, seek, no loadfile.
            cmd_tx.send(audition("A", 0.5, 0.8, true)).unwrap();
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["set_property", "pause", true])
            );
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["set_property", "ab-loop-a", 0.5])
            );
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["set_property", "ab-loop-b", 0.8])
            );
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["seek", 0.5, "absolute+exact"])
            );
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["set_property", "pause", false])
            );
            assert!(matches!(
                next_update(&mut events).await,
                PlayerUpdate::Playing { looped: true, .. }
            ));

            // A looped play never auto-pauses on time-pos.
            inject
                .send(json!({
                    "event": "property-change", "id": TIME_POS_ID, "name": "time-pos", "data": 5.0,
                }))
                .unwrap();

            // Stop pauses and clears the loop.
            cmd_tx.send(PlayerCmd::Stop).unwrap();
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["set_property", "pause", true])
            );
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["set_property", "ab-loop-a", "no"])
            );
            assert_eq!(
                next_cmd(&mut log).await,
                json!(["set_property", "ab-loop-b", "no"])
            );
            assert!(matches!(
                next_update(&mut events).await,
                PlayerUpdate::Stopped
            ));
            // Closing the command channel ends the actor.
            drop(cmd_tx);
        };

        let (result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(driver, script)
        })
        .await
        .expect("test timed out");
        result.unwrap();
    }
}
