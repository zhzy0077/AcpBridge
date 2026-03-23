//! Discord channel adapter implementation

use crate::channel::{ChannelAdapter, ChatId, MessageHandle, RawIncoming};
use anyhow::Result;
use async_trait::async_trait;
use dashmap::DashMap;
use serenity::all::{Context, EventHandler, GatewayIntents, Message, Ready};
use serenity::builder::EditMessage;
use serenity::Client;
use std::sync::LazyLock;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

/// Discord single message max length
const MAX_MESSAGE_LENGTH: usize = 2000;

/// Bot registry for MCP mentions: channel_name -> bot_user_id
/// This allows looking up a bot's Discord user ID by its channel name for native mentions
static DISCORD_BOT_REGISTRY: LazyLock<DashMap<String, u64>> = LazyLock::new(DashMap::new);

/// Discord adapter
#[derive(Debug, Clone)]
pub struct DiscordAdapter {
    bot_token: String,
    guild_ids: Option<Vec<String>>,
    channel_name: String,
}

impl DiscordAdapter {
    /// Create a new Discord adapter
    pub fn new(bot_token: String, guild_ids: Option<Vec<String>>, channel_name: String) -> Self {
        Self {
            bot_token,
            guild_ids,
            channel_name,
        }
    }

    /// Get the bot user ID for a channel name (for mention formatting)
    pub fn get_bot_user_id(channel_name: &str) -> Option<u64> {
        DISCORD_BOT_REGISTRY.get(channel_name).map(|v| *v)
    }
}

#[async_trait]
impl ChannelAdapter for DiscordAdapter {
    fn platform(&self) -> &'static str {
        "discord"
    }

    fn max_message_length(&self) -> usize {
        MAX_MESSAGE_LENGTH
    }

    fn supports_edit(&self) -> bool {
        true
    }

    async fn run_incoming(
        &self,
        incoming_tx: mpsc::Sender<RawIncoming>,
    ) -> anyhow::Result<()> {
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        // Get bot user ID and register in registry for MCP mentions
        let http = serenity::all::Http::new(&self.bot_token);
        let current_user = http.get_current_user().await?;
        let bot_user_id = current_user.id.get();

        // Register bot in registry for MCP mentions (channel_name -> bot_user_id)
        DISCORD_BOT_REGISTRY.insert(self.channel_name.clone(), bot_user_id);
        info!(bot_id = %bot_user_id, channel = %self.channel_name, "Registered Discord bot in MCP registry");

        let handler = DiscordHandler {
            incoming_tx,
            guild_ids: self.guild_ids.clone(),
            bot_user_id,
        };

        let mut client = Client::builder(&self.bot_token, intents)
            .event_handler(handler)
            .await?;

        info!("Starting Discord client");

        // Start client - this will block until shutdown
        if let Err(e) = client.start().await {
            error!(error = %e, "Discord client error");
        }

        Ok(())
    }

    async fn send_text(&self, chat_id: &ChatId, text: &str) -> Result<()> {
        let http = serenity::all::Http::new(&self.bot_token);
        let channel_id = parse_channel_id(chat_id)?;

        // Split if too long
        if text.len() > MAX_MESSAGE_LENGTH {
            for chunk in text.chars().collect::<Vec<_>>().chunks(MAX_MESSAGE_LENGTH) {
                let part = chunk.iter().collect::<String>();
                channel_id.say(&http, &part).await?;
            }
        } else {
            channel_id.say(&http, text).await?;
        }

        Ok(())
    }

    async fn send_and_track(&self, chat_id: &ChatId, text: &str) -> Result<MessageHandle> {
        let http = serenity::all::Http::new(&self.bot_token);
        let channel_id = parse_channel_id(chat_id)?;
        let sent = channel_id.say(&http, text).await?;
        Ok(MessageHandle(sent.id.get().to_string()))
    }

    async fn edit_message(
        &self,
        chat_id: &ChatId,
        handle: &MessageHandle,
        text: &str,
    ) -> Result<()> {
        let http = serenity::all::Http::new(&self.bot_token);
        let channel_id = parse_channel_id(chat_id)?;
        let msg_id: u64 = handle.0.parse()?;
        let builder = EditMessage::new().content(text);
        channel_id
            .edit_message(&http, serenity::all::MessageId::new(msg_id), builder)
            .await?;
        Ok(())
    }

    fn format_mention(
        &self,
        target_bot: &str,
        target_channel_name: Option<&str>,
        message: &str,
    ) -> String {
        // Look up target bot's Discord user ID for native mention
        if let Some(target_channel) = target_channel_name {
            if let Some(entry) = DISCORD_BOT_REGISTRY.get(target_channel) {
                // Native Discord mention: <@user_id>
                format!("<@{}> {}", *entry, message)
            } else {
                // Fall back to plain text if target not in registry
                format!("@{} {}", target_bot, message)
            }
        } else {
            // Check if target_bot matches current channel's bot
            // This is a simplification - in practice we'd need to know our channel name
            format!("@{} {}", target_bot, message)
        }
    }

}

fn parse_channel_id(chat_id: &ChatId) -> Result<serenity::all::ChannelId> {
    let id: u64 = chat_id.0.parse()?;
    Ok(serenity::all::ChannelId::new(id))
}

struct DiscordHandler {
    incoming_tx: mpsc::Sender<RawIncoming>,
    guild_ids: Option<Vec<String>>,
    bot_user_id: u64,
}

#[async_trait]
impl EventHandler for DiscordHandler {
    async fn message(&self, _ctx: Context, msg: Message) {
        let current_user_id = serenity::all::UserId::from(self.bot_user_id);

        // Ignore messages from the bot itself
        if msg.author.id == current_user_id {
            return;
        }

        // Optional guild filtering
        if let Some(ref allowed_guilds) = self.guild_ids
            && let Some(guild_id) = msg.guild_id
        {
            let guild_id_str = guild_id.to_string();
            if !allowed_guilds.contains(&guild_id_str) {
                debug!(guild_id = %guild_id_str, "Ignoring message from unlisted guild");
                return;
            }
        }

        let chat_id = ChatId(msg.channel_id.to_string());
        let is_group = msg.guild_id.is_some();

        // Check if bot is mentioned
        let is_mentioned = msg.mentions.iter().any(|u| u.id == current_user_id);

        // Strip bot mention prefix if present
        let text = if is_mentioned {
            let mention_str = format!("<@{}>", current_user_id);
            let mention_str_nick = format!("<@!{}>", current_user_id);
            msg.content
                .replace(&mention_str, "")
                .replace(&mention_str_nick, "")
                .trim()
                .to_string()
        } else {
            msg.content.clone()
        };

        if text.is_empty() {
            return;
        }

        let raw = RawIncoming {
            chat_id,
            text,
            is_group,
            is_mentioned,
        };

        let _ = self.incoming_tx.send(raw).await;
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        info!(
            username = %ready.user.name,
            bot_id = %ready.user.id,
            "Discord bot connected"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discord_adapter_new() {
        let adapter = DiscordAdapter::new("token123".to_string(), None, "test-channel".to_string());
        assert_eq!(adapter.platform(), "discord");
        assert_eq!(adapter.max_message_length(), 2000);
        assert!(adapter.supports_edit());
    }

    #[test]
    fn test_discord_adapter_with_guilds() {
        let guilds = vec!["123456".to_string(), "789012".to_string()];
        let adapter = DiscordAdapter::new("token123".to_string(), Some(guilds), "test-channel".to_string());
        assert!(adapter.guild_ids.is_some());
        assert_eq!(adapter.guild_ids.as_ref().unwrap().len(), 2);
    }
}
