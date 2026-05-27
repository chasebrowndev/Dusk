use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: &str = "0.2";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ShareKind {
    Screen,
    Cam,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ClientMsg {
    Join { nick: String, room: String },
    Send { content: String },
    SwitchRoom { room: String },
    CreateRoom { name: String },
    ListRooms,
    VoiceJoin,
    VoiceLeave,
    VoiceData { data: String }, // base64-encoded Opus frame
    ShareStart { kind: ShareKind, url: String },
    ShareStop { kind: Option<ShareKind> }, // None = stop all kinds for this user
    FetchHistory { room: String, after_id: u64 },
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Hello { version: String },
    Pong { version: String },
    Message { room: String, msg_id: u64, nick: String, content: String, ts: i64 },
    UserJoined { room: String, nick: String, users: Vec<String> },
    UserLeft { room: String, nick: String, users: Vec<String> },
    RoomList { rooms: Vec<String> },
    Joined { room: String, history: Vec<ChatMessage>, users: Vec<String> },
    SwitchedRoom { room: String, history: Vec<ChatMessage>, users: Vec<String> },
    History { room: String, messages: Vec<ChatMessage> },
    VoiceJoined { room: String, nick: String, users: Vec<String> },
    VoiceLeft { room: String, nick: String, users: Vec<String> },
    VoiceFrame { nick: String, data: String },
    ShareStarted { room: String, nick: String, kind: ShareKind, url: String },
    ShareStopped { room: String, nick: String, kind: Option<ShareKind> },
    Error { msg: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub msg_id: u64,
    pub nick: String,
    pub content: String,
    pub ts: i64,
}
