use serde::{Deserialize, Serialize};

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
    ShareStart { url: String }, // host:port of an external A/V stream
    ShareStop,
    Ping,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ServerMsg {
    Message { room: String, nick: String, content: String, ts: i64 },
    UserJoined { room: String, nick: String, users: Vec<String> },
    UserLeft { room: String, nick: String, users: Vec<String> },
    RoomList { rooms: Vec<String> },
    Joined { room: String, history: Vec<ChatMessage>, users: Vec<String> },
    SwitchedRoom { room: String, history: Vec<ChatMessage>, users: Vec<String> },
    VoiceJoined { room: String, nick: String, users: Vec<String> },
    VoiceLeft { room: String, nick: String, users: Vec<String> },
    VoiceFrame { nick: String, data: String }, // base64-encoded Opus frame
    ShareStarted { room: String, nick: String, url: String },
    ShareStopped { room: String, nick: String },
    Error { msg: String },
    Pong,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub nick: String,
    pub content: String,
    pub ts: i64,
}
