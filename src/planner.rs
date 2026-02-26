//! Task planner - decomposes PRDs into task graphs
//!
//! Supports both:
//! 1. Loading task definitions from YAML files
//! 2. Auto-planning via LLM (spawns a planner agent)

use anyhow::{Context, Result};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::dag::{TaskGraph, TaskNode};
use crate::gateway::{GatewayClient, GatewayConfig, GatewayEvent};
use crate::models::Complexity;

/// Planner for creating task graphs from PRDs
pub struct Planner<'a> {
    config: &'a Config,
}

impl<'a> Planner<'a> {
    pub fn new(config: &'a Config) -> Self {
        Self { config }
    }

    /// Auto-plan from a description using an LLM agent
    pub async fn auto_plan(&self, description: &str) -> Result<TaskGraph> {
        tracing::info!("Auto-planning from description");

        // For now, if we can't connect to gateway, return a simple single-task graph
        let gateway_config = match GatewayConfig::load() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("Cannot connect to gateway for auto-planning: {}", e);
                return self.fallback_single_task(description);
            }
        };

        // Create event channel
        let (event_tx, mut event_rx) = mpsc::channel::<GatewayEvent>(100);

        // Connect to gateway
        let gateway = match GatewayClient::connect(gateway_config, event_tx).await {
            Ok(g) => g,
            Err(e) => {
                tracing::warn!("Failed to connect to gateway: {}", e);
                return self.fallback_single_task(description);
            }
        };

        // Build planner prompt
        let prompt = self.build_planner_prompt(description);

        // Spawn planner agent
        let session_key = match gateway
            .spawn_session("planner", "dare-planner", &prompt)
            .await
        {
            Ok(key) => key,
            Err(e) => {
                tracing::warn!("Failed to spawn planner agent: {}, using fallback decomposition", e);
                return self.fallback_simple_decomposition(description);
            }
        };

        tracing::info!(session_key = %session_key, "Planner agent spawned");
        println!("  🧠 Planning agent spawned...");

        // Wait for completion with timeout
        let timeout = Duration::from_secs(120);
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            if tokio::time::Instant::now() > deadline {
                tracing::warn!("Planner agent timed out");
                let _ = gateway.kill_session(&session_key).await;
                return self.fallback_single_task(description);
            }

            match tokio::time::timeout(Duration::from_secs(5), event_rx.recv()).await {
                Ok(Some(GatewayEvent::SessionCompleted { session_key: key }))
                    if key == session_key =>
                {
                    tracing::info!("Planner agent completed");
                    println!("  ✓ Planning complete");
                    break;
                }
                Ok(Some(GatewayEvent::SessionFailed { session_key: key, error }))
                    if key == session_key =>
                {
                    tracing::warn!("Planner agent failed: {}", error);
                    return self.fallback_single_task(description);
                }
                _ => continue,
            }
        }

        // In a real implementation, we'd capture the agent's output and parse the YAML
        // For now, we'll return a simple decomposition
        // TODO: Implement output capture from gateway
        
        self.fallback_simple_decomposition(description)
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

1. Analyze the task and identify the discrete subtasks needed
2. Determine dependencies between subtasks (what must complete before what)
3. Identify which files each subtask will create or modify
4. Output a YAML task graph

## Output Format

Output ONLY valid YAML in this exact format (no explanation, just YAML):

```yaml
name: "Task Name"
description: "Brief description of the overall task"
tasks:
  - id: unique_snake_case_id
    description: "What this task does in detail"
    depends_on: []  # List of task IDs this depends on
    files: [path/to/file.rs]  # Files this task will create/modify
  
  - id: second_task
    description: "Second task description"
    depends_on: [unique_snake_case_id]
    files: [src/another.rs]
```

## Rules

1. Use short, descriptive task IDs (snake_case)
2. Each task should be atomic - completable by one agent in 2-10 minutes
3. Be explicit about dependencies - if B needs A's output, B depends on A
4. List all files each task will create or modify
5. Tasks with no dependencies can run in parallel
6. Validate that all dependency IDs exist in the task list

## Guidelines for Decomposition

- **Schema/migrations first** - Database changes before code
- **Models before services** - Data structures before business logic
- **Services before handlers** - Business logic before HTTP endpoints
- **Core before extras** - Main functionality before tests/docs
- **Tests last** - After implementation is complete

## Example

For "Add user authentication with JWT":

```yaml
name: "User Authentication"
description: "Implement JWT-based authentication"
tasks:
  - id: user_migration
    description: "Create users table with email, password_hash columns"
    depends_on: []
    files: [migrations/001_users.sql]
    
  - id: user_model
    description: "Implement User model with password hashing via bcrypt"
    depends_on: [user_migration]
    files: [src/models/user.rs, src/models/mod.rs]
    
  - id: jwt_util
    description: "JWT token generation and validation utilities"
    depends_on: []
    files: [src/auth/jwt.rs, src/auth/mod.rs]
    
  - id: auth_handlers
    description: "Login, logout, and token refresh endpoints"
    depends_on: [user_model, jwt_util]
    files: [src/handlers/auth.rs]
    
  - id: auth_middleware
    description: "JWT validation middleware for protected routes"
    depends_on: [jwt_util]
    files: [src/middleware/auth.rs]
    
  - id: auth_tests
    description: "Integration tests for authentication flow"
    depends_on: [auth_handlers, auth_middleware]
    files: [tests/auth_test.rs]
```

---

