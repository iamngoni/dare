//! Task planner - decomposes PRDs into task graphs

use anyhow::Result;
use std::path::PathBuf;

use crate::config::Config;
use crate::dag::{TaskGraph, TaskNode};
use crate::gateway::GatewayClient;

/// Planner for creating task graphs from PRDs
pub struct Planner<'a> {
    config: &'a Config,
}

impl<'a> Planner<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self { config }
    }

    /// Auto-plan from a description using an LLM
    pub async fn auto_plan(&self, description: &str) -> Result<TaskGraph> {
        tracing::info!("Auto-planning from description: {}", description);

        // Connect to gateway to spawn a planner agent
        let gateway = GatewayClient::connect(self.config.gateway.port).await?;

        // Build planner prompt
        let prompt = self.build_planner_prompt(description);

        // Spawn planner agent
        let session_id = gateway.spawn_agent("planner", &prompt).await?;

        // Wait for response
        // TODO: Implement proper response parsing
        // For now, return a placeholder graph
        tracing::info!("Planner session: {}", session_id);

        // In a real implementation, we would:
        // 1. Subscribe to the session output
        // 2. Wait for the YAML output
        // 3. Parse and validate the task graph

        // Placeholder: return a simple single-task graph
        let mut graph = TaskGraph::new();
        graph.name = Some(description.to_string());
        graph.add_task(TaskNode {
            id: "main".to_string(),
            description: description.to_string(),
            depends_on: vec![],
            outputs: vec![],
            estimated_complexity: None,
        });

        Ok(graph)
    }

    /// Build the prompt for the planner agent
    fn build_planner_prompt(&self, description: &str) -> String {
        format!(
            r#"# dare.run Task Planner

You are a planning agent for dare.run, a multi-agent orchestration tool.
Your job is to decompose a task description into a dependency graph of subtasks.

## Task to Plan
{description}

## Instructions

1. Analyze the task and identify the discrete subtasks
2. Determine dependencies between subtasks
3. Output a YAML task graph

## Output Format

Output ONLY valid YAML in this format:

```yaml
name: "Task Name"
description: "Brief description"
tasks:
  - id: unique_id
    description: "What this task does"
    depends_on: [list, of, dependency, ids]
    outputs: [list, of, output, files]
  
  - id: another_task
    description: "Another task"
    depends_on: [unique_id]
    outputs: [src/main.rs]
```

## Rules

1. Use short, descriptive task IDs (snake_case)
2. Each task should be atomic - doable by one agent in ~2-5 minutes
3. Be explicit about dependencies
4. List expected output files for each task
5. Tasks with no dependencies should be listed first
6. Validate that all dependency IDs exist

## Analyze the codebase first

Before planning, examine:
- Existing file structure
- Tech stack (look at Cargo.toml, package.json, etc.)
- Coding conventions

Then plan tasks that fit the existing codebase.

---

Now analyze and output the YAML task graph for the requested task.
Output ONLY the YAML, no explanation.
"#,
            description = description
        )
    }

    /// Validate and enhance a task graph
    pub fn validate_and_enhance(&self, graph: &mut TaskGraph) -> Result<()> {
        // Validate
        graph.validate()?;

        // Estimate complexity if enabled
        if self.config.planning.complexity_estimation {
            for node in graph.nodes.values_mut() {
                if node.estimated_complexity.is_none() {
                    node.estimated_complexity = Some(self.estimate_complexity(node));
                }
            }
        }

        Ok(())
    }

    /// Estimate task complexity based on description and outputs
    fn estimate_complexity(&self, node: &TaskNode) -> crate::models::Complexity {
        use crate::models::Complexity;

        let desc_len = node.description.len();
        let output_count = node.outputs.len();

        // Simple heuristic
        match (desc_len, output_count) {
            (_, 0) => Complexity::Small,
            (0..=100, 1) => Complexity::Small,
            (0..=100, _) => Complexity::Medium,
            (101..=300, _) => Complexity::Medium,
            (301..=500, _) => Complexity::Large,
            _ => Complexity::ExtraLarge,
        }
    }
}

/// Template for common task patterns
#[derive(Debug, Clone)]
pub struct TaskTemplate {
    pub name: String,
    pub tasks: Vec<TaskTemplateNode>,
}

#[derive(Debug, Clone)]
pub struct TaskTemplateNode {
    pub id_pattern: String,
    pub description_template: String,
    pub depends_on: Vec<String>,
    pub output_patterns: Vec<String>,
}

impl TaskTemplate {
    /// Create a template for a typical CRUD feature
    pub fn crud_feature(entity_name: &str) -> TaskGraph {
        let mut graph = TaskGraph::new();
        graph.name = Some(format!("CRUD for {}", entity_name));

        // Migration
        graph.add_task(TaskNode {
            id: format!("{}_migration", entity_name.to_lowercase()),
            description: format!("Create database migration for {} table", entity_name),
            depends_on: vec![],
            outputs: vec![PathBuf::from(format!(
                "migrations/xxx_{}.sql",
                entity_name.to_lowercase()
            ))],
            estimated_complexity: Some(crate::models::Complexity::Small),
        });

        // Model
        graph.add_task(TaskNode {
            id: format!("{}_model", entity_name.to_lowercase()),
            description: format!("Create {} model with validation", entity_name),
            depends_on: vec![format!("{}_migration", entity_name.to_lowercase())],
            outputs: vec![PathBuf::from(format!(
                "src/models/{}.rs",
                entity_name.to_lowercase()
            ))],
            estimated_complexity: Some(crate::models::Complexity::Medium),
        });

        // Handlers
        graph.add_task(TaskNode {
            id: format!("{}_handlers", entity_name.to_lowercase()),
            description: format!("Create CRUD handlers for {}", entity_name),
            depends_on: vec![format!("{}_model", entity_name.to_lowercase())],
            outputs: vec![PathBuf::from(format!(
                "src/handlers/{}.rs",
                entity_name.to_lowercase()
            ))],
            estimated_complexity: Some(crate::models::Complexity::Large),
        });

        // Tests
        graph.add_task(TaskNode {
            id: format!("{}_tests", entity_name.to_lowercase()),
            description: format!("Create integration tests for {}", entity_name),
            depends_on: vec![format!("{}_handlers", entity_name.to_lowercase())],
            outputs: vec![PathBuf::from(format!(
                "tests/{}_test.rs",
                entity_name.to_lowercase()
            ))],
            estimated_complexity: Some(crate::models::Complexity::Medium),
        });

        graph
    }
}
