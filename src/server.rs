//! Web dashboard server
//!
//! HTMX + SSE real-time dashboard for monitoring dare.run executions.
//! Design based on Penpot mockups: dark terminal aesthetic with
//! JetBrains Mono + Space Grotesk, #00FF88 accent, Lucide icons.

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

// Load the base HTML template at compile time
const BASE_HTML: &str = include_str!("../templates/base.html");

/// Dashboard server with shared state for executor integration
pub struct DashboardServer {
    port: u16,
    config: Config,
    shared_db: Option<Arc<Database>>,
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

    pub fn events_tx(&self) -> Option<broadcast::Sender<TaskMessage>> {
        self.shared_events_tx.clone()
    }

    pub async fn run(self) -> anyhow::Result<()> {
        let db = if let Some(db) = self.shared_db {
            db
        } else {
            let db = Database::connect(&self.config.database_path())
                .await
                .map_err(|e| anyhow::anyhow!("Failed to connect to database: {}", e))?;
            db.migrate().await?;
            Arc::new(db)
        };

        let events_tx = self.shared_events_tx.unwrap_or_else(|| {
            let (tx, _) = broadcast::channel::<TaskMessage>(100);
            tx
        });

        let state = Arc::new(AppState { db, events_tx });

        let app = Router::new()
            // Pages
            .route("/", get(page_overview))
            .route("/council/:id", get(page_council))
            .route("/tasks/:id", get(page_tasks))
            .route("/new", get(page_new_council))
            .route("/run/:id", get(page_run))
            
            // HTMX fragments
            .route("/htmx/stats", get(htmx_stats))
            .route("/htmx/councils", get(htmx_councils))
            .route("/htmx/agents", get(htmx_agents))
            .route("/htmx/feed", get(htmx_feed))
            .route("/htmx/run/:id/waves", get(htmx_waves))
            .route("/htmx/run/:id/messages", get(htmx_messages))
            
            // JSON API
            .route("/api/status", get(api_status))
            .route("/api/runs", get(api_runs))
            .route("/api/runs/:id", get(api_run))
            .route("/api/runs/:id/tasks", get(api_tasks))
            .route("/api/runs/:id/pause", post(api_pause))
            .route("/api/runs/:id/resume", post(api_resume))
            .route("/api/runs/:id/kill", post(api_kill))
            
            // SSE
            .route("/events", get(sse_handler))
            
            .layer(CorsLayer::permissive())
            .with_state(state);

        let addr = format!("0.0.0.0:{}", self.port);
        let listener = tokio::net::TcpListener::bind(&addr).await?;
        tracing::info!("Dashboard listening on http://localhost:{}", self.port);
        axum::serve(listener, app).await?;
        Ok(())
    }
}

struct AppState {
    db: Arc<Database>,
    events_tx: broadcast::Sender<TaskMessage>,
}

// ===== Template helpers =====

fn render_page(title: &str, body: &str) -> String {
    BASE_HTML
        .replace("{{TITLE}}", title)
        .replace("{{BODY}}", body)
}

fn sidebar(active: &str) -> String {
    let nav_items = vec![
        ("layout-dashboard", "OVERVIEW", "/"),
        ("message-square", "COUNCIL", "#"),
        ("git-branch", "TASKS", "#"),
        ("users", "AGENTS", "#"),
        ("archive", "HISTORY", "#"),
    ];

    let mut nav_html = String::new();
    for (icon, label, href) in &nav_items {
        let active_class = if *label == active.to_uppercase() { " active" } else { "" };
        nav_html.push_str(&format!(
            r#"<a href="{}" class="nav-item{}"><i data-lucide="{}"></i>{}</a>"#,
            href, active_class, icon, label
        ));
    }

    format!(
        r##"<nav class="sidebar">
        <div class="sidebar-top">
            <div class="brand">
                <div class="brand-dot"></div>
                <span class="brand-text">dare</span>
            </div>
            <div>
                <div class="nav-label">MAIN</div>
                <div class="nav-section">{}</div>
            </div>
        </div>
        <div class="sidebar-bottom">
            <div class="gw-status">
                <div class="gw-row">
                    <div class="gw-dot live-dot"></div>
                    <span class="gw-label">GATEWAY [ACTIVE]</span>
                </div>
                <span class="gw-sub">Ed25519 auth · ws://127.0.0.1:18789</span>
            </div>
            <a href="#" class="nav-item"><i data-lucide="settings"></i>SETTINGS</a>
            <span class="version-text">v0.1.0 · dare.run</span>
        </div>
    </nav>"##,
        nav_html
    )
}

