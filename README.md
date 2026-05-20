# Dusk

Lightweight TUI chat over Tailscale. Single Rust binary — `dusk serve` for the hub, `dusk` for clients.

```
·▄▄▄▄  ▄• ▄▌.▄▄ · ▄ •▄
██▪ ██ █▪██▌▐█ ▀. █▌▄▌▪
▐█· ▐█▌█▌▐█▌▄▀▀▀█▄▐▀▀▄·
██. ██ ▐█▄█▌▐█▄▪▐█▐█.█▌
▀▀▀▀▀•  ▀▀▀  ▀▀▀▀ ·▀  ▀
```

## Features

- Multi-room chat with persistent history (last 200 messages per room)
- Screen and camera sharing over Tailscale — signals peers, hands off to external tools
- Voice chat (Opus over UDP)
- Themeable UI with cyberpunk defaults

## Installation

### Arch Linux

```bash
# From AUR (once submitted)
yay -S dusk

# Or build from source with makepkg
git clone https://github.com/chasebrowndev/dusk
cd dusk
makepkg -si
```

### From Source

```bash
# Dependencies (Arch)
sudo pacman -S tailscale ffmpeg opus alsa-lib
yay -S wf-recorder   # or paru -S wf-recorder

cargo install --git https://github.com/chasebrowndev/dusk
```

## Dependencies

| Package | Purpose |
|---------|---------|
| `tailscale` | Network transport; peer IP detection via `tailscale ip -4` |
| `ffmpeg` | Screen/camera stream encoding and playback |
| `wf-recorder` | Wayland screen capture (`/share`) |
| `opus` | Voice chat codec |
| `alsa-lib` | Audio I/O for voice |

Screen sharing defaults can be overridden with environment variables:

| Variable | Default |
|----------|---------|
| `DUSK_SHARE_SCREEN` | `wf-recorder -f - --ffmpeg-muxer mpegts \| ffmpeg ... -listen 1 tcp://{addr}` |
| `DUSK_SHARE_CAM` | `ffmpeg -f v4l2 -i /dev/video0 ... -listen 1 tcp://{addr}` |
| `DUSK_SHARE_VIEW` | `ffplay -fflags nobuffer -flags low_delay -i tcp://{addr}` |

`{addr}` is replaced with the sharer's `tailscale-ip:7668`.

## Usage

```bash
# Hub machine — run this once on a machine your tailnet can reach
dusk serve

# Clients — point at the hub's Tailscale IP
dusk --server 100.x.y.z:7667 --nick yourname
```

## Key Bindings

| Key | Action |
|-----|--------|
| `Enter` | Send message |
| `Shift+Enter` / `Alt+Enter` | Insert newline |
| `← → ↑ ↓` | Move cursor in input |
| `Page Up` / `Page Down` | Scroll message history |
| `Tab` | Focus room sidebar (`Enter` joins, `Esc` returns) |
| `v` | Toggle voice (when input is empty) |
| `Ctrl+C` | Quit |

## Commands

| Command | Action |
|---------|--------|
| `/join <room>` or `/j <room>` | Join a room |
| `/create <name>` or `/new <name>` | Create a room |
| `/rooms` or `/list` | Refresh room list |
| `/theme [name]` | Switch theme, or list all themes |
| `/share [cam]` | Share screen (or camera) |
| `/share stop` | Stop sharing |
| `/watch [nick]` | Open a peer's shared stream |
| `/help` | Show help |

## Architecture

```
src/
  main.rs      — CLI (clap), dispatch to server or client mode
  protocol.rs  — ClientMsg / ServerMsg enums, ChatMessage struct
  server.rs    — hub: state (rooms + clients), connection handler, broadcast
  client.rs    — connect to hub, spawn reader/writer tasks, launch TUI
  ui.rs        — ratatui TUI: App state, draw functions, input handling
  voice.rs     — cpal + Opus voice chat
  config.rs    — user config (~/.config/dusk/config.toml)
  theme.rs     — built-in themes
```

Transport is newline-delimited JSON over TCP on port 7667. State is in-memory only — lost on server restart by design. Auth is handled by Tailscale at the network layer.

## License

MIT — see [LICENSE](LICENSE).
