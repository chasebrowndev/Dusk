use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use anyhow::Result;
use tracing::{info, warn};

use crate::protocol::{ChatMessage, ClientMsg, ServerMsg};

const HISTORY_LIMIT: usize = 200;
const DEFAULT_ROOM: &str = "general";

#[derive(Default)]
struct Room {
    history: VecDeque<ChatMessage>,
    members: Vec<SocketAddr>,
}

struct Client {
    nick: String,
    room: String,
    tx: mpsc::Sender<ServerMsg>,
}

struct State {
    rooms: HashMap<String, Room>,
    clients: HashMap<SocketAddr, Client>,
}

impl State {
    fn new() -> Self {
        let mut rooms = HashMap::new();
        rooms.insert(DEFAULT_ROOM.to_string(), Room::default());
        Self { rooms, clients: HashMap::new() }
    }

    fn ensure_room(&mut self, name: &str) {
        self.rooms.entry(name.to_string()).or_default();
    }

    fn room_senders(&self, room: &str, exclude: Option<SocketAddr>) -> Vec<mpsc::Sender<ServerMsg>> {
        let Some(r) = self.rooms.get(room) else { return vec![] };
        r.members.iter()
            .filter(|&&a| Some(a) != exclude)
            .filter_map(|a| self.clients.get(a).map(|c| c.tx.clone()))
            .collect()
    }

    fn room_list(&self) -> Vec<String> {
        let mut rooms: Vec<_> = self.rooms.keys().cloned().collect();
        rooms.sort();
        rooms
    }

    fn room_history(&self, room: &str) -> Vec<ChatMessage> {
        self.rooms.get(room)
            .map(|r| r.history.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn join_room(&mut self, room: &str, addr: SocketAddr) {
        self.ensure_room(room);
        let r = self.rooms.get_mut(room).unwrap();
        if !r.members.contains(&addr) {
            r.members.push(addr);
        }
    }

    fn leave_room(&mut self, room: &str, addr: SocketAddr) {
        if let Some(r) = self.rooms.get_mut(room) {
            r.members.retain(|&a| a != addr);
        }
    }

    fn push_history(&mut self, room: &str, msg: ChatMessage) {
        if let Some(r) = self.rooms.get_mut(room) {
            r.history.push_back(msg);
            while r.history.len() > HISTORY_LIMIT {
                r.history.pop_front();
            }
        }
    }
}

pub async fn run(bind: &str) -> Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!("dusk listening on {bind}");

    let state = Arc::new(Mutex::new(State::new()));

    loop {
        let (stream, addr) = listener.accept().await?;
        info!("connect {addr}");
        let state = state.clone();
        tokio::spawn(async move {
            if let Err(e) = handle(stream, addr, state).await {
                warn!("{addr} error: {e}");
            }
        });
    }
}

async fn handle(stream: TcpStream, addr: SocketAddr, state: Arc<Mutex<State>>) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let (tx, mut rx) = mpsc::channel::<ServerMsg>(64);

    tokio::spawn(async move {
        let mut w = write_half;
        while let Some(msg) = rx.recv().await {
            match serde_json::to_string(&msg) {
                Ok(mut line) => {
                    line.push('\n');
                    if w.write_all(line.as_bytes()).await.is_err() {
                        break;
                    }
                }
                Err(e) => warn!("serialize: {e}"),
            }
        }
    });

    let mut buf = String::new();
    let mut joined = false;

