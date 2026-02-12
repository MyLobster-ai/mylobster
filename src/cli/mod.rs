use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "mylobster", version, about = "Multi-channel AI gateway")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    Gateway(GatewayOpts),
    Agent(AgentOpts),
    Send(SendOpts),
    Config(ConfigOpts),
    Doctor,
    Version,
}

#[derive(clap::Args)]
pub struct GatewayOpts {
    #[arg(short, long)]
    pub config: Option<String>,
    #[arg(short, long)]
    pub port: Option<u16>,
    #[arg(short, long)]
    pub bind: Option<String>,
}

#[derive(clap::Args)]
pub struct AgentOpts {
    #[arg(short, long)]
    pub config: Option<String>,
    pub message: String,
    #[arg(short, long)]
    pub session_key: Option<String>,
}

#[derive(clap::Args)]
pub struct SendOpts {
    #[arg(short, long)]
    pub config: Option<String>,
    pub channel: String,
    pub to: String,
    pub message: String,
}

#[derive(clap::Args)]
pub struct ConfigOpts {
    #[arg(short, long)]
    pub config: Option<String>,
    #[command(subcommand)]
    pub action: ConfigAction,
}

#[derive(Subcommand)]
pub enum ConfigAction {
    Show,
    Validate,
    Init,
}
