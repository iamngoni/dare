//! Database operations for dare

use anyhow::Result;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use std::path::Path;

use crate::dag::TaskGraph;
use crate::models::{AgentLog, Run, RunStatus, Task, TaskStatus};

/// Database connection wrapper
pub struct Database {
    pool: SqlitePool,
}

impl Database {
    /// Connect to the database
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
        sqlx::migrate!("./migrations").run(&self.pool).await?;
        Ok(())
    }

    // ===== Runs =====

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
        .bind(run.created_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Get a run by ID
    pub async fn get_run(&self, id: &str) -> Result<Option<Run>> {
        let row = sqlx::query_as::<_, RunRow>(
            "SELECT * FROM runs WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Into::into))
    }

    /// Get the most recent run
    pub async fn get_latest_run(&self) -> Result<Option<Run>> {
        let row = sqlx::query_as::<_, RunRow>(
            "SELECT * FROM runs ORDER BY created_at DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Into::into))
    }

    /// Update run status
    pub async fn update_run_status(&self, id: &str, status: RunStatus) -> Result<()> {
        let now = chrono::Utc::now();

        match status {
            RunStatus::Running => {
                sqlx::query("UPDATE runs SET status = ?, started_at = ? WHERE id = ?")
                    .bind(status.to_string())
                    .bind(now)
                    .bind(id)
                    .execute(&self.pool)
                    .await?;
            }
            RunStatus::Completed | RunStatus::Failed => {
                sqlx::query("UPDATE runs SET status = ?, completed_at = ? WHERE id = ?")
                    .bind(status.to_string())
                    .bind(now)
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

    // ===== Tasks =====

    /// Get all tasks for a run
    pub async fn get_tasks(&self, run_id: &str) -> Result<Vec<Task>> {
        let rows = sqlx::query_as::<_, TaskRow>(
            "SELECT * FROM tasks WHERE run_id = ? ORDER BY wave, id",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Get a specific task
    pub async fn get_task(&self, task_id: &str) -> Result<Option<Task>> {
        let row = sqlx::query_as::<_, TaskRow>(
            "SELECT * FROM tasks WHERE id = ?",
        )
        .bind(task_id)
        .fetch_optional(&self.pool)
        .await?;

        Ok(row.map(Into::into))
    }

    /// Get running tasks for a run
    pub async fn get_running_tasks(&self, run_id: &str) -> Result<Vec<Task>> {
        let rows = sqlx::query_as::<_, TaskRow>(
            "SELECT * FROM tasks WHERE run_id = ? AND status IN ('spawned', 'executing')",
        )
        .bind(run_id)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Load task graph from database
    pub async fn load_task_graph(&self, _run_id: &str) -> Result<TaskGraph> {
        // TODO: Reconstruct TaskGraph from tasks and dependencies
        Ok(TaskGraph::new())
    }

    // ===== Logs =====

    /// Get logs for a run
    pub async fn get_logs(
        &self,
        run_id: &str,
        task_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AgentLog>> {
        let rows = if let Some(tid) = task_id {
            sqlx::query_as::<_, AgentLogRow>(
                "SELECT * FROM agent_logs WHERE run_id = ? AND task_id = ? ORDER BY created_at DESC LIMIT ?",
            )
            .bind(run_id)
            .bind(tid)
            .bind(limit as i32)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query_as::<_, AgentLogRow>(
                "SELECT * FROM agent_logs WHERE run_id = ? ORDER BY created_at DESC LIMIT ?",
            )
            .bind(run_id)
            .bind(limit as i32)
            .fetch_all(&self.pool)
            .await?
        };

        Ok(rows.into_iter().map(Into::into).collect())
    }

    /// Get recent logs (for streaming)
    pub async fn get_recent_logs(
        &self,
        run_id: &str,
        task_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AgentLog>> {
        // Same as get_logs but could filter by timestamp for incremental fetching
        self.get_logs(run_id, task_id, limit).await
    }
}

// Row types for sqlx

#[derive(sqlx::FromRow)]
struct RunRow {
    id: String,
    name: String,
    description: Option<String>,
    status: String,
    config_json: Option<String>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl From<RunRow> for Run {
    fn from(row: RunRow) -> Self {
        Run {
            id: row.id,
            name: row.name,
            description: row.description,
            status: match row.status.as_str() {
                "running" => RunStatus::Running,
                "paused" => RunStatus::Paused,
                "completed" => RunStatus::Completed,
                "failed" => RunStatus::Failed,
                _ => RunStatus::Pending,
            },
            config_json: row.config_json,
            started_at: row.started_at,
            completed_at: row.completed_at,
            created_at: row.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct TaskRow {
    id: String,
    run_id: String,
    description: String,
    status: String,
    wave: i32,
    agent_session_id: Option<String>,
    outputs_json: Option<String>,
    result_json: Option<String>,
    started_at: Option<chrono::DateTime<chrono::Utc>>,
    completed_at: Option<chrono::DateTime<chrono::Utc>>,
    error: Option<String>,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl From<TaskRow> for Task {
    fn from(row: TaskRow) -> Self {
        Task {
            id: row.id,
            run_id: row.run_id,
            description: row.description,
            status: match row.status.as_str() {
                "spawned" => TaskStatus::Spawned,
                "executing" => TaskStatus::Executing,
                "completed" => TaskStatus::Completed,
                "failed" => TaskStatus::Failed,
                _ => TaskStatus::Pending,
            },
            wave: row.wave as usize,
            agent_session_id: row.agent_session_id,
            outputs: row
                .outputs_json
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default(),
            result_json: row.result_json,
            started_at: row.started_at,
            completed_at: row.completed_at,
            error: row.error,
            created_at: row.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct AgentLogRow {
    id: i64,
    run_id: String,
    task_id: String,
    level: String,
    message: String,
    created_at: chrono::DateTime<chrono::Utc>,
}

impl From<AgentLogRow> for AgentLog {
    fn from(row: AgentLogRow) -> Self {
        AgentLog {
            id: row.id,
            run_id: row.run_id,
            task_id: row.task_id,
            level: row.level,
            message: row.message,
            created_at: row.created_at,
        }
    }
}
