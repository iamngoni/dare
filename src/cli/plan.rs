//! `dare plan` command - Preview or generate execution plan

use anyhow::Result;
use std::sync::Arc;

use crate::config::Config;
use crate::db::Database;
use crate::planner::Planner;

use super::{PlanArgs, PlanFormat};

/// Preview or generate execution plan
pub async fn run(args: PlanArgs, config: &Config) -> Result<()> {
    // Get or create task graph
    let graph = if let Some(ref description) = args.auto {
        println!("🧠 Planning: {}", description);
        println!();
        let db_path = config.database_path();
        let db = Arc::new(Database::connect(&db_path).await?);
        db.migrate().await?;
        let planner = Planner::new(config, &db);
        let council = planner.plan(description).await?;
        planner.to_task_graph(&council)?
    } else if let Some(ref file) = args.file {
        println!("📋 Loading: {}", file.display());
        println!();
        Planner::load_task_file(file)?
    } else {
        let default_file = std::path::PathBuf::from("dare.yaml");
        if default_file.exists() {
            println!("📋 Loading: dare.yaml");
            println!();
            Planner::load_task_file(&default_file)?
        } else {
            anyhow::bail!("No task file specified. Use --auto or provide a YAML file.");
        }
    };

    // Validate
    if args.validate {
        match graph.validate() {
            Ok(()) => {
                println!("✅ Plan is valid");
                println!("   {} tasks", graph.nodes.len());
                println!("   {} waves", graph.wave_count());
                return Ok(());
            }
            Err(e) => {
                println!("❌ Plan validation failed: {}", e);
                return Err(e);
            }
        }
    }

    // Output based on format
    match args.format {
        PlanFormat::Pretty => print_pretty(&graph),
        PlanFormat::Yaml => print_yaml(&graph)?,
        PlanFormat::Json => print_json(&graph)?,
    }

    Ok(())
}

fn print_pretty(graph: &crate::dag::TaskGraph) {
    println!(
        "dare.run v{} — Execution Plan",
        env!("CARGO_PKG_VERSION")
    );
    println!("━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━");
    println!();

    if let Some(ref name) = graph.name {
        println!("📋 Plan: {}", name);
    }
    if let Some(ref desc) = graph.description {
        println!("   {}", desc);
    }
    println!();

    let waves = graph.compute_waves();
    println!("📊 Summary: {} tasks • {} waves", graph.nodes.len(), waves.len());
    println!();

    // Show each wave
    for (i, task_ids) in waves.iter().enumerate() {
        println!("Wave {} ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━", i + 1);
        
        for task_id in task_ids {
            if let Some(node) = graph.nodes.get(task_id) {
                let agent_tag = node.agent_profile.as_deref().unwrap_or("unassigned");
                println!("  ○ {} [{}]", task_id, agent_tag);
                
                // Wrap description to 60 chars
                let desc = &node.description;
                if desc.len() > 60 {
                    println!("    {}", &desc[..60]);
                    let remaining = &desc[60..];
                    for chunk in remaining.as_bytes().chunks(60) {
                        println!("    {}", String::from_utf8_lossy(chunk));
                    }
                } else {
                    println!("    {}", desc);
                }

                if !node.depends_on.is_empty() {
                    println!("    ↳ depends on: {}", node.depends_on.join(", "));
                }

                if !node.outputs.is_empty() {
                    let outputs: Vec<_> = node.outputs.iter().map(|p| p.display().to_string()).collect();
                    println!("    → files: {}", outputs.join(", "));
                }
            }
        }
        println!();
    }

    // Show DAG structure
    println!("📈 Dependency Graph:");
    for (task_id, node) in &graph.nodes {
        let deps = if node.depends_on.is_empty() {
            "(root)".to_string()
        } else {
            format!("← {}", node.depends_on.join(", "))
        };
        println!("   {} {}", task_id, deps);
    }
    println!();
}

fn print_yaml(graph: &crate::dag::TaskGraph) -> Result<()> {
    // Convert to serializable format
    #[derive(serde::Serialize)]
    struct TaskGraphYaml {
        name: Option<String>,
        description: Option<String>,
        tasks: Vec<TaskNodeYaml>,
    }

    #[derive(serde::Serialize)]
    struct TaskNodeYaml {
        id: String,
        description: String,
        depends_on: Vec<String>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        files: Vec<String>,
    }

    let tasks: Vec<TaskNodeYaml> = graph
        .nodes
        .values()
        .map(|n| TaskNodeYaml {
            id: n.id.clone(),
            description: n.description.clone(),
            depends_on: n.depends_on.clone(),
            files: n.outputs.iter().map(|p| p.display().to_string()).collect(),
        })
        .collect();

    let yaml_graph = TaskGraphYaml {
        name: graph.name.clone(),
        description: graph.description.clone(),
        tasks,
    };

    println!("{}", serde_yaml::to_string(&yaml_graph)?);
    Ok(())
}

fn print_json(graph: &crate::dag::TaskGraph) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(&graph)?);
    Ok(())
}
