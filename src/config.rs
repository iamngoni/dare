//! Configuration management for dare

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Main configuration structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub general: GeneralConfig,

    #[serde(default)]
    pub execution: ExecutionConfig,

    #[serde(default)]
    pub gateway: GatewayConfig,

    #[serde(default)]
    pub planning: PlanningConfig,

    #[serde(default)]
    pub dashboard: DashboardConfig,

    #[serde(default)]
    pub security: SecurityConfig,

    #[serde(default)]
    pub logging: LoggingConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    pub workspace: PathBuf,
    pub database: PathBuf,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            workspace: PathBuf::from("."),
            database: PathBuf::from(".dare/dare.db"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionConfig {
    pub max_parallel_agents: usize,
    pub wave_timeout_seconds: u64,
    pub task_timeout_seconds: u64,
    pub retry_failed_tasks: bool,
    pub max_retries: usize,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            max_parallel_agents: 4,
            wave_timeout_seconds: 300,
            task_timeout_seconds: 120,
            retry_failed_tasks: true,
            max_retries: 2,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayConfig {
    pub host: String,
    pub port: u16,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 18789,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanningConfig {
    pub auto_plan_model: String,
    pub complexity_estimation: bool,
}

impl Default for PlanningConfig {
    fn default() -> Self {
        Self {
            auto_plan_model: "claude-sonnet-4-20250514".to_string(),
            complexity_estimation: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardConfig {
    pub enabled: bool,
    pub port: u16,
    pub open_browser: bool,
}

impl Default for DashboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            port: 8765,
            open_browser: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    pub max_agents_per_run: usize,
    pub max_concurrent_runs: usize,
    pub max_run_duration_minutes: u64,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            max_agents_per_run: 10,
            max_concurrent_runs: 3,
            max_run_duration_minutes: 60,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub file: Option<PathBuf>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            file: Some(PathBuf::from(".dare/dare.log")),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            execution: ExecutionConfig::default(),
            gateway: GatewayConfig::default(),
            planning: PlanningConfig::default(),
            dashboard: DashboardConfig::default(),
            security: SecurityConfig::default(),
            logging: LoggingConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration from dare.toml, falling back to defaults
    pub fn load() -> Result<Self> {
        let config_path = Path::new("dare.toml");

        if config_path.exists() {
            let content = std::fs::read_to_string(config_path)
                .context("Failed to read dare.toml")?;
            let config: Config = toml::from_str(&content)
                .context("Failed to parse dare.toml")?;
            Ok(config)
        } else {
            // Try home directory
            if let Some(home) = dirs::home_dir() {
                let global_config = home.join(".config/dare/config.toml");
                if global_config.exists() {
                    let content = std::fs::read_to_string(&global_config)?;
                    let config: Config = toml::from_str(&content)?;
                    return Ok(config);
                }
            }

            Ok(Config::default())
        }
    }

    /// Get absolute database path
    pub fn database_path(&self) -> PathBuf {
        if self.general.database.is_absolute() {
            self.general.database.clone()
        } else {
            self.general.workspace.join(&self.general.database)
        }
    }
}

// Add dirs as a dependency for home directory resolution
fn dirs_home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

mod dirs {
    use super::*;
    pub fn home_dir() -> Option<PathBuf> {
        dirs_home_dir()
    }
}
