//! `dare logs` command - View logs

use anyhow::Result;

use crate::config::Config;
use crate::db::Database;

use super::LogsArgs;

/// View logs for a run
pub async fn run(args: LogsArgs, config: &Config) -> Result<()> {
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

    if args.follow {
        // Follow mode - stream logs
        println!("📜 Following logs for run {}...", run.id);
        println!("   Press Ctrl+C to stop");
        println!();

        // TODO: Implement log streaming via SSE or polling
        loop {
            let logs = db
                .get_recent_logs(&run.id, args.task.as_deref(), 10)
                .await?;
            for log in logs {
                print_log_entry(&log);
            }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
    } else {
        // Static logs
        let logs = db
            .get_logs(&run.id, args.task.as_deref(), args.lines)
            .await?;

        if logs.is_empty() {
            println!("No logs found for run {}", run.id);
            return Ok(());
        }

        println!("📜 Logs for run {} (showing last {})", run.id, args.lines);
        println!();

        for log in logs {
            print_log_entry(&log);
        }
    }

    Ok(())
}

fn print_log_entry(log: &crate::models::AgentLog) {
    let level_icon = match log.level.as_str() {
        "ERROR" => "❌",
        "WARN" => "⚠️",
        "INFO" => "ℹ️",
        "DEBUG" => "🔍",
        _ => "📝",
    };

    println!(
        "{} {} [{}] {}",
        log.created_at.format("%H:%M:%S"),
        level_icon,
        log.task_id,
        log.message
    );
}
