use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{
    tcp::{OwnedReadHalf, OwnedWriteHalf},
    TcpStream,
};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::protocol::{ClientMsg, ServerMsg};

/// Reported to the TUI so it can render a "reconnecting in 4s (attempt N)"
/// banner. Kept here (not in `ui`) because the supervisor owns the lifecycle.
#[derive(Debug, Clone)]
pub enum ConnState {
    Connected,
    Reconnecting { attempt: u32, delay_secs: u32 },
}

/// Backoff schedule (seconds). Capped at 30. Resets to the start of the
/// sequence after any connection that stays up for more than `RESET_AFTER`.
const BACKOFF_SECS: &[u32] = &[1, 2, 4, 8, 16, 30];
const RESET_AFTER: Duration = Duration::from_secs(5);

pub async fn run(server: &str, nick: &str, theme_name: &str) -> Result<()> {
    // The UI owns these channels for its lifetime. The supervisor swaps in
    // fresh per-connection reader/writer tasks behind them, so the UI never
    // sees the disconnect — only ConnState transitions and a fresh stream
    // of ServerMsgs after each reconnect.
    let (net_tx, net_rx) = mpsc::channel::<ClientMsg>(64);
    let (srv_tx, srv_rx) = mpsc::channel::<ServerMsg>(128);
    let (state_tx, state_rx) = mpsc::unbounded_channel::<ConnState>();
    // TODO(handoff): wire `state_rx` into the TUI App so the status overlay
    // can render Connected / Reconnecting. The UI agent is restructuring
    // App concurrently; until then we just drop the receiver. ConnState
    // transitions are also emitted via `tracing::info!` for observability.
    let _ = state_rx;

    let server = server.to_string();
    let nick = nick.to_string();

    // Outbound queue lives across reconnects. We park it in an Option so the
    // supervisor can hand exclusive ownership of the Receiver to each
    // connect_and_run invocation.
    let mut net_rx_slot: Option<mpsc::Receiver<ClientMsg>> = Some(net_rx);
    let supervisor = {
        let server = server.clone();
        let nick = nick.clone();
        let srv_tx = srv_tx.clone();
        let state_tx = state_tx.clone();
        async move {
            let mut backoff_idx: usize = 0;
            let mut attempt: u32 = 0;

            loop {
                let net_rx = match net_rx_slot.take() {
                    Some(rx) => rx,
                    None => {
                        // Should be impossible: connect_and_run always returns
                        // the receiver back through the result.
                        return Err::<(), anyhow::Error>(anyhow!(
                            "supervisor: outbound queue lost"
                        ));
                    }
                };

                // TODO(phase2): once App.last_msg_id is wired, also send
                // ClientMsg::FetchHistory { room: last_room, after_id: last_msg_id_seen }
                // after the Join below to backfill anything we missed while
                // disconnected.
                let last_room = "general".to_string();

                let connect_start = Instant::now();
                let outcome = connect_and_run(
                    &server,
                    &nick,
                    &last_room,
                    srv_tx.clone(),
                    net_rx,
                    state_tx.clone(),
                )
                .await;

                let connected_for = connect_start.elapsed();

                match outcome {
                    ConnectOutcome::CleanShutdown => return Ok(()),
                    ConnectOutcome::Disconnected { net_rx, error } => {
                        net_rx_slot = Some(net_rx);

                        // A long-lived connection counts as a successful
                        // session; reset the backoff schedule.
                        if connected_for >= RESET_AFTER {
                            backoff_idx = 0;
                            attempt = 0;
                        }

                        attempt = attempt.saturating_add(1);
                        let delay_secs = BACKOFF_SECS[backoff_idx];
                        let delay = Duration::from_secs(delay_secs as u64);
                        backoff_idx = (backoff_idx + 1).min(BACKOFF_SECS.len() - 1);

                        warn!(
                            "client disconnected after {:?}: {} — reconnecting in {}s (attempt {})",
                            connected_for, error, delay_secs, attempt
                        );
                        let _ = state_tx.send(ConnState::Reconnecting {
                            attempt,
                            delay_secs,
                        });

                        tokio::time::sleep(delay).await;
                    }
                }
            }
        }
    };

    // Run the supervisor and the TUI concurrently. The TUI returns on
    // Ctrl+C / clean exit; when that happens we want the supervisor to
    // wind down too. Dropping `net_tx` will make the writer task exit,
    // which cascades into the supervisor's connect_and_run returning.
    let ui_fut = crate::ui::run(nick.clone(), theme_name.to_string(), net_tx, srv_rx);

    tokio::select! {
        ui_res = ui_fut => ui_res,
        sup_res = supervisor => sup_res,
    }
}

