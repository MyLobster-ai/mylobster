use clap::Parser;
use mylobster::cli::{Cli, Commands};
use mylobster::config::Config;
use mylobster::gateway::GatewayServer;
use mylobster::logging;
use tracing::{error, info};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    logging::init();

    let cli = Cli::parse();

    match cli.command {
        Commands::Gateway(opts) => {
            info!("Starting MyLobster gateway server");
            let config = Config::load(opts.config.as_deref())?;
            let server = GatewayServer::start(config, opts).await?;
            server.run_until_shutdown().await?;
        }
        Commands::Agent(opts) => {
            info!("Running agent for single message");
            let config = Config::load(opts.config.as_deref())?;
            mylobster::agents::run_single_message(&config, &opts.message, opts.session_key.as_deref())
                .await?;
        }
        Commands::Send(opts) => {
            info!("Sending message via channel");
            let config = Config::load(opts.config.as_deref())?;
            mylobster::channels::send_message(&config, &opts.channel, &opts.to, &opts.message)
                .await?;
        }
        Commands::Config(opts) => {
            let config = Config::load(opts.config.as_deref())?;
            match opts.action {
                mylobster::cli::ConfigAction::Show => {
                    println!("{}", serde_json::to_string_pretty(&config)?);
                }
                mylobster::cli::ConfigAction::Validate => {
                    info!("Configuration is valid");
                }
                mylobster::cli::ConfigAction::Init => {
                    Config::write_default(opts.config.as_deref().unwrap_or("mylobster.json"))?;
                    info!("Configuration file created");
                }
            }
        }
        Commands::Doctor => {
            info!("Running diagnostics...");
            mylobster::infra::doctor::run_diagnostics().await?;
        }
        Commands::Version => {
            println!("mylobster {}", env!("CARGO_PKG_VERSION"));
        }
    }

    Ok(())
}
