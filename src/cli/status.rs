//! `dare status` command - Show run status

use anyhow::Result;

use crate::config::Config;
use crate::db::Database;
use crate::models::{RunStatus, TaskStatus};

use super::StatusArgs;

/// Show status of current or specific run
pub async fn run(args: StatusArgs, config: &Config) -> Result<()> {
    let db = Database::connect(&config.database_path()).await?;
    db.migrate().await?;

    // Get run to show
    let run = if let Some(ref run_id) = args.run_id {
        db.get_run(run_id).await?.ok_or_else(|| {
            anyhow::anyhow!("Run '{}' not found", run_id)
        })?
    } else {
        db.get_latest_run().await?.ok_or_else(|| {
            anyhow::anyhow!("No runs found. Start a run with: dare run <task.yaml>")
        })?
    };

    // Get tasks
    let tasks = db.get_tasks(&run.id).await?;

    // Header
    println!();
    println!(
        "dare.run v{} — Status",
        env!("CARGO_PKG_VERSION")
    );
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    // Run info
    let status_icon = match run.status {
        RunStatus::Pending => "⏳",
        RunStatus::Running => "🔄",
        RunStatus::Paused => "⏸️",
        RunStatus::Completed => "✅",
        RunStatus::Failed => "❌",
    };

    println!();
    println!("{} Run: {} ", status_icon, run.name);
    println!("   ID: {}", run.id);
    println!("   Status: {:?}", run.status);

    if let Some(started) = run.started_at {
        println!("   Started: {}", started.format("%Y-%m-%d %H:%M:%S"));
    }
    if let Some(completed) = run.completed_at {
        println!("   Completed: {}", completed.format("%Y-%m-%d %H:%M:%S"));
    }

    // Task summary
    let pending_count = tasks.iter().filter(|t| t.status == TaskStatus::Pending).count();
    let running_count = tasks.iter().filter(|t| matches!(t.status, TaskStatus::Spawned | TaskStatus::Executing)).count();
    let completed_count = tasks.iter().filter(|t| t.status == TaskStatus::Completed).count();
    let failed_count = tasks.iter().filter(|t| t.status == TaskStatus::Failed).count();

    println!();
    println!("📊 Tasks: {} total", tasks.len());
    println!("   ⏳ Pending: {}", pending_count);
    println!("   🔄 Running: {}", running_count);
    println!("   ✅ Completed: {}", completed_count);
    println!("   ❌ Failed: {}", failed_count);

    // Task details
    if !tasks.is_empty() {
        println!();
        println!("📋 Task Details:");

        // Group by wave
        let max_wave = tasks.iter().map(|t| t.wave).max().unwrap_or(0);
        for wave in 0..=max_wave {
            let wave_tasks: Vec<_> = tasks.iter().filter(|t| t.wave == wave).collect();
            if wave_tasks.is_empty() {
                continue;
            }

            println!();
            println!("   Wave {}:", wave + 1);
            
            for task in wave_tasks {
                let icon = match task.status {
                    TaskStatus::Pending => "○",
                    TaskStatus::Spawned => "◐",
                    TaskStatus::Executing => "◑",
                    TaskStatus::Completed => "●",
                    TaskStatus::Failed => "✗",
                };

                let duration = match (task.started_at, task.completed_at) {
                    (Some(start), Some(end)) => {
                        let dur = end - start;
                        format!(" ({}s)", dur.num_seconds())
                    }
                    (Some(start), None) => {
                        let dur = chrono::Utc::now() - start;
                        format!(" ({}s...)", dur.num_seconds())
                    }
                    _ => String::new(),
                };

                let error_hint = task.error.as_ref().map(|e| {
                    let short = if e.len() > 40 {
                        format!("{}...", &e[..40])
                    } else {
                        e.clone()
                    };
                    format!(" — {}", short)
                }).unwrap_or_default();

                println!("     {} {}{}{}", icon, task.id, duration, error_hint);
            }
        }
    }

    println!();

    // Watch mode
    if args.watch {
        println!("Watch mode not yet implemented. Use: dare dashboard");
    }

    Ok(())
}
