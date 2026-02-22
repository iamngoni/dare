//! `dare plan` command - Preview or generate execution plans

use anyhow::{Context, Result};

use crate::config::Config;
use crate::dag::TaskGraph;
use crate::planner::Planner;

use super::{PlanArgs, PlanFormat};

/// Preview or generate execution plan
pub async fn run(args: PlanArgs, config: &Config) -> Result<()> {
    let graph = if let Some(ref description) = args.auto {
        // Auto-plan mode
        println!("🧠 Auto-planning: {}", description);
        println!();
        let planner = Planner::new(config);
        planner.auto_plan(description).await?
    } else if let Some(ref file) = args.file {
        // Load from file
        let content = std::fs::read_to_string(file)
            .with_context(|| format!("Failed to read {}", file.display()))?;
        serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", file.display()))?
    } else {
        anyhow::bail!("Provide a task file or use --auto");
    };

    // Validate
    if args.validate {
        match graph.validate() {
            Ok(()) => {
                println!("✅ Plan is valid");
                println!("   {} tasks, {} waves", graph.nodes.len(), graph.wave_count());
            }
            Err(e) => {
                println!("❌ Plan is invalid: {}", e);
                return Err(e);
            }
        }
        return Ok(());
    }

    // Output based on format
    match args.format {
        PlanFormat::Pretty => print_pretty(&graph),
        PlanFormat::Yaml => {
            println!("{}", serde_yaml::to_string(&graph)?);
        }
        PlanFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&graph)?);
        }
    }

    Ok(())
}

fn print_pretty(graph: &TaskGraph) {
    println!("📋 Execution Plan");
    if let Some(ref name) = graph.name {
        println!("   Name: {}", name);
    }
    println!("   Tasks: {}", graph.nodes.len());
    println!("   Waves: {}", graph.wave_count());
    println!();

    // Group by wave
    let waves = graph.compute_waves();
    for (wave_num, tasks) in waves.iter().enumerate() {
        println!(
            "Wave {} ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━",
            wave_num
        );
        for task_id in tasks {
            if let Some(task) = graph.nodes.get(task_id) {
                println!("  ○ {} - {}", task_id, task.description);
                if !task.outputs.is_empty() {
                    println!(
                        "    outputs: {}",
                        task.outputs
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
        }
        println!();
    }
}
