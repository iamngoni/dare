//! Wave executor - runs tasks in parallel waves via passive observation
//!
//! Key principle: Agents work naturally, we observe passively.
//! Session lifecycle = task lifecycle. When a session completes, the task is done.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

use crate::config::Config;
use crate::dag::{TaskGraph, TaskNode};
use crate::db::Database;
use crate::gateway::{GatewayClient, GatewayConfig, GatewayEvent, SessionState};
use crate::models::{Run, RunStatus, TaskId};

/// Result bus for inter-task data passing
pub struct ResultBus {
    results: HashMap<TaskId, TaskResult>,
}

/// Result from a completed task
#[derive(Debug, Clone)]
pub struct TaskResult {
    pub task_id: TaskId,
    pub summary: String,
}

impl ResultBus {
    pub fn new() -> Self {
        Self {
            results: HashMap::new(),
        }
    }

    /// Store result when task completes
    pub fn store(&mut self, task_id: TaskId, summary: String) {
        self.results.insert(
            task_id.clone(),
            TaskResult {
                task_id,
                summary,
            },
        );
    }

    /// Get context from completed dependencies
    pub fn get_dependency_context(&self, task: &TaskNode) -> String {
        let mut context = String::new();
        for dep_id in &task.depends_on {
            if let Some(result) = self.results.get(dep_id) {
                context.push_str(&format!(
                    "## Task `{}` (completed)\n{}\n\n",
                    dep_id, result.summary
                ));
            }
        }
        context
    }
}

/// File scope tracker - prevents concurrent tasks from conflicting on files
pub struct FileScopeTracker {
    // Maps file paths to the task that owns them
    active_claims: HashMap<String, TaskId>,
}

impl FileScopeTracker {
    pub fn new() -> Self {
        Self {
            active_claims: HashMap::new(),
        }
    }

    /// Check if a task can be scheduled (no file conflicts with active tasks)
    pub fn can_schedule(&self, task: &TaskNode, active_tasks: &HashSet<TaskId>) -> bool {
        for output in &task.outputs {
            let path_str = output.to_string_lossy().to_string();
            if let Some(owner) = self.active_claims.get(&path_str) {
                if active_tasks.contains(owner) {
                    return false;
                }
            }
        }
        true
    }

    /// Claim files for a task
    pub fn claim(&mut self, task: &TaskNode) {
        for output in &task.outputs {
            let path_str = output.to_string_lossy().to_string();
            self.active_claims.insert(path_str, task.id.clone());
        }
    }

    /// Release files when task completes
    pub fn release(&mut self, task_id: &TaskId) {
        self.active_claims.retain(|_, owner| owner != task_id);
    }
}

/// Tracks an active agent session
struct ActiveSession {
    session_key: String,
    task_id: TaskId,
}

/// Executes task graphs in parallel waves
pub struct WaveExecutor<'a> {
    config: &'a Config,
    db: &'a Database,
}

impl<'a> WaveExecutor<'a> {
    pub fn new(config: &'a Config, db: &'a Database) -> Self {
        Self { config, db }
    }

    /// Execute the entire task graph
    pub async fn execute(&self, run: &Run, graph: &TaskGraph) -> Result<()> {
        tracing::info!(run_id = %run.id, "Starting execution");

        // Update run status
        self.db.update_run_status(&run.id, RunStatus::Running).await?;

        // Load gateway config
        let gateway_config = GatewayConfig::load().context("Failed to load gateway config")?;

        // Create event channel for gateway events
        let (event_tx, mut event_rx) = mpsc::channel::<GatewayEvent>(100);

        // Connect to gateway
        let gateway = GatewayClient::connect(gateway_config, event_tx)
            .await
            .context("Failed to connect to gateway")?;

        // Initialize result bus and file tracker
        let mut result_bus = ResultBus::new();
        let mut file_tracker = FileScopeTracker::new();

        // Compute waves
        let waves = graph.compute_waves();
        let total_waves = waves.len();
        tracing::info!(wave_count = total_waves, "Execution plan computed");

        // Execute each wave
        for (wave_num, task_ids) in waves.iter().enumerate() {
            // Check if paused
            if let Some(current_run) = self.db.get_run(&run.id).await? {
                if current_run.status == RunStatus::Paused {
                    tracing::info!(wave = wave_num, "Run paused");
                    println!("⏸️  Run paused after wave {}", wave_num);
                    return Ok(());
                }
            }

            println!(
                "\nWave {}/{} ━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━",
                wave_num + 1,
                total_waves
            );
            tracing::info!(wave = wave_num, task_count = task_ids.len(), "Starting wave");

            // Get task nodes for this wave
            let tasks: Vec<&TaskNode> = task_ids
                .iter()
                .filter_map(|id| graph.nodes.get(id))
                .collect();

            // Execute wave with concurrency limit
            let max_parallel = self.config.execution.max_parallel_agents.min(tasks.len());

            let result = self
                .execute_wave(
                    run,
                    &tasks,
                    max_parallel,
                    &gateway,
                    &mut event_rx,
                    &mut result_bus,
                    &mut file_tracker,
                )
                .await;

            if let Err(e) = result {
                tracing::error!(wave = wave_num, error = %e, "Wave failed");
                return Err(e);
            }
        }

        Ok(())
    }

