use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::timeout;

mod client;
mod config;
mod protocol;
mod server;
mod store;
mod theme;
mod ui;
mod voice;

use config::{prompt_nick, Config};
use protocol::{ClientMsg, ServerMsg};

const DISCOVERY_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(300);
const DISCOVERY_SCAN_BUDGET: Duration = Duration::from_secs(3);

#[derive(Parser)]
#[command(name = "dusk", about = "Lightweight TUI chat over Tailscale", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Hub server address (host:port) — saved to config after first use
    #[arg(short, long, global = true)]
    server: Option<String>,

    /// Your nickname (overrides saved config)
    #[arg(short, long, global = true)]
    nick: Option<String>,

    /// Color theme name (run /theme in-app to list all)
    #[arg(short = 't', long, global = true)]
    theme: Option<String>,

    /// Connect as a guest (random nick, ignores saved identity)
    #[arg(short = 'g', long, global = true)]
    guest: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Run as the hub server
    Serve {
        #[arg(short, long, default_value = "0.0.0.0:7667")]
        bind: String,
    },
}

/// Result of the pre-TUI server resolution step. Carries an optional
/// human-readable summary that the TUI can surface in its status overlay
/// (e.g., "found alice@100.64.0.3"). Nothing is ever printed to stdout/stderr.
pub struct ResolvedServer {
    pub addr: String,
    pub discovery_summary: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // ALSA + libusb + a few graphics libs love to dump probe errors to stderr,
    // which scribbles over the ratatui alt-screen. The TUI is the only UX
    // surface, so muzzle fd 2 before anything cpal/v4l-adjacent runs.
    silence_stderr();

    let (nick, theme_name) = resolve_identity(&cli)?;
    match cli.command {
        Some(Command::Serve { bind }) => {
            // Host mode: run the hub *and* a local TUI client. Tracing would
            // clobber the alt-screen, so leave the global subscriber unset.
            let port = bind.rsplit_once(':').map(|(_, p)| p.to_string()).unwrap_or_else(|| "7667".into());
            let server_task = tokio::spawn(async move { server::run(&bind).await });
            // Derive a localhost connect address from the bind string. 0.0.0.0
            // and :: are special-cased; otherwise use the bind host verbatim.
            let connect_addr = format!("127.0.0.1:{port}");
            // Wait briefly for the listener to come up before connecting.
            for _ in 0..20 {
                if tokio::net::TcpStream::connect(&connect_addr).await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let res = client::run(&connect_addr, &nick, &theme_name).await;
            server_task.abort();
            res
        }
        None => {
            let resolved = resolve_server(&cli).await?;
            // TODO(handoff): pass resolved.discovery_summary into App initial
            // state once the TUI agent exposes a status-overlay hook.
            let _ = resolved.discovery_summary;
            client::run(&resolved.addr, &nick, &theme_name).await
        }
    }
}

async fn resolve_server(cli: &Cli) -> Result<ResolvedServer> {
    // Explicit --server flag wins; save it so future runs skip discovery.
    if let Some(s) = &cli.server {
        if let Ok(Some(mut cfg)) = Config::load() {
            cfg.server = Some(s.clone());
            let _ = cfg.save();
        }
        return Ok(ResolvedServer {
            addr: s.clone(),
            discovery_summary: None,
        });
    }

    // Saved server from a previous run.
    if let Ok(Some(cfg)) = Config::load() {
        if let Some(s) = cfg.server {
            return Ok(ResolvedServer {
                addr: s,
                discovery_summary: None,
            });
        }
    }

    // Auto-discover a Dusk server on the Tailscale network.
    tracing::info!("no server configured — scanning Tailscale peers on :7667");
    if let Some(peer) = discover_server().await? {
        if let Ok(Some(mut cfg)) = Config::load() {
            cfg.server = Some(peer.addr.clone());
            let _ = cfg.save();
        }
        let summary = match &peer.nick {
            Some(nick) => format!("found {}@{}", nick, peer.addr),
            None => format!("found {}", peer.addr),
        };
        tracing::info!("{summary}");
        return Ok(ResolvedServer {
            addr: peer.addr,
            discovery_summary: Some(summary),
        });
    }

    // Local fallback for development.
    Ok(ResolvedServer {
        addr: "127.0.0.1:7667".into(),
        discovery_summary: None,
    })
}

/// A peer discovered on the Tailscale network that responded with a valid
/// Dusk handshake.
struct DiscoveredPeer {
    addr: String,
    /// Tailscale-known hostname (if we could pull one out of `tailscale status`).
    nick: Option<String>,
}

async fn discover_server() -> Result<Option<DiscoveredPeer>> {
    let output = match tokio::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!("tailscale not available: {e}");
            return Ok(None);
        }
    };

    if !output.status.success() {
        return Ok(None);
    }

    let json: serde_json::Value = match serde_json::from_slice(&output.stdout) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("tailscale status JSON parse failed: {e}");
            return Ok(None);
        }
    };

    let Some(peers) = json["Peer"].as_object() else {
        return Ok(None);
    };

    let candidates: Vec<(String, Option<String>)> = peers
        .values()
        .filter(|p| p["Online"].as_bool().unwrap_or(false))
        .filter_map(|p| {
            let ips = p["TailscaleIPs"].as_array()?;
            let ip = ips
                .iter()
                .find_map(|ip| ip.as_str().filter(|s| s.contains('.')))?;
            let nick = p["HostName"]
                .as_str()
                .map(|s| s.trim_end_matches('.').to_string())
                .filter(|s| !s.is_empty());
            Some((format!("{ip}:7667"), nick))
        })
        .collect();

    if candidates.is_empty() {
        return Ok(None);
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<DiscoveredPeer>(1);
    for (addr, nick) in candidates {
        let tx = tx.clone();
        tokio::spawn(async move {
            match handshake_probe(&addr).await {
                Ok(version) => {
                    tracing::info!("handshake ok with {addr} (v{version})");
                    let _ = tx.send(DiscoveredPeer { addr, nick }).await;
                }
                Err(e) => {
                    tracing::debug!("handshake failed for {addr}: {e}");
                }
            }
        });
    }
    drop(tx);

    Ok(timeout(DISCOVERY_SCAN_BUDGET, rx.recv()).await.ok().flatten())
}

