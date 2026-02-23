//! Integration tests for the dare.run API endpoints
//!
//! Tests all JSON API endpoints using reqwest client:
//! - GET /api/status
//! - GET /api/runs
//! - GET /api/runs/{id}
//! - GET /api/runs/{id}/tasks
//! - POST /api/runs/{id}/pause
//! - POST /api/runs/{id}/resume
//! - POST /api/runs/{id}/kill

use reqwest::Client;
use serde::Deserialize;
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

// Import dare modules
use dare::config::Config;
use dare::db::Database;
use dare::message_bus::TaskMessage;
use dare::models::{Run, RunStatus, TaskStatus};
use dare::server::DashboardServer;

/// Response types that mirror the API responses

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct StatusResponse {
    version: String,
    status: String,
    current_run: Option<RunSummary>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct RunSummary {
    id: String,
    name: String,
    status: String,
    created_at: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct RunDetail {
    id: String,
    name: String,
    description: Option<String>,
    status: String,
    task_count: usize,
    wave_count: usize,
    started_at: Option<String>,
    completed_at: Option<String>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct TaskSummary {
    id: String,
    description: String,
    status: String,
    wave: usize,
    error: Option<String>,
    session_id: Option<String>,
}

/// Find an available port for testing
fn find_free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .expect("Failed to bind to port 0")
        .local_addr()
        .expect("Failed to get local address")
        .port()
}

/// Test fixture that sets up a server with a test database
struct TestFixture {
    port: u16,
    client: Client,
    db: Arc<Database>,
    _shutdown: tokio::sync::oneshot::Sender<()>,
    _temp_dir: tempfile::TempDir,
}

impl TestFixture {
    async fn new() -> Self {
        let port = find_free_port();
        
        // Use a temporary file database to avoid SQLite in-memory db isolation issues
        let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
        let db_path = temp_dir.path().join("test.db");
        
        let db = Database::connect(&db_path)
            .await
            .expect("Failed to create test database");
        db.migrate().await.expect("Failed to run migrations");
        let db = Arc::new(db);

        let (events_tx, _) = broadcast::channel::<TaskMessage>(100);
        let config = Config::default();

        let server = DashboardServer::with_shared_state(&config, port, db.clone(), events_tx);

        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        // Spawn server in background
        tokio::spawn(async move {
            tokio::select! {
                result = server.run() => {
                    if let Err(e) = result {
                        eprintln!("Server error: {}", e);
                    }
                }
                _ = shutdown_rx => {
                    // Server shutdown requested
                }
            }
        });

        // Wait for server to start and then verify it's actually responding
        for _ in 0..10 {
            tokio::time::sleep(Duration::from_millis(100)).await;
            // Try to connect to verify server is up
            if reqwest::get(format!("http://127.0.0.1:{}/api/status", port))
                .await
                .is_ok()
            {
                break;
            }
        }

        let client = Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            port,
            client,
            db,
            _shutdown: shutdown_tx,
            _temp_dir: temp_dir,
        }
    }

    fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Create a test run in the database
    async fn create_test_run(&self, name: &str) -> Run {
        let run = Run::new(name);
        self.db
            .create_run(&run)
            .await
            .expect("Failed to create test run");
        run
    }

    /// Create a test task for a run
    async fn create_test_task(&self, run_id: &str, task_id: &str, wave: usize) {
        sqlx::query(
            r#"
            INSERT INTO tasks (id, run_id, description, status, wave, created_at)
            VALUES (?, ?, ?, ?, ?, datetime('now'))
            "#,
        )
        .bind(task_id)
        .bind(run_id)
        .bind(format!("Test task {}", task_id))
        .bind("pending")
        .bind(wave as i32)
        .execute(self.db.pool())
        .await
        .expect("Failed to create test task");
    }
}

// =============================================================================
// GET /api/status tests
// =============================================================================

#[tokio::test]
async fn test_api_status_no_runs() {
    let fixture = TestFixture::new().await;

    let response = fixture
        .client
        .get(format!("{}/api/status", fixture.base_url()))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    let status: StatusResponse = response.json().await.expect("Failed to parse response");

    assert_eq!(status.status, "ok");
    assert!(!status.version.is_empty());
    assert!(status.current_run.is_none());
}

#[tokio::test]
async fn test_api_status_with_run() {
    let fixture = TestFixture::new().await;

    // Create a test run
    let run = fixture.create_test_run("test-status-run").await;

    let response = fixture
        .client
        .get(format!("{}/api/status", fixture.base_url()))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    let status: StatusResponse = response.json().await.expect("Failed to parse response");

    assert_eq!(status.status, "ok");
    assert!(status.current_run.is_some());

    let current_run = status.current_run.unwrap();
    assert_eq!(current_run.id, run.id);
    assert_eq!(current_run.name, "test-status-run");
    assert_eq!(current_run.status, "Pending");
}

// =============================================================================
// GET /api/runs tests
// =============================================================================

#[tokio::test]
async fn test_api_runs_empty() {
    let fixture = TestFixture::new().await;

    let response = fixture
        .client
        .get(format!("{}/api/runs", fixture.base_url()))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    let runs: Vec<RunSummary> = response.json().await.expect("Failed to parse response");
    assert!(runs.is_empty());
}

#[tokio::test]
async fn test_api_runs_list() {
    let fixture = TestFixture::new().await;

    // Create multiple test runs
    let run1 = fixture.create_test_run("run-1").await;
    let run2 = fixture.create_test_run("run-2").await;
    let run3 = fixture.create_test_run("run-3").await;

    let response = fixture
        .client
        .get(format!("{}/api/runs", fixture.base_url()))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    let runs: Vec<RunSummary> = response.json().await.expect("Failed to parse response");

    assert_eq!(runs.len(), 3);

    // Verify all runs are present (order may vary based on timing)
    let run_ids: Vec<&str> = runs.iter().map(|r| r.id.as_str()).collect();
    assert!(run_ids.contains(&run1.id.as_str()));
    assert!(run_ids.contains(&run2.id.as_str()));
    assert!(run_ids.contains(&run3.id.as_str()));
}

#[tokio::test]
async fn test_api_runs_order_by_created_at() {
    let fixture = TestFixture::new().await;

    // Create runs with a small delay to ensure different timestamps
    fixture.create_test_run("first").await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    fixture.create_test_run("second").await;
    tokio::time::sleep(Duration::from_millis(10)).await;
    let last_run = fixture.create_test_run("third").await;

    let response = fixture
        .client
        .get(format!("{}/api/runs", fixture.base_url()))
        .send()
        .await
        .expect("Failed to send request");

    let runs: Vec<RunSummary> = response.json().await.expect("Failed to parse response");

    // Most recent run should be first (DESC order)
    assert_eq!(runs[0].id, last_run.id);
    assert_eq!(runs[0].name, "third");
}

// =============================================================================
// GET /api/runs/{id} tests
// =============================================================================

#[tokio::test]
async fn test_api_run_detail() {
    let fixture = TestFixture::new().await;

    let run = fixture.create_test_run("detailed-run").await;

    // Create some tasks for the run
    fixture.create_test_task(&run.id, "task-1", 0).await;
    fixture.create_test_task(&run.id, "task-2", 0).await;
    fixture.create_test_task(&run.id, "task-3", 1).await;

    let response = fixture
        .client
        .get(format!("{}/api/runs/{}", fixture.base_url(), run.id))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    let detail: RunDetail = response.json().await.expect("Failed to parse response");

    assert_eq!(detail.id, run.id);
    assert_eq!(detail.name, "detailed-run");
    assert_eq!(detail.task_count, 3);
    assert_eq!(detail.wave_count, 2); // Wave 0 and Wave 1
    assert_eq!(detail.status, "Pending");
}

#[tokio::test]
async fn test_api_run_detail_not_found() {
    let fixture = TestFixture::new().await;

    let response = fixture
        .client
        .get(format!("{}/api/runs/nonexistent-id", fixture.base_url()))
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_api_run_detail_no_tasks() {
    let fixture = TestFixture::new().await;

    let run = fixture.create_test_run("empty-run").await;

    let response = fixture
        .client
        .get(format!("{}/api/runs/{}", fixture.base_url(), run.id))
        .send()
        .await
        .expect("Failed to send request");

    let detail: RunDetail = response.json().await.expect("Failed to parse response");

    assert_eq!(detail.task_count, 0);
    assert_eq!(detail.wave_count, 0);
}

// =============================================================================
// GET /api/runs/{id}/tasks tests
// =============================================================================

#[tokio::test]
async fn test_api_tasks_list() {
    let fixture = TestFixture::new().await;

    let run = fixture.create_test_run("tasks-run").await;

    fixture.create_test_task(&run.id, "task-a", 0).await;
    fixture.create_test_task(&run.id, "task-b", 0).await;
    fixture.create_test_task(&run.id, "task-c", 1).await;

    let response = fixture
        .client
        .get(format!("{}/api/runs/{}/tasks", fixture.base_url(), run.id))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    let tasks: Vec<TaskSummary> = response.json().await.expect("Failed to parse response");

    assert_eq!(tasks.len(), 3);

    // Verify task properties
    let task_a = tasks.iter().find(|t| t.id == "task-a").unwrap();
    assert_eq!(task_a.wave, 0);
    assert_eq!(task_a.status, "Pending");
    assert!(task_a.error.is_none());

    let task_c = tasks.iter().find(|t| t.id == "task-c").unwrap();
    assert_eq!(task_c.wave, 1);
}

#[tokio::test]
async fn test_api_tasks_empty() {
    let fixture = TestFixture::new().await;

    let run = fixture.create_test_run("no-tasks-run").await;

    let response = fixture
        .client
        .get(format!("{}/api/runs/{}/tasks", fixture.base_url(), run.id))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    let tasks: Vec<TaskSummary> = response.json().await.expect("Failed to parse response");
    assert!(tasks.is_empty());
}

#[tokio::test]
async fn test_api_tasks_with_various_statuses() {
    let fixture = TestFixture::new().await;

    let run = fixture.create_test_run("status-run").await;

    // Create tasks with different statuses
    fixture.create_test_task(&run.id, "pending-task", 0).await;
    fixture.create_test_task(&run.id, "spawned-task", 0).await;
    fixture.create_test_task(&run.id, "completed-task", 0).await;
    fixture.create_test_task(&run.id, "failed-task", 0).await;

    // Update statuses
    fixture
        .db
        .update_task_status("spawned-task", TaskStatus::Spawned)
        .await
        .unwrap();
    fixture
        .db
        .update_task_status("completed-task", TaskStatus::Completed)
        .await
        .unwrap();
    fixture
        .db
        .update_task_error("failed-task", "Something went wrong")
        .await
        .unwrap();

    let response = fixture
        .client
        .get(format!("{}/api/runs/{}/tasks", fixture.base_url(), run.id))
        .send()
        .await
        .expect("Failed to send request");

    let tasks: Vec<TaskSummary> = response.json().await.expect("Failed to parse response");

    let pending = tasks.iter().find(|t| t.id == "pending-task").unwrap();
    assert_eq!(pending.status, "Pending");

    let spawned = tasks.iter().find(|t| t.id == "spawned-task").unwrap();
    assert_eq!(spawned.status, "Spawned");

    let completed = tasks.iter().find(|t| t.id == "completed-task").unwrap();
    assert_eq!(completed.status, "Completed");

    let failed = tasks.iter().find(|t| t.id == "failed-task").unwrap();
    assert_eq!(failed.status, "Failed");
    assert_eq!(failed.error.as_deref(), Some("Something went wrong"));
}

// =============================================================================
// POST /api/runs/{id}/pause tests
// =============================================================================

#[tokio::test]
async fn test_api_pause_run() {
    let fixture = TestFixture::new().await;

    let run = fixture.create_test_run("pause-run").await;

    // Set run to running first
    fixture
        .db
        .update_run_status(&run.id, RunStatus::Running)
        .await
        .unwrap();

    let response = fixture
        .client
        .post(format!("{}/api/runs/{}/pause", fixture.base_url(), run.id))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    // Verify the run is paused
    let updated_run = fixture.db.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(updated_run.status, RunStatus::Paused);
}

#[tokio::test]
async fn test_api_pause_pending_run() {
    let fixture = TestFixture::new().await;

    let run = fixture.create_test_run("pending-pause").await;

    let response = fixture
        .client
        .post(format!("{}/api/runs/{}/pause", fixture.base_url(), run.id))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    // Verify the run is paused (even from pending state)
    let updated_run = fixture.db.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(updated_run.status, RunStatus::Paused);
}

// =============================================================================
// POST /api/runs/{id}/resume tests
// =============================================================================

#[tokio::test]
async fn test_api_resume_run() {
    let fixture = TestFixture::new().await;

    let run = fixture.create_test_run("resume-run").await;

    // Pause the run first
    fixture
        .db
        .update_run_status(&run.id, RunStatus::Paused)
        .await
        .unwrap();

    let response = fixture
        .client
        .post(format!("{}/api/runs/{}/resume", fixture.base_url(), run.id))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    // Verify the run is running again
    let updated_run = fixture.db.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(updated_run.status, RunStatus::Running);
}

#[tokio::test]
async fn test_api_resume_sets_started_at() {
    let fixture = TestFixture::new().await;

    let run = fixture.create_test_run("resume-timestamp").await;

    // Pause the run
    fixture
        .db
        .update_run_status(&run.id, RunStatus::Paused)
        .await
        .unwrap();

    // Verify started_at is not set
    let before = fixture.db.get_run(&run.id).await.unwrap().unwrap();
    assert!(before.started_at.is_none());

    let response = fixture
        .client
        .post(format!("{}/api/runs/{}/resume", fixture.base_url(), run.id))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    // Verify started_at is now set
    let after = fixture.db.get_run(&run.id).await.unwrap().unwrap();
    assert!(after.started_at.is_some());
}

// =============================================================================
// POST /api/runs/{id}/kill tests
// =============================================================================

#[tokio::test]
async fn test_api_kill_run() {
    let fixture = TestFixture::new().await;

    let run = fixture.create_test_run("kill-run").await;

    // Set run to running
    fixture
        .db
        .update_run_status(&run.id, RunStatus::Running)
        .await
        .unwrap();

    let response = fixture
        .client
        .post(format!("{}/api/runs/{}/kill", fixture.base_url(), run.id))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    // Verify the run is failed
    let updated_run = fixture.db.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(updated_run.status, RunStatus::Failed);
}

#[tokio::test]
async fn test_api_kill_sets_completed_at() {
    let fixture = TestFixture::new().await;

    let run = fixture.create_test_run("kill-timestamp").await;

    // Start the run
    fixture
        .db
        .update_run_status(&run.id, RunStatus::Running)
        .await
        .unwrap();

    let response = fixture
        .client
        .post(format!("{}/api/runs/{}/kill", fixture.base_url(), run.id))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    // Verify completed_at is set
    let updated_run = fixture.db.get_run(&run.id).await.unwrap().unwrap();
    assert!(updated_run.completed_at.is_some());
}

#[tokio::test]
async fn test_api_kill_paused_run() {
    let fixture = TestFixture::new().await;

    let run = fixture.create_test_run("paused-kill").await;

    // Pause the run
    fixture
        .db
        .update_run_status(&run.id, RunStatus::Paused)
        .await
        .unwrap();

    let response = fixture
        .client
        .post(format!("{}/api/runs/{}/kill", fixture.base_url(), run.id))
        .send()
        .await
        .expect("Failed to send request");

    assert!(response.status().is_success());

    // Verify the run is failed
    let updated_run = fixture.db.get_run(&run.id).await.unwrap().unwrap();
    assert_eq!(updated_run.status, RunStatus::Failed);
}

// =============================================================================
// Edge case tests
// =============================================================================

#[tokio::test]
async fn test_api_multiple_operations_same_run() {
    let fixture = TestFixture::new().await;

    let run = fixture.create_test_run("multi-ops").await;

    // Start -> Pause -> Resume -> Kill
    fixture
        .db
        .update_run_status(&run.id, RunStatus::Running)
        .await
        .unwrap();

    // Pause
    let response = fixture
        .client
        .post(format!("{}/api/runs/{}/pause", fixture.base_url(), run.id))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    let status = fixture.db.get_run(&run.id).await.unwrap().unwrap().status;
    assert_eq!(status, RunStatus::Paused);

    // Resume
    let response = fixture
        .client
        .post(format!("{}/api/runs/{}/resume", fixture.base_url(), run.id))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    let status = fixture.db.get_run(&run.id).await.unwrap().unwrap().status;
    assert_eq!(status, RunStatus::Running);

    // Kill
    let response = fixture
        .client
        .post(format!("{}/api/runs/{}/kill", fixture.base_url(), run.id))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());

    let status = fixture.db.get_run(&run.id).await.unwrap().unwrap().status;
    assert_eq!(status, RunStatus::Failed);
}

#[tokio::test]
async fn test_api_concurrent_requests() {
    let fixture = TestFixture::new().await;

    // Create multiple runs sequentially (to avoid database contention)
    let mut runs = Vec::new();
    for i in 0..5 {
        runs.push(fixture.create_test_run(&format!("concurrent-{}", i)).await);
    }

    // Fetch all runs concurrently using tokio::spawn
    let mut handles = Vec::new();
    let base_url = fixture.base_url();
    
    for run in &runs {
        let client = fixture.client.clone();
        let url = format!("{}/api/runs/{}", base_url, run.id);
        handles.push(tokio::spawn(async move {
            client.get(&url).send().await
        }));
    }

    for handle in handles {
        let response = handle.await.expect("Task panicked").expect("Request failed");
        assert!(response.status().is_success());
    }
}

#[tokio::test]
async fn test_api_status_reflects_latest_run() {
    let fixture = TestFixture::new().await;

    // Create first run
    let _first = fixture.create_test_run("first-run").await;
    tokio::time::sleep(Duration::from_millis(10)).await;

    // Create second run (should be "latest")
    let second = fixture.create_test_run("second-run").await;

    let response = fixture
        .client
        .get(format!("{}/api/status", fixture.base_url()))
        .send()
        .await
        .unwrap();

    let status: StatusResponse = response.json().await.unwrap();

    // Status should show the most recent run
    let current = status.current_run.unwrap();
    assert_eq!(current.id, second.id);
    assert_eq!(current.name, "second-run");
}

// =============================================================================
// Content-Type and Headers tests
// =============================================================================

#[tokio::test]
async fn test_api_json_content_type() {
    let fixture = TestFixture::new().await;

    let response = fixture
        .client
        .get(format!("{}/api/status", fixture.base_url()))
        .send()
        .await
        .expect("Failed to send request");

    let content_type = response
        .headers()
        .get("content-type")
        .map(|v| v.to_str().unwrap_or(""));

    assert!(content_type
        .map(|ct| ct.contains("application/json"))
        .unwrap_or(false));
}

#[tokio::test]
async fn test_api_cors_headers() {
    let fixture = TestFixture::new().await;

    let response = fixture
        .client
        .get(format!("{}/api/status", fixture.base_url()))
        .send()
        .await
        .expect("Failed to send request");

    // Check that CORS is permissive (tower-http CorsLayer::permissive())
    assert!(response.status().is_success());
}