Now analyze the task and output the YAML task graph.
Output ONLY the YAML, no explanation before or after.
"#,
            description = description
        )
    }

    /// Fallback: return a single-task graph when planning fails
    fn fallback_single_task(&self, description: &str) -> Result<TaskGraph> {
        tracing::info!("Using fallback single-task graph");
        
        let mut graph = TaskGraph::new();
        graph.name = Some(description.to_string());
        graph.description = Some("Auto-generated single-task plan".to_string());
        let prefix = &uuid::Uuid::new_v4().to_string()[..8];
        
        graph.add_task(TaskNode {
            id: format!("{}-main", prefix),
            description: description.to_string(),
            depends_on: vec![],
            outputs: vec![],
            estimated_complexity: Some(Complexity::Medium),
        });

        Ok(graph)
    }

    /// Fallback: simple heuristic-based decomposition
    fn fallback_simple_decomposition(&self, description: &str) -> Result<TaskGraph> {
        tracing::info!("Using heuristic decomposition");
        
        let mut graph = TaskGraph::new();
        graph.name = Some(description.to_string());
        
        // Generate a short unique prefix to avoid task ID collisions across runs
        let prefix = &uuid::Uuid::new_v4().to_string()[..8];
        
        let desc_lower = description.to_lowercase();
        
        // Check for common patterns and create appropriate task structure
        if desc_lower.contains("api") || desc_lower.contains("endpoint") || desc_lower.contains("rest") {
            // REST API pattern
            graph.add_task(TaskNode {
                id: format!("{}-models", prefix).to_string(),
                description: format!("Create data models for: {}", description),
                depends_on: vec![],
                outputs: vec![PathBuf::from("src/models/")],
                estimated_complexity: Some(Complexity::Small),
            });
            
            graph.add_task(TaskNode {
                id: format!("{}-handlers", prefix).to_string(),
                description: format!("Implement API handlers for: {}", description),
                depends_on: vec![format!("{}-models", prefix)],
                outputs: vec![PathBuf::from("src/handlers/")],
                estimated_complexity: Some(Complexity::Medium),
            });
            
            graph.add_task(TaskNode {
                id: format!("{}-routes", prefix).to_string(),
                description: "Configure routes and wire up handlers".to_string(),
                depends_on: vec![format!("{}-handlers", prefix)],
                outputs: vec![PathBuf::from("src/routes.rs")],
                estimated_complexity: Some(Complexity::Small),
            });
        } else if desc_lower.contains("database") || desc_lower.contains("migration") {
            // Database pattern
            graph.add_task(TaskNode {
                id: format!("{}-migration", prefix).to_string(),
                description: format!("Create database migration for: {}", description),
                depends_on: vec![],
                outputs: vec![PathBuf::from("migrations/")],
                estimated_complexity: Some(Complexity::Small),
            });
            
            graph.add_task(TaskNode {
                id: format!("{}-db_module", prefix).to_string(),
                description: "Implement database access layer".to_string(),
                depends_on: vec![format!("{}-migration", prefix)],
                outputs: vec![PathBuf::from("src/db.rs")],
                estimated_complexity: Some(Complexity::Medium),
            });
        } else {
            // Generic single task
            graph.add_task(TaskNode {
                id: format!("{}-main", prefix).to_string(),
                description: description.to_string(),
                depends_on: vec![],
                outputs: vec![],
                estimated_complexity: Some(Complexity::Medium),
            });
        }

        Ok(graph)
    }

    /// Load and validate a task file
    pub fn load_task_file(path: &PathBuf) -> Result<TaskGraph> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read {}", path.display()))?;

        let graph: TaskGraph = serde_yaml::from_str(&content)
            .with_context(|| format!("Failed to parse {}", path.display()))?;

        graph.validate()?;
        
        Ok(graph)
    }

    /// Validate and optionally enhance a task graph
    pub fn validate(&self, graph: &TaskGraph) -> Result<()> {
        graph.validate()
    }

    /// Estimate complexity of a task node
    pub fn estimate_complexity(node: &TaskNode) -> Complexity {
        let desc_len = node.description.len();
        let output_count = node.outputs.len();

        match (desc_len, output_count) {
            (_, 0) if desc_len < 100 => Complexity::Small,
            (0..=100, 1) => Complexity::Small,
            (0..=100, _) => Complexity::Medium,
            (101..=300, _) => Complexity::Medium,
            (301..=500, _) => Complexity::Large,
            _ => Complexity::ExtraLarge,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_load_valid_task_file() {
        let yaml = r#"
name: "Test Plan"
tasks:
  - id: task1
    description: "First task"
    depends_on: []
    outputs: [src/main.rs]
  - id: task2
    description: "Second task"
    depends_on: [task1]
    outputs: [src/lib.rs]
"#;

        let mut file = NamedTempFile::new().unwrap();
        file.write_all(yaml.as_bytes()).unwrap();

        let graph = Planner::load_task_file(&file.path().to_path_buf()).unwrap();
        assert_eq!(graph.nodes.len(), 2);
        assert!(graph.nodes.contains_key("task1"));
        assert!(graph.nodes.contains_key("task2"));
    }

    #[test]
    fn test_complexity_estimation() {
        let small_task = TaskNode {
            id: "small".to_string(),
            description: "Short".to_string(),
            depends_on: vec![],
            outputs: vec![],
            estimated_complexity: None,
        };
        assert_eq!(Planner::estimate_complexity(&small_task), Complexity::Small);

        let large_task = TaskNode {
            id: "large".to_string(),
            description: "A".repeat(400),
            depends_on: vec![],
            outputs: vec![PathBuf::from("a"), PathBuf::from("b"), PathBuf::from("c")],
            estimated_complexity: None,
        };
        assert_eq!(Planner::estimate_complexity(&large_task), Complexity::Large);
    }
}
