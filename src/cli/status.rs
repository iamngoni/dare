//! `dare status` command - Show run status

use anyhow::Result;

use crate::config::Config;
use crate::db::Database;
use crate::models::TaskStatus;

use super::StatusArgs;

/// Show status of current or specific run
pub async fn run(args: StatusArgs, config: &Config) -> Result<()> {
    let db = Database::connect(&config.general.database).await?;

    // Get run (specified or most recent)
    let run = if let Some(ref run_id) = args.run_id {
        db.get_run(run_id).await?
    } else {
        db.get_latest_run().await?
    };

    let Some(run) = run else {
        println!("No runs found");
        return Ok(());
    };

    if args.watch {
        // TUI mode
        #[cfg(feature = "tui")]
        {
            crate::cli::status::tui::run_tui(&db, &run).await?;
        }
        #[cfg(not(feature = "tui"))]
        {
            println!("TUI feature not enabled. Rebuild with --features tui");
        }
        return Ok(());
    }

    // Static output
    println!("📊 Run Status");
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    println!("Run:     {}", run.id);
    println!("Name:    {}", run.name);
    println!("Status:  {:?}", run.status);
    if let Some(ref started) = run.started_at {
        println!("Started: {}", started);
    }
    if let Some(ref completed) = run.completed_at {
        println!("Ended:   {}", completed);
    }
    println!();

    // Task list
    let tasks = db.get_tasks(&run.id).await?;
    println!("Tasks:");
    for task in &tasks {
        let icon = match task.status {
            TaskStatus::Pending => "○",
            TaskStatus::Spawned => "◐",
            TaskStatus::Executing => "●",
            TaskStatus::Completed => "✓",
            TaskStatus::Failed => "✗",
        };
        println!(
            "  {} [{}] {} - {}",
            icon,
            task.wave,
            task.id,
            task.description.chars().take(50).collect::<String>()
        );
    }

    Ok(())
}

#[cfg(feature = "tui")]
mod tui {
    use super::*;
    use crate::models::Run;

    pub async fn run_tui(_db: &Database, _run: &Run) -> Result<()> {
        // TODO: Implement TUI with ratatui
        println!("TUI mode coming soon...");
        Ok(())
    }
}
