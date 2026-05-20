use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Join { nick: String, room: String },
    Send { content: String },
    SwitchRoom { room: String },
    CreateRoom { name: String },
    ListRooms,
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Message { room: String, nick: String, content: String, ts: i64 },
    UserJoined { room: String, nick: String },
    UserLeft { room: String, nick: String },
    RoomList { rooms: Vec<String> },
    Joined { room: String, history: Vec<ChatMessage> },
    SwitchedRoom { room: String, history: Vec<ChatMessage> },
    Error { msg: String },
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub nick: String,
    pub content: String,
    pub ts: i64,
}
