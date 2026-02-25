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
use crate::models::{RunStatus, TaskStatus};

/// Dashboard server with shared state for executor integration
pub struct DashboardServer {
    port: u16,
    config: Config,
    /// Optional shared database (when running alongside executor)
    shared_db: Option<Arc<Database>>,
    /// Optional shared event channel (when running alongside executor)
    shared_events_tx: Option<broadcast::Sender<TaskMessage>>,
}

impl DashboardServer {
    pub fn new(config: &Config, port: u16) -> Self {
        Self {
            port,
            config: config.clone(),
            shared_db: None,
            shared_events_tx: None,
        }
    }

    /// Create server with shared database and event channel (for integration with executor)
    pub fn with_shared_state(
        config: &Config,
        port: u16,
        db: Arc<Database>,
        events_tx: broadcast::Sender<TaskMessage>,
    ) -> Self {
        Self {
            port,
            config: config.clone(),
            shared_db: Some(db),
            shared_events_tx: Some(events_tx),
        }
    }

    /// Get the event sender (for executor to publish events)
    pub fn events_tx(&self) -> Option<broadcast::Sender<TaskMessage>> {
        self.shared_events_tx.clone()
    }

    pub async fn run(self) -> anyhow::Result<()> {
        // Use shared database or create new connection
        let db = if let Some(db) = self.shared_db {
            db
        } else {
            let db = Database::connect(&self.config.database_path())
                .await
                .map_err(|e| anyhow::anyhow!("Failed to connect to database: {}", e))?;
            db.migrate().await?;
            Arc::new(db)
        };

        // Use shared event channel or create new one
        let events_tx = self.shared_events_tx.unwrap_or_else(|| {
            let (tx, _) = broadcast::channel::<TaskMessage>(100);
            tx
        });

        let state = Arc::new(AppState { db, events_tx });

        // Build router with both HTMX and JSON API endpoints
        let app = Router::new()
            // Main dashboard
            .route("/", get(index_handler))
            .route("/:id", get(run_page_handler))
            .route("/run/:id", get(run_page_handler))
            
            // HTMX endpoints (return HTML fragments)
            .route("/htmx/status", get(htmx_status_handler))
            .route("/htmx/runs", get(htmx_runs_handler))
            .route("/htmx/run/:id", get(htmx_run_detail_handler))
            .route("/htmx/run/:id/waves", get(htmx_waves_handler))
            .route("/htmx/run/:id/tasks", get(htmx_tasks_handler))
            .route("/htmx/events", get(htmx_events_handler))
            
            // JSON API endpoints
            .route("/api/status", get(api_status_handler))
            .route("/api/runs", get(api_runs_handler))
            .route("/api/runs/:id", get(api_run_handler))
            .route("/api/runs/:id/tasks", get(api_tasks_handler))
            .route("/api/runs/:id/pause", post(api_pause_handler))
            .route("/api/runs/:id/resume", post(api_resume_handler))
            .route("/api/runs/:id/kill", post(api_kill_handler))
            
            // SSE endpoint for real-time updates
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

// ===== HTML Templates =====

fn status_badge_html(status: &str) -> String {
    match status.to_lowercase().as_str() {
        "pending" => r#"<span class="px-2 py-1 text-xs font-medium rounded-full bg-gray-700 text-gray-300">Pending</span>"#.to_string(),
        "spawned" => r#"<span class="px-2 py-1 text-xs font-medium rounded-full bg-blue-900 text-blue-300 animate-pulse">Spawned</span>"#.to_string(),
        "running" | "executing" => r#"<span class="px-2 py-1 text-xs font-medium rounded-full bg-yellow-900 text-yellow-300 animate-pulse">Running</span>"#.to_string(),
        "completed" => r#"<span class="px-2 py-1 text-xs font-medium rounded-full bg-green-900 text-green-300">Completed</span>"#.to_string(),
        "failed" => r#"<span class="px-2 py-1 text-xs font-medium rounded-full bg-red-900 text-red-300">Failed</span>"#.to_string(),
        "paused" => r#"<span class="px-2 py-1 text-xs font-medium rounded-full bg-purple-900 text-purple-300">Paused</span>"#.to_string(),
        _ => r#"<span class="px-2 py-1 text-xs font-medium rounded-full bg-gray-700 text-gray-300">Unknown</span>"#.to_string(),
    }
}

fn status_icon(status: &str) -> &'static str {
    match status.to_lowercase().as_str() {
        "pending" => "⏳",
        "spawned" => "🚀",
        "running" | "executing" => "🔄",
        "completed" => "✓",
        "failed" => "✗",
        "paused" => "⏸",
        _ => "?",
    }
}

// ===== Main Page Handler =====

async fn index_handler() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn run_page_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    let run = state.db.get_run(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let tasks = state.db.get_tasks(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let completed = tasks.iter().filter(|t| t.status == TaskStatus::Completed).count();
    let total = tasks.len();
    let progress = if total > 0 { (completed * 100) / total } else { 0 };

    let mut html = String::new();
    html.push_str(r#"<!DOCTYPE html><html lang="en"><head><meta charset="UTF-8"><meta name="viewport" content="width=device-width, initial-scale=1.0"><title>dare.run — Run Details</title><script src="https://unpkg.com/htmx.org@1.9.10"></script><script src="https://unpkg.com/htmx.org@1.9.10/dist/ext/sse.js"></script><script src="https://cdn.tailwindcss.com"></script></head><body class="bg-gray-900 text-gray-100 min-h-screen">"#);
    html.push_str(r#"<header class="bg-gray-800 border-b border-gray-700 px-6 py-4"><div class="max-w-7xl mx-auto flex items-center justify-between"><a href="/" class="text-gray-300 hover:text-white">← Back to dashboard</a><div class="text-sm text-gray-400">Run focus view</div></div></header>"#);
    html.push_str(r#"<main class="max-w-7xl mx-auto px-6 py-8">"#);
    html.push_str(&format!(r#"<div class="mb-6"><h1 class="text-2xl font-semibold">{}</h1><p class="text-gray-400 text-sm">{}</p><div class="mt-2">{}</div><div class="mt-3 h-2 bg-gray-700 rounded-full overflow-hidden"><div class="h-full bg-gradient-to-r from-blue-500 to-indigo-500" style="width:{}%"></div></div><p class="text-xs text-gray-400 mt-1">{}% complete</p></div>"#, run.name, run.id, status_badge_html(&format!("{:?}", run.status)), progress, progress));
    html.push_str(&format!(r#"<div id="waves-container" hx-get="/htmx/run/{}/waves" hx-trigger="load, every 5s" hx-swap="innerHTML"></div>"#, id));
    html.push_str(r#"</main></body></html>"#);

    Ok(Html(html))
}

// ===== HTMX Handlers (return HTML fragments) =====

async fn htmx_status_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    let run = state.db.get_latest_run().await.ok().flatten();
    
    let html = if let Some(run) = run {
        let tasks = state.db.get_tasks(&run.id).await.unwrap_or_default();
        let completed = tasks.iter().filter(|t| t.status == TaskStatus::Completed).count();
        let failed = tasks.iter().filter(|t| t.status == TaskStatus::Failed).count();
        let running = tasks.iter().filter(|t| t.status == TaskStatus::Spawned || t.status == TaskStatus::Executing).count();
        let total = tasks.len();
        let progress = if total > 0 { (completed * 100) / total } else { 0 };
        
        let mut html = String::new();
        html.push_str(r#"<div class="bg-gray-800 rounded-lg p-6 border border-gray-700">"#);
        html.push_str(r#"<div class="flex items-center justify-between mb-4"><div>"#);
        html.push_str(&format!(r#"<h2 class="text-xl font-semibold text-white">{}</h2>"#, run.name));
        html.push_str(&format!(r#"<p class="text-gray-400 text-sm">{}</p>"#, run.id));
        html.push_str(r#"</div><div class="text-right">"#);
        html.push_str(&status_badge_html(&format!("{:?}", run.status)));
        html.push_str(r#"</div></div>"#);
        
        // Progress Bar
        html.push_str(r#"<div class="mb-4">"#);
        html.push_str(&format!(r#"<div class="flex justify-between text-sm text-gray-400 mb-1"><span>Progress</span><span>{}%</span></div>"#, progress));
        html.push_str(&format!(r#"<div class="h-2 bg-gray-700 rounded-full overflow-hidden"><div class="h-full bg-gradient-to-r from-blue-500 to-indigo-500 transition-all duration-500" style="width: {}%"></div></div>"#, progress));
        html.push_str(r#"</div>"#);
        
        // Stats
        html.push_str(r#"<div class="grid grid-cols-4 gap-4 text-center">"#);
        html.push_str(&format!(r#"<div class="bg-gray-700/50 rounded p-2"><div class="text-2xl font-bold text-white">{}</div><div class="text-xs text-gray-400">Total</div></div>"#, total));
        html.push_str(&format!(r#"<div class="bg-green-900/30 rounded p-2"><div class="text-2xl font-bold text-green-400">{}</div><div class="text-xs text-gray-400">Completed</div></div>"#, completed));
        html.push_str(&format!(r#"<div class="bg-yellow-900/30 rounded p-2"><div class="text-2xl font-bold text-yellow-400">{}</div><div class="text-xs text-gray-400">Running</div></div>"#, running));
        html.push_str(&format!(r#"<div class="bg-red-900/30 rounded p-2"><div class="text-2xl font-bold text-red-400">{}</div><div class="text-xs text-gray-400">Failed</div></div>"#, failed));
        html.push_str(r#"</div>"#);
        
        // Actions
        html.push_str(r##"<div class="mt-4 flex gap-2">"##);
        html.push_str(r##"<a href="/"##);
        html.push_str(&run.id);
        html.push_str(r##"" class="px-4 py-2 bg-indigo-600 hover:bg-indigo-700 rounded text-sm font-medium">View Details</a>"##);
        html.push_str(r##"<button hx-post="/api/runs/"##);
        html.push_str(&run.id);
        html.push_str(r##"/pause" hx-swap="none" class="px-4 py-2 bg-gray-700 hover:bg-gray-600 rounded text-sm font-medium">Pause</button>"##);
        html.push_str(r##"</div></div>"##);
        
        html
    } else {
        r#"<div class="bg-gray-800 rounded-lg p-6 border border-gray-700 text-center"><p class="text-gray-400">No active runs</p><p class="text-sm text-gray-500 mt-2">Start a run with: dare run &lt;task.yaml&gt;</p></div>"#.to_string()
    };
    
    Html(html)
}

async fn htmx_runs_handler(State(state): State<Arc<AppState>>) -> Html<String> {
    let runs = state.db.list_runs(20).await.unwrap_or_default();
    
    if runs.is_empty() {
        return Html(r#"<p class="text-gray-400 text-center py-4">No runs found</p>"#.to_string());
    }
    
    let mut html = String::new();
    for run in runs {
        html.push_str(r##"<a class="block bg-gray-800 rounded-lg p-4 border border-gray-700 hover:border-gray-600 cursor-pointer" href="/"##);
        html.push_str(&run.id);
        html.push_str(r##""><div class="flex items-center justify-between"><div><h3 class="font-medium text-white">"##);
        html.push_str(&run.name);
        html.push_str(r##"</h3><p class="text-xs text-gray-500">"##);
        html.push_str(&run.created_at.format("%Y-%m-%d %H:%M").to_string());
        html.push_str(r##"</p></div>"##);
        html.push_str(&status_badge_html(&format!("{:?}", run.status)));
        html.push_str(r##"</div></a>"##);
    }
    
    Html(html)
}

async fn htmx_run_detail_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    let run = state.db.get_run(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    
    let tasks = state.db.get_tasks(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    let waves: std::collections::HashMap<usize, Vec<_>> = tasks.iter()
        .fold(std::collections::HashMap::new(), |mut acc, task| {
            acc.entry(task.wave).or_default().push(task);
            acc
        });
    
    let max_wave = waves.keys().max().copied().unwrap_or(0);
    let waves_html = build_waves_html(&waves, max_wave);
    
    let mut html = String::new();
    html.push_str(r#"<div class="bg-gray-800 rounded-lg p-6 border border-gray-700">"#);
    html.push_str(r#"<div class="flex items-center justify-between mb-6"><div>"#);
    html.push_str(&format!(r#"<h2 class="text-xl font-semibold text-white">{}</h2>"#, run.name));
    html.push_str(&format!(r#"<p class="text-sm text-gray-400">{}</p>"#, run.description.as_deref().unwrap_or("")));
    html.push_str(r#"</div><div class="flex items-center gap-4">"#);
    html.push_str(&status_badge_html(&format!("{:?}", run.status)));
    html.push_str(r#"<button onclick="document.getElementById('run-detail').innerHTML = ''" class="text-gray-400 hover:text-white transition-colors">✕</button>"#);
    html.push_str(r#"</div></div>"#);
    
    html.push_str(&format!(r#"<div id="waves-container" hx-get="/htmx/run/{}/waves" hx-trigger="every 5s" hx-swap="innerHTML">{}</div>"#, id, waves_html));
    html.push_str(r#"</div>"#);
    
    Ok(Html(html))
}

fn build_waves_html(waves: &std::collections::HashMap<usize, Vec<&crate::models::Task>>, max_wave: usize) -> String {
    let mut html = String::new();
    
    for wave_num in 0..=max_wave {
        let wave_tasks = waves.get(&wave_num).map(|v| v.as_slice()).unwrap_or(&[]);
        let all_complete = wave_tasks.iter().all(|t| t.status == TaskStatus::Completed);
        let any_failed = wave_tasks.iter().any(|t| t.status == TaskStatus::Failed);
        let any_running = wave_tasks.iter().any(|t| t.status == TaskStatus::Spawned || t.status == TaskStatus::Executing);
        
        let wave_status = if any_failed { "failed" } 
            else if all_complete { "completed" }
            else if any_running { "running" }
            else { "pending" };
        
        html.push_str(r#"<div class="mb-6">"#);
        html.push_str(&format!(r#"<div class="flex items-center gap-2 mb-3"><span class="text-lg font-semibold text-white">Wave {}</span>{}</div>"#, wave_num + 1, status_badge_html(wave_status)));
        html.push_str(r#"<div class="grid grid-cols-1 md:grid-cols-2 lg:grid-cols-3 gap-3">"#);
        
        for task in wave_tasks {
            let error_html = if let Some(ref err) = task.error {
                let truncated = if err.len() > 50 { &err[..50] } else { err };
                format!(r#"<p class="text-xs text-red-400 mt-2 truncate" title="{}">{}</p>"#, err, truncated)
            } else {
                String::new()
            };
            
            let timing = if let (Some(start), Some(end)) = (task.started_at, task.completed_at) {
                let duration = end.signed_duration_since(start);
                format!("{}s", duration.num_seconds())
            } else if task.started_at.is_some() {
                "Running...".to_string()
            } else {
                "—".to_string()
            };
            
            html.push_str(r#"<div class="bg-gray-700/50 rounded-lg p-3 border border-gray-600">"#);
            html.push_str(&format!(r#"<div class="flex items-start justify-between mb-2"><span class="font-mono text-sm text-indigo-400">{}</span>{}</div>"#, task.id, status_badge_html(&format!("{:?}", task.status))));
            html.push_str(&format!(r#"<p class="text-sm text-gray-300 line-clamp-2">{}</p>"#, task.description));
            html.push_str(&format!(r#"<div class="flex items-center justify-between mt-2 text-xs text-gray-500"><span>{}</span><span>{}</span></div>"#, status_icon(&format!("{:?}", task.status)), timing));
            html.push_str(&error_html);
            html.push_str(r#"</div>"#);
        }
        
        html.push_str(r#"</div></div>"#);
    }
    
    html
}

async fn htmx_waves_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    let tasks = state.db.get_tasks(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    let waves: std::collections::HashMap<usize, Vec<_>> = tasks.iter()
        .fold(std::collections::HashMap::new(), |mut acc, task| {
            acc.entry(task.wave).or_default().push(task);
            acc
        });
    
    let max_wave = waves.keys().max().copied().unwrap_or(0);
    let html = build_waves_html(&waves, max_wave);
    
    Ok(Html(html))
}

async fn htmx_tasks_handler(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    let tasks = state.db.get_tasks(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    let mut html = String::new();
    for task in tasks {
        html.push_str(&format!(
            r#"<div class="bg-gray-700/50 rounded p-3 border border-gray-600"><div class="flex items-center justify-between"><span class="font-mono text-sm text-indigo-400">{}</span>{}</div><p class="text-sm text-gray-300 mt-1">{}</p></div>"#,
            task.id,
            status_badge_html(&format!("{:?}", task.status)),
            task.description
        ));
    }
    
    Ok(Html(html))
}

async fn htmx_events_handler(State(_state): State<Arc<AppState>>) -> Html<&'static str> {
    Html(r#"<p class="text-gray-500 italic">Waiting for events...</p>"#)
}

// ===== JSON API Handlers =====

async fn api_status_handler(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
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

async fn api_runs_handler(State(state): State<Arc<AppState>>) -> Json<Vec<RunSummary>> {
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

async fn api_run_handler(
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

async fn api_tasks_handler(
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
                session_id: t.agent_session_id,
            })
            .collect(),
    ))
}

async fn api_pause_handler(
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

async fn api_resume_handler(
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

async fn api_kill_handler(
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

// ===== SSE Handler =====

async fn sse_handler(
    State(state): State<Arc<AppState>>,
) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let rx = state.events_tx.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(task_msg) => {
            let event_type = match &task_msg {
                TaskMessage::Spawned { .. } => "spawned",
                TaskMessage::Running { .. } => "running",
                TaskMessage::Completed { .. } => "completed",
                TaskMessage::Failed { .. } => "failed",
            };
            
            // Create HTML fragment for the event log
            let html = match &task_msg {
                TaskMessage::Spawned { task_id, session_key } => {
                    let session_short = if session_key.len() > 20 { &session_key[..20] } else { session_key };
                    format!(
                        r#"<div class="text-blue-400 border-l-2 border-blue-500 pl-2 py-1"><span class="text-gray-500 text-xs">{}</span> <span class="font-mono">[{}]</span> ⏳ Spawned <span class="text-gray-500 text-xs">({}...)</span></div>"#,
                        chrono::Local::now().format("%H:%M:%S"),
                        task_id, 
                        session_short
                    )
                }
                TaskMessage::Running { task_id } => {
                    format!(
                        r#"<div class="text-yellow-400 border-l-2 border-yellow-500 pl-2 py-1"><span class="text-gray-500 text-xs">{}</span> <span class="font-mono">[{}]</span> 🔄 Running</div>"#,
                        chrono::Local::now().format("%H:%M:%S"),
                        task_id
                    )
                }
                TaskMessage::Completed { task_id, summary } => {
                    format!(
                        r#"<div class="text-green-400 border-l-2 border-green-500 pl-2 py-1"><span class="text-gray-500 text-xs">{}</span> <span class="font-mono">[{}]</span> ✓ Completed <span class="text-gray-500 text-xs">{}</span></div>"#,
                        chrono::Local::now().format("%H:%M:%S"),
                        task_id,
                        summary
                    )
                }
                TaskMessage::Failed { task_id, error } => {
                    format!(
                        r#"<div class="text-red-400 border-l-2 border-red-500 pl-2 py-1"><span class="text-gray-500 text-xs">{}</span> <span class="font-mono">[{}]</span> ✗ Failed <span class="text-red-300 text-xs">{}</span></div>"#,
                        chrono::Local::now().format("%H:%M:%S"),
                        task_id,
                        error
                    )
                }
            };
            
            Some(Ok(Event::default()
                .event(event_type)
                .data(html)))
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
    session_id: Option<String>,
}

// ===== HTML Template =====

const INDEX_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>dare.run — Dashboard</title>
    <script src="https://unpkg.com/htmx.org@1.9.10"></script>
    <script src="https://unpkg.com/htmx.org@1.9.10/dist/ext/sse.js"></script>
    <script src="https://cdn.tailwindcss.com"></script>
    <style>
        @keyframes pulse {
            0%, 100% { opacity: 1; }
            50% { opacity: 0.5; }
        }
        .animate-pulse { animation: pulse 2s ease-in-out infinite; }
        .line-clamp-2 {
            display: -webkit-box;
            -webkit-line-clamp: 2;
            -webkit-box-orient: vertical;
            overflow: hidden;
        }
        /* Custom scrollbar */
        ::-webkit-scrollbar { width: 8px; height: 8px; }
        ::-webkit-scrollbar-track { background: #1f2937; }
        ::-webkit-scrollbar-thumb { background: #4b5563; border-radius: 4px; }
        ::-webkit-scrollbar-thumb:hover { background: #6b7280; }
    </style>
</head>
<body class="bg-gray-900 text-gray-100 min-h-screen">
    <!-- Header -->
    <header class="bg-gray-800 border-b border-gray-700 px-6 py-4 sticky top-0 z-50">
        <div class="flex items-center justify-between max-w-7xl mx-auto">
            <h1 class="text-2xl font-bold">
                <span class="text-indigo-400">dare</span><span class="text-gray-500">.run</span>
            </h1>
            <div class="flex items-center gap-4">
                <span class="text-sm text-gray-400">Multi-agent orchestration</span>
                <span class="px-2 py-1 text-xs bg-gray-700 rounded">v0.1.0</span>
            </div>
        </div>
    </header>

    <main class="max-w-7xl mx-auto px-6 py-8" hx-ext="sse" sse-connect="/events">
        <!-- Current Status Section -->
        <section class="mb-8">
            <h2 class="text-lg font-semibold text-gray-300 mb-4">Current Run</h2>
            <div id="status-section" hx-get="/htmx/status" hx-trigger="load, every 3s" hx-swap="innerHTML">
                <div class="bg-gray-800 rounded-lg p-6 animate-pulse">
                    <div class="h-6 bg-gray-700 rounded w-1/3 mb-4"></div>
                    <div class="h-4 bg-gray-700 rounded w-2/3"></div>
                </div>
            </div>
        </section>

        <!-- Run Details (populated when a run is selected) -->
        <section class="mb-8">
            <div id="run-detail"></div>
        </section>

        <!-- Two Column Layout: Runs List + Event Log -->
        <div class="grid grid-cols-1 lg:grid-cols-2 gap-8">
            <!-- Recent Runs -->
            <section>
                <h2 class="text-lg font-semibold text-gray-300 mb-4">Recent Runs</h2>
                <div id="runs-list" class="space-y-2 max-h-96 overflow-y-auto" 
                     hx-get="/htmx/runs" hx-trigger="load, every 10s" hx-swap="innerHTML">
                    <div class="bg-gray-800 rounded-lg p-4 animate-pulse">
                        <div class="h-4 bg-gray-700 rounded w-2/3"></div>
                    </div>
                </div>
            </section>

            <!-- Live Event Log -->
            <section>
                <h2 class="text-lg font-semibold text-gray-300 mb-4">Live Events</h2>
                <div id="events-log" 
                     class="bg-gray-800 rounded-lg p-4 h-96 overflow-y-auto font-mono text-sm border border-gray-700"
                     sse-swap="spawned,running,completed,failed"
                     hx-swap="afterbegin">
                    <p class="text-gray-500 italic">Waiting for events...</p>
                </div>
            </section>
        </div>
    </main>

    <!-- Footer -->
    <footer class="border-t border-gray-700 px-6 py-4 mt-8">
        <div class="max-w-7xl mx-auto flex items-center justify-between text-sm text-gray-500">
            <span>dare.run — Multi-agent orchestration for OpenClaw</span>
            <span>
                <a href="https://github.com/iamngoni/dare" class="hover:text-indigo-400 transition-colors">GitHub</a>
            </span>
        </div>
    </footer>

    <script>
        // Remove "waiting for events" message when first event arrives
        document.body.addEventListener('htmx:sseMessage', function(e) {
            const eventsLog = document.getElementById('events-log');
            const waiting = eventsLog.querySelector('.italic');
            if (waiting) {
                waiting.remove();
            }
            
            // Keep only last 100 events
            while (eventsLog.children.length > 100) {
                eventsLog.removeChild(eventsLog.lastChild);
            }
            
            // Also refresh status when we get an event
            htmx.trigger('#status-section', 'htmx:load');
        });
        
        // Refresh run details when task completes
        document.body.addEventListener('htmx:sseMessage', function(e) {
            const runDetail = document.getElementById('run-detail');
            if (runDetail.children.length > 0) {
                const wavesContainer = document.getElementById('waves-container');
                if (wavesContainer) {
                    htmx.trigger(wavesContainer, 'htmx:load');
                }
            }
        });
    </script>
</body>
</html>
"##;
