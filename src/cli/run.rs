//! `dare run` command - Execute tasks

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::config::Config;
use crate::db::Database;
use crate::executor::WaveExecutor;
use crate::message_bus::TaskMessage;
use crate::models::{Run, RunStatus};
use crate::planner::Planner;
use crate::server::DashboardServer;

use super::RunArgs;

/// Run tasks from a file or auto-planned description
pub async fn run(args: RunArgs, config: &Config) -> Result<()> {
    println!();
    println!(
        "dare.run v{} — Multi-agent orchestration",
        env!("CARGO_PKG_VERSION")
    );
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");

    // Initialize database (shared Arc for dashboard integration)
    let db_path = config.database_path();
    tracing::info!(path = %db_path.display(), "Connecting to database");
    let db = Arc::new(Database::connect(&db_path).await?);
    db.migrate().await?;
    
    // Create broadcast channel for task events (shared with dashboard)
    let (events_tx, _) = broadcast::channel::<TaskMessage>(100);

    // Get or create task graph
    let (run_name, graph) = if let Some(ref description) = args.auto {
        // Auto-plan mode: two-phase planning (decompose → cast team)
        println!("\n🧠 Planning: {}", description);
        let planner = Planner::new(config, &db);
        let council = planner.plan(description).await?;
        let graph = planner.to_task_graph(&council)?;
        let name = council.name.clone();
        (name, graph)
    } else if let Some(ref run_id) = args.continue_run {
        // Continue interrupted run
        println!("\n▶️  Continuing run: {}", run_id);
        let existing_run = db
            .get_run(run_id)
            .await?
            .context(format!("Run '{}' not found", run_id))?;

        if existing_run.status == RunStatus::Completed {
            anyhow::bail!("Run '{}' is already completed", run_id);
        }

        let graph = db.load_task_graph(&existing_run.id).await?;
        (existing_run.name, graph)
    } else if let Some(ref file) = args.file {
        // Load from file
        println!("\n📋 Loading: {}", file.display());
        let graph = Planner::load_task_file(file)?;
        let name = graph
            .name
            .clone()
            .unwrap_or_else(|| file.display().to_string());
        (name, graph)
    } else {
        // Look for dare.yaml in current directory
        let default_file = PathBuf::from("dare.yaml");
        if default_file.exists() {
            println!("\n📋 Loading: dare.yaml");
            let graph = Planner::load_task_file(&default_file)?;
            let name = graph.name.clone().unwrap_or_else(|| "dare.yaml".to_string());
            (name, graph)
        } else {
            println!();
            println!("Usage:");
            println!("  dare run <task.yaml>              Run tasks from file");
            println!("  dare run --auto \"description\"     Auto-plan and run");
            println!("  dare run --continue <run_id>      Continue interrupted run");
            println!();
            println!("Create a dare.yaml in the current directory, or use --auto to plan automatically.");
            anyhow::bail!("No task file specified");
        }
    };

    // Validate graph
    graph.validate()?;

    // Create run record
    let run = Run::new(&run_name);
    let run_id = run.id.clone();
    db.create_run(&run).await?;
    
    // Store tasks in database
    db.create_tasks_from_graph(&run_id, &graph).await?;

    println!();
    println!("📋 Plan: {}", run_name);
    println!(
        "   {} tasks • {} waves • max {} parallel",
        graph.nodes.len(),
        graph.wave_count(),
        args.max_parallel
            .unwrap_or(config.execution.max_parallel_agents)
    );

    // Show wave breakdown
    let waves = graph.compute_waves();
    for (i, task_ids) in waves.iter().enumerate() {
        println!("   Wave {}: {}", i + 1, task_ids.join(", "));
    }
    println!();

    // Override max parallel if specified
    let mut exec_config = config.clone();
    if let Some(max) = args.max_parallel {
        exec_config.execution.max_parallel_agents = max;
    }
    
    // Optionally start dashboard server in background
    let dashboard_port = config.dashboard.port;
    if !args.no_dashboard {
        let dashboard_db = Arc::clone(&db);
        let dashboard_events_tx = events_tx.clone();
        let dashboard_config = config.clone();
        
        tokio::spawn(async move {
            let server = DashboardServer::with_shared_state(
                &dashboard_config,
                dashboard_port,
                dashboard_db,
                dashboard_events_tx,
            );
            
            // Give server a moment to start, then open browser
            if dashboard_config.dashboard.open_browser {
                tokio::spawn(async move {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    let url = format!("http://localhost:{}", dashboard_port);
                    let _ = open::that(&url);
                });
            }
            
            if let Err(e) = server.run().await {
                tracing::warn!(error = %e, "Dashboard server error");
            }
        });
        
        println!("🌐 Dashboard: http://localhost:{}", dashboard_port);
    }

    // Execute waves with event broadcasting
    let executor = WaveExecutor::with_events(&exec_config, &db, events_tx);
    let result = executor.execute(&run, &graph).await;

    match result {
        Ok(()) => {
            db.update_run_status(&run_id, RunStatus::Completed).await?;
            println!();
            println!("✅ Run completed successfully!");
            println!("   Run ID: {}", run_id);
        }
        Err(e) => {
            db.update_run_status(&run_id, RunStatus::Failed).await?;
            println!();
            println!("❌ Run failed: {}", e);
            println!("   Run ID: {}", run_id);
            println!();
            println!("View logs with: dare logs {}", run_id);
            return Err(e);
        }
    }

    Ok(())
}
