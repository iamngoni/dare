//! Web dashboard server
//!
//! HTMX + SSE real-time dashboard for monitoring dare.run executions.

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::{
        sse::{Event, Sse},
        Html, IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;

use crate::config::Config;
use crate::db::Database;
use crate::message_bus::TaskMessage;
use crate::models::RunStatus;

/// Dashboard server
pub struct DashboardServer {
    port: u16,
    config: Config,
}

impl DashboardServer {
    pub fn new(config: &Config, port: u16) -> Self {
        Self {
            port,
            config: config.clone(),
        }
    }

    pub async fn run(self) -> anyhow::Result<()> {
        // Connect to database
        let db = Arc::new(
            Database::connect(&self.config.database_path())
                .await
                .map_err(|e| anyhow::anyhow!("Failed to connect to database: {}", e))?,
        );
        db.migrate().await?;

        // Create broadcast channel for events
        let (tx, _) = broadcast::channel::<TaskMessage>(100);
        let state = Arc::new(AppState {
            db,
            events_tx: tx,
        });

        // Build router
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/api/status", get(status_handler))
            .route("/api/runs", get(runs_handler))
            .route("/api/runs/{id}", get(run_handler))
            .route("/api/runs/{id}/tasks", get(tasks_handler))
            .route("/api/runs/{id}/pause", post(pause_handler))
            .route("/api/runs/{id}/resume", post(resume_handler))
            .route("/api/runs/{id}/kill", post(kill_handler))
            .route("/events", get(sse_handler))
            .layer(CorsLayer::permissive())
            .with_state(state);

        // Start server
        let addr = format!("0.0.0.0:{}", self.port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!("Dashboard listening on http://localhost:{}", self.port);

        axum::serve(listener, app).await?;

        Ok(())
    }
}

/// Shared application state
struct AppState {
    db: Arc<Database>,
    events_tx: broadcast::Sender<TaskMessage>,
}

// ===== Handlers =====

async fn index_handler() -> Html<String> {
    Html(INDEX_HTML.to_string())
}

async fn status_handler(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let run = state.db.get_latest_run().await.ok().flatten();
    
    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        status: "ok".to_string(),
        current_run: run.map(|r| RunSummary {
            id: r.id,
            name: r.name,
            status: format!("{:?}", r.status),
            created_at: r.created_at.to_rfc3339(),
        }),
    })
}

async fn runs_handler(State(state): State<Arc<AppState>>) -> Json<Vec<RunSummary>> {
    let runs = state.db.list_runs(20).await.unwrap_or_default();
    
    Json(
        runs.into_iter()
            .map(|r| RunSummary {
                id: r.id,
                name: r.name,
                status: format!("{:?}", r.status),
                created_at: r.created_at.to_rfc3339(),
            })
            .collect(),
    )
}

async fn run_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<RunDetail>, StatusCode> {
    let run = state
        .db
        .get_run(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let tasks = state
        .db
        .get_tasks(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let waves = tasks.iter().map(|t| t.wave).max().map(|m| m + 1).unwrap_or(0);

    Ok(Json(RunDetail {
        id: run.id,
        name: run.name,
        description: run.description,
        status: format!("{:?}", run.status),
        task_count: tasks.len(),
        wave_count: waves,
        started_at: run.started_at.map(|t| t.to_rfc3339()),
        completed_at: run.completed_at.map(|t| t.to_rfc3339()),
    }))
}

async fn tasks_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<TaskSummary>>, StatusCode> {
    let tasks = state
        .db
        .get_tasks(&id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(
        tasks
            .into_iter()
            .map(|t| TaskSummary {
                id: t.id,
                description: t.description,
                status: format!("{:?}", t.status),
                wave: t.wave,
                error: t.error,
            })
            .collect(),
    ))
}

async fn pause_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    state
        .db
        .update_run_status(&id, RunStatus::Paused)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

async fn resume_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    state
        .db
        .update_run_status(&id, RunStatus::Running)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

async fn kill_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    state
        .db
        .update_run_status(&id, RunStatus::Failed)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    // TODO: Actually kill running sessions via gateway
    Ok(StatusCode::OK)
}

async fn sse_handler(
    State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(task_msg) => {
            let json = serde_json::to_string(&task_msg).ok()?;
            Some(Ok(Event::default().data(json)))
        }
        Err(_) => None,
    });

    Sse::new(stream)
}

// ===== Response types =====

#[derive(Serialize)]
struct StatusResponse {
    version: String,
    status: String,
    current_run: Option<RunSummary>,
}

#[derive(Serialize)]
struct RunSummary {
    id: String,
    name: String,
    status: String,
    created_at: String,
}

#[derive(Serialize)]
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

#[derive(Serialize)]
struct TaskSummary {
    id: String,
    description: String,
    status: String,
    wave: usize,
    error: Option<String>,
}

