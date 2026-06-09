use anyhow::{anyhow, Result};
use tracing::warn;
use rusqlite::Connection;
use std::path::PathBuf;
use std::sync::mpsc;
use tokio::sync::oneshot;

use crate::protocol::ChatMessage;

pub struct Store {
    tx: mpsc::Sender<StoreCmd>,
}

enum StoreCmd {
    Insert {
        room: String,
        nick: String,
        content: String,
        ts: i64,
        reply: oneshot::Sender<Result<u64>>,
    },
    FetchRecent {
        room: String,
        limit: usize,
        reply: oneshot::Sender<Result<Vec<ChatMessage>>>,
    },
    FetchAfter {
        room: String,
        after_id: u64,
        reply: oneshot::Sender<Result<Vec<ChatMessage>>>,
    },
}

impl Store {
    pub fn open() -> Result<Self> {
        let path = db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(&path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                room TEXT NOT NULL,
                nick TEXT NOT NULL,
                content TEXT NOT NULL,
                ts INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_messages_room_id ON messages(room, id);",
        )?;

        let (tx, rx) = mpsc::channel::<StoreCmd>();

        std::thread::spawn(move || worker(conn, rx));

        Ok(Self { tx })
    }

    pub async fn insert(&self, room: &str, nick: &str, content: &str, ts: i64) -> Result<u64> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StoreCmd::Insert {
                room: room.to_string(),
                nick: nick.to_string(),
                content: content.to_string(),
                ts,
                reply,
            })
            .map_err(|_| anyhow!("store worker dead"))?;
        rx.await.map_err(|_| anyhow!("store reply dropped"))?
    }

    pub async fn fetch_recent(&self, room: &str, limit: usize) -> Result<Vec<ChatMessage>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StoreCmd::FetchRecent {
                room: room.to_string(),
                limit,
                reply,
            })
            .map_err(|_| anyhow!("store worker dead"))?;
        rx.await.map_err(|_| anyhow!("store reply dropped"))?
    }

    pub async fn fetch_after(&self, room: &str, after_id: u64) -> Result<Vec<ChatMessage>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StoreCmd::FetchAfter {
                room: room.to_string(),
                after_id,
                reply,
            })
            .map_err(|_| anyhow!("store worker dead"))?;
        rx.await.map_err(|_| anyhow!("store reply dropped"))?
    }
}

fn worker(conn: Connection, rx: mpsc::Receiver<StoreCmd>) {
    while let Ok(cmd) = rx.recv() {
        match cmd {
            StoreCmd::Insert {
                room,
                nick,
                content,
                ts,
                reply,
            } => {
                let res = (|| -> Result<u64> {
                    conn.execute(
                        "INSERT INTO messages (room, nick, content, ts) VALUES (?1, ?2, ?3, ?4)",
                        rusqlite::params![room, nick, content, ts],
                    )?;
                    Ok(conn.last_insert_rowid() as u64)
                })();
                let _ = reply.send(res);
            }
            StoreCmd::FetchRecent { room, limit, reply } => {
                let res = (|| -> Result<Vec<ChatMessage>> {
                    let mut stmt = conn.prepare(
                        "SELECT id, nick, content, ts FROM messages
                         WHERE room = ?1 ORDER BY id DESC LIMIT ?2",
                    )?;
                    let rows = stmt.query_map(
                        rusqlite::params![room, limit as i64],
                        |row| {
                            Ok(ChatMessage {
                                msg_id: row.get::<_, i64>(0)? as u64,
                                nick: row.get(1)?,
                                content: row.get(2)?,
                                ts: row.get(3)?,
                            })
                        },
                    )?;
                    let mut out: Vec<ChatMessage> = rows
                        .filter_map(|r| r.map_err(|e| warn!("store row: {e}")).ok())
                        .collect();
                    out.reverse();
                    Ok(out)
                })();
                let _ = reply.send(res);
            }
            StoreCmd::FetchAfter {
                room,
                after_id,
                reply,
            } => {
                let res = (|| -> Result<Vec<ChatMessage>> {
                    let mut stmt = conn.prepare(
                        "SELECT id, nick, content, ts FROM messages
                         WHERE room = ?1 AND id > ?2 ORDER BY id ASC",
                    )?;
                    let rows = stmt.query_map(
                        rusqlite::params![room, after_id as i64],
                        |row| {
                            Ok(ChatMessage {
                                msg_id: row.get::<_, i64>(0)? as u64,
                                nick: row.get(1)?,
                                content: row.get(2)?,
                                ts: row.get(3)?,
                            })
                        },
                    )?;
                    let out: Vec<ChatMessage> = rows
                        .filter_map(|r| r.map_err(|e| warn!("store row: {e}")).ok())
                        .collect();
                    Ok(out)
                })();
                let _ = reply.send(res);
            }
        }
    }
}

fn db_path() -> Result<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
        let mut p = PathBuf::from(xdg);
        p.push("dusk");
        p.push("dusk.db");
        return Ok(p);
    }
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME not set"))?;
    let mut p = PathBuf::from(home);
    p.push(".local");
    p.push("share");
    p.push("dusk");
    p.push("dusk.db");
    Ok(p)
}