    /// Execute a single wave of tasks
    async fn execute_wave(
        &self,
        _run: &Run,
        tasks: &[&TaskNode],
        max_parallel: usize,
        gateway: &Arc<GatewayClient>,
        event_rx: &mut mpsc::Receiver<GatewayEvent>,
        result_bus: &mut ResultBus,
        file_tracker: &mut FileScopeTracker,
    ) -> Result<()> {
        let wave_timeout = Duration::from_secs(self.config.execution.wave_timeout_seconds);

        // Track active sessions and pending tasks
        let mut active: HashMap<String, ActiveSession> = HashMap::new(); // session_key -> session
        let mut active_task_ids: HashSet<TaskId> = HashSet::new();
        let mut pending: Vec<&TaskNode> = tasks.to_vec();
        let mut completed: Vec<TaskId> = Vec::new();
        let mut failed: Vec<(TaskId, String)> = Vec::new();

        // Main execution loop with timeout
        let deadline = tokio::time::Instant::now() + wave_timeout;

        loop {
            // Spawn tasks up to max_parallel, respecting file scope
            while active.len() < max_parallel && !pending.is_empty() {
                // Find next task that can be scheduled (no file conflicts)
                let schedulable_idx = pending.iter().position(|task| {
                    file_tracker.can_schedule(task, &active_task_ids)
                });

                if let Some(idx) = schedulable_idx {
                    let task = pending.remove(idx);
                    
                    // Build context for the task
                    let context = self.build_agent_context(task, result_bus);
                    let label = format!("dare-{}", task.id);

                    // Spawn via gateway
                    match gateway.spawn_session(&task.id, &label, &context).await {
                        Ok(session_key) => {
                            println!("  ⏳ {} (spawned)", task.id);
                            
                            // Claim files
                            file_tracker.claim(task);
                            active_task_ids.insert(task.id.clone());
                            
                            active.insert(
                                session_key.clone(),
                                ActiveSession {
                                    session_key,
                                    task_id: task.id.clone(),
                                },
                            );
                        }
                        Err(e) => {
                            println!("  ✗ {} (spawn failed: {})", task.id, e);
                            failed.push((task.id.clone(), e.to_string()));
                        }
                    }
                } else {
                    // No schedulable tasks, wait for active ones to complete
                    break;
                }
            }

            // Check if done
            if active.is_empty() && pending.is_empty() {
                break;
            }

            // Check timeout
            if tokio::time::Instant::now() > deadline {
                tracing::warn!("Wave timed out");
                
                // Kill remaining sessions
                for (session_key, session) in &active {
                    let _ = gateway.kill_session(session_key).await;
                    failed.push((session.task_id.clone(), "Timed out".to_string()));
                }
                break;
            }

            // Wait for events with timeout
            let wait_timeout = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(wait_timeout, event_rx.recv()).await {
                Ok(Some(event)) => {
                    match event {
                        GatewayEvent::SessionCompleted { session_key } => {
                            if let Some(session) = active.remove(&session_key) {
                                println!("  ✓ {} (completed)", session.task_id);
                                tracing::info!(task_id = %session.task_id, "Task completed");
                                
                                // Release files
                                file_tracker.release(&session.task_id);
                                active_task_ids.remove(&session.task_id);
                                
                                // Store result
                                result_bus.store(
                                    session.task_id.clone(),
                                    format!("Task {} completed successfully", session.task_id),
                                );
                                
                                completed.push(session.task_id);
                            }
                        }
                        GatewayEvent::SessionFailed { session_key, error } => {
                            if let Some(session) = active.remove(&session_key) {
                                println!("  ✗ {} (failed: {})", session.task_id, error);
                                tracing::error!(task_id = %session.task_id, error = %error, "Task failed");
                                
                                // Release files
                                file_tracker.release(&session.task_id);
                                active_task_ids.remove(&session.task_id);
                                
                                failed.push((session.task_id, error));
                            }
                        }
                        GatewayEvent::SessionStateChanged { session_key, state, .. } => {
                            if let Some(session) = active.get(&session_key) {
                                tracing::debug!(task_id = %session.task_id, state = %state, "Session state changed");
                            }
                        }
                        GatewayEvent::Other { event_type, .. } => {
                            tracing::trace!(event_type = %event_type, "Other gateway event");
                        }
                    }
                }
                Ok(None) => {
                    // Channel closed
                    tracing::warn!("Event channel closed");
                    break;
                }
                Err(_) => {
                    // Timeout - check session states directly
                    for (session_key, session) in active.iter() {
                        if let Some(state) = gateway.get_session_state(session_key).await {
                            match state {
                                SessionState::Completed => {
                                    // Will be handled by event
                                }
                                SessionState::Failed(e) => {
                                    tracing::warn!(task_id = %session.task_id, error = %e, "Session failed (poll)");
                                }
                                _ => {}
                            }
                        }
                    }
                }
            }
        }

        // Report results
        if !failed.is_empty() {
            let failure_count = failed.len();
            let failures: Vec<String> = failed
                .iter()
                .map(|(id, err)| format!("{}: {}", id, err))
                .collect();
            anyhow::bail!(
                "Wave failed: {} task(s) failed\n{}",
                failure_count,
                failures.join("\n")
            );
        }

        Ok(())
    }

