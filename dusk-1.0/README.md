# Dusk v1.0

P2P voice, text, and video communicator over Tailscale.

## Requirements

Both machines must be on the same Tailscale network.

## Install (Arch Linux)

```bash
# Install from the dusk directory using makepkg
cd dusk-1.0
makepkg -si
```

This installs:
- App files to `/opt/dusk/`
- `dusk`, `dusk-host`, `dusk-client` commands to `/usr/bin/`
- Desktop entry for app launchers

### AUR / pip dependencies not in official repos

```bash
# Opus codec bindings (AUR)
yay -S python-opuslib

# Or via pip
pip install opuslib --break-system-packages
```

## Run without installing (dev)

```bash
# Host machine
./launch-host.sh

# Client machine
./launch-client.sh 100.x.x.x
# or set host_ip in config.toml and just run:
./launch-client.sh
```

## Config

Edit `config.toml` (or `/opt/dusk/config.toml` after install):

```toml
name    = "YourName"
host_ip = "100.x.x.x"   # Tailscale IP of host machine (client only)
port    = 7337
```

## After install

```bash
# Host machine
dusk-host

# Client machine
dusk-client 100.x.x.x
```

## Dependencies

| Package | Purpose |
|---|---|
| `python-pywebview` | UI |
| `python-pyaudio` + `portaudio` | Audio I/O |
| `opus` | Audio codec |
| `python-opencv` + `python-numpy` | Camera/screen capture |
| `python-mss` | Screen capture |
| `webkit2gtk` | Web renderer |
| `tailscale` | Network transport |
| `ttf-nerd-fonts-symbols` | UI icons |
