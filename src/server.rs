//! Web dashboard server
//!
//! HTMX + SSE real-time dashboard for monitoring dare.run executions.
//! Design based on Penpot mockups: dark terminal aesthetic with
//! JetBrains Mono + Space Grotesk, #00FF88 accent, Lucide icons.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{
        sse::{Event, Sse},
        Html, IntoResponse, Redirect,
    },
    routing::{get, post},
    Form, Json, Router,
};
use serde::{Deserialize, Serialize};
use std::convert::Infallible;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::CorsLayer;

use crate::config::Config;
use crate::dag::{TaskGraph, TaskNode};
use crate::db::Database;
use crate::executor::WaveExecutor;
use crate::message_bus::TaskMessage;
use crate::models::{Run, RunStatus, TaskStatus};

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

        let config = self.config.clone();
        let state = Arc::new(AppState { db, events_tx, config });

        let app = Router::new()
            // Pages
            .route("/", get(page_overview))
            .route("/history", get(page_history))
            .route("/agents", get(page_agents))
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
            
            // Actions (form submissions)
            .route("/action/launch", post(action_launch_council))
            .route("/action/message/:id", post(action_send_message))
            
            // Agent messaging API (used by spawned agents to communicate)
            .route("/api/council/:id/post", post(api_agent_post_message))
            .route("/api/council/:id/messages", get(api_agent_get_messages))
            
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
    config: Config,
}

// ===== Template helpers =====

fn render_page(title: &str, body: &str) -> String {
    BASE_HTML
        .replace("{{TITLE}}", title)
        .replace("{{BODY}}", body)
}