// ===== HTML Template =====

const INDEX_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>dare.run — Dashboard</title>
    <script src="https://unpkg.com/htmx.org@1.9.10"></script>
    <script src="https://cdn.tailwindcss.com"></script>
    <script src="https://unpkg.com/htmx.org/dist/ext/sse.js"></script>
    <style>
        @keyframes pulse {
            0%, 100% { opacity: 1; }
            50% { opacity: 0.5; }
        }
        .animate-pulse-slow { animation: pulse 2s infinite; }
    </style>
</head>
<body class="bg-gray-900 text-gray-100 min-h-screen">
    <header class="bg-gray-800 border-b border-gray-700 px-6 py-4">
        <div class="flex items-center justify-between max-w-7xl mx-auto">
            <h1 class="text-2xl font-bold">
                <span class="text-blue-400">dare</span><span class="text-gray-500">.run</span>
            </h1>
            <div id="controls" class="flex gap-3">
                <!-- Controls will be populated based on current run -->
            </div>
        </div>
    </header>

    <main class="max-w-7xl mx-auto px-6 py-8" hx-ext="sse" sse-connect="/events">
        <!-- Status Section -->
        <section class="mb-8" id="status-section" hx-get="/api/status" hx-trigger="load, every 5s" hx-swap="innerHTML">
            <div class="bg-gray-800 rounded-lg p-6 animate-pulse">
                <p class="text-gray-400">Loading...</p>
            </div>
        </section>

        <!-- Runs List -->
        <section class="mb-8">
            <h2 class="text-xl font-semibold mb-4">Recent Runs</h2>
            <div id="runs-list" class="space-y-2" hx-get="/api/runs" hx-trigger="load, every 10s">
                <p class="text-gray-400">Loading runs...</p>
            </div>
        </section>

        <!-- Current Run Details -->
        <section class="mb-8" id="current-run">
            <!-- Populated when a run is selected -->
        </section>

        <!-- Live Events -->
        <section>
            <h2 class="text-xl font-semibold mb-4">Live Events</h2>
            <div id="events-log" class="bg-gray-800 rounded-lg p-4 h-64 overflow-y-auto font-mono text-sm"
                 sse-swap="message" hx-swap="afterbegin">
                <p class="text-gray-500 italic">Waiting for events...</p>
            </div>
        </section>
    </main>

    <footer class="border-t border-gray-700 px-6 py-4 text-center text-gray-500">
        dare.run v0.1.0 — Multi-agent orchestration for OpenClaw
    </footer>

    <script>
        // Custom HTMX response handlers
        htmx.on('htmx:afterSwap', function(evt) {
            if (evt.detail.target.id === 'status-section') {
                // Status loaded, update controls if needed
            }
        });

        // SSE message handler
        document.body.addEventListener('sse:message', function(e) {
            try {
                const msg = JSON.parse(e.detail.data);
                const el = document.getElementById('events-log');
                const time = new Date().toLocaleTimeString();
                
                // Remove "waiting" message if present
                const waiting = el.querySelector('.italic');
                if (waiting) waiting.remove();
                
                let html = '';
                if (msg.Spawned) {
                    html = `<div class="text-blue-400">${time} [${msg.Spawned.task_id}] ⏳ Spawned (${msg.Spawned.session_key})</div>`;
                } else if (msg.Running) {
                    html = `<div class="text-yellow-400">${time} [${msg.Running.task_id}] 🔄 Running</div>`;
                } else if (msg.Completed) {
                    html = `<div class="text-green-400">${time} [${msg.Completed.task_id}] ✓ Completed: ${msg.Completed.summary}</div>`;
                } else if (msg.Failed) {
                    html = `<div class="text-red-400">${time} [${msg.Failed.task_id}] ✗ Failed: ${msg.Failed.error}</div>`;
                } else {
                    html = `<div class="text-gray-400">${time} ${JSON.stringify(msg)}</div>`;
                }
                
                el.insertAdjacentHTML('afterbegin', html);
                
                // Keep only last 100 events
                while (el.children.length > 100) {
                    el.removeChild(el.lastChild);
                }
            } catch (err) {
                console.error('SSE parse error:', err);
            }
        });
    </script>

    <!-- Templates for HTMX responses -->
    <template id="status-template">
        <div class="bg-gray-800 rounded-lg p-6">
            <div class="flex items-center justify-between">
                <div>
                    <h2 class="text-lg font-semibold" id="run-name"></h2>
                    <p class="text-gray-400" id="run-status"></p>
                </div>
                <div class="text-right">
                    <p class="text-2xl font-bold" id="task-count"></p>
                    <p class="text-gray-400">tasks</p>
                </div>
            </div>
        </div>
    </template>
</body>
</html>
"##;
