//! CLI interface for dare

pub mod config;
pub mod control;
pub mod dashboard;
pub mod init;
pub mod logs;
pub mod plan;
pub mod run;
pub mod status;

use clap::{Parser, Subcommand};
use std::path::PathBuf;

/// dare.run - Multi-agent orchestration for OpenClaw/Claude agents
#[derive(Parser)]
#[command(name = "dare")]
#[command(author = "Ngonidzashe Mangudya <hi@iamngoni.dev>")]
#[command(version)]
#[command(about = "Multi-agent orchestration for OpenClaw/Claude agents")]
#[command(long_about = "
dare.run transforms complex tasks into coordinated agent swarms.
Instead of one AI assistant working sequentially, dare decomposes tasks
into dependency graphs and executes them through parallel worker agents.

Examples:
  dare run task.yaml              Run tasks from file
  dare run --auto \"Build a REST API\"  Auto-plan and run
  dare status --watch             Live TUI status
  dare dashboard                  Open web dashboard
")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize dare in the current directory
    Init,

    /// Run a task file or auto-planned task
    Run(RunArgs),

    /// Preview or generate execution plan
    Plan(PlanArgs),

    /// Show status of current or specific run
    Status(StatusArgs),

    /// View logs
    Logs(LogsArgs),

    /// Pause execution after current wave
    Pause,

    /// Resume paused execution
    Resume,

    /// Kill agents and abort run
    Kill(KillArgs),

    /// Open web dashboard
    Dashboard(DashboardArgs),

    /// Show current configuration
    Config,
}

#[derive(Parser)]
pub struct RunArgs {
    /// Task file (YAML) or run ID to continue
    pub file: Option<PathBuf>,

    /// Auto-plan from description instead of file
    #[arg(long)]
    pub auto: Option<String>,

    /// Continue an interrupted run
    #[arg(long, name = "RUN_ID")]
    pub continue_run: Option<String>,

    /// Don't open dashboard automatically
    #[arg(long)]
    pub no_dashboard: bool,

    /// Override max parallel agents
    #[arg(long)]
    pub max_parallel: Option<usize>,
}

#[derive(Parser)]
pub struct PlanArgs {
    /// Task file (YAML) to plan
    pub file: Option<PathBuf>,

    /// Auto-generate plan from description
    #[arg(long)]
    pub auto: Option<String>,

    /// Validate plan without showing
    #[arg(long)]
    pub validate: bool,

    /// Output format
    #[arg(long, default_value = "pretty")]
    pub format: PlanFormat,
}

#[derive(Clone, clap::ValueEnum)]
pub enum PlanFormat {
    Pretty,
    Yaml,
    Json,
}

#[derive(Parser)]
pub struct StatusArgs {
    /// Run ID (defaults to current run)
    pub run_id: Option<String>,

    /// Watch mode (live TUI)
    #[arg(short, long)]
    pub watch: bool,
}

#[derive(Parser)]
pub struct LogsArgs {
    /// Run ID (defaults to current run)
    pub run_id: Option<String>,

    /// Filter by task ID
    #[arg(long)]
    pub task: Option<String>,

    /// Follow logs (stream)
    #[arg(short, long)]
    pub follow: bool,

    /// Number of lines to show
    #[arg(short = 'n', long, default_value = "100")]
    pub lines: usize,

    /// Log level filter
    #[arg(long)]
    pub level: Option<String>,
}

#[derive(Parser)]
pub struct KillArgs {
    /// Kill specific task instead of entire run
    #[arg(long)]
    pub task: Option<String>,

    /// Force kill without confirmation
    #[arg(short, long)]
    pub force: bool,
}

#[derive(Parser)]
pub struct DashboardArgs {
    /// Port to run dashboard on
    #[arg(short, long, default_value = "8765")]
    pub port: u16,

    /// Don't open browser automatically
    #[arg(long)]
    pub no_open: bool,
}
