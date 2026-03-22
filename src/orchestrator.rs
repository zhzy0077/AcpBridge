//! Orchestrator - Bot factory, registry, and supervisor
//!
//! The Orchestrator manages the lifecycle of all Bot instances:
//! - Creates Bots on demand when a Channel receives a message for a new chat
//! - Tracks which Bot is active per (channel, chat_id)
//! - Handles /bot switching
//! - Provides search_bots/mention_bot for MCP Server
//! - Cleans up idle Bots

use crate::bot::Bot;
use crate::channel::{Channel, ChatId, OutgoingContent, OutgoingMessage};
use crate::config::Config;
use crate::message_bus::{BotInstanceKey, MessageBus};
use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::{error, info, warn};

/// Info about a running bot instance
struct RunningBot {
    last_active: Instant,
}

/// Orchestrator manages all Bot instances across all Channels.
pub struct Orchestrator {
    /// Full config (to look up BotConfig by name)
    config: Config,
    /// MessageBus for registering bots
    message_bus: Arc<MessageBus>,
    /// MCP server port (if configured)
    mcp_server_port: Option<u16>,
    /// Currently active bot name per (channel, chat_id).
    /// When a user sends a message, this determines which bot receives it.
    active_bots: Mutex<HashMap<(String, ChatId), String>>,
    /// Running bot instances.
    /// Key: (channel_name, chat_id, bot_name)
    running_bots: Mutex<HashMap<(String, ChatId, String), RunningBot>>,
    /// Channel instances registered at startup, used for search_bots_in_chat.
    channels: Mutex<HashMap<String, Arc<dyn Channel>>>,
}

impl Orchestrator {
    /// Create a new Orchestrator.
    pub fn new(config: Config, message_bus: Arc<MessageBus>, mcp_server_port: Option<u16>) -> Self {
        Self {
            config,
            message_bus,
            mcp_server_port,
            active_bots: Mutex::new(HashMap::new()),
            running_bots: Mutex::new(HashMap::new()),
            channels: Mutex::new(HashMap::new()),
        }
    }

    /// Register a channel instance for bot discovery via search_bots_in_chat.
    pub async fn register_channel(&self, name: String, channel: Arc<dyn Channel>) {
        self.channels.lock().await.insert(name, channel);
    }

    /// Get or create the active bot for a (channel, chat_id).
    ///
    /// Called by Channel when it receives a message.
    /// Returns the active bot's name. If no bot exists yet, creates one
    /// using the channel's default_bot config.
    ///
    /// # Returns
    /// The bot name that should receive the message, or None if creation failed.
    pub async fn get_or_create(
        self: &Arc<Self>,
        channel_name: &str,
        chat_id: &ChatId,
    ) -> Option<String> {
        let key = (channel_name.to_string(), chat_id.clone());

        // Check if there's already an active bot for this chat
        if let Some(bot_name) = self.active_bots.lock().await.get(&key).cloned() {
            let rkey = (channel_name.to_string(), chat_id.clone(), bot_name.clone());
            if let Some(rb) = self.running_bots.lock().await.get_mut(&rkey) {
                rb.last_active = Instant::now();
                return Some(bot_name);
            }

            warn!(
                bot = %bot_name,
                channel = %channel_name,
                chat_id = %chat_id.0,
                "Active bot missing from running registry, recreating"
            );
            self.active_bots.lock().await.remove(&key);
            self.message_bus
                .unregister_bot(&BotInstanceKey {
                    channel_name: channel_name.to_string(),
                    chat_id: chat_id.clone(),
                    bot_name,
                })
                .await;
        }

        // Find the channel config to determine default bot
        let channel_config = self
            .config
            .channels
            .iter()
            .find(|c| c.name == channel_name)?;

        let bot_name = channel_config
            .default_bot
            .clone()
            .or_else(|| channel_config.bots.first().cloned())?;

        // Spawn the bot
        if self
            .spawn_bot(channel_name, chat_id, &bot_name)
            .await
            .is_ok()
        {
            Some(bot_name)
        } else {
            None
        }
    }