    /// Build the context/prompt for an agent
    fn build_agent_context(&self, task: &TaskNode, result_bus: &ResultBus) -> String {
        let dep_context = result_bus.get_dependency_context(task);
        
        let files_list = if task.outputs.is_empty() {
            "No specific files assigned - work in the appropriate locations".to_string()
        } else {
            task.outputs
                .iter()
                .map(|p| format!("- {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n")
        };

        format!(
            r#"# Task: {task_id}

{description}

## Files You're Working On
{files}

{dep_section}

## Instructions
Complete this task. When you're done, the task is complete — no special markers needed.
Just do your work naturally and ensure the expected files are properly created/modified.

Focus on:
1. Understanding the task requirements
2. Implementing the solution correctly
3. Ensuring code compiles and is well-structured
"#,
            task_id = task.id,
            description = task.description,
            files = files_list,
            dep_section = if dep_context.is_empty() {
                String::new()
            } else {
                format!("## Context from Completed Tasks\n{}", dep_context)
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_file_scope_tracker() {
        let mut tracker = FileScopeTracker::new();
        
        let task1 = TaskNode {
            id: "task1".to_string(),
            description: "Test task 1".to_string(),
            depends_on: vec![],
            outputs: vec![PathBuf::from("src/main.rs")],
            estimated_complexity: None,
        };
        
        let task2 = TaskNode {
            id: "task2".to_string(),
            description: "Test task 2".to_string(),
            depends_on: vec![],
            outputs: vec![PathBuf::from("src/main.rs")], // Same file!
            estimated_complexity: None,
        };
        
        let task3 = TaskNode {
            id: "task3".to_string(),
            description: "Test task 3".to_string(),
            depends_on: vec![],
            outputs: vec![PathBuf::from("src/lib.rs")], // Different file
            estimated_complexity: None,
        };
        
        let mut active = HashSet::new();
        
        // Task 1 can be scheduled
        assert!(tracker.can_schedule(&task1, &active));
        tracker.claim(&task1);
        active.insert("task1".to_string());
        
        // Task 2 conflicts with task 1
        assert!(!tracker.can_schedule(&task2, &active));
        
        // Task 3 doesn't conflict
        assert!(tracker.can_schedule(&task3, &active));
        
        // Release task 1
        tracker.release(&"task1".to_string());
        active.remove(&"task1".to_string());
        
        // Now task 2 can be scheduled
        assert!(tracker.can_schedule(&task2, &active));
    }

    #[test]
    fn test_result_bus() {
        let mut bus = ResultBus::new();
        
        bus.store("task1".to_string(), "Created database schema".to_string());
        bus.store("task2".to_string(), "Implemented models".to_string());
        
        let task = TaskNode {
            id: "task3".to_string(),
            description: "Test".to_string(),
            depends_on: vec!["task1".to_string(), "task2".to_string()],
            outputs: vec![],
            estimated_complexity: None,
        };
        
        let context = bus.get_dependency_context(&task);
        assert!(context.contains("task1"));
        assert!(context.contains("task2"));
        assert!(context.contains("database schema"));
        assert!(context.contains("models"));
    }
}