fn badge_html(status: &str) -> String {
    let (class, label) = match status.to_lowercase().as_str() {
        "pending" => ("badge-pending", "[PENDING]"),
        "spawned" | "building" => ("badge-building", "[BUILDING]"),
        "running" | "executing" | "active" => ("badge-active", "[ACTIVE]"),
        "completed" | "done" => ("badge-done", "[DONE]"),
        "failed" => ("badge-failed", "[FAILED]"),
        "paused" => ("badge-pending", "[PAUSED]"),
        "debating" => ("badge-debating", "[DEBATING]"),
        "finishing" => ("badge-finishing", "[FINISHING]"),
        _ => ("badge-pending", "[UNKNOWN]"),
    };
    format!(r#"<span class="badge {}">{}</span>"#, class, label)
}

// ===== Page Handlers =====

async fn page_overview(State(state): State<Arc<AppState>>) -> Html<String> {
    let runs = state.db.list_runs(20).await.unwrap_or_default();
    let active_runs: Vec<_> = runs.iter().filter(|r| r.status == RunStatus::Running).collect();
    let completed_runs: Vec<_> = runs.iter().filter(|r| r.status == RunStatus::Completed).collect();

    // Count total tasks across all runs
    let mut total_tasks = 0;
    let mut completed_tasks = 0;
    let mut active_agents = 0;
    for run in &runs {
        if let Ok(tasks) = state.db.get_tasks(&run.id).await {
            total_tasks += tasks.len();
            completed_tasks += tasks.iter().filter(|t| t.status == TaskStatus::Completed).count();
            active_agents += tasks.iter().filter(|t| t.status == TaskStatus::Spawned || t.status == TaskStatus::Executing).count();
        }
    }
    let success_rate = if total_tasks > 0 {
        format!("{:.1}%", (completed_tasks as f64 / total_tasks as f64) * 100.0)
    } else {
        "—".to_string()
    };

    // Build active councils HTML
    let mut councils_html = String::new();
    if runs.is_empty() {
        councils_html.push_str(r#"<div style="text-align:center;padding:40px 0;color:var(--c-muted);">
            <p>No councils yet</p>
            <p style="font-size:10px;margin-top:8px;color:#3f3f3f;">Start one with: dare run &lt;task.yaml&gt;</p>
        </div>"#);
    }
    for run in runs.iter().take(10) {
        let tasks = state.db.get_tasks(&run.id).await.unwrap_or_default();
        let max_wave = tasks.iter().map(|t| t.wave).max().unwrap_or(0) + 1;
        let current_wave = tasks.iter()
            .filter(|t| t.status == TaskStatus::Spawned || t.status == TaskStatus::Executing)
            .map(|t| t.wave + 1)
            .max()
            .unwrap_or(0);
        let running_agents = tasks.iter()
            .filter(|t| t.status == TaskStatus::Spawned || t.status == TaskStatus::Executing)
            .count();

        let elapsed = if let Some(start) = run.started_at {
            let dur = if let Some(end) = run.completed_at {
                end.signed_duration_since(start)
            } else {
                chrono::Utc::now().signed_duration_since(start)
            };
            let mins = dur.num_minutes();
            let secs = dur.num_seconds() % 60;
            if mins > 0 { format!("{}m {}s", mins, secs) } else { format!("{}s", secs) }
        } else {
            "—".to_string()
        };

        let dot_class = match run.status {
            RunStatus::Running => "active",
            RunStatus::Failed => "failed",
            _ => "active",
        };
        let run_badge = match run.status {
            RunStatus::Running => badge_html("building"),
            RunStatus::Completed => badge_html("done"),
            RunStatus::Failed => badge_html("failed"),
            RunStatus::Paused => badge_html("paused"),
            RunStatus::Pending => badge_html("pending"),
        };

        let meta = format!("Wave {}/{} • {} agents • {} elapsed", 
            current_wave, max_wave, running_agents, elapsed);

        councils_html.push_str(&format!(
            r#"<a href="/run/{}" class="council-row">
                <div class="council-dot {}"></div>
                <div class="council-info">
                    <span class="council-name">{}</span>
                    <span class="council-meta">{}</span>
                </div>
                {}
            </a>"#,
            run.id, dot_class, run.name, meta, run_badge
        ));
    }

    // Build feed HTML
    let mut feed_html = String::new();
    for run in runs.iter().take(3) {
        if let Ok(tasks) = state.db.get_tasks(&run.id).await {
            for task in tasks.iter().rev().take(4) {
                let (class, text) = match task.status {
                    TaskStatus::Completed => ("", format!("{} completed {}", run.name, task.id)),
                    TaskStatus::Failed => ("warning", format!("{} failed on {}", run.name, task.id)),
                    TaskStatus::Spawned => ("info", format!("{} spawned for {}", task.id, run.name)),
                    TaskStatus::Executing => ("", format!("{} executing for {}", task.id, run.name)),
                    _ => ("", format!("{} pending", task.id)),
                };
                let time_ago = if let Some(t) = task.completed_at.or(task.started_at) {
                    let dur = chrono::Utc::now().signed_duration_since(t);
                    if dur.num_hours() > 0 { format!("{}h ago", dur.num_hours()) }
                    else if dur.num_minutes() > 0 { format!("{}m ago", dur.num_minutes()) }
                    else { format!("{}s ago", dur.num_seconds()) }
                } else {
                    "—".to_string()
                };
                feed_html.push_str(&format!(
                    r#"<div class="feed-item"><span class="feed-text {}">{}</span><span class="feed-time">{}</span></div>"#,
                    class, text, time_ago
                ));
            }
        }
    }
    if feed_html.is_empty() {
        feed_html = r#"<div class="feed-item"><span class="feed-text" style="color:#3f3f3f;">No activity yet</span></div>"#.to_string();
    }

    let body = format!(
        r#"{}
    <div class="main-content" hx-ext="sse" sse-connect="/events">
        <div class="header-row">
            <div>
                <h1 class="header-title">Command Center</h1>
                <div class="header-sub">// WAVE-BASED ORCHESTRATION OVERVIEW</div>
            </div>
            <div class="header-right">
                <div class="status-pill">
                    <div class="dot live-dot"></div>
                    <span class="label">{} ACTIVE</span>
                </div>
                <a href="/new" class="btn-primary">
                    <i data-lucide="plus" style="width:14px;height:14px;"></i>
                    NEW COUNCIL
                </a>
            </div>
        </div>

        <div class="stats-row" id="stats-row" hx-get="/htmx/stats" hx-trigger="load, every 10s" hx-swap="innerHTML">
            <div class="metric-card"><span class="metric-label">LOADING...</span></div>
        </div>

        <div class="content-row">
            <div class="main-col">
                <div class="panel" style="flex:1;">
                    <div class="panel-header">
                        <span class="panel-title">ACTIVE COUNCILS</span>
                        <span class="panel-count">{} running</span>
                    </div>
                    <div id="councils-list" style="display:flex;flex-direction:column;gap:8px;">
                        {}
                    </div>
                </div>
            </div>
            <div class="side-col">
                <div class="panel">
                    <span class="panel-title">AGENT HEALTH</span>
                    <div id="agents-panel" hx-get="/htmx/agents" hx-trigger="load, every 5s" hx-swap="innerHTML">
                        <span style="font-size:10px;color:#3f3f3f;">Loading...</span>
                    </div>
                </div>
                <div class="panel" style="flex:1;">
                    <div class="panel-header">
                        <span class="panel-title">ACTIVITY FEED</span>
                        <div class="chat-live">
                            <div class="chat-live-dot live-dot"></div>
                            <span class="chat-live-text">LIVE</span>
                        </div>
                    </div>
                    <div id="feed-list" style="display:flex;flex-direction:column;gap:12px;"
                         sse-swap="spawned,running,completed,failed" hx-swap="afterbegin">
                        {}
                    </div>
                </div>
            </div>
        </div>
    </div>"#,
        sidebar("overview"),
        active_runs.len(),
        active_runs.len(),
        councils_html,
        feed_html
    );

    Html(render_page("Command Center", &body))
}

async fn page_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    let run = state.db.get_run(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let tasks = state.db.get_tasks(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let max_wave = tasks.iter().map(|t| t.wave).max().unwrap_or(0);
    let completed = tasks.iter().filter(|t| t.status == TaskStatus::Completed).count();
    let total = tasks.len();
    let running_agents = tasks.iter()
        .filter(|t| t.status == TaskStatus::Spawned || t.status == TaskStatus::Executing)
        .count();

    // Wave progress segments
    let mut wave_progress = String::new();
    for w in 0..=max_wave {
        let wave_tasks: Vec<_> = tasks.iter().filter(|t| t.wave == w).collect();
        let all_done = wave_tasks.iter().all(|t| t.status == TaskStatus::Completed);
        let seg_class = if all_done { "done" } else { "pending" };
        wave_progress.push_str(&format!(r#"<div class="wave-progress-seg {}"></div>"#, seg_class));
    }

    // Build wave columns
    let waves_html = build_task_graph_html(&tasks, max_wave);

    let run_badge = badge_html(&format!("{:?}", run.status).to_lowercase());

    let body = format!(
        r#"{}
    <div class="main-content" hx-ext="sse" sse-connect="/events">
        <div class="header-row">
            <div>
                <h1 class="header-title">Task Graph</h1>
                <div class="header-sub">// {} • {} WAVES • {} TASKS</div>
            </div>
            <div class="header-right">
                <a href="/" class="btn-outline">
                    <i data-lucide="arrow-left" style="width:14px;height:14px;"></i>
                    BACK
                </a>
                <a href="/council/{}" class="btn-outline">
                    <i data-lucide="message-square" style="width:14px;height:14px;"></i>
                    COUNCIL
                </a>
                {}
            </div>
        </div>

        <div class="wave-progress">{}</div>

        <div id="waves-container" hx-get="/htmx/run/{}/waves" hx-trigger="every 5s" hx-swap="innerHTML"
             class="waves-row" style="flex:1;">
            {}
        </div>
    </div>"#,
        sidebar("tasks"),
        run.name.to_uppercase(),
        max_wave + 1,
        total,
        id,
        run_badge,
        wave_progress,
        id,
        waves_html
    );

    Ok(Html(render_page(&format!("Tasks — {}", run.name), &body)))
}

async fn page_council(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    let run = state.db.get_run(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let tasks = state.db.get_tasks(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let max_wave = tasks.iter().map(|t| t.wave).max().unwrap_or(0) + 1;
    let current_wave = tasks.iter()
        .filter(|t| t.status == TaskStatus::Spawned || t.status == TaskStatus::Executing)
        .map(|t| t.wave + 1)
        .max()
        .unwrap_or(max_wave);
    let total_agents = tasks.len();
    let total_messages = tasks.iter()
        .filter(|t| t.status != TaskStatus::Pending)
        .count() * 3; // Estimate ~3 messages per active task

    let run_badge = badge_html(&format!("{:?}", run.status).to_lowercase());

    // Build participants list
    let mut participants_html = String::new();
    for (i, task) in tasks.iter().enumerate() {
        let role = if i == 0 { "planner" } else if task.description.to_lowercase().contains("test") { "testing" }
            else if task.description.to_lowercase().contains("review") { "code review" }
            else { "implementation" };
        let dot_color = match task.status {
            TaskStatus::Completed => "var(--c-accent)",
            TaskStatus::Failed => "var(--c-danger)",
            TaskStatus::Spawned | TaskStatus::Executing => "var(--c-accent)",
            TaskStatus::Pending => "var(--c-muted)",
        };
        let name_color = if task.status == TaskStatus::Spawned || task.status == TaskStatus::Executing {
            "var(--c-accent)"
        } else {
            "var(--c-text)"
        };
        participants_html.push_str(&format!(
            r#"<div class="participant">
                <div class="participant-dot" style="background:{}"></div>
                <span class="participant-name" style="color:{}">{}</span>
                <span class="participant-role">{}</span>
            </div>"#,
            dot_color, name_color, task.id, role
        ));
    }

    // Build messages from task lifecycle
    let mut messages_html = String::new();
    for task in &tasks {
        if task.status == TaskStatus::Pending { continue; }
        
        // Spawned message
        let avatar_class = if task.description.to_lowercase().contains("test") { "tester" }
            else if task.description.to_lowercase().contains("review") { "reviewer" }
            else { "coder" };
        let name_class = avatar_class;
        let initials = task.id.chars().take(2).collect::<String>().to_uppercase();

        let time_str = task.started_at
            .map(|t| t.format("%H:%M").to_string())
            .unwrap_or_else(|| "—".to_string());

        messages_html.push_str(&format!(
            r#"<div class="msg">
                <div class="msg-avatar {}">{}</div>
                <div class="msg-body">
                    <div class="msg-header">
                        <span class="msg-name {}">{}</span>
                        <span class="msg-time">{}</span>
                    </div>
                    <div class="msg-text">Picking up task: {}</div>
                </div>
            </div>"#,
            avatar_class, initials, name_class, task.id, time_str, task.description
        ));

        // Completion message
        if task.status == TaskStatus::Completed {
            let end_time = task.completed_at
                .map(|t| t.format("%H:%M").to_string())
                .unwrap_or_else(|| "—".to_string());
            let duration = if let (Some(s), Some(e)) = (task.started_at, task.completed_at) {
                let d = e.signed_duration_since(s);
                format!(" ({}s)", d.num_seconds())
            } else {
                String::new()
            };
            messages_html.push_str(&format!(
                r#"<div class="msg">
                    <div class="msg-avatar {}">{}</div>
                    <div class="msg-body">
                        <div class="msg-header">
                            <span class="msg-name {}">{}</span>
                            <span class="msg-time">{}</span>
                            <span class="msg-tag decision">DONE</span>
                        </div>
                        <div class="msg-text">Task completed{}</div>
                    </div>
                </div>"#,
                avatar_class, initials, name_class, task.id, end_time, duration
            ));
        }

        // Failed message
        if task.status == TaskStatus::Failed {
            let err = task.error.as_deref().unwrap_or("Unknown error");
            messages_html.push_str(&format!(
                r#"<div class="msg">
                    <div class="msg-avatar reviewer">{}</div>
                    <div class="msg-body">
                        <div class="msg-header">
                            <span class="msg-name reviewer">{}</span>
                            <span class="msg-tag challenge">FAILED</span>
                        </div>
                        <div class="msg-text" style="color:var(--c-danger);">{}</div>
                    </div>
                </div>"#,
                initials, task.id, err
            ));
        }
    }

    if messages_html.is_empty() {
        messages_html = r#"<div style="text-align:center;padding:60px 0;color:#3f3f3f;">
            <p>Council is assembling...</p>
            <p style="font-size:10px;margin-top:8px;">Messages will appear here as agents communicate</p>
        </div>"#.to_string();
    }

    // Recent decisions
    let mut decisions_html = String::new();
    for task in tasks.iter().filter(|t| t.status == TaskStatus::Completed).rev().take(3) {
        let time_ago = task.completed_at.map(|t| {
            let d = chrono::Utc::now().signed_duration_since(t);
            if d.num_minutes() > 0 { format!("{}m ago", d.num_minutes()) } else { format!("{}s ago", d.num_seconds()) }
        }).unwrap_or_else(|| "—".to_string());
        decisions_html.push_str(&format!(
            r#"<div class="decision-card">
                <span class="decision-text">Completed: {}</span>
                <span class="decision-by">by {} • {}</span>
            </div>"#,
            task.description, task.id, time_ago
        ));
    }

    let body = format!(
        r#"{}
    <div class="chat-main" hx-ext="sse" sse-connect="/events">
        <div class="chat-header">
            <div class="chat-header-left">
                <span class="chat-header-title">{}</span>
                {}
            </div>
            <div class="chat-header-right">
                <span class="chat-agents">{} AGENTS</span>
                <div class="chat-live">
                    <div class="chat-live-dot live-dot"></div>
                    <span class="chat-live-text">LIVE</span>
                </div>
                <a href="/tasks/{}" class="btn-outline">
                    <i data-lucide="git-branch" style="width:14px;height:14px;"></i>
                    TASKS
                </a>
            </div>
        </div>

        <div style="display:flex;flex:1;min-height:0;">
            <div class="messages-area" id="messages-area"
                 hx-get="/htmx/run/{}/messages" hx-trigger="every 5s" hx-swap="innerHTML">
                {}
            </div>
            <div class="context-panel">
                <span class="ctx-title">COUNCIL CONTEXT</span>
                <div class="ctx-info">
                    <div class="ctx-row"><span class="ctx-label">STATUS</span><span class="ctx-value" style="color:var(--c-accent);">{}</span></div>
                    <div class="ctx-row"><span class="ctx-label">WAVE</span><span class="ctx-value">{} of {}</span></div>
                    <div class="ctx-row"><span class="ctx-label">MESSAGES</span><span class="ctx-value">{}</span></div>
                </div>
                <div class="ctx-divider"></div>
                <span class="ctx-title">PARTICIPANTS</span>
                {}
                <div class="ctx-divider"></div>
                <span class="ctx-title">RECENT DECISIONS</span>
                {}
            </div>
        </div>

        <div class="chat-input">
            <span class="input-tag">YOU</span>
            <input type="text" placeholder="Drop a message into the council..." disabled>
        </div>
    </div>"#,
        sidebar("council"),
        run.name,
        run_badge,
        total_agents,
        id,
        id,
        messages_html,
        format!("[{:?}]", run.status).to_uppercase(),
        current_wave,
        max_wave,
        total_messages,
        participants_html,
        decisions_html
    );

    Ok(Html(render_page(&format!("Council — {}", run.name), &body)))
}

async fn page_tasks(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    // Redirect to /run/:id which shows the task graph
    page_run(State(state), Path(id)).await
}

async fn page_new_council() -> Html<String> {
    let body = format!(
        r#"<div class="top-header">
        <div class="brand">
            <div class="brand-dot"></div>
            <span class="brand-text">dare</span>
        </div>
        <a href="/" class="btn-outline">
            <i data-lucide="arrow-left" style="width:14px;height:14px;"></i>
            BACK TO DASHBOARD
        </a>
    </div>
    <div style="margin-left:0;" class="main-content">
        <div class="form-center">
            <div class="form-title-block">
                <h1 class="form-title">New Council</h1>
                <div class="form-sub">// DESCRIBE YOUR GOAL AND THE COUNCIL WILL FIGURE OUT THE REST</div>
            </div>

            <div class="form-section">
                <span class="form-label">GOAL</span>
                <textarea class="form-textarea" placeholder="Refactor the authentication system to use JWT tokens with&#10;refresh token rotation. Extract middleware to shared module.&#10;Add comprehensive tests."></textarea>
                <span class="form-hint">PRD, feature spec, or just a sentence. The planner will decompose it.</span>
            </div>

            <div class="form-row">
                <div class="form-field">
                    <span class="form-label">MODEL</span>
                    <select class="form-select">
                        <option>claude-opus-4</option>
                        <option>claude-sonnet-4</option>
                        <option>gpt-4o</option>
                    </select>
                </div>
                <div class="form-field">
                    <span class="form-label">MAX AGENTS</span>
                    <select class="form-select">
                        <option>4 agents</option>
                        <option>6 agents</option>
                        <option>8 agents</option>
                        <option>12 agents</option>
                    </select>
                </div>
            </div>

            <div class="form-section" style="gap:16px;">
                <span class="form-label">OPTIONS</span>
                <div class="option-row">
                    <div class="option-left">
                        <span class="option-name">Auto-resolve conflicts</span>
                        <span class="option-desc">Let the orchestrator break deadlocks automatically</span>
                    </div>
                    <div class="toggle on" onclick="this.classList.toggle('on');this.classList.toggle('off');">
                        <div class="toggle-knob"></div>
                    </div>
                </div>
                <div class="option-row">
                    <div class="option-left">
                        <span class="option-name">Human approval gates</span>
                        <span class="option-desc">Pause at wave boundaries for your review</span>
                    </div>
                    <div class="toggle off" onclick="this.classList.toggle('on');this.classList.toggle('off');">
                        <div class="toggle-knob"></div>
                    </div>
                </div>
                <div class="option-row">
                    <div class="option-left">
                        <span class="option-name">Verbose council logging</span>
                        <span class="option-desc">Log all inter-agent messages to output</span>
                    </div>
                    <div class="toggle on" onclick="this.classList.toggle('on');this.classList.toggle('off');">
                        <div class="toggle-knob"></div>
                    </div>
                </div>
            </div>

            <div class="launch-row">
                <span class="launch-info">Estimated: ~6 agents • 3-5 waves</span>
                <button class="btn-launch" disabled title="Coming soon — use CLI: dare run &lt;task.yaml&gt;">
                    <i data-lucide="zap" style="width:16px;height:16px;"></i>
                    LAUNCH COUNCIL
                </button>
            </div>
        </div>
    </div>"#
    );

    Html(render_page("New Council", &body))
}

// ===== HTMX Fragment Handlers =====

async fn htmx_stats(State(state): State<Arc<AppState>>) -> Html<String> {
    let runs = state.db.list_runs(100).await.unwrap_or_default();
    let today_count = runs.iter().filter(|r| {
        r.created_at.date_naive() == chrono::Utc::now().date_naive()
    }).count();

    let mut total_tasks = 0;
    let mut completed_tasks = 0;
    let mut active_agents = 0;
    for run in &runs {
        if let Ok(tasks) = state.db.get_tasks(&run.id).await {
            total_tasks += tasks.len();
            completed_tasks += tasks.iter().filter(|t| t.status == TaskStatus::Completed).count();
            active_agents += tasks.iter().filter(|t| t.status == TaskStatus::Spawned || t.status == TaskStatus::Executing).count();
        }
    }
    let success_rate = if total_tasks > 0 {
        format!("{:.1}%", (completed_tasks as f64 / total_tasks as f64) * 100.0)
    } else {
        "—".to_string()
    };

    let html = format!(
        r#"<div class="metric-card">
            <span class="metric-label">COUNCILS TODAY</span>
            <span class="metric-value">{}</span>
        </div>
        <div class="metric-card">
            <span class="metric-label">ACTIVE AGENTS</span>
            <span class="metric-value">{}</span>
        </div>
        <div class="metric-card">
            <span class="metric-label">TASKS COMPLETED</span>
            <span class="metric-value">{}</span>
        </div>
        <div class="metric-card">
            <span class="metric-label">SUCCESS RATE</span>
            <span class="metric-value">{}</span>
        </div>"#,
        today_count, active_agents, completed_tasks, success_rate
    );

    Html(html)
}

async fn htmx_councils(State(state): State<Arc<AppState>>) -> Html<String> {
    // Reuse overview logic — for now return empty
    Html(String::new())
}

async fn htmx_agents(State(state): State<Arc<AppState>>) -> Html<String> {
    let runs = state.db.list_runs(5).await.unwrap_or_default();
    let mut html = String::new();

    for run in &runs {
        if let Ok(tasks) = state.db.get_tasks(&run.id).await {
            for task in tasks.iter().filter(|t| t.status != TaskStatus::Pending) {
                let (dot_color, status_text, status_color) = match task.status {
                    TaskStatus::Completed => ("var(--c-accent)", "done", "var(--c-accent)"),
                    TaskStatus::Failed => ("var(--c-danger)", "failed", "var(--c-danger)"),
                    TaskStatus::Spawned => ("var(--c-accent)", "spawned", "var(--c-accent)"),
                    TaskStatus::Executing => ("var(--c-accent)", "building", "var(--c-accent)"),
                    TaskStatus::Pending => ("var(--c-muted)", "idle", "var(--c-muted)"),
                };
                html.push_str(&format!(
                    r#"<div class="agent-row">
                        <div class="agent-dot" style="background:{}"></div>
                        <span class="agent-name">{}</span>
                        <span class="agent-status" style="color:{}">{}</span>
                    </div>"#,
                    dot_color, task.id, status_color, status_text
                ));
            }
        }
    }

    if html.is_empty() {
        html = r#"<span style="font-size:10px;color:#3f3f3f;">No agents active</span>"#.to_string();
    }

    Html(html)
}

async fn htmx_feed(State(_state): State<Arc<AppState>>) -> Html<String> {
    Html(String::new())
}

async fn htmx_waves(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    let tasks = state.db.get_tasks(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let max_wave = tasks.iter().map(|t| t.wave).max().unwrap_or(0);
    Ok(Html(build_task_graph_html(&tasks, max_wave)))
}

async fn htmx_messages(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    // Return messages HTML — simplified version of council page messages
    let tasks = state.db.get_tasks(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let mut html = String::new();
    for task in &tasks {
        if task.status == TaskStatus::Pending { continue; }
        let avatar_class = if task.description.to_lowercase().contains("test") { "tester" }
            else if task.description.to_lowercase().contains("review") { "reviewer" }
            else { "coder" };
        let initials = task.id.chars().take(2).collect::<String>().to_uppercase();
        let time_str = task.started_at
            .map(|t| t.format("%H:%M").to_string())
            .unwrap_or_else(|| "—".to_string());

        html.push_str(&format!(
            r#"<div class="msg">
                <div class="msg-avatar {}">{}</div>
                <div class="msg-body">
                    <div class="msg-header">
                        <span class="msg-name {}">{}</span>
                        <span class="msg-time">{}</span>
                    </div>
                    <div class="msg-text">{}</div>
                </div>
            </div>"#,
            avatar_class, initials, avatar_class, task.id, time_str, task.description
        ));

        if task.status == TaskStatus::Completed {
            let end_time = task.completed_at.map(|t| t.format("%H:%M").to_string()).unwrap_or_default();
            html.push_str(&format!(
                r#"<div class="msg">
                    <div class="msg-avatar {}">{}</div>
                    <div class="msg-body">
                        <div class="msg-header">
                            <span class="msg-name {}">{}</span>
                            <span class="msg-time">{}</span>
                            <span class="msg-tag decision">DONE</span>
                        </div>
                        <div class="msg-text">Task completed</div>
                    </div>
                </div>"#,
                avatar_class, initials, avatar_class, task.id, end_time
            ));
        }
    }

    Ok(Html(html))
}

// ===== Task Graph Builder =====

fn build_task_graph_html(tasks: &[crate::models::Task], max_wave: usize) -> String {
    let mut html = String::new();

    for wave_num in 0..=max_wave {
        let wave_tasks: Vec<_> = tasks.iter().filter(|t| t.wave == wave_num).collect();
        let all_done = wave_tasks.iter().all(|t| t.status == TaskStatus::Completed);
        let any_active = wave_tasks.iter().any(|t| t.status == TaskStatus::Spawned || t.status == TaskStatus::Executing);

        let label_class = if all_done || any_active { "active" } else { "pending" };
        let status_text = if all_done { "[DONE]" } else if any_active { "[ACTIVE]" } else { "[PENDING]" };

        html.push_str(&format!(
            r#"<div class="wave-col">
                <div class="wave-label">
                    <span class="wave-label-text {}">WAVE {:02}</span>
                    <span class="wave-label-status {}">{}</span>
                </div>"#,
            label_class, wave_num + 1, label_class, status_text
        ));

        for task in &wave_tasks {
            let (card_class, meta_class) = match task.status {
                TaskStatus::Completed => ("done", "done"),
                TaskStatus::Spawned | TaskStatus::Executing => ("active", "active"),
                TaskStatus::Failed => ("done", "done"), // Use border styling
                TaskStatus::Pending => ("", "waiting"),
            };

            let timing = if let (Some(s), Some(e)) = (task.started_at, task.completed_at) {
                format!("{} • {}s", task.id, e.signed_duration_since(s).num_seconds())
            } else if task.started_at.is_some() {
                format!("{} • in progress", task.id)
            } else {
                format!("{} • waiting", task.id)
            };

            let name_class = if task.status == TaskStatus::Pending { r#" class="task-name muted""# } else { r#" class="task-name""# };

            html.push_str(&format!(
                r#"<div class="task-card {}">
                    <span{}>{}</span>
                    <span class="task-meta {}">{}</span>
                </div>"#,
                card_class, name_class, task.description, meta_class, timing
            ));
        }

        html.push_str("</div>");
    }

    html
}

// ===== JSON API Handlers =====

async fn api_status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    let run = state.db.get_latest_run().await.ok().flatten();
    Json(StatusResponse {
        version: env!("CARGO_PKG_VERSION").to_string(),
        status: "ok".to_string(),
        current_run: run.map(|r| RunSummary {
            id: r.id, name: r.name,
            status: format!("{:?}", r.status),
            created_at: r.created_at.to_rfc3339(),
        }),
    })
}

async fn api_runs(State(state): State<Arc<AppState>>) -> Json<Vec<RunSummary>> {
    let runs = state.db.list_runs(20).await.unwrap_or_default();
    Json(runs.into_iter().map(|r| RunSummary {
        id: r.id, name: r.name,
        status: format!("{:?}", r.status),
        created_at: r.created_at.to_rfc3339(),
    }).collect())
}

async fn api_run(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<RunDetail>, StatusCode> {
    let run = state.db.get_run(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let tasks = state.db.get_tasks(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let waves = tasks.iter().map(|t| t.wave).max().map(|m| m + 1).unwrap_or(0);
    Ok(Json(RunDetail {
        id: run.id, name: run.name, description: run.description,
        status: format!("{:?}", run.status),
        task_count: tasks.len(), wave_count: waves,
        started_at: run.started_at.map(|t| t.to_rfc3339()),
        completed_at: run.completed_at.map(|t| t.to_rfc3339()),
    }))
}

async fn api_tasks(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Json<Vec<TaskSummary>>, StatusCode> {
    let tasks = state.db.get_tasks(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(tasks.into_iter().map(|t| TaskSummary {
        id: t.id, description: t.description,
        status: format!("{:?}", t.status),
        wave: t.wave, error: t.error,
        session_id: t.agent_session_id,
    }).collect()))
}

async fn api_pause(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    state.db.update_run_status(&id, RunStatus::Paused).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

async fn api_resume(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    state.db.update_run_status(&id, RunStatus::Running).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(StatusCode::OK)
}

async fn api_kill(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, StatusCode> {
    state.db.update_run_status(&id, RunStatus::Failed).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
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

            let html = match &task_msg {
                TaskMessage::Spawned { task_id, session_key } => {
                    let short = if session_key.len() > 16 { &session_key[..16] } else { session_key };
                    format!(
                        r#"<div class="feed-item"><span class="feed-text info">{} spawned ({}...)</span><span class="feed-time">{}</span></div>"#,
                        task_id, short, chrono::Local::now().format("%H:%M:%S")
                    )
                }
                TaskMessage::Running { task_id } => {
                    format!(
                        r#"<div class="feed-item"><span class="feed-text">{} executing</span><span class="feed-time">{}</span></div>"#,
                        task_id, chrono::Local::now().format("%H:%M:%S")
                    )
                }
                TaskMessage::Completed { task_id, summary } => {
                    format!(
                        r#"<div class="feed-item"><span class="feed-text">{} completed {}</span><span class="feed-time">{}</span></div>"#,
                        task_id, summary, chrono::Local::now().format("%H:%M:%S")
                    )
                }
                TaskMessage::Failed { task_id, error } => {
                    format!(
                        r#"<div class="feed-item"><span class="feed-text warning">{} failed: {}</span><span class="feed-time">{}</span></div>"#,
                        task_id, error, chrono::Local::now().format("%H:%M:%S")
                    )
                }
            };

            Some(Ok(Event::default().event(event_type).data(html)))
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
