//! Web dashboard server

use axum::{
    extract::State,
    http::StatusCode,
    response::{
        sse::{Event, Sse},
        Html, IntoResponse,
    },
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;

use crate::config::Config;
use crate::message_bus::BusMessage;

/// Dashboard server
pub struct DashboardServer {
    port: u16,
    #[allow(dead_code)]
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
        // Shared state
        let (tx, _) = broadcast::channel::<BusMessage>(100);
        let state = Arc::new(AppState { events_tx: tx });

        // Build router
        let app = Router::new()
            .route("/", get(index_handler))
            .route("/api/status", get(status_handler))
            .route("/api/runs", get(runs_handler))
            .route("/api/runs/:id", get(run_handler))
            .route("/api/runs/:id/tasks", get(tasks_handler))
            .route("/api/runs/:id/messages", get(messages_handler))
            .route("/api/runs/:id/pause", post(pause_handler))
            .route("/api/runs/:id/resume", post(resume_handler))
            .route("/api/runs/:id/kill", post(kill_handler))
            .route("/events", get(sse_handler))
            .layer(CorsLayer::permissive())
            .with_state(state);

        // Start server
        let addr = format!("0.0.0.0:{}", self.port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!("Dashboard listening on http://{}", addr);

        axum::serve(listener, app).await?;

        Ok(())
    }
}

/// Shared application state
struct AppState {
    events_tx: broadcast::Sender<BusMessage>,
}

// ===== Handlers =====

async fn index_handler() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn status_handler() -> Json<StatusResponse> {
    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        status: "ok".to_string(),
    })
}

async fn runs_handler() -> Json<Vec<RunSummary>> {
    // TODO: Load from database
    Json(vec![])
}

async fn run_handler() -> Json<Option<RunDetail>> {
    // TODO: Load from database
    Json(None)
}

async fn tasks_handler() -> Json<Vec<TaskSummary>> {
    // TODO: Load from database
    Json(vec![])
}

async fn messages_handler() -> Json<Vec<MessageSummary>> {
    // TODO: Load from database
    Json(vec![])
}

async fn pause_handler() -> impl IntoResponse {
    // TODO: Implement pause
    StatusCode::OK
}

async fn resume_handler() -> impl IntoResponse {
    // TODO: Implement resume
    StatusCode::OK
}

async fn kill_handler() -> impl IntoResponse {
    // TODO: Implement kill
    StatusCode::OK
}

async fn sse_handler(
    State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| {
        match msg {
            Ok(bus_msg) => {
                let json = serde_json::to_string(&bus_msg).ok()?;
                Some(Ok(Event::default().data(json)))
            }
            Err(_) => None,
        }
    });

    Sse::new(stream)
}

// ===== Response types =====

#[derive(Serialize)]
struct StatusResponse {
    version: String,
    status: String,
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
    progress: Option<u8>,
}

#[derive(Serialize)]
struct MessageSummary {
    id: i64,
    task_id: Option<String>,
    message_type: String,
    created_at: String,
}

// ===== HTML Template =====

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>dare.run - Dashboard</title>
    <script src="https://unpkg.com/htmx.org@1.9.10"></script>
    <script src="https://cdn.tailwindcss.com"></script>
    <script src="https://unpkg.com/htmx.org/dist/ext/sse.js"></script>
</head>
<body class="bg-gray-900 text-gray-100 min-h-screen">
    <header class="bg-gray-800 border-b border-gray-700 px-6 py-4">
        <div class="flex items-center justify-between">
            <h1 class="text-2xl font-bold">
                <span class="text-blue-400">dare</span>.run
            </h1>
            <div class="flex gap-4">
                <button class="px-4 py-2 bg-yellow-600 hover:bg-yellow-500 rounded"
                        hx-post="/api/runs/current/pause" hx-swap="none">
                    Pause
                </button>
                <button class="px-4 py-2 bg-red-600 hover:bg-red-500 rounded"
                        hx-post="/api/runs/current/kill" hx-swap="none"
                        hx-confirm="Are you sure you want to kill all agents?">
                    Kill
                </button>
            </div>
        </div>
    </header>

    <main class="container mx-auto px-6 py-8" hx-ext="sse" sse-connect="/events">
        <!-- Status Section -->
        <section class="mb-8">
            <div id="run-status" class="bg-gray-800 rounded-lg p-6"
                 hx-get="/api/status" hx-trigger="load, every 5s">
                <p class="text-gray-400">Loading...</p>
            </div>
        </section>

        <!-- Task Graph Visualization -->
        <section class="mb-8">
            <h2 class="text-xl font-semibold mb-4">Task Graph</h2>
            <div id="task-graph" class="bg-gray-800 rounded-lg p-6 min-h-64">
                <p class="text-gray-400 text-center">No active run</p>
            </div>
        </section>

        <!-- Task List -->
        <section class="mb-8">
            <h2 class="text-xl font-semibold mb-4">Tasks</h2>
            <div id="task-list" class="space-y-2" hx-get="/api/runs/current/tasks" hx-trigger="load">
                <p class="text-gray-400">Loading tasks...</p>
            </div>
        </section>

        <!-- Message Bus -->
        <section>
            <h2 class="text-xl font-semibold mb-4">Message Bus</h2>
            <div id="message-bus" class="bg-gray-800 rounded-lg p-4 h-64 overflow-y-auto font-mono text-sm"
                 sse-swap="message" hx-swap="afterbegin">
                <p class="text-gray-500">Waiting for messages...</p>
            </div>
        </section>
    </main>

    <footer class="border-t border-gray-700 px-6 py-4 text-center text-gray-500">
        dare.run v0.1.0 — Multi-agent orchestration for OpenClaw
    </footer>

    <script>
        // SSE message handler
        document.body.addEventListener('htmx:sseMessage', function(e) {
            try {
                const msg = JSON.parse(e.detail.data);
                const el = document.getElementById('message-bus');
                const time = new Date().toLocaleTimeString();
                
                let html = '';
                if (msg.Progress) {
                    html = `<div class="text-blue-400">${time} [${msg.Progress.task_id}] Progress: ${msg.Progress.percent}% - ${msg.Progress.detail}</div>`;
                } else if (msg.Completed) {
                    html = `<div class="text-green-400">${time} [${msg.Completed.task_id}] ✓ Completed</div>`;
                } else if (msg.Failed) {
                    html = `<div class="text-red-400">${time} [${msg.Failed.task_id}] ✗ Failed: ${msg.Failed.error}</div>`;
                } else {
                    html = `<div class="text-gray-400">${time} ${JSON.stringify(msg)}</div>`;
                }
                
                el.insertAdjacentHTML('afterbegin', html);
            } catch (err) {
                console.error('SSE parse error:', err);
            }
        });
    </script>
</body>
</html>
"#;
