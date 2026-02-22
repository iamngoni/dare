//! `dare pause`, `dare resume`, `dare kill` commands

use anyhow::Result;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::db::Database;
use crate::gateway::{GatewayClient, GatewayConfig, GatewayEvent};
use crate::models::RunStatus;

use super::KillArgs;

/// Pause execution after current wave
pub async fn pause(config: &Config) -> Result<()> {
    let db = Database::connect(&config.database_path()).await?;
    db.migrate().await?;

    let run = db.get_latest_run().await?;
    let Some(run) = run else {
        println!("No active run found");
        return Ok(());
    };

    if run.status != RunStatus::Running {
        println!("Run is not currently running (status: {:?})", run.status);
        return Ok(());
    }

    db.update_run_status(&run.id, RunStatus::Paused).await?;
    println!("⏸️  Run {} paused. Will stop after current wave.", run.id);
    println!("   Use `dare resume` to continue.");

    Ok(())
}

/// Resume paused execution
pub async fn resume(config: &Config) -> Result<()> {
    let db = Database::connect(&config.database_path()).await?;
    db.migrate().await?;

    let run = db.get_latest_run().await?;
    let Some(run) = run else {
        println!("No run found");
        return Ok(());
    };

    if run.status != RunStatus::Paused {
        println!("Run is not paused (status: {:?})", run.status);
        return Ok(());
    }

    db.update_run_status(&run.id, RunStatus::Running).await?;
    println!("▶️  Run {} resumed.", run.id);

    // Note: Resume is advisory - the executor needs to be restarted
    println!("   Note: Re-run `dare run --continue {}` to continue execution.", run.id);

    Ok(())
}

/// Kill agents and abort run
pub async fn kill(args: KillArgs, config: &Config) -> Result<()> {
    let db = Database::connect(&config.database_path()).await?;
    db.migrate().await?;

    let run = db.get_latest_run().await?;
    let Some(run) = run else {
        println!("No active run found");
        return Ok(());
    };

    // Confirmation unless --force
    if !args.force {
        println!("⚠️  This will kill all running agents for run: {}", run.id);
        print!("   Continue? [y/N] ");
        use std::io::Write;
        std::io::stdout().flush()?;

        let mut input = String::new();
        std::io::stdin().read_line(&mut input)?;
        if !input.trim().eq_ignore_ascii_case("y") {
            println!("Cancelled.");
            return Ok(());
        }
    }

    // Try to connect to gateway
    let gateway_config = match GatewayConfig::load() {
        Ok(c) => c,
        Err(e) => {
            println!("⚠️  Cannot connect to gateway: {}", e);
            println!("   Marking run as failed without killing agents.");
            db.update_run_status(&run.id, RunStatus::Failed).await?;
            return Ok(());
        }
    };

    // Create event channel (we don't need to process events for kill)
    let (event_tx, _event_rx) = mpsc::channel::<GatewayEvent>(10);

    let gateway = GatewayClient::connect(gateway_config, event_tx).await?;

    if let Some(ref task_id) = args.task {
        // Kill specific task
        let task = db.get_task(task_id).await?;
        let Some(task) = task else {
            println!("Task {} not found", task_id);
            return Ok(());
        };

        if let Some(ref session_id) = task.agent_session_id {
            gateway.kill_session(session_id).await?;
            db.update_task_error(task_id, "Killed by user").await?;
            println!("🔪 Killed task: {}", task_id);
        } else {
            println!("Task {} has no active agent session", task_id);
        }
    } else {
        // Kill all agents in run
        let tasks = db.get_running_tasks(&run.id).await?;
        let mut killed = 0;

        for task in tasks {
            if let Some(ref session_id) = task.agent_session_id {
                if gateway.kill_session(session_id).await.is_ok() {
                    killed += 1;
                }
                let _ = db.update_task_error(&task.id, "Killed by user").await;
            }
        }

        db.update_run_status(&run.id, RunStatus::Failed).await?;
        println!("🔪 Killed {} agents. Run aborted.", killed);
    }

    Ok(())
}
