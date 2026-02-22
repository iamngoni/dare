//! Database operations for dare

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
        // Run migrations manually since we're not using sqlx-cli
        sqlx::query(include_str!("../migrations/001_initial.sql"))
            .execute(&self.pool)
            .await?;
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

    // ===== Tasks =====

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
}

// Helper functions to convert rows to structs

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
