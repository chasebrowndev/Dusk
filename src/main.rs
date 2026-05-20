use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

mod client;
mod config;
mod protocol;
mod server;
mod theme;
mod ui;
mod voice;

use config::{prompt_nick, Config};

#[derive(Parser)]
#[command(name = "dusk", about = "Lightweight TUI chat over Tailscale", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Hub server address (host:port) — saved to config after first use
    #[arg(short, long)]
    server: Option<String>,

    /// Your nickname (overrides saved config)
    #[arg(short, long)]
    nick: Option<String>,

    /// Color theme name (run /theme in-app to list all)
    #[arg(short = 't', long)]
    theme: Option<String>,

    /// Connect as a guest (random nick, ignores saved identity)
    #[arg(short = 'g', long)]
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

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Serve { bind }) => {
            tracing_subscriber::fmt::init();
            server::run(&bind).await
        }
        None => {
            let (nick, theme_name) = resolve_identity(&cli)?;
            let server = resolve_server(&cli).await?;
            client::run(&server, &nick, &theme_name).await
        }
    }
}

async fn resolve_server(cli: &Cli) -> Result<String> {
    // Explicit --server flag wins; save it so future runs skip discovery.
    if let Some(s) = &cli.server {
        if let Ok(Some(mut cfg)) = Config::load() {
            cfg.server = Some(s.clone());
            let _ = cfg.save();
        }
        return Ok(s.clone());
    }

    // Saved server from a previous run.
    if let Ok(Some(cfg)) = Config::load() {
        if let Some(s) = cfg.server {
            return Ok(s);
        }
    }

    // Auto-discover a Dusk server on the Tailscale network.
    eprintln!("No server configured — scanning Tailscale peers on :7667…");
    if let Some(s) = discover_server().await {
        eprintln!("Found server at {s} — saving to config.");
        if let Ok(Some(mut cfg)) = Config::load() {
            cfg.server = Some(s.clone());
            let _ = cfg.save();
        }
        return Ok(s);
    }

    // Local fallback for development.
    Ok("127.0.0.1:7667".into())
}

async fn discover_server() -> Option<String> {
    let output = std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .ok()?;

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).ok()?;
    let peers = json["Peer"].as_object()?;

    let addrs: Vec<String> = peers
        .values()
        .filter(|p| p["Online"].as_bool().unwrap_or(false))
        .filter_map(|p| {
            let ips = p["TailscaleIPs"].as_array()?;
            // Prefer IPv4
            ips.iter()
                .find_map(|ip| ip.as_str().filter(|s| s.contains('.')))
                .map(|ip| format!("{ip}:7667"))
        })
        .collect();

    if addrs.is_empty() {
        return None;
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(1);
    for addr in addrs {
        let tx = tx.clone();
        tokio::spawn(async move {
            if tokio::time::timeout(
                Duration::from_secs(2),
                tokio::net::TcpStream::connect(&addr),
            )
            .await
            .is_ok()
            {
                let _ = tx.send(addr).await;
            }
        });
    }
    drop(tx);

    tokio::time::timeout(Duration::from_secs(3), rx.recv())
        .await
        .ok()
        .flatten()
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
        eprintln!("Saved as \"{}\". Run with --nick to override.", config.nick);
    }

    Ok((config.nick, config.theme))
}