fn sidebar(active: &str, latest_run_id: Option<&str>) -> String {
    let council_href = latest_run_id.map(|id| format!("/council/{}", id)).unwrap_or_else(|| "#".to_string());
    let tasks_href = latest_run_id.map(|id| format!("/run/{}", id)).unwrap_or_else(|| "#".to_string());

    let items = vec![
        ("layout-dashboard", "OVERVIEW", "/".to_string()),
        ("message-square", "COUNCIL", council_href),
        ("git-branch", "TASKS", tasks_href),
        ("users", "AGENTS", "/agents".to_string()),
        ("archive", "HISTORY", "/history".to_string()),
    ];

    let mut nav_html = String::new();
    for (icon, label, href) in &items {
        let active_class = if *label == active.to_uppercase() { " active" } else { "" };
        nav_html.push_str(&format!(
            r##"<a href="{}" class="nav-item{}"><i data-lucide="{}"></i>{}</a>"##,
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

fn time_ago(dt: chrono::DateTime<chrono::Utc>) -> String {
    let dur = chrono::Utc::now().signed_duration_since(dt);
    if dur.num_days() > 0 { format!("{}d ago", dur.num_days()) }
    else if dur.num_hours() > 0 { format!("{}h ago", dur.num_hours()) }
    else if dur.num_minutes() > 0 { format!("{}m ago", dur.num_minutes()) }
    else { format!("{}s ago", dur.num_seconds()) }
}

fn elapsed_str(run: &Run) -> String {
    if let Some(start) = run.started_at {
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
    }
}

fn agent_role(desc: &str) -> &'static str {
    let d = desc.to_lowercase();
    if d.contains("test") { "tester" }
    else if d.contains("review") { "reviewer" }
    else if d.contains("plan") || d.contains("spec") || d.contains("schema") { "planner" }
    else { "coder" }
}

async fn get_latest_run_id(db: &Database) -> Option<String> {
    db.get_latest_run().await.ok().flatten().map(|r| r.id)
}

// ===== Page: Overview (Command Center) =====

async fn page_overview(State(state): State<Arc<AppState>>) -> Html<String> {
    let runs = state.db.list_runs(20).await.unwrap_or_default();
    let active_count = runs.iter().filter(|r| r.status == RunStatus::Running).count();
    let latest_id = runs.first().map(|r| r.id.as_str());

    // Build councils list
    let mut councils_html = String::new();
    if runs.is_empty() {
        councils_html.push_str(r##"<div style="text-align:center;padding:40px 0;color:var(--c-muted);">
            <p>No councils yet</p>
            <p style="font-size:10px;margin-top:8px;color:#3f3f3f;">
                <a href="/new" style="color:var(--c-accent);">Launch your first council</a> or use CLI: dare run &lt;task.yaml&gt;
            </p>
        </div>"##);
    }
    for run in runs.iter().take(10) {
        let tasks = state.db.get_tasks(&run.id).await.unwrap_or_default();
        let max_wave = tasks.iter().map(|t| t.wave).max().unwrap_or(0) + 1;
        let current_wave = tasks.iter()
            .filter(|t| t.status == TaskStatus::Spawned || t.status == TaskStatus::Executing)
            .map(|t| t.wave + 1).max().unwrap_or(0);
        let running_agents = tasks.iter()
            .filter(|t| t.status == TaskStatus::Spawned || t.status == TaskStatus::Executing).count();
        let elapsed = elapsed_str(run);

        let dot_class = if run.status == RunStatus::Failed { "failed" } else { "active" };
        let run_badge = badge_html(&format!("{:?}", run.status).to_lowercase());
        let meta = format!("Wave {}/{} · {} agents · {}", current_wave, max_wave, running_agents, elapsed);

        councils_html.push_str(&format!(
            r##"<a href="/council/{}" class="council-row">
                <div class="council-dot {}"></div>
                <div class="council-info">
                    <span class="council-name">{}</span>
                    <span class="council-meta">{}</span>
                </div>
                {}
            </a>"##,
            run.id, dot_class, run.name, meta, run_badge
        ));
    }

    // Build feed
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
                let ta = task.completed_at.or(task.started_at).map(time_ago).unwrap_or_else(|| "—".to_string());
                feed_html.push_str(&format!(
                    r##"<div class="feed-item"><span class="feed-text {}">{}</span><span class="feed-time">{}</span></div>"##,
                    class, text, ta
                ));
            }
        }
    }
    if feed_html.is_empty() {
        feed_html = r##"<div class="feed-item"><span class="feed-text" style="color:#3f3f3f;">No activity yet</span></div>"##.to_string();
    }

    let body = format!(
        r##"{}
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
                        <div class="chat-live"><div class="chat-live-dot live-dot"></div><span class="chat-live-text">LIVE</span></div>
                    </div>
                    <div id="feed-list" style="display:flex;flex-direction:column;gap:12px;"
                         sse-swap="spawned,running,completed,failed" hx-swap="afterbegin">
                        {}
                    </div>
                </div>
            </div>
        </div>
    </div>"##,
        sidebar("overview", latest_id),
        active_count,
        active_count,
        councils_html,
        feed_html
    );

    Html(render_page("Command Center", &body))
}

// ===== Page: History =====

async fn page_history(State(state): State<Arc<AppState>>) -> Html<String> {
    let runs = state.db.list_runs(100).await.unwrap_or_default();
    let latest_id = get_latest_run_id(&state.db).await;

    let mut rows_html = String::new();
    for run in &runs {
        let tasks = state.db.get_tasks(&run.id).await.unwrap_or_default();
        let completed = tasks.iter().filter(|t| t.status == TaskStatus::Completed).count();
        let total = tasks.len();
        let max_wave = tasks.iter().map(|t| t.wave).max().unwrap_or(0) + 1;
        let elapsed = elapsed_str(run);
        let run_badge = badge_html(&format!("{:?}", run.status).to_lowercase());
        let created = run.created_at.format("%Y-%m-%d %H:%M").to_string();

        rows_html.push_str(&format!(
            r##"<a href="/council/{}" class="council-row">
                <div class="council-info" style="flex:2;">
                    <span class="council-name">{}</span>
                    <span class="council-meta">{}</span>
                </div>
                <span style="flex:1;font-size:10px;color:var(--c-muted);">{} waves · {}/{} tasks</span>
                <span style="flex:1;font-size:10px;color:var(--c-muted);">{}</span>
                {}
            </a>"##,
            run.id, run.name, created, max_wave, completed, total, elapsed, run_badge
        ));
    }

    if rows_html.is_empty() {
        rows_html = r##"<div style="text-align:center;padding:60px 0;color:#3f3f3f;">
            <p>No runs yet</p>
            <p style="font-size:10px;margin-top:8px;"><a href="/new" style="color:var(--c-accent);">Launch your first council</a></p>
        </div>"##.to_string();
    }

    let body = format!(
        r##"{}
    <div class="main-content">
        <div class="header-row">
            <div>
                <h1 class="header-title">History</h1>
                <div class="header-sub">// ALL COUNCIL RUNS</div>
            </div>
            <div class="header-right">
                <a href="/new" class="btn-primary">
                    <i data-lucide="plus" style="width:14px;height:14px;"></i>
                    NEW COUNCIL
                </a>
            </div>
        </div>

        <div class="panel" style="flex:1;">
            <div class="panel-header">
                <span class="panel-title">RUNS</span>
                <span class="panel-count">{} total</span>
            </div>
            <div style="display:flex;flex-direction:column;gap:8px;">
                {}
            </div>
        </div>
    </div>"##,
        sidebar("history", latest_id.as_deref()),
        runs.len(),
        rows_html
    );

    Html(render_page("History", &body))
}

// ===== Page: Agents =====

async fn page_agents(State(state): State<Arc<AppState>>) -> Html<String> {
    let runs = state.db.list_runs(20).await.unwrap_or_default();
    let latest_id = get_latest_run_id(&state.db).await;

    let mut agents_html = String::new();
    let mut total_agents = 0;
    let mut active_agents = 0;

    for run in &runs {
        if let Ok(tasks) = state.db.get_tasks(&run.id).await {
            let run_has_active = tasks.iter().any(|t| t.status == TaskStatus::Spawned || t.status == TaskStatus::Executing);
            if !run_has_active && run.status != RunStatus::Running { continue; }

            agents_html.push_str(&format!(
                r##"<div style="margin-bottom:16px;">
                    <div style="display:flex;align-items:center;gap:8px;margin-bottom:8px;">
                        <span class="panel-title">{}</span>
                        {}
                    </div>"##,
                run.name, badge_html(&format!("{:?}", run.status).to_lowercase())
            ));

            for task in &tasks {
                if task.status == TaskStatus::Pending { continue; }
                total_agents += 1;
                let is_active = task.status == TaskStatus::Spawned || task.status == TaskStatus::Executing;
                if is_active { active_agents += 1; }

                let (dot_color, status_text) = match task.status {
                    TaskStatus::Completed => ("var(--c-accent)", "done"),
                    TaskStatus::Failed => ("var(--c-danger)", "failed"),
                    TaskStatus::Spawned => ("var(--c-accent)", "spawned"),
                    TaskStatus::Executing => ("var(--c-accent)", "building"),
                    TaskStatus::Pending => ("var(--c-muted)", "idle"),
                };
                let role = agent_role(&task.description);
                let session_info = task.agent_session_id.as_deref()
                    .map(|s| if s.len() > 12 { format!("{}...", &s[..12]) } else { s.to_string() })
                    .unwrap_or_else(|| "—".to_string());
                let timing = if let (Some(s), Some(e)) = (task.started_at, task.completed_at) {
                    format!("{}s", e.signed_duration_since(s).num_seconds())
                } else if task.started_at.is_some() {
                    "running...".to_string()
                } else {
                    "—".to_string()
                };

                agents_html.push_str(&format!(
                    r##"<div class="council-row" style="gap:16px;">
                        <div class="agent-dot" style="background:{}"></div>
                        <div style="flex:2;display:flex;flex-direction:column;gap:2px;">
                            <span style="font-size:12px;font-weight:600;">{}</span>
                            <span style="font-size:9px;color:#3f3f3f;">{}</span>
                        </div>
                        <span style="flex:1;font-size:10px;color:var(--c-muted);">{}</span>
                        <span style="flex:1;font-size:10px;color:var(--c-muted);">{}</span>
                        <span class="badge {}">[{}]</span>
                    </div>"##,
                    dot_color, task.id, session_info, role, timing,
                    if is_active { "badge-active" } else if task.status == TaskStatus::Completed { "badge-done" } else { "badge-failed" },
                    status_text.to_uppercase()
                ));
            }

            agents_html.push_str("</div>");
        }
    }

    if agents_html.is_empty() {
        agents_html = r##"<div style="text-align:center;padding:60px 0;color:#3f3f3f;">
            <p>No agents have been spawned yet</p>
            <p style="font-size:10px;margin-top:8px;"><a href="/new" style="color:var(--c-accent);">Launch a council to see agents</a></p>
        </div>"##.to_string();
    }

    let body = format!(
        r##"{}
    <div class="main-content">
        <div class="header-row">
            <div>
                <h1 class="header-title">Agents</h1>
                <div class="header-sub">// {} TOTAL · {} ACTIVE</div>
            </div>
        </div>
        <div class="panel" style="flex:1;">
            {}
        </div>
    </div>"##,
        sidebar("agents", latest_id.as_deref()),
        total_agents,
        active_agents,
        agents_html
    );

    Html(render_page("Agents", &body))
}

// ===== Page: Council Chat =====

async fn page_council(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    let run = state.db.get_run(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let tasks = state.db.get_tasks(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Get stored messages
    let messages = state.db.get_messages(&id, 100).await.unwrap_or_default();

    let max_wave = tasks.iter().map(|t| t.wave).max().unwrap_or(0) + 1;
    let current_wave = tasks.iter()
        .filter(|t| t.status == TaskStatus::Spawned || t.status == TaskStatus::Executing)
        .map(|t| t.wave + 1).max().unwrap_or(max_wave);
    let total_agents = tasks.len();
    let run_badge = badge_html(&format!("{:?}", run.status).to_lowercase());

    // Build participants
    let mut participants_html = String::new();
    for task in &tasks {
        let role = agent_role(&task.description);
        let dot_color = match task.status {
            TaskStatus::Completed => "var(--c-accent)",
            TaskStatus::Failed => "var(--c-danger)",
            TaskStatus::Spawned | TaskStatus::Executing => "var(--c-accent)",
            TaskStatus::Pending => "var(--c-muted)",
        };
        participants_html.push_str(&format!(
            r##"<div class="participant">
                <div class="participant-dot" style="background:{}"></div>
                <span class="participant-name">{}</span>
                <span class="participant-role">{}</span>
            </div>"##,
            dot_color, task.id, role
        ));
    }

    // Build messages from stored messages + task lifecycle
    let mut messages_html = build_council_messages(&tasks, &messages);
    if messages_html.is_empty() {
        messages_html = r##"<div style="text-align:center;padding:60px 0;color:#3f3f3f;">
            <p>Council is assembling...</p>
            <p style="font-size:10px;margin-top:8px;">Messages will appear here as agents communicate</p>
        </div>"##.to_string();
    }

    // Recent decisions
    let mut decisions_html = String::new();
    for task in tasks.iter().filter(|t| t.status == TaskStatus::Completed).rev().take(3) {
        let ta = task.completed_at.map(time_ago).unwrap_or_else(|| "—".to_string());
        decisions_html.push_str(&format!(
            r##"<div class="decision-card">
                <span class="decision-text">Completed: {}</span>
                <span class="decision-by">by {} · {}</span>
            </div>"##,
            task.description, task.id, ta
        ));
    }

    let is_running = run.status == RunStatus::Running;
    let input_disabled = if is_running { "" } else { " disabled" };
    let input_placeholder = if is_running {
        "Drop a message into the council..."
    } else {
        "Council is not active"
    };

    let body = format!(
        r##"{}
    <div class="chat-main" hx-ext="sse" sse-connect="/events">
        <div class="chat-header">
            <div class="chat-header-left">
                <a href="/" style="color:var(--c-muted);font-size:12px;"><i data-lucide="arrow-left" style="width:14px;height:14px;"></i></a>
                <span class="chat-header-title">{}</span>
                {}
            </div>
            <div class="chat-header-right">
                <span class="chat-agents">{} AGENTS</span>
                <div class="chat-live"><div class="chat-live-dot live-dot"></div><span class="chat-live-text">LIVE</span></div>
                <a href="/run/{}" class="btn-outline">
                    <i data-lucide="git-branch" style="width:14px;height:14px;"></i>
                    TASKS
                </a>
            </div>
        </div>

        <div style="display:flex;flex:1;min-height:0;">
            <div class="messages-area" id="messages-area"
                 hx-get="/htmx/run/{}/messages" hx-trigger="every 3s" hx-swap="innerHTML">
                {}
            </div>
            <div class="context-panel">
                <span class="ctx-title">COUNCIL CONTEXT</span>
                <div class="ctx-info">
                    <div class="ctx-row"><span class="ctx-label">STATUS</span><span class="ctx-value" style="color:var(--c-accent);">{}</span></div>
                    <div class="ctx-row"><span class="ctx-label">WAVE</span><span class="ctx-value">{} of {}</span></div>
                    <div class="ctx-row"><span class="ctx-label">ELAPSED</span><span class="ctx-value">{}</span></div>
                </div>
                <div class="ctx-divider"></div>
                <span class="ctx-title">PARTICIPANTS</span>
                {}
                <div class="ctx-divider"></div>
                <span class="ctx-title">RECENT DECISIONS</span>
                {}
            </div>
        </div>

        <form class="chat-input" hx-post="/action/message/{}" hx-swap="none" hx-on::after-request="this.querySelector('input').value=''">
            <span class="input-tag">YOU</span>
            <input type="text" name="message" placeholder="{}"{} autocomplete="off">
            <button type="submit" class="btn-outline" style="padding:4px 10px;"{}>
                <i data-lucide="send" style="width:14px;height:14px;"></i>
            </button>
        </form>
    </div>"##,
        sidebar("council", Some(&id)),
        run.name,
        run_badge,
        total_agents,
        id, id,
        messages_html,
        format!("[{:?}]", run.status).to_uppercase(),
        current_wave, max_wave,
        elapsed_str(&run),
        participants_html,
        decisions_html,
        id,
        input_placeholder,
        input_disabled,
        input_disabled
    );

    Ok(Html(render_page(&format!("Council — {}", run.name), &body)))
}

// ===== Page: Task Graph =====

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
    let total = tasks.len();

    // Wave progress segments
    let mut wave_progress = String::new();
    for w in 0..=max_wave {
        let wave_tasks: Vec<_> = tasks.iter().filter(|t| t.wave == w).collect();
        let all_done = wave_tasks.iter().all(|t| t.status == TaskStatus::Completed);
        let seg_class = if all_done { "done" } else { "pending" };
        wave_progress.push_str(&format!(r#"<div class="wave-progress-seg {}"></div>"#, seg_class));
    }

    let waves_html = build_task_graph_html(&tasks, max_wave);
    let run_badge = badge_html(&format!("{:?}", run.status).to_lowercase());
    let is_running = run.status == RunStatus::Running;

    let action_buttons = if is_running {
        format!(
            r##"<button hx-post="/api/runs/{}/pause" hx-swap="none" class="btn-outline" hx-on::after-request="location.reload()">
                <i data-lucide="pause" style="width:14px;height:14px;"></i> PAUSE
            </button>
            <button hx-post="/api/runs/{}/kill" hx-swap="none" class="btn-outline" style="border-color:var(--c-danger);color:var(--c-danger);" hx-confirm="Kill this run?" hx-on::after-request="location.reload()">
                <i data-lucide="x" style="width:14px;height:14px;"></i> KILL
            </button>"##,
            id, id
        )
    } else if run.status == RunStatus::Paused {
        format!(
            r##"<button hx-post="/api/runs/{}/resume" hx-swap="none" class="btn-primary" hx-on::after-request="location.reload()">
                <i data-lucide="play" style="width:14px;height:14px;"></i> RESUME
            </button>"##,
            id
        )
    } else {
        String::new()
    };

    let body = format!(
        r##"{}
    <div class="main-content" hx-ext="sse" sse-connect="/events">
        <div class="header-row">
            <div>
                <h1 class="header-title">Task Graph</h1>
                <div class="header-sub">// {} · {} WAVES · {} TASKS</div>
            </div>
            <div class="header-right">
                <a href="/" class="btn-outline">
                    <i data-lucide="arrow-left" style="width:14px;height:14px;"></i> BACK
                </a>
                <a href="/council/{}" class="btn-outline">
                    <i data-lucide="message-square" style="width:14px;height:14px;"></i> COUNCIL
                </a>
                {}
                {}
            </div>
        </div>

        <div class="wave-progress">{}</div>

        <div id="waves-container" hx-get="/htmx/run/{}/waves" hx-trigger="every 5s" hx-swap="innerHTML"
             class="waves-row" style="flex:1;">
            {}
        </div>
    </div>"##,
        sidebar("tasks", Some(&id)),
        run.name.to_uppercase(),
        max_wave + 1,
        total,
        id,
        run_badge,
        action_buttons,
        wave_progress,
        id,
        waves_html
    );

    Ok(Html(render_page(&format!("Tasks — {}", run.name), &body)))
}

async fn page_tasks(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
) -> Result<Html<String>, StatusCode> {
    page_run(State(state), Path(id)).await
}

// ===== Page: New Council =====

async fn page_new_council(State(state): State<Arc<AppState>>) -> Html<String> {
    let latest_id = get_latest_run_id(&state.db).await;

    let body = format!(
        r##"{}
    <div class="main-content">
        <div class="form-center">
            <div class="form-title-block">
                <h1 class="form-title">New Council</h1>
                <div class="form-sub">// DESCRIBE YOUR GOAL AND THE COUNCIL WILL FIGURE OUT THE REST</div>
            </div>

            <form hx-post="/action/launch" hx-target="#launch-result" hx-swap="innerHTML" hx-indicator="#launch-spinner">
                <div class="form-section">
                    <span class="form-label">GOAL</span>
                    <textarea class="form-textarea" name="goal" placeholder="Refactor the authentication system to use JWT tokens with&#10;refresh token rotation. Extract middleware to shared module.&#10;Add comprehensive tests." required></textarea>
                    <span class="form-hint">PRD, feature spec, or just a sentence. The planner will decompose it.</span>
                </div>

                <div class="form-row" style="margin-top:24px;">
                    <div class="form-field">
                        <span class="form-label">MODEL</span>
                        <select class="form-select" name="model">
                            <option value="claude-opus-4">claude-opus-4</option>
                            <option value="claude-sonnet-4" selected>claude-sonnet-4</option>
                            <option value="gpt-4o">gpt-4o</option>
                        </select>
                    </div>
                    <div class="form-field">
                        <span class="form-label">MAX AGENTS</span>
                        <select class="form-select" name="max_agents">
                            <option value="4">4 agents</option>
                            <option value="6" selected>6 agents</option>
                            <option value="8">8 agents</option>
                            <option value="12">12 agents</option>
                        </select>
                    </div>
                </div>

                <div class="form-section" style="gap:16px;margin-top:24px;">
                    <span class="form-label">OPTIONS</span>
                    <div class="option-row">
                        <div class="option-left">
                            <span class="option-name">Auto-resolve conflicts</span>
                            <span class="option-desc">Let the orchestrator break deadlocks automatically</span>
                        </div>
                        <div class="toggle on" onclick="this.classList.toggle('on');this.classList.toggle('off');this.querySelector('input').value=this.classList.contains('on')?'true':'false';">
                            <input type="hidden" name="auto_resolve" value="true">
                            <div class="toggle-knob"></div>
                        </div>
                    </div>
                    <div class="option-row">
                        <div class="option-left">
                            <span class="option-name">Human approval gates</span>
                            <span class="option-desc">Pause at wave boundaries for your review</span>
                        </div>
                        <div class="toggle off" onclick="this.classList.toggle('on');this.classList.toggle('off');this.querySelector('input').value=this.classList.contains('on')?'true':'false';">
                            <input type="hidden" name="human_approval" value="false">
                            <div class="toggle-knob"></div>
                        </div>
                    </div>
                    <div class="option-row">
                        <div class="option-left">
                            <span class="option-name">Verbose council logging</span>
                            <span class="option-desc">Log all inter-agent messages to output</span>
                        </div>
                        <div class="toggle on" onclick="this.classList.toggle('on');this.classList.toggle('off');this.querySelector('input').value=this.classList.contains('on')?'true':'false';">
                            <input type="hidden" name="verbose" value="true">
                            <div class="toggle-knob"></div>
                        </div>
                    </div>
                </div>

                <div class="launch-row" style="margin-top:32px;">
                    <span class="launch-info" id="launch-spinner" style="display:none;">
                        <span class="live-dot" style="display:inline-block;width:8px;height:8px;border-radius:50%;background:var(--c-accent);"></span>
                        Planning...
                    </span>
                    <span class="launch-info" id="launch-est">Estimated: ~6 agents · 3-5 waves</span>
                    <button type="submit" class="btn-launch">
                        <i data-lucide="zap" style="width:16px;height:16px;"></i>
                        LAUNCH COUNCIL
                    </button>
                </div>
            </form>

            <div id="launch-result" style="margin-top:16px;"></div>
        </div>
    </div>"##,
        sidebar("overview", latest_id.as_deref())
    );

    Html(render_page("New Council", &body))
}

// ===== Action Handlers =====

#[derive(Deserialize)]
struct LaunchForm {
    goal: String,
    model: Option<String>,
    max_agents: Option<String>,
    auto_resolve: Option<String>,
    human_approval: Option<String>,
    verbose: Option<String>,
}

async fn action_launch_council(
    State(state): State<Arc<AppState>>,
    Form(form): Form<LaunchForm>,
) -> Html<String> {
    let goal = form.goal.trim().to_string();
    if goal.is_empty() {
        return Html(r##"<div style="color:var(--c-danger);font-size:12px;">Goal cannot be empty</div>"##.to_string());
    }

    // Use the auto-plan feature to decompose the goal
    let planner = crate::planner::Planner::new(&state.config);
    let graph = match planner.auto_plan(&goal).await {
        Ok(g) => g,
        Err(e) => {
            return Html(format!(
                r##"<div style="color:var(--c-danger);font-size:12px;">Planning failed: {}</div>"##,
                e
            ));
        }
    };

    let task_count = graph.nodes.len();
    let waves = graph.compute_waves();
    let wave_count = waves.len();

    // Create the run
    let mut run = Run::new(graph.name.as_deref().unwrap_or(&goal[..goal.len().min(60)]));
    run.description = Some(goal.clone());
    run.status = RunStatus::Running;
    run.started_at = Some(chrono::Utc::now());

    if let Err(e) = state.db.create_run(&run).await {
        return Html(format!(
            r##"<div style="color:var(--c-danger);font-size:12px;">Failed to create run: {}</div>"##, e
        ));
    }

    if let Err(e) = state.db.create_tasks_from_graph(&run.id, &graph).await {
        return Html(format!(
            r##"<div style="color:var(--c-danger);font-size:12px;">Failed to create tasks: {}</div>"##, e
        ));
    }

    // Spawn executor in background
    let db = state.db.clone();
    let config = state.config.clone();
    let events_tx = state.events_tx.clone();
    let run_id = run.id.clone();
    let run_clone = run.clone();
    let graph_clone = graph.clone();
    tokio::spawn(async move {
        let executor = WaveExecutor::with_events(&config, &db, events_tx);
        match executor.execute(&run_clone, &graph_clone).await {
            Ok(_) => {
                let _ = db.update_run_status(&run_id, RunStatus::Completed).await;
                tracing::info!(run_id = %run_id, "Run completed successfully");
            }
            Err(e) => {
                let _ = db.update_run_status(&run_id, RunStatus::Failed).await;
                tracing::error!(run_id = %run_id, error = %e, "Run failed");
            }
        }
    });

    // Return redirect to the council page
    Html(format!(
        r##"<div style="padding:12px;background:var(--c-accent-soft);border:1px solid var(--c-accent);">
            <span style="color:var(--c-accent);font-weight:700;font-size:12px;">COUNCIL LAUNCHED</span>
            <p style="font-size:11px;color:var(--c-muted);margin-top:4px;">{} tasks across {} waves</p>
            <a href="/council/{}" class="btn-primary" style="margin-top:12px;display:inline-flex;">
                <i data-lucide="message-square" style="width:14px;height:14px;"></i>
                OPEN COUNCIL
            </a>
        </div>"##,
        task_count, wave_count, run.id
    ))
}

#[derive(Deserialize)]
struct MessageForm {
    message: String,
}

async fn action_send_message(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Form(form): Form<MessageForm>,
) -> Result<Html<String>, StatusCode> {
    let msg = form.message.trim().to_string();
    if msg.is_empty() {
        return Ok(Html(String::new()));
    }

    // Store the message
    let payload = serde_json::json!({
        "sender": "human",
        "text": msg,
    });
    let _ = state.db.insert_message(&id, None, "human", &payload).await;

    // TODO: Forward to active agent sessions via gateway
    // For now, just store it so it shows up in the chat

    Ok(Html(String::new()))
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

    Html(format!(
        r##"<div class="metric-card">
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
        </div>"##,
        today_count, active_agents, completed_tasks, success_rate
    ))
}

async fn htmx_councils(State(_state): State<Arc<AppState>>) -> Html<String> {
    Html(String::new())
}

async fn htmx_agents(State(state): State<Arc<AppState>>) -> Html<String> {
    let runs = state.db.list_runs(5).await.unwrap_or_default();
    let mut html = String::new();

    for run in &runs {
        if let Ok(tasks) = state.db.get_tasks(&run.id).await {
            for task in tasks.iter().filter(|t| t.status != TaskStatus::Pending) {
                let (dot_color, status_text) = match task.status {
                    TaskStatus::Completed => ("var(--c-accent)", "done"),
                    TaskStatus::Failed => ("var(--c-danger)", "failed"),
                    TaskStatus::Spawned => ("var(--c-accent)", "spawned"),
                    TaskStatus::Executing => ("var(--c-accent)", "building"),
                    TaskStatus::Pending => ("var(--c-muted)", "idle"),
                };
                html.push_str(&format!(
                    r##"<div class="agent-row">
                        <div class="agent-dot" style="background:{}"></div>
                        <span class="agent-name">{}</span>
                        <span class="agent-status" style="color:{}">{}</span>
                    </div>"##,
                    dot_color, task.id, dot_color, status_text
                ));
            }
        }
    }

    if html.is_empty() {
        html = r##"<span style="font-size:10px;color:#3f3f3f;">No agents active</span>"##.to_string();
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
    let tasks = state.db.get_tasks(&id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let messages = state.db.get_messages(&id, 100).await.unwrap_or_default();
    Ok(Html(build_council_messages(&tasks, &messages)))
}

// ===== Message Builders =====

fn build_council_messages(
    tasks: &[crate::models::Task],
    stored_messages: &[(i64, Option<String>, String, serde_json::Value, chrono::DateTime<chrono::Utc>)],
) -> String {
    // Collect all events into a timeline
    let mut timeline: Vec<(chrono::DateTime<chrono::Utc>, String)> = Vec::new();

    // Task lifecycle messages
    for task in tasks {
        if task.status == TaskStatus::Pending { continue; }

        let role = agent_role(&task.description);
        let initials = task.id.chars().take(2).collect::<String>().to_uppercase();

        if let Some(start) = task.started_at {
            let time_str = start.format("%H:%M").to_string();
            let html = format!(
                r##"<div class="msg">
                    <div class="msg-avatar {}">{}</div>
                    <div class="msg-body">
                        <div class="msg-header">
                            <span class="msg-name {}">{}</span>
                            <span class="msg-time">{}</span>
                        </div>
                        <div class="msg-text">Picking up task: {}</div>
                    </div>
                </div>"##,
                role, initials, role, task.id, time_str, task.description
            );
            timeline.push((start, html));
        }

        if task.status == TaskStatus::Completed {
            if let Some(end) = task.completed_at {
                let time_str = end.format("%H:%M").to_string();
                let duration = task.started_at.map(|s| format!(" ({}s)", end.signed_duration_since(s).num_seconds())).unwrap_or_default();
                let html = format!(
                    r##"<div class="msg">
                        <div class="msg-avatar {}">{}</div>
                        <div class="msg-body">
                            <div class="msg-header">
                                <span class="msg-name {}">{}</span>
                                <span class="msg-time">{}</span>
                                <span class="msg-tag decision">DONE</span>
                            </div>
                            <div class="msg-text">Task completed{}</div>
                        </div>
                    </div>"##,
                    role, initials, role, task.id, time_str, duration
                );
                timeline.push((end, html));
            }
        }

        if task.status == TaskStatus::Failed {
            let err = task.error.as_deref().unwrap_or("Unknown error");
            let time = task.completed_at.or(task.started_at).unwrap_or_else(chrono::Utc::now);
            let time_str = time.format("%H:%M").to_string();
            let html = format!(
                r##"<div class="msg">
                    <div class="msg-avatar reviewer">{}</div>
                    <div class="msg-body">
                        <div class="msg-header">
                            <span class="msg-name reviewer">{}</span>
                            <span class="msg-time">{}</span>
                            <span class="msg-tag challenge">FAILED</span>
                        </div>
                        <div class="msg-text" style="color:var(--c-danger);">{}</div>
                    </div>
                </div>"##,
                initials, task.id, time_str, err
            );
            timeline.push((time, html));
        }
    }

    // Stored messages (human input, agent chat, system messages)
    for (_id, _task_id, msg_type, payload, created_at) in stored_messages.iter().rev() {
        let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let sender = payload.get("sender").and_then(|v| v.as_str()).unwrap_or("system");
        let tag_value = payload.get("tag").and_then(|v| v.as_str()).unwrap_or("");
        let time_str = created_at.format("%H:%M").to_string();

        let (avatar_class, name_style, avatar_content) = match msg_type.as_str() {
            "human" => ("system", "color:var(--c-accent);", "▶".to_string()),
            "agent" => {
                let role = agent_role(sender);
                let initials = sender.chars().take(2).collect::<String>().to_uppercase();
                (role, match role {
                    "planner" => "color:var(--c-accent);",
                    "reviewer" => "color:var(--c-warning);",
                    "tester" => "color:var(--c-info);",
                    _ => "color:var(--c-text);",
                }, initials)
            }
            _ => ("system", "color:var(--c-muted);", "⚙".to_string()),
        };

        let display_name = match msg_type.as_str() {
            "human" => "YOU".to_string(),
            "agent" => sender.to_string(),
            _ => "SYSTEM".to_string(),
        };

        let tag_html = match tag_value {
            "question" => r##"<span class="msg-tag question">QUESTION</span>"##,
            "decision" => r##"<span class="msg-tag decision">DECISION</span>"##,
            "challenge" => r##"<span class="msg-tag challenge">CHALLENGE</span>"##,
            "request" => r##"<span class="msg-tag question">REQUEST</span>"##,
            "update" => r##"<span class="msg-tag decision">UPDATE</span>"##,
            _ if msg_type == "human" => r##"<span class="msg-tag decision">HUMAN</span>"##,
            _ => "",
        };

        let html = format!(
            r##"<div class="msg">
                <div class="msg-avatar {}">{}</div>
                <div class="msg-body">
                    <div class="msg-header">
                        <span class="msg-name" style="{}">{}</span>
                        <span class="msg-time">{}</span>
                        {}
                    </div>
                    <div class="msg-text">{}</div>
                </div>
            </div>"##,
            avatar_class, avatar_content, name_style, display_name, time_str, tag_html, text
        );
        timeline.push((*created_at, html));
    }

    // Sort by time
    timeline.sort_by_key(|(t, _)| *t);
    timeline.into_iter().map(|(_, html)| html).collect::<Vec<_>>().join("\n")
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
            r##"<div class="wave-col">
                <div class="wave-label">
                    <span class="wave-label-text {}">WAVE {:02}</span>
                    <span class="wave-label-status {}">{}</span>
                </div>"##,
            label_class, wave_num + 1, label_class, status_text
        ));

        for task in &wave_tasks {
            let (card_class, meta_class) = match task.status {
                TaskStatus::Completed => ("done", "done"),
                TaskStatus::Spawned | TaskStatus::Executing => ("active", "active"),
                TaskStatus::Failed => ("", ""),
                TaskStatus::Pending => ("", "waiting"),
            };

            let timing = if let (Some(s), Some(e)) = (task.started_at, task.completed_at) {
                format!("{} · {}s", task.id, e.signed_duration_since(s).num_seconds())
            } else if task.started_at.is_some() {
                format!("{} · in progress", task.id)
            } else {
                format!("{} · waiting", task.id)
            };

            let name_class = if task.status == TaskStatus::Pending { " muted" } else { "" };
            let failed_style = if task.status == TaskStatus::Failed {
                r##" style="border-color:var(--c-danger);""##
            } else { "" };

            html.push_str(&format!(
                r##"<div class="task-card {}"{}>
                    <span class="task-name{}">{}</span>
                    <span class="task-meta {}">{}</span>
                </div>"##,
                card_class, failed_style, name_class, task.description, meta_class, timing
            ));
        }

        html.push_str("</div>");
    }

    html
}

// ===== Agent Messaging API =====
// These endpoints are called by spawned agents to communicate with each other

#[derive(Deserialize)]
struct AgentPostMessage {
    sender: String,
    text: String,
    #[serde(default)]
    tag: Option<String>, // "question", "decision", "challenge", "update", "request"
}

async fn api_agent_post_message(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
    Json(msg): Json<AgentPostMessage>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Verify run exists
    let _run = state.db.get_run(&run_id).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    let payload = serde_json::json!({
        "sender": msg.sender,
        "text": msg.text,
        "tag": msg.tag,
    });

    // Check if sender is a valid task_id in this run
    let task_ref = state.db.get_task(&msg.sender).await.ok().flatten();
    let task_id_ref = task_ref.as_ref().map(|t| t.id.as_str());
    
    let msg_id = state.db.insert_message(&run_id, task_id_ref, "agent", &payload).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Broadcast via SSE so the dashboard updates in real-time
    let feed_html = format!(
        r##"<div class="feed-item"><span class="feed-text info">{}: {}</span><span class="feed-time">{}</span></div>"##,
        msg.sender,
        if msg.text.len() > 60 { format!("{}...", &msg.text[..60]) } else { msg.text.clone() },
        chrono::Local::now().format("%H:%M:%S")
    );
    let _ = state.events_tx.send(TaskMessage::Running { task_id: format!("msg:{}", msg.sender) });

    Ok(Json(serde_json::json!({ "ok": true, "id": msg_id })))
}

#[derive(Deserialize)]
struct AgentGetMessagesQuery {
    #[serde(default = "default_limit")]
    limit: usize,
    #[serde(default)]
    since_id: Option<i64>,
}

fn default_limit() -> usize { 50 }

async fn api_agent_get_messages(
    State(state): State<Arc<AppState>>,
    Path(run_id): Path<String>,
    Query(params): Query<AgentGetMessagesQuery>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    let messages = state.db.get_messages(&run_id, params.limit).await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let result: Vec<serde_json::Value> = messages.iter()
        .rev() // Reverse to chronological order
        .filter(|(id, _, _, _, _)| {
            if let Some(since) = params.since_id {
                *id > since
            } else {
                true
            }
        })
        .map(|(id, task_id, msg_type, payload, created_at)| {
            serde_json::json!({
                "id": id,
                "task_id": task_id,
                "type": msg_type,
                "sender": payload.get("sender").and_then(|v| v.as_str()).unwrap_or("unknown"),
                "text": payload.get("text").and_then(|v| v.as_str()).unwrap_or(""),
                "tag": payload.get("tag").and_then(|v| v.as_str()),
                "timestamp": created_at.to_rfc3339(),
            })
        })
        .collect();

    Ok(Json(result))
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
                        r##"<div class="feed-item"><span class="feed-text info">{} spawned ({}...)</span><span class="feed-time">{}</span></div>"##,
                        task_id, short, chrono::Local::now().format("%H:%M:%S")
                    )
                }
                TaskMessage::Running { task_id } => {
                    format!(
                        r##"<div class="feed-item"><span class="feed-text">{} executing</span><span class="feed-time">{}</span></div>"##,
                        task_id, chrono::Local::now().format("%H:%M:%S")
                    )
                }
                TaskMessage::Completed { task_id, summary } => {
                    format!(
                        r##"<div class="feed-item"><span class="feed-text">{} completed {}</span><span class="feed-time">{}</span></div>"##,
                        task_id, summary, chrono::Local::now().format("%H:%M:%S")
                    )
                }
                TaskMessage::Failed { task_id, error } => {
                    format!(
                        r##"<div class="feed-item"><span class="feed-text warning">{} failed: {}</span><span class="feed-time">{}</span></div>"##,
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
