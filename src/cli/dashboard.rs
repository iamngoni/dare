//! `dare dashboard` command - Open web dashboard

use anyhow::Result;

use crate::config::Config;
use crate::server::DashboardServer;

use super::DashboardArgs;

/// Open web dashboard
pub async fn run(args: DashboardArgs, config: &Config) -> Result<()> {
    let port = args.port;
    let url = format!("http://localhost:{}", port);

    println!("🌐 Starting dare dashboard on {}", url);

    // Start server
    let server = DashboardServer::new(config, port);

    // Open browser unless disabled
    if !args.no_open && config.dashboard.open_browser {
        // Give server a moment to start
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            let _ = open::that(&url);
        });
    }

    println!("   Press Ctrl+C to stop");

    server.run().await?;

    Ok(())
}