    /// Spawn a new Bot instance.
    async fn spawn_bot(
        self: &Arc<Self>,
        channel_name: &str,
        chat_id: &ChatId,
        bot_name: &str,
    ) -> Result<()> {
        let bot_config = self
            .config
            .get_bot(bot_name)
            .ok_or_else(|| anyhow!("Bot config not found: {}", bot_name))?
            .clone();

        info!(
            bot = %bot_name,
            channel = %channel_name,
            chat_id = %chat_id.0,
            "Spawning bot instance"
        );

        // Register bot in MessageBus and get receivers
        let mb_key = BotInstanceKey {
            channel_name: channel_name.to_string(),
            chat_id: chat_id.clone(),
            bot_name: bot_name.to_string(),
        };
        let receiver = self.message_bus.register_bot(mb_key).await;

        // Create and run the Bot
        let bot = Bot::new(
            bot_config,
            channel_name.to_string(),
            chat_id.clone(),
            self.message_bus.clone(),
            self.mcp_server_port,
        );

        self.active_bots.lock().await.insert(
            (channel_name.to_string(), chat_id.clone()),
            bot_name.to_string(),
        );
        self.running_bots.lock().await.insert(
            (
                channel_name.to_string(),
                chat_id.clone(),
                bot_name.to_string(),
            ),
            RunningBot {
                last_active: Instant::now(),
            },
        );

        let orchestrator = self.clone();
        let exit_channel = channel_name.to_string();
        let exit_chat_id = chat_id.clone();
        let exit_bot_name = bot_name.to_string();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("Failed to build tokio runtime for bot");
            let local = tokio::task::LocalSet::new();
            rt.block_on(local.run_until(async move {
                bot.run(receiver).await;
                orchestrator
                    .handle_bot_exit(exit_channel, exit_chat_id, exit_bot_name)
                    .await;
            }));
        });

        Ok(())
    }

    async fn handle_bot_exit(&self, channel_name: String, chat_id: ChatId, bot_name: String) {
        let active_key = (channel_name.clone(), chat_id.clone());
        let running_key = (channel_name.clone(), chat_id.clone(), bot_name.clone());
        let mb_key = BotInstanceKey {
            channel_name: channel_name.clone(),
            chat_id: chat_id.clone(),
            bot_name: bot_name.clone(),
        };

        self.message_bus.unregister_bot(&mb_key).await;
        self.running_bots.lock().await.remove(&running_key);

        let mut active = self.active_bots.lock().await;
        if active.get(&active_key) == Some(&bot_name) {
            active.remove(&active_key);
        }

        info!(
            bot = %bot_name,
            channel = %channel_name,
            chat_id = %chat_id.0,
            "Bot instance exited"
        );
    }

    /// Handle /bot command: list available bots or switch to a different one.
    ///
    /// Called by Channel when it intercepts a /bot command BEFORE dispatching.
    pub async fn handle_bot_command(
        self: &Arc<Self>,
        channel_name: &str,
        chat_id: &ChatId,
        args: Option<String>,
    ) -> Result<()> {
        let channel_config = self
            .config
            .channels
            .iter()
            .find(|c| c.name == channel_name)
            .ok_or_else(|| anyhow!("Channel not found: {}", channel_name))?;

        let current_bot = {
            let active = self.active_bots.lock().await;
            active
                .get(&(channel_name.to_string(), chat_id.clone()))
                .cloned()
        };

        match args {
            None => {
                // List available bots
                let mut response = "Available bots:\n".to_string();
                for bot_name in &channel_config.bots {
                    let marker = if current_bot.as_ref() == Some(bot_name) {
                        " (current)"
                    } else {
                        ""
                    };
                    let desc = self
                        .config
                        .get_bot(bot_name)
                        .and_then(|b| b.description.as_deref())
                        .unwrap_or("No description");
                    response.push_str(&format!("- {}{} — {}\n", bot_name, marker, desc));
                }
                self.send_text(channel_name, chat_id, response.trim()).await;
            }
            Some(target_name) => {
                let target_name = target_name.trim().to_string();

                // Validate the target bot exists and is available on this channel
                if !channel_config.bots.contains(&target_name) {
                    self.send_error(
                        channel_name,
                        chat_id,
                        &format!(
                            "Bot '{}' is not available on this channel. Use /bot to list.",
                            target_name
                        ),
                    )
                    .await;
                    return Ok(());
                }

                // Already using this bot?
                if current_bot.as_ref() == Some(&target_name) {
                    self.send_text(
                        channel_name,
                        chat_id,
                        &format!("Already using bot: {}", target_name),
                    )
                    .await;
                    return Ok(());
                }

                // Stop the old bot instance gracefully
                if let Some(old_name) = &current_bot {
                    let rkey = (channel_name.to_string(), chat_id.clone(), old_name.clone());
                    if let Some(_old) = self.running_bots.lock().await.remove(&rkey) {
                        // Send graceful shutdown signal via MessageBus
                        let mb_key = BotInstanceKey {
                            channel_name: channel_name.to_string(),
                            chat_id: chat_id.clone(),
                            bot_name: old_name.clone(),
                        };
                        self.message_bus.unregister_bot(&mb_key).await;

                        // Give the bot a moment to shut down gracefully
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                        info!(bot = %old_name, "Stopped old bot instance");
                    }
                    // Also remove from active bots
                    self.active_bots
                        .lock()
                        .await
                        .remove(&(channel_name.to_string(), chat_id.clone()));
                }

                // Spawn new bot
                self.send_text(
                    channel_name,
                    chat_id,
                    &format!("Switching to bot: {}...", target_name),
                )
                .await;

                match self.spawn_bot(channel_name, chat_id, &target_name).await {
                    Ok(()) => {
                        self.send_text(
                            channel_name,
                            chat_id,
                            &format!("Switched to bot: {}", target_name),
                        )
                        .await;
                    }
                    Err(e) => {
                        self.send_error(
                            channel_name,
                            chat_id,
                            &format!("Failed to start bot: {}", e),
                        )
                        .await;
                    }
                }
            }
        }

        Ok(())
    }

    /// Clean up idle bot instances.
    pub async fn cleanup_idle(&self, timeout: std::time::Duration) {
        let now = Instant::now();
        // Lock in consistent order: active_bots first, then running_bots
        // to avoid deadlock with get_or_create
        let mut active = self.active_bots.lock().await;
        let mut running = self.running_bots.lock().await;

        let expired: Vec<(String, ChatId, String)> = running
            .iter()
            .filter(|(_, rb)| now.duration_since(rb.last_active) > timeout)
            .map(|(k, _)| k.clone())
            .collect();

        for key in expired {
            if let Some(_rb) = running.remove(&key) {
                // Send graceful shutdown signal
                let mb_key = BotInstanceKey {
                    channel_name: key.0.clone(),
                    chat_id: key.1.clone(),
                    bot_name: key.2.clone(),
                };
                self.message_bus.unregister_bot(&mb_key).await;

                // Give the bot a moment to shut down gracefully
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;

                active.remove(&(key.0.clone(), key.1.clone()));
                info!(
                    bot = %key.2,
                    channel = %key.0,
                    chat_id = %key.1.0,
                    "Cleaned up idle bot"
                );
            }
        }
    }

    /// Search for other bots in the same chat (for MCP).
    ///
    /// Calls the platform API via the channel's `search_bots_in_chat` to find which
    /// other bots are actually present in the group. Falls back to returning all
    /// configured bots for the channel if the platform doesn't support discovery.
    pub async fn search_bots(
        &self,
        channel_name: &str,
        chat_id: &ChatId,
        caller_bot: &str,
    ) -> Vec<BotInfo> {
        let channel = self.channels.lock().await.get(channel_name).cloned();

        if let Some(channel) = channel {
            let found_channel_names = channel.search_bots_in_chat(chat_id).await;
            if !found_channel_names.is_empty() {
                // Map discovered channel_names to their configured bots
                let mut result = Vec::new();
                for cn in &found_channel_names {
                    if let Some(channel_config) =
                        self.config.channels.iter().find(|c| &c.name == cn)
                    {
                        for bot_name in &channel_config.bots {
                            if bot_name != caller_bot
                                && let Some(b) = self.config.get_bot(bot_name)
                            {
                                result.push(BotInfo {
                                    name: b.name.clone(),
                                    description: b.description.clone(),
                                });
                            }
                        }
                    }
                }
                return result;
            }
        }

        // Fallback: return all configured bots for this channel
        let channel_config = match self.config.channels.iter().find(|c| c.name == channel_name) {
            Some(c) => c,
            None => return vec![],
        };
        channel_config
            .bots
            .iter()
            .filter(|name| name.as_str() != caller_bot)
            .filter_map(|name| {
                self.config.get_bot(name).map(|b| BotInfo {
                    name: b.name.clone(),
                    description: b.description.clone(),
                })
            })
            .collect()
    }

    /// Mention another bot (for MCP): send a Mention message through the channel.
    /// Each Channel handles the platform-specific @mention formatting.
    pub async fn mention_bot(
        &self,
        channel_name: &str,
        chat_id: &ChatId,
        _caller_bot: &str,
        target_bot: &str,
        message: &str,
    ) -> Result<()> {
        // Resolve which channel the target_bot is configured in (prefer a different channel)
        let target_channel_name = self
            .config
            .channels
            .iter()
            .find(|c| c.bots.contains(&target_bot.to_string()) && c.name != channel_name)
            .or_else(|| {
                self.config
                    .channels
                    .iter()
                    .find(|c| c.bots.contains(&target_bot.to_string()))
            })
            .map(|c| c.name.clone());

        let msg = OutgoingMessage {
            content: OutgoingContent::Mention {
                target_bot: target_bot.to_string(),
                target_channel_name,
                message: message.to_string(),
            },
        };
        self.message_bus
            .send(channel_name, chat_id.clone(), msg)
            .await
            .map_err(|e| anyhow!("Failed to send mention: {}", e))
    }

    /// Helper: send text through MessageBus
    async fn send_text(&self, channel_name: &str, chat_id: &ChatId, text: &str) {
        let msg = OutgoingMessage {
            content: OutgoingContent::Text(text.to_string()),
        };
        if let Err(e) = self
            .message_bus
            .send(channel_name, chat_id.clone(), msg)
            .await
        {
            error!(error = %e, "Orchestrator failed to send text");
        }
    }

    /// Helper: send error through MessageBus
    async fn send_error(&self, channel_name: &str, chat_id: &ChatId, text: &str) {
        let msg = OutgoingMessage {
            content: OutgoingContent::Error(text.to_string()),
        };
        if let Err(e) = self
            .message_bus
            .send(channel_name, chat_id.clone(), msg)
            .await
        {
            error!(error = %e, "Orchestrator failed to send error");
        }
    }
}

