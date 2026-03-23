//! ACP Connector - Bridge between messaging platforms and ACP Agent CLI

use acpbridge::{config, channel, message_bus, orchestrator, mcp_server};

use anyhow::Result;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    // 1. Load configuration
    let config_path = std::env::var("ACP_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("config.yaml"));

    info!(path = %config_path.display(), "Loading configuration");
    let config = config::Config::from_file(&config_path)?;

    if config.channels.is_empty() {
        error!("No channels configured.");
        std::process::exit(1);
    }

    info!(
        bot_count = config.bots.len(),
        channel_count = config.channels.len(),
        "Starting ACP Connector"
    );

    // 2. Create MessageBus
    let message_bus = Arc::new(message_bus::MessageBus::new());

    // 3. Create Orchestrator
    let mcp_port = config.mcp_server.as_ref().map(|m| m.port);
    let orchestrator = Arc::new(orchestrator::Orchestrator::new(
        config.clone(),
        message_bus.clone(),
        mcp_port,
    ));

    // 4. Start MCP Server if configured — bind eagerly so port conflicts fail at startup
    if let Some(mcp_config) = &config.mcp_server {
        let orch = orchestrator.clone();
        let listener = mcp_server::bind_mcp_server(mcp_config.port).await?;
        tokio::spawn(async move {
            if let Err(e) = mcp_server::serve_mcp(listener, orch).await {
                error!(error = %e, "MCP Server error");
            }
        });
    }

    // 5. Start all channels within a LocalSet
    // This is required because Bot uses spawn_local for ACP client tasks
    let local_set = tokio::task::LocalSet::new();

    local_set
        .run_until(async {
            let mut handles = Vec::new();

            for channel_config in &config.channels {
                let channel_name = channel_config.name.clone();

                // Create the platform-specific channel
                let channel: Arc<dyn channel::Channel> = match &channel_config.platform {
                    config::PlatformConfig::Telegram { telegram } => {
                        Arc::new(channel::TelegramChannel::new(
                            telegram.token.clone(),
                            channel_config.mention_only,
                        ))
                    }
                    config::PlatformConfig::Qq { qq } => Arc::new(channel::QqChannel::new(
                        qq.app_id.clone(),
                        qq.client_secret.clone(),
                        channel_config.mention_only,
                    )),
                    config::PlatformConfig::Lark { lark } => Arc::new(channel::LarkChannel::new(
                        lark.app_id.clone(),
                        lark.app_secret.clone(),
                        lark.base_url.clone(),
                        channel_config.mention_only,
                    )),
                    config::PlatformConfig::Wechat { wechat } => {
                        let wechat_config = channel::WeChatConfig::new(
                            wechat.bot_token.clone(),
                            Some(wechat.base_url.clone()),
                        );
                        Arc::new(channel::WeChatChannel::new(
                            wechat_config,
                            channel_config.mention_only,
                        ))
                    }
                    config::PlatformConfig::Discord { discord } => {
                        Arc::new(channel::DiscordChannel::new(
                            discord.bot_token.clone(),
                            discord.guild_ids.clone(),
                            channel_config.mention_only,
                        ))
                    }
                };

                let orch = orchestrator.clone();
                let mb = message_bus.clone();
                let name = channel_name.clone();

                let handle = tokio::spawn(async move {
                    if let Err(e) = channel.start(name, orch, mb).await {
                        error!(error = %e, channel = %channel_name, "Channel error");
                    }
                });
                handles.push(handle);
            }

            // 6. Start idle cleanup task
            let orch_cleanup = orchestrator.clone();
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    interval.tick().await;
                    orch_cleanup
                        .cleanup_idle(std::time::Duration::from_secs(30 * 60))
                        .await;
                }
            });

            // 7. Wait for all channels
            for handle in handles {
                if let Err(e) = handle.await {
                    error!(error = %e, "Channel task panicked");
                }
            }

            Ok(())
        })
        .await
}

fn init_logging() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,acpconnector=debug"));

    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .with_thread_ids(true)
        .with_file(true)
        .with_line_number(true)
        .init();
}
