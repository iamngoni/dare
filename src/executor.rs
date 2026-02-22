//! Wave executor - runs tasks in parallel waves via cron-based agent spawning
//!
//! Key principle: Use OpenClaw's cron API to spawn isolated agent sessions.
//! Each task becomes a one-shot cron job with an agentTurn payload.
//! Task lifecycle = session lifecycle. When a session completes, the task is done.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
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
            task_id,
            TaskResult { summary },
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

/// Tracks an active task with its cron job
struct ActiveTask {
    job_id: String,
    task_id: TaskId,
    session_key: Option<String>,
}

/// Executes task graphs in parallel waves using cron-based spawning
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
        println!("\n🔌 Connecting to OpenClaw Gateway...");
        let gateway = GatewayClient::connect(gateway_config, event_tx)
            .await
            .context("Failed to connect to gateway")?;
        println!("   ✓ Connected and authenticated");

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
        gateway: &std::sync::Arc<GatewayClient>,
        event_rx: &mut mpsc::Receiver<GatewayEvent>,
        result_bus: &mut ResultBus,
        file_tracker: &mut FileScopeTracker,
    ) -> Result<()> {
        let wave_timeout = Duration::from_secs(self.config.execution.wave_timeout_seconds);

        // Track active tasks and pending tasks
        let mut active: HashMap<String, ActiveTask> = HashMap::new(); // job_id -> task
        let mut session_to_job: HashMap<String, String> = HashMap::new(); // session_key -> job_id
        let mut active_task_ids: HashSet<TaskId> = HashSet::new();
        let mut spawned_task_ids: HashSet<TaskId> = HashSet::new(); // CRITICAL: track all spawned tasks to prevent duplicates
        let mut pending: Vec<&TaskNode> = tasks.to_vec();
        let mut completed: Vec<TaskId> = Vec::new();
        let mut failed: Vec<(TaskId, String)> = Vec::new();

        // Main execution loop with timeout
        let deadline = tokio::time::Instant::now() + wave_timeout;
        
        tracing::info!(
            pending_count = pending.len(),
            task_ids = ?pending.iter().map(|t| &t.id).collect::<Vec<_>>(),
            "Starting wave execution loop"
        );

        loop {
            // Spawn tasks up to max_parallel, respecting file scope
            while active.len() < max_parallel && !pending.is_empty() {
                // Find next task that can be scheduled (no file conflicts, not already spawned)
                let schedulable_idx = pending.iter().position(|task| {
                    !spawned_task_ids.contains(&task.id) && 
                    file_tracker.can_schedule(task, &active_task_ids)
                });

                if let Some(idx) = schedulable_idx {
                    let task = pending.remove(idx);
                    
                    // Double-check we haven't already spawned this task (defensive)
                    if spawned_task_ids.contains(&task.id) {
                        tracing::warn!(task_id = %task.id, "Skipping already-spawned task");
                        continue;
                    }
                    
                    // Mark as spawned BEFORE the async call to prevent race conditions
                    spawned_task_ids.insert(task.id.clone());
                    
                    // Build context for the task
                    let context = self.build_agent_context(task, result_bus);
                    let label = format!("dare-{}", task.id);

                    // Spawn via gateway cron API
                    match gateway.spawn_task(&task.id, &label, &context).await {
                        Ok(job_id) => {
                            println!("  ⏳ {} (spawned: {})", task.id, &job_id[..20.min(job_id.len())]);
                            
                            // Claim files
                            file_tracker.claim(task);
                            active_task_ids.insert(task.id.clone());
                            
                            active.insert(
                                job_id.clone(),
                                ActiveTask {
                                    job_id,
                                    task_id: task.id.clone(),
                                    session_key: None,
                                },
                            );
                        }
                        Err(e) => {
                            println!("  ✗ {} (spawn failed: {})", task.id, e);
                            failed.push((task.id.clone(), e.to_string()));
                            // Note: task stays in spawned_task_ids to prevent retry
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
                
                // Clean up remaining jobs
                for (job_id, task) in &active {
                    let _ = gateway.remove_job(job_id).await;
                    failed.push((task.task_id.clone(), "Timed out".to_string()));
                }
                break;
            }

            // Wait for events with timeout
            let wait_timeout = deadline - tokio::time::Instant::now();
            match tokio::time::timeout(wait_timeout, event_rx.recv()).await {
                Ok(Some(event)) => {
                    match event {
                        GatewayEvent::Connected => {
                            // Reconnected, might need to handle this
                            tracing::info!("Gateway connection restored");
                        }
                        GatewayEvent::CronTriggered { job_id, session_key } => {
                            // Update session key mapping
                            if let Some(task) = active.get_mut(&job_id) {
                                task.session_key = Some(session_key.clone());
                                session_to_job.insert(session_key.clone(), job_id.clone());
                                println!("  🔄 {} (running: {}...)", task.task_id, &session_key[..20.min(session_key.len())]);
                            }
                        }
                        GatewayEvent::SessionStateChanged { session_key, state } => {
                            if let Some(job_id) = session_to_job.get(&session_key) {
                                if let Some(task) = active.get(job_id) {
                                    tracing::debug!(task_id = %task.task_id, state = %state, "Task state changed");
                                }
                            }
                        }
                        GatewayEvent::SessionCompleted { session_key } => {
                            tracing::info!(session_key = %session_key, "Received session completion event");
                            
                            // Find the task - the session_key format is:
                            // agent:main:cron:<jobId>:run:<runId>
                            // We need to extract the jobId to correlate with our active tasks
                            
                            let job_id = session_to_job.remove(&session_key).or_else(|| {
                                // Try to find by session_key in active tasks
                                active.iter()
                                    .find(|(_, t)| t.session_key.as_ref() == Some(&session_key))
                                    .map(|(id, _)| id.clone())
                            }).or_else(|| {
                                // Check if session_key is the job_id directly
                                if active.contains_key(&session_key) {
                                    return Some(session_key.clone());
                                }
                                
                                // Extract job_id from session_key if it contains our job IDs
                                // Session key format: agent:main:cron:<jobId>:run:<runId>
                                for (active_job_id, _) in active.iter() {
                                    if session_key.contains(active_job_id) {
                                        return Some(active_job_id.clone());
                                    }
                                }
                                None
                            });
                            
                            if let Some(job_id) = job_id {
                                if let Some(task) = active.remove(&job_id) {
                                    println!("  ✓ {} (completed)", task.task_id);
                                    tracing::info!(task_id = %task.task_id, job_id = %job_id, "Task completed");
                                    
                                    // Release files
                                    file_tracker.release(&task.task_id);
                                    active_task_ids.remove(&task.task_id);
                                    
                                    // Store result
                                    result_bus.store(
                                        task.task_id.clone(),
                                        format!("Task {} completed successfully", task.task_id),
                                    );
                                    
                                    // Clean up cron job
                                    let _ = gateway.remove_job(&job_id).await;
                                    
                                    completed.push(task.task_id);
                                }
                            } else {
                                tracing::warn!(session_key = %session_key, "Received completion for unknown session");
                            }
                        }
                        GatewayEvent::SessionFailed { session_key, error } => {
                            tracing::info!(session_key = %session_key, error = %error, "Received session failure event");
                            
                            // Find the task - same logic as SessionCompleted
                            let job_id = session_to_job.remove(&session_key).or_else(|| {
                                active.iter()
                                    .find(|(_, t)| t.session_key.as_ref() == Some(&session_key))
                                    .map(|(id, _)| id.clone())
                            }).or_else(|| {
                                if active.contains_key(&session_key) {
                                    return Some(session_key.clone());
                                }
                                for (active_job_id, _) in active.iter() {
                                    if session_key.contains(active_job_id) {
                                        return Some(active_job_id.clone());
                                    }
                                }
                                None
                            });
                            
                            if let Some(job_id) = job_id {
                                if let Some(task) = active.remove(&job_id) {
                                    println!("  ✗ {} (failed: {})", task.task_id, error);
                                    tracing::error!(task_id = %task.task_id, error = %error, "Task failed");
                                    
                                    file_tracker.release(&task.task_id);
                                    active_task_ids.remove(&task.task_id);
                                    
                                    let _ = gateway.remove_job(&job_id).await;
                                    failed.push((task.task_id, error));
                                }
                            } else {
                                tracing::warn!(session_key = %session_key, "Received failure for unknown session");
                            }
                        }
                    }
                }
                Ok(None) => {
                    // Channel closed
                    tracing::warn!("Event channel closed");
                    break;
                }
                Err(_) => {
                    // Timeout - poll task states directly
                    let mut to_complete = Vec::new();
                    
                    for (job_id, task) in active.iter() {
                        if let Some(state) = gateway.get_task_state(job_id).await {
                            match state {
                                SessionState::Completed => {
                                    to_complete.push((job_id.clone(), task.task_id.clone(), None));
                                }
                                SessionState::Failed(e) => {
                                    to_complete.push((job_id.clone(), task.task_id.clone(), Some(e)));
                                }
                                _ => {}
                            }
                        }
                    }
                    
                    // Process completions found via polling
                    for (job_id, task_id, error) in to_complete {
                        if let Some(task) = active.remove(&job_id) {
                            if let Some(e) = error {
                                println!("  ✗ {} (failed: {})", task_id, e);
                                file_tracker.release(&task_id);
                                active_task_ids.remove(&task_id);
                                let _ = gateway.remove_job(&job_id).await;
                                failed.push((task_id, e));
                            } else {
                                println!("  ✓ {} (completed)", task_id);
                                file_tracker.release(&task_id);
                                active_task_ids.remove(&task_id);
                                result_bus.store(task_id.clone(), format!("Task {} completed", task_id));
                                let _ = gateway.remove_job(&job_id).await;
                                completed.push(task_id);
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
