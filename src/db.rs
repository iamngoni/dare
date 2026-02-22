//! Database operations for dare
//!
//! SQLite-backed persistence for runs, tasks, and execution state.

use anyhow::Result;
use chrono::{DateTime, Utc};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::Row;
use std::path::Path;

use crate::dag::TaskGraph;
use crate::models::{AgentLog, Run, RunStatus, Task, TaskStatus};

/// Database connection wrapper
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Connect to the database (creates if doesn't exist)
    pub async fn connect(path: &Path) -> Result<Self> {
        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let url = format!("sqlite:{}?mode=rwc", path.display());
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect(&url)
            .await?;

        Ok(Self { pool })
    }

    /// Run database migrations
    pub async fn migrate(&self) -> Result<()> {
        sqlx::query(include_str!("../migrations/001_initial.sql"))
            .execute(&self.pool)
            .await?;
        Ok(())
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

fn parse_datetime(s: Option<String>) -> Option<DateTime<Utc>> {
    s.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|dt| dt.with_timezone(&Utc))
    })
}
