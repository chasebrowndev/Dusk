use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use anyhow::Result;
use tracing::{info, warn};

use crate::protocol::{ChatMessage, ClientMsg, ServerMsg, ShareKind, PROTOCOL_VERSION};
use crate::store::Store;

const HISTORY_LIMIT: usize = 200;
const DEFAULT_ROOM: &str = "general";

#[derive(Default)]
struct Room {
    history: VecDeque<ChatMessage>,
    members: Vec<SocketAddr>,
    /// Whether the hot cache was warmed from the store.
    warmed: bool,
}

struct Client {
    nick: String,
    room: String,
    tx: mpsc::Sender<ServerMsg>,
}

struct State {
    rooms: HashMap<String, Room>,
    clients: HashMap<SocketAddr, Client>,
    voice: HashMap<String, Vec<SocketAddr>>, // room -> voice members
    room_shares: HashMap<String, Vec<(String, ShareKind, String)>>, // room -> [(nick, kind, url)]
    store: Arc<Store>,
}

impl State {
    fn new(store: Arc<Store>) -> Self {
        let mut rooms = HashMap::new();
        rooms.insert(DEFAULT_ROOM.to_string(), Room::default());
        Self {
            rooms,
            clients: HashMap::new(),
            voice: HashMap::new(),
            room_shares: HashMap::new(),
            store,
        }
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

    fn all_senders(&self) -> Vec<mpsc::Sender<ServerMsg>> {
        self.clients.values().map(|c| c.tx.clone()).collect()
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

    fn room_nicks(&self, room: &str) -> Vec<String> {
        self.rooms.get(room).map(|r| {
            r.members.iter()
                .filter_map(|a| self.clients.get(a).map(|c| c.nick.clone()))
                .collect()
        }).unwrap_or_default()
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

    fn voice_join(&mut self, room: &str, addr: SocketAddr) {
        let members = self.voice.entry(room.to_string()).or_default();
        if !members.contains(&addr) {
            members.push(addr);
        }
    }

    fn voice_leave(&mut self, room: &str, addr: SocketAddr) {
        if let Some(members) = self.voice.get_mut(room) {
            members.retain(|&a| a != addr);
        }
    }

    fn voice_nicks(&self, room: &str) -> Vec<String> {
        self.voice.get(room).map(|members| {
            members.iter()
                .filter_map(|a| self.clients.get(a).map(|c| c.nick.clone()))
                .collect()
        }).unwrap_or_default()
    }

    fn voice_senders(&self, room: &str, exclude: Option<SocketAddr>) -> Vec<mpsc::Sender<ServerMsg>> {
        self.voice.get(room).map(|members| {
            members.iter()
                .filter(|&&a| Some(a) != exclude)
                .filter_map(|a| self.clients.get(a).map(|c| c.tx.clone()))
                .collect()
        }).unwrap_or_default()
    }

    fn push_history(&mut self, room: &str, msg: ChatMessage) {
        if let Some(r) = self.rooms.get_mut(room) {
            r.history.push_back(msg);
            while r.history.len() > HISTORY_LIMIT {
                r.history.pop_front();
            }
        }
    }

    fn room_warmed(&self, room: &str) -> bool {
        self.rooms.get(room).map(|r| r.warmed).unwrap_or(false)
    }

    fn set_warmed(&mut self, room: &str, history: Vec<ChatMessage>) {
        if let Some(r) = self.rooms.get_mut(room) {
            r.history = history.into_iter().collect();
            while r.history.len() > HISTORY_LIMIT {
                r.history.pop_front();
            }
            r.warmed = true;
        }
    }

    fn add_share(&mut self, room: &str, nick: &str, kind: ShareKind, url: String) {
        let v = self.room_shares.entry(room.to_string()).or_default();
        v.retain(|(n, k, _)| !(n == nick && *k == kind));
        v.push((nick.to_string(), kind, url));
    }

    fn remove_share(&mut self, room: &str, nick: &str, kind: Option<ShareKind>) {
        if let Some(v) = self.room_shares.get_mut(room) {
            match kind {
                Some(k) => v.retain(|(n, kk, _)| !(n == nick && *kk == k)),
                None => v.retain(|(n, _, _)| n != nick),
            }
        }
    }

    fn room_active_shares(&self, room: &str) -> Vec<(String, ShareKind, String)> {
        self.room_shares.get(room).cloned().unwrap_or_default()
    }
}

/// Lazily warm the hot cache for a room from SQLite. Acquires the store
/// handle without holding the state lock, then writes results back.
async fn warm_room(state: &Arc<Mutex<State>>, room: &str) {
    let (store, needed) = {
        let s = state.lock().await;
        (s.store.clone(), !s.room_warmed(room))
    };
    if !needed {
        return;
    }
    match store.fetch_recent(room, HISTORY_LIMIT).await {
        Ok(history) => {
            let mut s = state.lock().await;
            s.set_warmed(room, history);
        }
        Err(e) => warn!("warm {room}: {e}"),
    }
}

pub async fn run(bind: &str) -> Result<()> {
    let listener = TcpListener::bind(bind).await?;
    info!("dusk listening on {bind}");

    let store = Arc::new(Store::open()?);
    let state = Arc::new(Mutex::new(State::new(store)));

    loop {
        let (stream, addr) = listener.accept().await?;
        if let Err(e) = stream.set_nodelay(true) {
            warn!("set_nodelay failed for {addr}: {e}");
        }
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

    // Unsolicited Hello on raw TCP accept.
    let _ = tx.send(ServerMsg::Hello { version: PROTOCOL_VERSION.to_string() }).await;

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
            // Ping works pre-Join (discovery handshake).
            ClientMsg::Ping => {
                let _ = tx.send(ServerMsg::Pong { version: PROTOCOL_VERSION.to_string() }).await;
            }

            ClientMsg::Join { nick, room } => {
                let room = sanitize(&room);
                let room_was_new = {
                    let s = state.lock().await;
                    !s.rooms.contains_key(&room)
                };

                warm_room(&state, &room).await;

                let (history, users) = {
                    let mut s = state.lock().await;
                    s.join_room(&room, addr);
                    s.clients.insert(addr, Client { nick: nick.clone(), room: room.clone(), tx: tx.clone() });
                    (s.room_history(&room), s.room_nicks(&room))
                };

                let _ = tx.send(ServerMsg::Joined { room: room.clone(), history, users: users.clone() }).await;
                joined = true;

                let senders = {
                    let s = state.lock().await;
                    s.room_senders(&room, Some(addr))
                };
                for s in senders {
                    let _ = s.send(ServerMsg::UserJoined { room: room.clone(), nick: nick.clone(), users: users.clone() }).await;
                }

                let (rooms, all) = {
                    let s = state.lock().await;
                    (s.room_list(), s.all_senders())
                };
                if room_was_new {
                    // New room created via Join — notify everyone.
                    for s in all {
                        let _ = s.send(ServerMsg::RoomList { rooms: rooms.clone() }).await;
                    }
                } else {
                    let _ = tx.send(ServerMsg::RoomList { rooms }).await;
                }

                // Replay any in-progress shares so late joiners see them.
                let active_shares = {
                    let s = state.lock().await;
                    s.room_active_shares(&room)
                };
                for (share_nick, kind, url) in active_shares {
                    let _ = tx.send(ServerMsg::ShareStarted { room: room.clone(), nick: share_nick, kind, url }).await;
                }

                info!("{nick} joined #{room}");
            }

            ClientMsg::Send { content } if joined => {
                let (nick, room, store) = {
                    let s = state.lock().await;
                    match s.clients.get(&addr) {
                        Some(c) => (c.nick.clone(), c.room.clone(), s.store.clone()),
                        None => continue,
                    }
                };

                let ts = chrono::Utc::now().timestamp_millis();
                let msg_id = match store.insert(&room, &nick, &content, ts).await {
                    Ok(id) => id,
                    Err(e) => {
                        warn!("store insert: {e}");
                        let _ = tx.send(ServerMsg::Error { msg: "persist failed".into() }).await;
                        continue;
                    }
                };

                let chat = ChatMessage { msg_id, nick: nick.clone(), content: content.clone(), ts };

                let senders = {
                    let mut s = state.lock().await;
                    s.push_history(&room, chat);
                    s.room_senders(&room, None)
                };

                let server_msg = ServerMsg::Message {
                    room: room.clone(),
                    msg_id,
                    nick,
                    content,
                    ts,
                };
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

                let room_was_new = {
                    let s = state.lock().await;
                    !s.rooms.contains_key(&room)
                };

                {
                    let mut s = state.lock().await;
                    s.leave_room(&old_room, addr);
                    s.remove_share(&old_room, &nick, None);
                    if let Some(c) = s.clients.get_mut(&addr) {
                        c.room = room.clone();
                    }
                }

                let (old_senders, old_users) = {
                    let s = state.lock().await;
                    (s.room_senders(&old_room, None), s.room_nicks(&old_room))
                };
                for s in old_senders {
                    let _ = s.send(ServerMsg::UserLeft { room: old_room.clone(), nick: nick.clone(), users: old_users.clone() }).await;
                }

                warm_room(&state, &room).await;

                let (history, new_users) = {
                    let mut s = state.lock().await;
                    s.join_room(&room, addr);
                    (s.room_history(&room), s.room_nicks(&room))
                };

                let _ = tx.send(ServerMsg::SwitchedRoom { room: room.clone(), history, users: new_users.clone() }).await;

                // Replay in-progress shares for the new room.
                let active_shares = {
                    let s = state.lock().await;
                    s.room_active_shares(&room)
                };
                for (share_nick, kind, url) in active_shares {
                    let _ = tx.send(ServerMsg::ShareStarted { room: room.clone(), nick: share_nick, kind, url }).await;
                }

                let new_senders = {
                    let s = state.lock().await;
                    s.room_senders(&room, Some(addr))
                };
                for s in new_senders {
                    let _ = s.send(ServerMsg::UserJoined { room: room.clone(), nick: nick.clone(), users: new_users.clone() }).await;
                }

                if room_was_new {
                    let (rooms, all) = {
                        let s = state.lock().await;
                        (s.room_list(), s.all_senders())
                    };
                    for s in all {
                        let _ = s.send(ServerMsg::RoomList { rooms: rooms.clone() }).await;
                    }
                }
                info!("{nick} -> #{room}");
            }

            ClientMsg::CreateRoom { name } if joined => {
                let name = sanitize(&name);
                let (rooms, all) = {
                    let mut s = state.lock().await;
                    s.ensure_room(&name);
                    (s.room_list(), s.all_senders())
                };
                for s in all {
                    let _ = s.send(ServerMsg::RoomList { rooms: rooms.clone() }).await;
                }
            }

            ClientMsg::FetchHistory { room, after_id } if joined => {
                let room = sanitize(&room);
                let store = {
                    let s = state.lock().await;
                    s.store.clone()
                };
                let messages = store.fetch_after(&room, after_id).await.unwrap_or_default();
                let _ = tx.send(ServerMsg::History { room, messages }).await;
            }

            ClientMsg::VoiceJoin if joined => {
                let (nick, room) = {
                    let s = state.lock().await;
                    match s.clients.get(&addr) {
                        Some(c) => (c.nick.clone(), c.room.clone()),
                        None => continue,
                    }
                };
                let (users, senders) = {
                    let mut s = state.lock().await;
                    s.voice_join(&room, addr);
                    (s.voice_nicks(&room), s.room_senders(&room, None))
                };
                let msg = ServerMsg::VoiceJoined { room: room.clone(), nick: nick.clone(), users };
                for s in senders {
                    let _ = s.send(msg.clone()).await;
                }
                info!("{nick} joined voice in #{room}");
            }

            ClientMsg::VoiceLeave if joined => {
                let (nick, room) = {
                    let s = state.lock().await;
                    match s.clients.get(&addr) {
                        Some(c) => (c.nick.clone(), c.room.clone()),
                        None => continue,
                    }
                };
                let (users, senders) = {
                    let mut s = state.lock().await;
                    s.voice_leave(&room, addr);
                    (s.voice_nicks(&room), s.room_senders(&room, None))
                };
                let msg = ServerMsg::VoiceLeft { room: room.clone(), nick: nick.clone(), users };
                for s in senders {
                    let _ = s.send(msg.clone()).await;
                }
            }

            ClientMsg::VoiceData { data } if joined => {
                let (nick, senders) = {
                    let s = state.lock().await;
                    let Some(c) = s.clients.get(&addr) else { continue };
                    let nick = c.nick.clone();
                    let room = c.room.clone();
                    (nick, s.voice_senders(&room, Some(addr)))
                };
                let frame = ServerMsg::VoiceFrame { nick, data };
                for s in senders {
                    let _ = s.send(frame.clone()).await;
                }
            }

            ClientMsg::ShareStart { kind, url } if joined => {
                let (nick, room, senders) = {
                    let mut s = state.lock().await;
                    let Some(c) = s.clients.get(&addr) else { continue };
                    let nick = c.nick.clone();
                    let room = c.room.clone();
                    s.add_share(&room, &nick, kind, url.clone());
                    let senders = s.room_senders(&room, None);
                    (nick, room, senders)
                };
                let msg = ServerMsg::ShareStarted { room: room.clone(), nick: nick.clone(), kind, url };
                for s in senders {
                    let _ = s.send(msg.clone()).await;
                }
                let kind_str = match kind { ShareKind::Screen => "screen", ShareKind::Cam => "cam" };
                info!("{nick} sharing {kind_str} in #{room}");
            }

            ClientMsg::ShareStop { kind } if joined => {
                let (nick, room, senders) = {
                    let mut s = state.lock().await;
                    let Some(c) = s.clients.get(&addr) else { continue };
                    let nick = c.nick.clone();
                    let room = c.room.clone();
                    s.remove_share(&room, &nick, kind);
                    let senders = s.room_senders(&room, None);
                    (nick, room, senders)
                };
                let msg = ServerMsg::ShareStopped { room, nick, kind };
                for s in senders {
                    let _ = s.send(msg.clone()).await;
                }
            }

            ClientMsg::ListRooms => {
                let rooms = {
                    let s = state.lock().await;
                    s.room_list()
                };
                let _ = tx.send(ServerMsg::RoomList { rooms }).await;
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
        let nick = c.nick.clone();
        let room = c.room.clone();
        s.leave_room(&room, addr);
        s.voice_leave(&room, addr);
        s.remove_share(&room, &nick, None);
        (nick, room)
    };
    let (senders, users) = {
        let s = state.lock().await;
        (s.room_senders(&room, None), s.room_nicks(&room))
    };
    for s in senders {
        let _ = s.send(ServerMsg::UserLeft { room: room.clone(), nick: nick.clone(), users: users.clone() }).await;
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
