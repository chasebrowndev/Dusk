# Dusk

Lightweight TUI chat over Tailscale. Single Rust binary — `dusk serve` for the hub, `dusk` for clients.

## Architecture

```
src/
  main.rs       — CLI (clap), dispatch to server or client mode
  protocol.rs   — ClientMsg / ServerMsg enums, ChatMessage struct
  server.rs     — hub: state (rooms + clients), connection handler, broadcast
  client.rs     — connect to hub, spawn reader/writer tasks, launch TUI
  ui.rs         — ratatui TUI: App state, draw functions, input handling
```

**Transport**: newline-delimited JSON over TCP (port 7667)  
**State**: in-memory only, lost on server restart (by design)  
**Auth**: none — Tailscale handles it at the network layer

## Running

```bash
# Hub machine
dusk serve

# Client machines (--server takes the hub's Tailscale IP)
dusk --server 100.x.y.z:7667 --nick yourname
```

## Key Bindings

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `Shift+Enter` or `Alt+Enter` | Insert a newline |
| `← / → / ↑ / ↓` | Move cursor through typed text |
| `Page Up` / `Page Down` | Scroll message history |
| `Tab` | Focus room sidebar (`Enter` joins, `Esc` returns) |
| `v` | Toggle voice (when input is empty) |
| `Ctrl+C` | Quit |
| `/join <room>` or `/j <room>` | Join a room |
| `/create <name>` or `/new <name>` | Create a room |
| `/rooms` or `/list` | Refresh room list |
| `/theme [name]` | Switch theme, or list all themes |
| `/share [cam]` or `/share stop` | Share screen (or camera) over Tailscale |
| `/watch [nick]` | Open a peer's shared stream |

## Protocol Rules

- Clients must send `Join` before any other message
- Server sanitizes room names: lowercase alphanum + `-_`, max 32 chars, strips leading `#`
- Server broadcasts `UserJoined` / `UserLeft` to the room on connect/disconnect
- `Joined`, `SwitchedRoom`, `UserJoined`, `UserLeft` all carry the room's current `users` list
- Server sends `Joined` (with history) on initial join and `RoomList` after join
- Server sends `SwitchedRoom` (with history) when client switches rooms
- `ShareStart` / `ShareStop` are relayed to the room as `ShareStarted` / `ShareStopped`; the hub keeps no share state (late joiners miss an in-progress share)

## Design Constraints

- Never hold a Mutex guard across an `.await` — collect needed data first, release lock, then await
- Hub keeps last 200 messages per room in a VecDeque
- Protocol tag field is `t` (short, appears in every message)

## Roadmap

- [x] Screen / camera sharing — `/share` signals peers, hands off to an external capture/playback tool (commands overridable via `DUSK_SHARE_*`); uses `wf-recorder` + `ffmpeg` by default
- [ ] Voice chat: cpal + libopus, UDP transport
- [ ] Page Up/Down scroll through message history
- [ ] Horizontal scroll / long input handling
- [ ] Reconnect on server drop
- [ ] Direct messages between users
- [ ] `/nick <name>` rename command
