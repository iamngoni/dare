//! Database operations for dare
//!
//! SQLite-backed persistence for runs, tasks, and execution state.
//!
//! This module provides:
//! - Connection pool management with automatic retry
//! - Full CRUD operations for runs, tasks, logs, and messages
//! - File locking mechanism for coordinated file access
//! - Transaction support for atomic multi-operation batches

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, Sqlite, Transaction};
use std::path::Path;
use std::time::Duration;

use crate::dag::TaskGraph;
use crate::models::{AgentLog, Run, RunStatus, Task, TaskStatus};

/// Default connection pool size
const DEFAULT_POOL_SIZE: u32 = 5;

/// Connection timeout in seconds
const CONNECT_TIMEOUT_SECS: u64 = 30;

/// Idle timeout for connections in seconds
const IDLE_TIMEOUT_SECS: u64 = 600;

/// File lock record
#[derive(Debug, Clone)]
pub struct FileLock {
    pub path: String,
    pub run_id: String,
    pub task_id: String,
    pub lock_type: String,
    pub acquired_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// Task status counts
#[derive(Debug, Clone, Default)]
pub struct TaskCounts {
    pub pending: usize,
    pub spawned: usize,
    pub executing: usize,
    pub completed: usize,
    pub failed: usize,
}

/// Database health information
#[derive(Debug, Clone)]
pub struct DatabaseHealth {
    pub healthy: bool,
    pub latency_ms: u64,
    pub pool_size: u32,
    pub pool_idle: u32,
    pub run_count: u64,
    pub task_count: u64,
}

impl TaskCounts {
    pub fn total(&self) -> usize {
        self.pending + self.spawned + self.executing + self.completed + self.failed
    }

    pub fn in_progress(&self) -> usize {
        self.spawned + self.executing
    }
}

/// Database connection wrapper
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Connect to the database (creates if doesn't exist)
    pub async fn connect(path: &Path) -> Result<Self> {
        Self::connect_with_options(path, DEFAULT_POOL_SIZE).await
    }

    /// Connect with custom pool size
    pub async fn connect_with_options(path: &Path, max_connections: u32) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .context("Failed to create database directory")?;
        }