/// Bot info for MCP search_bots response
#[derive(Debug, Clone, serde::Serialize)]
pub struct BotInfo {
    pub name: String,
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AgentConfig, BotConfig, ChannelConfig, Config, PlatformConfig, TelegramConfig,
    };
    use crate::message_bus::{BotInstanceKey, MessageBus};
    use std::collections::BTreeMap;

    fn test_config() -> Config {
        Config {
            mcp_server: None,
            bots: vec![BotConfig {
                name: "test-bot".to_string(),
                description: None,
                agent: AgentConfig {
                    command: "missing-agent".to_string(),
                    args: vec!["acp".to_string()],
                },
                working_dir: None,
                model: None,
                mode: None,
                config_options: BTreeMap::new(),
                instructions: None,
                show_thinking: true,
                show_auto_approved: true,
            }],
            channels: vec![ChannelConfig {
                name: "test-channel".to_string(),
                platform: PlatformConfig::Telegram {
                    telegram: TelegramConfig {
                        token: "token".to_string(),
                    },
                },
                default_bot: Some("test-bot".to_string()),
                bots: vec!["test-bot".to_string()],
                mention_only: false,
            }],
        }
    }

    #[tokio::test]
    async fn bot_exit_cleans_up_stale_state() {
        let message_bus = Arc::new(MessageBus::new());
        let orchestrator = Arc::new(Orchestrator::new(test_config(), message_bus.clone(), None));
        let chat_id = ChatId("chat-1".to_string());
        let active_key = ("test-channel".to_string(), chat_id.clone());
        let running_key = (
            "test-channel".to_string(),
            chat_id.clone(),
            "test-bot".to_string(),
        );
        let mb_key = BotInstanceKey {
            channel_name: "test-channel".to_string(),
            chat_id: chat_id.clone(),
            bot_name: "test-bot".to_string(),
        };

        let _receiver = message_bus.register_bot(mb_key.clone()).await;
        orchestrator
            .active_bots
            .lock()
            .await
            .insert(active_key, "test-bot".to_string());
        orchestrator.running_bots.lock().await.insert(
            running_key,
            RunningBot {
                last_active: Instant::now(),
            },
        );

        orchestrator
            .handle_bot_exit(
                "test-channel".to_string(),
                chat_id.clone(),
                "test-bot".to_string(),
            )
            .await;

        assert!(orchestrator.active_bots.lock().await.is_empty());
        assert!(orchestrator.running_bots.lock().await.is_empty());
        assert!(
            message_bus
                .dispatch(
                    &mb_key,
                    crate::channel::IncomingMessage::text("hello".to_string())
                )
                .await
                .unwrap_err()
                .to_string()
                .contains("Bot instance not found")
        );
    }

    #[tokio::test]
    async fn bot_exit_keeps_replacement_active_bot() {
        let message_bus = Arc::new(MessageBus::new());
        let orchestrator = Arc::new(Orchestrator::new(test_config(), message_bus.clone(), None));
        let chat_id = ChatId("chat-1".to_string());
        let active_key = ("test-channel".to_string(), chat_id.clone());
        let running_key = (
            "test-channel".to_string(),
            chat_id.clone(),
            "test-bot".to_string(),
        );
        let mb_key = BotInstanceKey {
            channel_name: "test-channel".to_string(),
            chat_id: chat_id.clone(),
            bot_name: "test-bot".to_string(),
        };

        let _receiver = message_bus.register_bot(mb_key).await;
        orchestrator
            .active_bots
            .lock()
            .await
            .insert(active_key.clone(), "replacement-bot".to_string());
        orchestrator.running_bots.lock().await.insert(
            running_key,
            RunningBot {
                last_active: Instant::now(),
            },
        );

        orchestrator
            .handle_bot_exit("test-channel".to_string(), chat_id, "test-bot".to_string())
            .await;

        assert_eq!(
            orchestrator.active_bots.lock().await.get(&active_key),
            Some(&"replacement-bot".to_string())
        );
        assert!(orchestrator.running_bots.lock().await.is_empty());
    }
}
