//! `dare run` command - Execute tasks

use anyhow::{Context, Result};
use std::path::PathBuf;

use crate::config::Config;
use crate::dag::TaskGraph;
use crate::db::Database;
use crate::executor::WaveExecutor;
use crate::models::{Run, RunStatus};
use crate::planner::Planner;

use super::RunArgs;

/// Run tasks from a file or auto-planned description
pub async fn run(args: RunArgs, config: &Config) -> Result<()> {
    tracing::info!("Starting dare run");

    // Initialize database
    let db = Database::connect(&config.general.database).await?;
    db.migrate().await?;

    // Get or create task graph
    let (run_name, graph) = if let Some(ref description) = args.auto {
        // Auto-plan mode
        println!("🧠 Auto-planning: {}", description);
        let planner = Planner::new(config);
        let graph = planner.auto_plan(description).await?;
        (description.clone(), graph)
    } else if let Some(ref run_id) = args.continue_run {
        // Continue interrupted run
        println!("▶️  Continuing run: {}", run_id);
        let run = db
            .get_run(run_id)
            .await?
            .context("Run not found")?;
        let graph = db.load_task_graph(&run.id).await?;
        (run.name, graph)
    } else if let Some(ref file) = args.file {
        // Load from file
        println!("📋 Loading: {}", file.display());
        let graph = load_task_file(file)?;
        let name = graph.name.clone().unwrap_or_else(|| file.display().to_string());
        (name, graph)
    } else {
        // Look for dare.yaml in current directory
        let default_file = PathBuf::from("dare.yaml");
        if default_file.exists() {
            println!("📋 Loading: dare.yaml");
            let graph = load_task_file(&default_file)?;
            let name = graph.name.clone().unwrap_or_else(|| "dare.yaml".to_string());
            (name, graph)
        } else {
            anyhow::bail!("No task file specified. Use --auto or provide a YAML file.");
        }
    };

    // Create run record
    let run = Run::new(&run_name);
    db.create_run(&run).await?;

    println!();
    println!("dare.run v{}", env!("CARGO_PKG_VERSION"));
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();
    println!("📋 Plan: {}", run_name);
    println!(
        "   {} tasks • {} waves",
        graph.nodes.len(),
        graph.wave_count()
    );
    println!();

    // Execute waves
    let executor = WaveExecutor::new(config, &db);
    let result = executor.execute(&run, &graph).await;

    match result {
        Ok(()) => {
            db.update_run_status(&run.id, RunStatus::Completed).await?;
            println!();
            println!("✅ Run completed successfully!");
        }
        Err(e) => {
            db.update_run_status(&run.id, RunStatus::Failed).await?;
            println!();
            println!("❌ Run failed: {}", e);
            return Err(e);
        }
    }

    Ok(())
}

fn load_task_file(path: &PathBuf) -> Result<TaskGraph> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let graph: TaskGraph = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))?;

    graph.validate()?;

    Ok(graph)
}