enum ConnectOutcome {
    /// The TUI dropped net_tx (clean shutdown). Stop the supervisor loop.
    CleanShutdown,
    /// Network / parse error. The receiver is returned so the supervisor can
    /// reuse it on the next attempt.
    Disconnected {
        net_rx: mpsc::Receiver<ClientMsg>,
        error: String,
    },
}

async fn connect_and_run(
    server: &str,
    nick: &str,
    last_room: &str,
    srv_tx: mpsc::Sender<ServerMsg>,
    mut net_rx: mpsc::Receiver<ClientMsg>,
    state_tx: mpsc::UnboundedSender<ConnState>,
) -> ConnectOutcome {
    let stream = match TcpStream::connect(server).await {
        Ok(s) => s,
        Err(e) => {
            return ConnectOutcome::Disconnected {
                net_rx,
                error: format!("connect: {e}"),
            };
        }
    };
    let (read_half, write_half) = stream.into_split();

    info!("client connected to {server}");
    let _ = state_tx.send(ConnState::Connected);

    // Send Join first thing — the server demands it before any other frame.
    let join = ClientMsg::Join {
        nick: nick.to_string(),
        room: last_room.to_string(),
    };

    // Internal channel between the writer task and the reader/select loop.
    // We don't reuse `net_rx` directly inside the writer because we want to
    // hand it back to the supervisor on disconnect.
    let (writer_tx, writer_rx) = mpsc::channel::<ClientMsg>(64);
    let _ = writer_tx.send(join).await;

    let (writer_done_tx, mut writer_done_rx) = mpsc::channel::<String>(1);
    let writer_handle = tokio::spawn(writer_task(write_half, writer_rx, writer_done_tx));

    let (reader_done_tx, mut reader_done_rx) = mpsc::channel::<String>(1);
    let reader_handle = tokio::spawn(reader_task(
        BufReader::new(read_half),
        srv_tx,
        reader_done_tx,
    ));

    // Pump messages from the supervisor-owned net_rx into the writer task.
    // The select! gives us a single place to observe: writer death, reader
    // death, or TUI shutdown (net_rx returning None).
    let disconnect_reason: Option<String> = loop {
        tokio::select! {
            msg = net_rx.recv() => {
                match msg {
                    Some(m) => {
                        if writer_tx.send(m).await.is_err() {
                            // Writer died; loop one more turn so the done channel fires.
                            break Some("writer channel closed".into());
                        }
                    }
                    None => {
                        // TUI dropped net_tx → clean shutdown.
                        drop(writer_tx);
                        writer_handle.abort();
                        reader_handle.abort();
                        return ConnectOutcome::CleanShutdown;
                    }
                }
            }
            reason = reader_done_rx.recv() => {
                break reason;
            }
            reason = writer_done_rx.recv() => {
                break reason;
            }
        }
    };

    // Tear down both halves and return the receiver to the supervisor.
    drop(writer_tx);
    reader_handle.abort();
    writer_handle.abort();

    ConnectOutcome::Disconnected {
        net_rx,
        error: disconnect_reason.unwrap_or_else(|| "unknown".into()),
    }
}

async fn reader_task(
    mut reader: BufReader<OwnedReadHalf>,
    tx: mpsc::Sender<ServerMsg>,
    done: mpsc::Sender<String>,
) {
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) => {
                let _ = done.send("server closed connection".into()).await;
                return;
            }
            Err(e) => {
                let _ = done.send(format!("read error: {e}")).await;
                return;
            }
            Ok(_) => match serde_json::from_str::<ServerMsg>(buf.trim()) {
                Ok(msg) => {
                    if tx.send(msg).await.is_err() {
                        let _ = done.send("server channel closed".into()).await;
                        return;
                    }
                }
                Err(e) => {
                    warn!("bad server msg: {e}");
                    let _ = done.send(format!("parse error: {e}")).await;
                    return;
                }
            },
        }
    }
}

async fn writer_task(
    mut writer: OwnedWriteHalf,
    mut rx: mpsc::Receiver<ClientMsg>,
    done: mpsc::Sender<String>,
) {
    while let Some(msg) = rx.recv().await {
        match serde_json::to_string(&msg) {
            Ok(mut line) => {
                line.push('\n');
                if let Err(e) = writer.write_all(line.as_bytes()).await {
                    let _ = done.send(format!("write error: {e}")).await;
                    return;
                }
            }
            Err(e) => warn!("serialize error: {e}"),
        }
    }
    // rx closed (supervisor tore down the connection).
    let _ = done.send("writer rx closed".into()).await;
}