/// Perform a full Dusk handshake against a candidate peer: connect, send Ping,
/// expect Pong (optionally preceded by an unsolicited Hello). Total budget is
/// `DISCOVERY_HANDSHAKE_TIMEOUT` end-to-end. Returns the peer's protocol
/// version string on success.
async fn handshake_probe(addr: &str) -> Result<String> {
    timeout(DISCOVERY_HANDSHAKE_TIMEOUT, async move {
        let parsed: SocketAddr = addr.parse().context("invalid socket addr")?;
        let stream = TcpStream::connect(parsed).await.context("tcp connect")?;
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);

        // Send Ping.
        let mut ping_line = serde_json::to_string(&ClientMsg::Ping)?;
        ping_line.push('\n');
        write_half.write_all(ping_line.as_bytes()).await?;

        // Accept at most one Hello frame, then require a Pong.
        let mut saw_hello = false;
        for _ in 0..2 {
            let mut buf = String::new();
            let n = reader.read_line(&mut buf).await?;
            if n == 0 {
                return Err(anyhow!("eof during handshake"));
            }
            let msg: ServerMsg = serde_json::from_str(buf.trim())
                .context("invalid server msg during handshake")?;
            match msg {
                ServerMsg::Pong { version } => {
                    if version != protocol::PROTOCOL_VERSION {
                        tracing::warn!(
                            "peer {addr} protocol version mismatch: peer={version} ours={ours}",
                            ours = protocol::PROTOCOL_VERSION
                        );
                    }
                    return Ok(version);
                }
                ServerMsg::Hello { version } => {
                    if saw_hello {
                        return Err(anyhow!("duplicate Hello"));
                    }
                    saw_hello = true;
                    tracing::debug!("peer {addr} Hello v{version}");
                    continue;
                }
                other => {
                    return Err(anyhow!("unexpected handshake frame: {:?}", other));
                }
            }
        }
        Err(anyhow!("handshake did not produce a Pong"))
    })
    .await
    .map_err(|_| anyhow!("handshake timed out"))?
}

fn silence_stderr() {
    unsafe {
        let devnull = libc::open(b"/dev/null\0".as_ptr() as *const i8, libc::O_WRONLY);
        if devnull >= 0 {
            libc::dup2(devnull, 2);
            libc::close(devnull);
        }
    }
}

fn resolve_identity(cli: &Cli) -> Result<(String, String)> {
    if cli.guest {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
            % 10000;
        let nick = format!("guest{n:04}");
        let theme = cli.theme.clone().unwrap_or_else(|| "cyberpunk".into());
        return Ok((nick, theme));
    }

    let mut config = Config::load()?.unwrap_or_else(|| Config {
        nick: String::new(),
        machine_id: Config::machine_id(),
        theme: "cyberpunk".into(),
        server: None,
        audio_input: None,
        audio_output: None,
        video_device: None,
    });

    // CLI overrides (don't persist --nick, do persist --theme)
    if let Some(nick) = &cli.nick {
        return Ok((nick.clone(), cli.theme.clone().unwrap_or(config.theme)));
    }
    if let Some(theme) = &cli.theme {
        config.theme = theme.clone();
        config.save()?;
    }

    if config.nick.is_empty() {
        config.nick = prompt_nick()?;
        config.save()?;
        // TODO(handoff): surface "saved as <nick>" hint via TUI status overlay.
    }

    Ok((config.nick, config.theme))
}