    loop {
        buf.clear();
        match reader.read_line(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }

        let msg: ClientMsg = match serde_json::from_str(buf.trim()) {
            Ok(m) => m,
            Err(e) => {
                let _ = tx.send(ServerMsg::Error { msg: format!("parse: {e}") }).await;
                continue;
            }
        };

        match msg {
            ClientMsg::Join { nick, room } => {
                let room = sanitize(&room);
                let history = {
                    let mut s = state.lock().await;
                    s.join_room(&room, addr);
                    s.clients.insert(addr, Client { nick: nick.clone(), room: room.clone(), tx: tx.clone() });
                    s.room_history(&room)
                };

                let _ = tx.send(ServerMsg::Joined { room: room.clone(), history }).await;
                joined = true;

                let senders = state.lock().await.room_senders(&room, Some(addr));
                for s in senders {
                    let _ = s.send(ServerMsg::UserJoined { room: room.clone(), nick: nick.clone() }).await;
                }

                let rooms = state.lock().await.room_list();
                let _ = tx.send(ServerMsg::RoomList { rooms }).await;
                info!("{nick} joined #{room}");
            }

            ClientMsg::Send { content } if joined => {
                let (nick, room) = {
                    let s = state.lock().await;
                    match s.clients.get(&addr) {
                        Some(c) => (c.nick.clone(), c.room.clone()),
                        None => continue,
                    }
                };

                let ts = chrono::Utc::now().timestamp();
                let chat = ChatMessage { nick: nick.clone(), content: content.clone(), ts };

                {
                    let mut s = state.lock().await;
                    s.push_history(&room, chat);
                }

                let server_msg = ServerMsg::Message { room: room.clone(), nick, content, ts };
                let senders = state.lock().await.room_senders(&room, None);
                for s in senders {
                    let _ = s.send(server_msg.clone()).await;
                }
            }

            ClientMsg::SwitchRoom { room } if joined => {
                let room = sanitize(&room);
                let (nick, old_room) = {
                    let s = state.lock().await;
                    match s.clients.get(&addr) {
                        Some(c) => (c.nick.clone(), c.room.clone()),
                        None => continue,
                    }
                };

                if room == old_room { continue; }

                {
                    let mut s = state.lock().await;
                    s.leave_room(&old_room, addr);
                    if let Some(c) = s.clients.get_mut(&addr) {
                        c.room = room.clone();
                    }
                }

                let old_senders = state.lock().await.room_senders(&old_room, None);
                for s in old_senders {
                    let _ = s.send(ServerMsg::UserLeft { room: old_room.clone(), nick: nick.clone() }).await;
                }

                let history = {
                    let mut s = state.lock().await;
                    s.join_room(&room, addr);
                    s.room_history(&room)
                };

                let _ = tx.send(ServerMsg::SwitchedRoom { room: room.clone(), history }).await;

                let new_senders = state.lock().await.room_senders(&room, Some(addr));
                for s in new_senders {
                    let _ = s.send(ServerMsg::UserJoined { room: room.clone(), nick: nick.clone() }).await;
                }
                info!("{nick} -> #{room}");
            }

            ClientMsg::CreateRoom { name } if joined => {
                let name = sanitize(&name);
                { state.lock().await.ensure_room(&name); }
                let rooms = state.lock().await.room_list();
                let _ = tx.send(ServerMsg::RoomList { rooms }).await;
            }

            ClientMsg::ListRooms => {
                let rooms = state.lock().await.room_list();
                let _ = tx.send(ServerMsg::RoomList { rooms }).await;
            }

            ClientMsg::Ping => {
                let _ = tx.send(ServerMsg::Pong).await;
            }

            _ => {
                let _ = tx.send(ServerMsg::Error { msg: "send Join first".into() }).await;
            }
        }
    }

    disconnect(addr, &state).await;
    Ok(())
}

async fn disconnect(addr: SocketAddr, state: &Arc<Mutex<State>>) {
    let (nick, room) = {
        let mut s = state.lock().await;
        let Some(c) = s.clients.remove(&addr) else { return };
        s.leave_room(&c.room, addr);
        (c.nick, c.room)
    };
    let senders = state.lock().await.room_senders(&room, None);
    for s in senders {
        let _ = s.send(ServerMsg::UserLeft { room: room.clone(), nick: nick.clone() }).await;
    }
    info!("{nick} disconnected");
}

fn sanitize(name: &str) -> String {
    let s: String = name
        .trim_start_matches('#')
        .chars()
        .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '_')
        .take(32)
        .collect::<String>()
        .to_lowercase();
    if s.is_empty() { DEFAULT_ROOM.to_string() } else { s }
}
