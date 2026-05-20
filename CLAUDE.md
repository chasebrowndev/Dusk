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
| `Alt+Up` / `Alt+Down` | Switch rooms |
| `Esc` or `Ctrl+C` | Quit |
| `/join <room>` or `/j <room>` | Join a room |
| `/create <name>` or `/new <name>` | Create a room |
| `/rooms` or `/list` | Refresh room list |

## Protocol Rules

- Clients must send `Join` before any other message
- Server sanitizes room names: lowercase alphanum + `-_`, max 32 chars, strips leading `#`
- Server broadcasts `UserJoined` / `UserLeft` to the room on connect/disconnect
- Server sends `Joined` (with history) on initial join and `RoomList` after join
- Server sends `SwitchedRoom` (with history) when client switches rooms

## Design Constraints

- Never hold a Mutex guard across an `.await` — collect needed data first, release lock, then await
- Hub keeps last 200 messages per room in a VecDeque
- Protocol tag field is `t` (short, appears in every message)

## Roadmap

- [ ] Voice chat: cpal + libopus, UDP transport
- [ ] Page Up/Down scroll through message history
- [ ] Horizontal scroll / long input handling
- [ ] Reconnect on server drop
- [ ] Direct messages between users
- [ ] `/nick <name>` rename command
