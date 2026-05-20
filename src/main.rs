use anyhow::Result;
use clap::{Parser, Subcommand};

mod client;
mod protocol;
mod server;
mod ui;

#[derive(Parser)]
#[command(name = "dusk", about = "Lightweight TUI chat over Tailscale")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    /// Hub server address (host:port)
    #[arg(short, long, default_value = "127.0.0.1:7667")]
    server: String,

    /// Your nickname
    #[arg(short, long)]
    nick: Option<String>,
}

#[derive(Subcommand)]
enum Command {
    /// Run as the hub server
    Serve {
        /// Address to listen on
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
            let nick = cli
                .nick
                .unwrap_or_else(|| std::env::var("USER").unwrap_or_else(|_| "anon".into()));
            client::run(&cli.server, &nick).await
        }
    }
}
