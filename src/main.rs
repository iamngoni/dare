//! dare.run - Multi-agent orchestration for OpenClaw/Claude agents
//!
//! Dare (Shona: "council") transforms complex tasks into coordinated agent swarms.

mod cli;

// Re-export library modules for use by the cli module
pub use dare::config;
pub use dare::dag;
pub use dare::db;
pub use dare::executor;
pub use dare::gateway;
pub use dare::message_bus;
pub use dare::models;
pub use dare::planner;
pub use dare::server;

use anyhow::Result;
use clap::Parser;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use crate::cli::{Cli, Commands};

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dare=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Parse CLI arguments
    let cli = Cli::parse();

    // Load configuration
    let config = config::Config::load()?;

    match cli.command {
        Commands::Init => {
            cli::init::run().await?;
        }
        Commands::Run(args) => {
            cli::run::run(args, &config).await?;
        }
        Commands::Plan(args) => {
            cli::plan::run(args, &config).await?;
        }
        Commands::Status(args) => {
            cli::status::run(args, &config).await?;
        }
        Commands::Logs(args) => {
            cli::logs::run(args, &config).await?;
        }
        Commands::Pause => {
            cli::control::pause(&config).await?;
        }
        Commands::Resume => {
            cli::control::resume(&config).await?;
        }
        Commands::Kill(args) => {
            cli::control::kill(args, &config).await?;
        }
        Commands::Dashboard(args) => {
            cli::dashboard::run(args, &config).await?;
        }
        Commands::Config => {
            cli::config::run(&config).await?;
        }
    }

    Ok(())
}
