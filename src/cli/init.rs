//! `dare init` command - Initialize dare in current directory

use anyhow::Result;
use std::path::Path;

/// Initialize dare in the current directory
pub async fn run() -> Result<()> {
    let config_path = Path::new("dare.toml");
    let db_dir = Path::new(".dare");

    if config_path.exists() {
        println!("⚠️  dare.toml already exists");
        return Ok(());
    }

    // Create .dare directory
    std::fs::create_dir_all(db_dir)?;

    // Create default config
    let default_config = r#"# dare.run configuration
# See: https://github.com/iamngoni/dare

[general]
workspace = "."
database = ".dare/dare.db"

[execution]
max_parallel_agents = 4
wave_timeout_seconds = 300
task_timeout_seconds = 120
retry_failed_tasks = true
max_retries = 2

[gateway]
host = "localhost"
port = 18789

[planning]
auto_plan_model = "claude-sonnet-4-20250514"
complexity_estimation = true

[dashboard]
enabled = true
port = 8765
open_browser = true

[security]
max_agents_per_run = 10
max_concurrent_runs = 3
max_run_duration_minutes = 60

[logging]
level = "info"
file = ".dare/dare.log"
"#;

    std::fs::write(config_path, default_config)?;

    println!("✅ Initialized dare in current directory");
    println!("   Created: dare.toml");
    println!("   Created: .dare/");
    println!();
    println!("Next steps:");
    println!("  1. Create a dare.yaml with your tasks, or");
    println!("  2. Run: dare run --auto \"your task description\"");

    Ok(())
}
