use anyhow::Result;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{
    tcp::{OwnedReadHalf, OwnedWriteHalf},
    TcpStream,
};
use tokio::sync::mpsc;
use tracing::warn;

use crate::protocol::{ClientMsg, ServerMsg};

pub async fn run(server: &str, nick: &str) -> Result<()> {
    let stream = TcpStream::connect(server).await?;
    let (read_half, write_half) = stream.into_split();

    let (net_tx, net_rx) = mpsc::channel::<ClientMsg>(64);
    let (srv_tx, srv_rx) = mpsc::channel::<ServerMsg>(128);

    tokio::spawn(reader_task(BufReader::new(read_half), srv_tx));
    tokio::spawn(writer_task(write_half, net_rx));

    net_tx
        .send(ClientMsg::Join {
            nick: nick.to_string(),
            room: "general".to_string(),
        })
        .await?;

    crate::ui::run(nick.to_string(), net_tx, srv_rx).await
}

async fn reader_task(mut reader: BufReader<OwnedReadHalf>, tx: mpsc::Sender<ServerMsg>) {
    let mut buf = String::new();
    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => match serde_json::from_str::<ServerMsg>(buf.trim()) {
                Ok(msg) => {
                    if tx.send(msg).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("bad server msg: {e}"),
            },
        }
    }
}

async fn writer_task(mut writer: OwnedWriteHalf, mut rx: mpsc::Receiver<ClientMsg>) {
    while let Some(msg) = rx.recv().await {
        match serde_json::to_string(&msg) {
            Ok(mut line) => {
                line.push('\n');
                if writer.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
            Err(e) => warn!("serialize error: {e}"),
        }
    }
}