        let url = format!("sqlite:{}?mode=rwc", path.display());
        let pool = SqlitePoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS))
            .idle_timeout(Duration::from_secs(IDLE_TIMEOUT_SECS))
            .connect(&url)
            .await
            .context("Failed to connect to database")?;

        Ok(Self { pool })
    }

    /// Connect to an in-memory database (useful for testing)
    pub async fn connect_memory() -> Result<Self> {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .context("Failed to create in-memory database")?;

        Ok(Self { pool })
    }

    /// Begin a transaction for atomic operations
    pub async fn begin(&self) -> Result<Transaction<'_, Sqlite>> {
        self.pool
            .begin()
            .await
            .context("Failed to begin transaction")
    }

    /// Run database migrations
    pub async fn migrate(&self) -> Result<()> {
        sqlx::query(include_str!("../migrations/001_initial.sql"))
            .execute(&self.pool)
            .await
            .context("Failed to run database migrations")?;
        Ok(())
    }

    /// Run migrations and return Self (chainable)
    pub async fn migrated(self) -> Result<Self> {
        self.migrate().await?;
        Ok(self)
    }

    /// Get the underlying pool (for advanced use cases)
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ========================================================================
    // Runs
    // ========================================================================

    /// Create a new run
    pub async fn create_run(&self, run: &Run) -> Result<()> {
        sqlx::query(
            r#"
            INSERT INTO runs (id, name, description, status, config_json, created_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(&run.id)
        .bind(&run.name)
        .bind(&run.description)
        .bind(run.status.to_string())
        .bind(&run.config_json)
        .bind(run.created_at.to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get a run by ID
    pub async fn get_run(&self, id: &str) -> Result<Option<Run>> {
        let row: Option<SqliteRow> = sqlx::query("SELECT * FROM runs WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.map(|r| row_to_run(&r)))
    }

    /// Get the most recent run
    pub async fn get_latest_run(&self) -> Result<Option<Run>> {
        let row: Option<SqliteRow> =
            sqlx::query("SELECT * FROM runs ORDER BY created_at DESC LIMIT 1")
                .fetch_optional(&self.pool)
                .await?;

        Ok(row.map(|r| row_to_run(&r)))
    }

    /// List all runs
    pub async fn list_runs(&self, limit: usize) -> Result<Vec<Run>> {
        let rows: Vec<SqliteRow> =
            sqlx::query("SELECT * FROM runs ORDER BY created_at DESC LIMIT ?")
                .bind(limit as i32)
                .fetch_all(&self.pool)
                .await?;

        Ok(rows.iter().map(row_to_run).collect())
    }

    /// Update run status
    pub async fn update_run_status(&self, id: &str, status: RunStatus) -> Result<()> {
        let now = Utc::now().to_rfc3339();

        match status {
            RunStatus::Running => {
                sqlx::query("UPDATE runs SET status = ?, started_at = ? WHERE id = ?")
                    .bind(status.to_string())
                    .bind(&now)
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
            RunStatus::Completed | RunStatus::Failed => {
                sqlx::query("UPDATE runs SET status = ?, completed_at = ? WHERE id = ?")
                    .bind(status.to_string())
                    .bind(&now)
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
            _ => {
                sqlx::query("UPDATE runs SET status = ? WHERE id = ?")
                    .bind(status.to_string())
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
        }

        Ok(())
    }

    // ========================================================================
    // Tasks
    // ========================================================================

    /// Create tasks from a task graph
    pub async fn create_tasks_from_graph(&self, run_id: &str, graph: &TaskGraph) -> Result<()> {
        let waves = graph.compute_waves();

        // Create tasks
        for (wave_num, task_ids) in waves.iter().enumerate() {
            for task_id in task_ids {
                if let Some(node) = graph.nodes.get(task_id) {
                    let outputs_json = serde_json::to_string(&node.outputs)?;
                    
                    sqlx::query(
                        r#"
                        INSERT INTO tasks (id, run_id, description, status, wave, outputs_json, created_at)
                        VALUES (?, ?, ?, ?, ?, ?, ?)
                        "#,
                    )
                    .bind(task_id)
                    .bind(run_id)
                    .bind(&node.description)
                    .bind(TaskStatus::Pending.to_string())
                    .bind(wave_num as i32)
                    .bind(&outputs_json)
                    .bind(Utc::now().to_rfc3339())
                    .execute(&self.pool)
                    .await?;

                    // Create dependencies
                    for dep_id in &node.depends_on {
                        sqlx::query(
                            "INSERT INTO task_dependencies (task_id, depends_on_task_id) VALUES (?, ?)",
                        )
                        .bind(task_id)
                        .bind(dep_id)
                        .execute(&self.pool)
                        .await?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Get all tasks for a run
    pub async fn get_tasks(&self, run_id: &str) -> Result<Vec<Task>> {
        let rows: Vec<SqliteRow> =
            sqlx::query("SELECT * FROM tasks WHERE run_id = ? ORDER BY wave, id")
                .bind(run_id)
                .fetch_all(&self.pool)
                .await?;

        Ok(rows.iter().map(row_to_task).collect())
    }

    /// Get a specific task
    pub async fn get_task(&self, task_id: &str) -> Result<Option<Task>> {
        let row: Option<SqliteRow> = sqlx::query("SELECT * FROM tasks WHERE id = ?")
            .bind(task_id)
            .fetch_optional(&self.pool)
            .await?;

        Ok(row.map(|r| row_to_task(&r)))
    }

    /// Get tasks by status for a run
    pub async fn get_tasks_by_status(&self, run_id: &str, status: TaskStatus) -> Result<Vec<Task>> {
        let rows: Vec<SqliteRow> =
            sqlx::query("SELECT * FROM tasks WHERE run_id = ? AND status = ? ORDER BY wave, id")
                .bind(run_id)
                .bind(status.to_string())
                .fetch_all(&self.pool)
                .await?;

        Ok(rows.iter().map(row_to_task).collect())
    }

    /// Get running tasks for a run
    pub async fn get_running_tasks(&self, run_id: &str) -> Result<Vec<Task>> {
        let rows: Vec<SqliteRow> = sqlx::query(
            "SELECT * FROM tasks WHERE run_id = ? AND status IN ('spawned', 'executing')",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_task).collect())
    }

    /// Update task status
    pub async fn update_task_status(&self, task_id: &str, status: TaskStatus) -> Result<()> {
        let now = Utc::now().to_rfc3339();

        match status {
            TaskStatus::Spawned | TaskStatus::Executing => {
                sqlx::query("UPDATE tasks SET status = ?, started_at = ? WHERE id = ?")
                    .bind(status.to_string())
                    .bind(&now)
                    .bind(task_id)
                    .execute(&self.pool)
                    .await?;
            }
            TaskStatus::Completed => {
                sqlx::query("UPDATE tasks SET status = ?, completed_at = ? WHERE id = ?")
                    .bind(status.to_string())
                    .bind(&now)
                    .bind(task_id)
                    .execute(&self.pool)
                    .await?;
            }
            TaskStatus::Failed => {
                sqlx::query("UPDATE tasks SET status = ?, completed_at = ? WHERE id = ?")
                    .bind(status.to_string())
                    .bind(&now)
                    .bind(task_id)
                    .execute(&self.pool)
                    .await?;
            }
            _ => {
                sqlx::query("UPDATE tasks SET status = ? WHERE id = ?")
                    .bind(status.to_string())
                    .bind(task_id)
                    .execute(&self.pool)
                    .await?;
            }
        }

        Ok(())
    }

    /// Update task with session key
    pub async fn update_task_session(&self, task_id: &str, session_key: &str) -> Result<()> {
        sqlx::query("UPDATE tasks SET agent_session_id = ?, status = ? WHERE id = ?")
            .bind(session_key)
            .bind(TaskStatus::Spawned.to_string())
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Update task with error
    pub async fn update_task_error(&self, task_id: &str, error: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query("UPDATE tasks SET status = ?, error = ?, completed_at = ? WHERE id = ?")
            .bind(TaskStatus::Failed.to_string())
            .bind(error)
            .bind(&now)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Update task with result
    pub async fn update_task_result(&self, task_id: &str, result: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query("UPDATE tasks SET status = ?, result_json = ?, completed_at = ? WHERE id = ?")
            .bind(TaskStatus::Completed.to_string())
            .bind(result)
            .bind(&now)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Load task graph from database (for resuming runs)
    pub async fn load_task_graph(&self, run_id: &str) -> Result<TaskGraph> {
        let tasks = self.get_tasks(run_id).await?;
        
        // Load dependencies
        let deps: Vec<(String, String)> = sqlx::query(
            r#"
            SELECT td.task_id, td.depends_on_task_id 
            FROM task_dependencies td
            JOIN tasks t ON td.task_id = t.id
            WHERE t.run_id = ?
            "#,
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?
        .iter()
        .map(|row| (row.get("task_id"), row.get("depends_on_task_id")))
        .collect();

        // Build graph
        let mut graph = TaskGraph::new();
        
        for task in tasks {
            let task_deps: Vec<String> = deps
                .iter()
                .filter(|(tid, _)| tid == &task.id)
                .map(|(_, dep)| dep.clone())
                .collect();
            
            graph.add_task(crate::dag::TaskNode {
                id: task.id,
                description: task.description,
                depends_on: task_deps,
                outputs: task.outputs,
                estimated_complexity: None,
            });
        }

        Ok(graph)
    }

    // ========================================================================
    // Logs
    // ========================================================================

    /// Insert a log entry
    pub async fn insert_log(&self, run_id: &str, task_id: &str, level: &str, message: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO agent_logs (run_id, task_id, level, message, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(run_id)
        .bind(task_id)
        .bind(level)
        .bind(message)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get logs for a run
    pub async fn get_logs(
        &self,
        run_id: &str,
        task_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AgentLog>> {
        let rows: Vec<SqliteRow> = if let Some(tid) = task_id {
            sqlx::query(
                "SELECT * FROM agent_logs WHERE run_id = ? AND task_id = ? ORDER BY created_at DESC LIMIT ?",
            )
            .bind(run_id)
            .bind(tid)
            .bind(limit as i32)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT * FROM agent_logs WHERE run_id = ? ORDER BY created_at DESC LIMIT ?",
            )
            .bind(run_id)
            .bind(limit as i32)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows.iter().map(row_to_agent_log).collect())
    }

    /// Get recent logs (for streaming)
    pub async fn get_recent_logs(
        &self,
        run_id: &str,
        task_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AgentLog>> {
        self.get_logs(run_id, task_id, limit).await
    }

    // ========================================================================
    // Messages
    // ========================================================================

    /// Insert a message
    pub async fn insert_message(
        &self,
        run_id: &str,
        task_id: Option<&str>,
        message_type: &str,
        payload: &serde_json::Value,
    ) -> Result<i64> {
        let result = sqlx::query(
            "INSERT INTO messages (run_id, task_id, message_type, payload_json, created_at) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(run_id)
        .bind(task_id)
        .bind(message_type)
        .bind(serde_json::to_string(payload)?)
        .bind(Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        
        Ok(result.last_insert_rowid())
    }

    /// Get messages for a run
    pub async fn get_messages(
        &self,
        run_id: &str,
        limit: usize,
    ) -> Result<Vec<(i64, Option<String>, String, serde_json::Value, DateTime<Utc>)>> {
        let rows: Vec<SqliteRow> = sqlx::query(
            "SELECT * FROM messages WHERE run_id = ? ORDER BY created_at DESC LIMIT ?",
        )
        .bind(run_id)
        .bind(limit as i32)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .iter()
            .map(|row| {
                (
                    row.get("id"),
                    row.get("task_id"),
                    row.get("message_type"),
                    serde_json::from_str(row.get::<&str, _>("payload_json")).unwrap_or_default(),
                    parse_datetime(row.get::<Option<String>, _>("created_at")).unwrap_or_else(Utc::now),
                )
            })
            .collect())
    }

    // ========================================================================
    // File Locks
    // ========================================================================

    /// Acquire a file lock
    pub async fn acquire_lock(
        &self,
        path: &str,
        run_id: &str,
        task_id: &str,
        lock_type: &str,
        ttl_seconds: i64,
    ) -> Result<bool> {
        let now = Utc::now();
        let expires_at = now + chrono::Duration::seconds(ttl_seconds);

        // First, clean up expired locks
        sqlx::query("DELETE FROM file_locks WHERE expires_at < ?")
            .bind(now.to_rfc3339())
            .execute(&self.pool)
            .await?;

        // Check for existing lock
        let existing: Option<SqliteRow> = sqlx::query(
            "SELECT * FROM file_locks WHERE path = ? AND expires_at > ?",
        )
        .bind(path)
        .bind(now.to_rfc3339())
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = existing {
            let existing_task: String = row.get("task_id");
            let existing_type: String = row.get("lock_type");

            // Same task can re-acquire
            if existing_task == task_id {
                sqlx::query(
                    "UPDATE file_locks SET expires_at = ? WHERE path = ?",
                )
                .bind(expires_at.to_rfc3339())
                .bind(path)
                .execute(&self.pool)
                .await?;
                return Ok(true);
            }

            // Shared locks can coexist (simplified - in reality we'd track multiple)
            if lock_type == "shared" && existing_type == "shared" {
                return Ok(true);
            }

            // Conflict
            return Ok(false);
        }

        // No existing lock, acquire it
        sqlx::query(
            r#"
            INSERT INTO file_locks (path, run_id, task_id, lock_type, acquired_at, expires_at)
            VALUES (?, ?, ?, ?, ?, ?)
            "#,
        )
        .bind(path)
        .bind(run_id)
        .bind(task_id)
        .bind(lock_type)
        .bind(now.to_rfc3339())
        .bind(expires_at.to_rfc3339())
        .execute(&self.pool)
        .await?;

        Ok(true)
    }

    /// Release a file lock
    pub async fn release_lock(&self, path: &str, task_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM file_locks WHERE path = ? AND task_id = ?")
            .bind(path)
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Release all locks held by a task
    pub async fn release_task_locks(&self, task_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM file_locks WHERE task_id = ?")
            .bind(task_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Get locks for a run
    pub async fn get_locks(&self, run_id: &str) -> Result<Vec<FileLock>> {
        let rows: Vec<SqliteRow> = sqlx::query(
            "SELECT * FROM file_locks WHERE run_id = ? AND expires_at > ?",
        )
        .bind(run_id)
        .bind(Utc::now().to_rfc3339())
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_file_lock).collect())
    }

    /// Check if a path is locked
    pub async fn is_locked(&self, path: &str) -> Result<bool> {
        let row: Option<SqliteRow> = sqlx::query(
            "SELECT 1 FROM file_locks WHERE path = ? AND expires_at > ?",
        )
        .bind(path)
        .bind(Utc::now().to_rfc3339())
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.is_some())
    }

    // ========================================================================
    // Utility
    // ========================================================================

    /// Delete a run and all associated data (cascades)
    pub async fn delete_run(&self, run_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM runs WHERE id = ?")
            .bind(run_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Get pool statistics
    pub fn pool_size(&self) -> u32 {
        self.pool.size()
    }

    /// Close the database connection pool
    pub async fn close(&self) {
        self.pool.close().await;
    }

    /// Check database health
    pub async fn health_check(&self) -> Result<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .context("Database health check failed")?;
        Ok(())
    }

    /// Check database health with detailed stats
    pub async fn health_check_detailed(&self) -> Result<DatabaseHealth> {
        let start = std::time::Instant::now();
        
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .context("Database health check failed")?;
        
        let latency_ms = start.elapsed().as_millis() as u64;
        
        // Get table counts
        let run_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);
        
        let task_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tasks")
            .fetch_one(&self.pool)
            .await
            .unwrap_or(0);
        
        Ok(DatabaseHealth {
            healthy: true,
            latency_ms,
            pool_size: self.pool.size(),
            pool_idle: self.pool.num_idle() as u32,
            run_count: run_count as u64,
            task_count: task_count as u64,
        })
    }

    /// Count tasks by status for a run
    pub async fn count_tasks_by_status(&self, run_id: &str) -> Result<TaskCounts> {
        let rows: Vec<SqliteRow> = sqlx::query(
            "SELECT status, COUNT(*) as count FROM tasks WHERE run_id = ? GROUP BY status",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;

        let mut counts = TaskCounts::default();
        for row in rows {
            let status: String = row.get("status");
            let count: i32 = row.get("count");
            match status.as_str() {
                "pending" => counts.pending = count as usize,
                "spawned" => counts.spawned = count as usize,
                "executing" => counts.executing = count as usize,
                "completed" => counts.completed = count as usize,
                "failed" => counts.failed = count as usize,
                _ => {}
            }
        }

        Ok(counts)
    }

    /// Get pending tasks that are ready to execute (all dependencies completed)
    pub async fn get_ready_tasks(&self, run_id: &str) -> Result<Vec<Task>> {
        let rows: Vec<SqliteRow> = sqlx::query(
            r#"
            SELECT t.* FROM tasks t
            WHERE t.run_id = ? 
              AND t.status = 'pending'
              AND NOT EXISTS (
                SELECT 1 FROM task_dependencies td
                JOIN tasks dep ON td.depends_on_task_id = dep.id
                WHERE td.task_id = t.id
                  AND dep.status != 'completed'
              )
            ORDER BY t.wave, t.id
            "#,
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.iter().map(row_to_task).collect())
    }

    /// Batch update task statuses (atomic)
    pub async fn batch_update_task_status(
        &self,
        updates: &[(String, TaskStatus)],
    ) -> Result<()> {
        let mut tx = self.begin().await?;
        let now = Utc::now().to_rfc3339();

        for (task_id, status) in updates {
            match status {
                TaskStatus::Spawned | TaskStatus::Executing => {
                    sqlx::query("UPDATE tasks SET status = ?, started_at = COALESCE(started_at, ?) WHERE id = ?")
                        .bind(status.to_string())
                        .bind(&now)
                        .bind(task_id)
                        .execute(&mut *tx)
                        .await?;
                }
                TaskStatus::Completed | TaskStatus::Failed => {
                    sqlx::query("UPDATE tasks SET status = ?, completed_at = ? WHERE id = ?")
                        .bind(status.to_string())
                        .bind(&now)
                        .bind(task_id)
                        .execute(&mut *tx)
                        .await?;
                }
                _ => {
                    sqlx::query("UPDATE tasks SET status = ? WHERE id = ?")
                        .bind(status.to_string())
                        .bind(task_id)
                        .execute(&mut *tx)
                        .await?;
                }
            }
        }

        tx.commit().await.context("Failed to commit batch update")?;
        Ok(())
    }

    /// Reset all spawned/executing tasks back to pending (for run recovery)
    pub async fn reset_in_progress_tasks(&self, run_id: &str) -> Result<usize> {
        let result = sqlx::query(
            r#"
            UPDATE tasks 
            SET status = 'pending', 
                agent_session_id = NULL, 
                started_at = NULL 
            WHERE run_id = ? 
              AND status IN ('spawned', 'executing')
            "#,
        )
        .bind(run_id)
        .execute(&self.pool)
        .await?;

        Ok(result.rows_affected() as usize)
    }

    /// Clean up expired file locks
    pub async fn cleanup_expired_locks(&self) -> Result<usize> {
        let result = sqlx::query("DELETE FROM file_locks WHERE expires_at < ?")
            .bind(Utc::now().to_rfc3339())
            .execute(&self.pool)
            .await?;

        Ok(result.rows_affected() as usize)
    }

    /// Get run summary statistics
    pub async fn get_run_summary(&self, run_id: &str) -> Result<Option<RunSummary>> {
        let run = match self.get_run(run_id).await? {
            Some(r) => r,
            None => return Ok(None),
        };

        let counts = self.count_tasks_by_status(run_id).await?;
        let active_locks = self.get_locks(run_id).await?.len();

        Ok(Some(RunSummary {
            run,
            task_counts: counts,
            active_locks,
        }))
    }
}

/// Summary of a run with task statistics
#[derive(Debug, Clone)]
pub struct RunSummary {
    pub run: Run,
    pub task_counts: TaskCounts,
    pub active_locks: usize,
}

// ============================================================================
// Helper functions
// ============================================================================

fn row_to_run(row: &SqliteRow) -> Run {
    Run {
        id: row.get("id"),
        name: row.get("name"),
        description: row.get("description"),
        status: match row.get::<String, _>("status").as_str() {
            "running" => RunStatus::Running,
            "paused" => RunStatus::Paused,
            "completed" => RunStatus::Completed,
            "failed" => RunStatus::Failed,
            _ => RunStatus::Pending,
        },
        config_json: row.get("config_json"),
        started_at: parse_datetime(row.get("started_at")),
        completed_at: parse_datetime(row.get("completed_at")),
        created_at: parse_datetime(row.get::<Option<String>, _>("created_at"))
            .unwrap_or_else(Utc::now),
    }
}

fn row_to_task(row: &SqliteRow) -> Task {
    Task {
        id: row.get("id"),
        run_id: row.get("run_id"),
        description: row.get("description"),
        status: match row.get::<String, _>("status").as_str() {
            "spawned" => TaskStatus::Spawned,
            "executing" => TaskStatus::Executing,
            "completed" => TaskStatus::Completed,
            "failed" => TaskStatus::Failed,
            _ => TaskStatus::Pending,
        },
        wave: row.get::<i32, _>("wave") as usize,
        agent_session_id: row.get("agent_session_id"),
        outputs: row
            .get::<Option<String>, _>("outputs_json")
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default(),
        result_json: row.get("result_json"),
        started_at: parse_datetime(row.get("started_at")),
        completed_at: parse_datetime(row.get("completed_at")),
        error: row.get("error"),
        created_at: parse_datetime(row.get::<Option<String>, _>("created_at"))
            .unwrap_or_else(Utc::now),
    }
}

fn row_to_agent_log(row: &SqliteRow) -> AgentLog {
    AgentLog {
        id: row.get("id"),
        run_id: row.get("run_id"),
        task_id: row.get("task_id"),
        level: row.get("level"),
        message: row.get("message"),
        created_at: parse_datetime(row.get::<Option<String>, _>("created_at"))
            .unwrap_or_else(Utc::now),
    }
}

fn row_to_file_lock(row: &SqliteRow) -> FileLock {
    FileLock {
        path: row.get("path"),
        run_id: row.get("run_id"),
        task_id: row.get("task_id"),
        lock_type: row.get("lock_type"),
        acquired_at: parse_datetime(row.get::<Option<String>, _>("acquired_at"))
            .unwrap_or_else(Utc::now),
        expires_at: parse_datetime(row.get::<Option<String>, _>("expires_at"))
            .unwrap_or_else(Utc::now),
    }
}

fn parse_datetime(s: Option<String>) -> Option<DateTime<Utc>> {
    s.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    })
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dag::TaskNode;
    use std::path::PathBuf;

    async fn setup_test_db() -> Database {
        let db = Database::connect_memory().await.unwrap();
        db.migrate().await.unwrap();
        db
    }

    #[tokio::test]
    async fn test_create_and_get_run() {
        let db = setup_test_db().await;

        let run = Run::new("test-run");
        db.create_run(&run).await.unwrap();

        let fetched = db.get_run(&run.id).await.unwrap();
        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().name, "test-run");
    }

    #[tokio::test]
    async fn test_update_run_status() {
        let db = setup_test_db().await;

        let run = Run::new("test-run");
        db.create_run(&run).await.unwrap();
        
        db.update_run_status(&run.id, RunStatus::Running).await.unwrap();
        
        let fetched = db.get_run(&run.id).await.unwrap().unwrap();
        assert_eq!(fetched.status, RunStatus::Running);
        assert!(fetched.started_at.is_some());
    }

    #[tokio::test]
    async fn test_create_tasks_from_graph() {
        let db = setup_test_db().await;

        let run = Run::new("test-run");
        db.create_run(&run).await.unwrap();

        let mut graph = TaskGraph::new();
        graph.add_task(TaskNode {
            id: "task-a".to_string(),
            description: "First task".to_string(),
            depends_on: vec![],
            outputs: vec![PathBuf::from("output.txt")],
            estimated_complexity: None,
        });
        graph.add_task(TaskNode {
            id: "task-b".to_string(),
            description: "Second task".to_string(),
            depends_on: vec!["task-a".to_string()],
            outputs: vec![],
            estimated_complexity: None,
        });

        db.create_tasks_from_graph(&run.id, &graph).await.unwrap();

        let tasks = db.get_tasks(&run.id).await.unwrap();
        assert_eq!(tasks.len(), 2);
    }

    #[tokio::test]
    async fn test_task_status_updates() {
        let db = setup_test_db().await;

        let run = Run::new("test-run");
        db.create_run(&run).await.unwrap();

        let mut graph = TaskGraph::new();
        graph.add_task(TaskNode {
            id: "task-1".to_string(),
            description: "Test task".to_string(),
            depends_on: vec![],
            outputs: vec![],
            estimated_complexity: None,
        });
        db.create_tasks_from_graph(&run.id, &graph).await.unwrap();

        // Test status transitions
        db.update_task_status("task-1", TaskStatus::Spawned).await.unwrap();
        let task = db.get_task("task-1").await.unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Spawned);
        assert!(task.started_at.is_some());

        db.update_task_status("task-1", TaskStatus::Completed).await.unwrap();
        let task = db.get_task("task-1").await.unwrap().unwrap();
        assert_eq!(task.status, TaskStatus::Completed);
        assert!(task.completed_at.is_some());
    }

    #[tokio::test]
    async fn test_file_locking() {
        let db = setup_test_db().await;

        let run = Run::new("test-run");
        db.create_run(&run).await.unwrap();

        let mut graph = TaskGraph::new();
        graph.add_task(TaskNode {
            id: "task-1".to_string(),
            description: "Test task".to_string(),
            depends_on: vec![],
            outputs: vec![],
            estimated_complexity: None,
        });
        db.create_tasks_from_graph(&run.id, &graph).await.unwrap();

        // Acquire lock
        let acquired = db
            .acquire_lock("/test/file.txt", &run.id, "task-1", "exclusive", 60)
            .await
            .unwrap();
        assert!(acquired);

        // Check if locked
        assert!(db.is_locked("/test/file.txt").await.unwrap());

        // Same task can re-acquire
        let reacquired = db
            .acquire_lock("/test/file.txt", &run.id, "task-1", "exclusive", 60)
            .await
            .unwrap();
        assert!(reacquired);

        // Release lock
        db.release_lock("/test/file.txt", "task-1").await.unwrap();
        assert!(!db.is_locked("/test/file.txt").await.unwrap());
    }

    #[tokio::test]
    async fn test_task_counts() {
        let db = setup_test_db().await;

        let run = Run::new("test-run");
        db.create_run(&run).await.unwrap();

        let mut graph = TaskGraph::new();
        for i in 0..5 {
            graph.add_task(TaskNode {
                id: format!("task-{}", i),
                description: format!("Task {}", i),
                depends_on: vec![],
                outputs: vec![],
                estimated_complexity: None,
            });
        }
        db.create_tasks_from_graph(&run.id, &graph).await.unwrap();

        let counts = db.count_tasks_by_status(&run.id).await.unwrap();
        assert_eq!(counts.pending, 5);
        assert_eq!(counts.total(), 5);

        db.update_task_status("task-0", TaskStatus::Completed).await.unwrap();
        db.update_task_status("task-1", TaskStatus::Failed).await.unwrap();

        let counts = db.count_tasks_by_status(&run.id).await.unwrap();
        assert_eq!(counts.pending, 3);
        assert_eq!(counts.completed, 1);
        assert_eq!(counts.failed, 1);
    }

    #[tokio::test]
    async fn test_health_check() {
        let db = setup_test_db().await;
        
        db.health_check().await.unwrap();
        
        let health = db.health_check_detailed().await.unwrap();
        assert!(health.healthy);
        assert!(health.latency_ms < 1000); // Should be fast for in-memory
    }

    #[tokio::test]
    async fn test_batch_update_status() {
        let db = setup_test_db().await;

        let run = Run::new("test-run");
        db.create_run(&run).await.unwrap();

        let mut graph = TaskGraph::new();
        for i in 0..3 {
            graph.add_task(TaskNode {
                id: format!("task-{}", i),
                description: format!("Task {}", i),
                depends_on: vec![],
                outputs: vec![],
                estimated_complexity: None,
            });
        }
        db.create_tasks_from_graph(&run.id, &graph).await.unwrap();

        let updates = vec![
            ("task-0".to_string(), TaskStatus::Completed),
            ("task-1".to_string(), TaskStatus::Executing),
            ("task-2".to_string(), TaskStatus::Spawned),
        ];
        db.batch_update_task_status(&updates).await.unwrap();

        let counts = db.count_tasks_by_status(&run.id).await.unwrap();
        assert_eq!(counts.completed, 1);
        assert_eq!(counts.executing, 1);
        assert_eq!(counts.spawned, 1);
    }

    #[tokio::test]
    async fn test_reset_in_progress_tasks() {
        let db = setup_test_db().await;

        let run = Run::new("test-run");
        db.create_run(&run).await.unwrap();

        let mut graph = TaskGraph::new();
        for i in 0..3 {
            graph.add_task(TaskNode {
                id: format!("task-{}", i),
                description: format!("Task {}", i),
                depends_on: vec![],
                outputs: vec![],
                estimated_complexity: None,
            });
        }
        db.create_tasks_from_graph(&run.id, &graph).await.unwrap();

        // Put some tasks in progress
        db.update_task_status("task-0", TaskStatus::Spawned).await.unwrap();
        db.update_task_status("task-1", TaskStatus::Executing).await.unwrap();

        let reset_count = db.reset_in_progress_tasks(&run.id).await.unwrap();
        assert_eq!(reset_count, 2);

        let counts = db.count_tasks_by_status(&run.id).await.unwrap();
        assert_eq!(counts.pending, 3);
        assert_eq!(counts.in_progress(), 0);
    }
}
