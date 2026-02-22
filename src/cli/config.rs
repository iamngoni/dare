//! `dare config` command - Show current configuration

use anyhow::Result;

use crate::config::Config;

/// Show current configuration
pub async fn run(config: &Config) -> Result<()> {
    println!("📝 dare configuration");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    println!("[general]");
    println!("  workspace = {:?}", config.general.workspace);
    println!("  database  = {:?}", config.general.database);
    println!();

    println!("[execution]");
    println!(
        "  max_parallel_agents   = {}",
        config.execution.max_parallel_agents
    );
    println!(
        "  wave_timeout_seconds  = {}",
        config.execution.wave_timeout_seconds
    );
    println!(
        "  task_timeout_seconds  = {}",
        config.execution.task_timeout_seconds
    );
    println!(
        "  retry_failed_tasks    = {}",
        config.execution.retry_failed_tasks
    );
    println!("  max_retries           = {}", config.execution.max_retries);
    println!();

    println!("[gateway]");
    println!("  host = {:?}", config.gateway.host);
    println!("  port = {}", config.gateway.port);
    println!();

    println!("[dashboard]");
    println!("  enabled      = {}", config.dashboard.enabled);
    println!("  port         = {}", config.dashboard.port);
    println!("  open_browser = {}", config.dashboard.open_browser);
    println!();

    println!("[security]");
    println!(
        "  max_agents_per_run        = {}",
        config.security.max_agents_per_run
    );
    println!(
        "  max_concurrent_runs       = {}",
        config.security.max_concurrent_runs
    );
    println!(
        "  max_run_duration_minutes  = {}",
        config.security.max_run_duration_minutes
    );

    Ok(())
}
