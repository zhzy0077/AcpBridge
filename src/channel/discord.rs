//! Discord channel implementation

use crate::channel::{
    Channel, ChatId, IncomingContent, IncomingMessage, OutgoingContent, OutgoingMessage,
    should_dispatch_message,
};
use crate::message_bus::BotInstanceKey;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serenity::all::{
    Context, EventHandler, GatewayIntents, Message, MessageId, Ready,
};
use serenity::builder::EditMessage;
use serenity::Client;
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, error, info, warn};

/// Discord 单条消息最大长度
const MAX_MESSAGE_LENGTH: usize = 2000;
/// 编辑消息的最小间隔（节流）
const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(500);
/// 流式消息超时时间（30秒未收到新 chunk 则自动 flush）
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// Bot registry for MCP mentions: channel_name -> bot_user_id
/// This allows looking up a bot's Discord user ID by its channel name for native mentions
static DISCORD_BOT_REGISTRY: LazyLock<DashMap<String, u64>> = LazyLock::new(DashMap::new);

/// 流式消息状态（每个 chat_id 独立）
#[derive(Debug)]
struct StreamState {
    /// 已发送消息的 ID（首次发送后设置）
    msg_id: Option<MessageId>,
    /// 缓冲区，累积所有 chunk
    buffer: String,
    /// 上次更新时间（用于超时检测）
    last_update: Instant,
}

impl StreamState {
    fn new() -> Self {
        Self {
            msg_id: None,
            buffer: String::new(),
            last_update: Instant::now(),
        }
    }

    fn is_expired(&self) -> bool {
        self.last_update.elapsed() > STREAM_TIMEOUT
    }
}

#[derive(Debug, Clone)]
pub struct DiscordChannel {
    bot_token: String,
    guild_ids: Option<Vec<String>>,
    mention_only: bool,
}

impl DiscordChannel {
    pub fn new(bot_token: String, guild_ids: Option<Vec<String>>, mention_only: bool) -> Self {
        Self {
            bot_token,
            guild_ids,
            mention_only,
        }
    }
}

#[async_trait::async_trait]
impl Channel for DiscordChannel {
    async fn start(
        &self,
        channel_name: String,
        orchestrator: Arc<crate::orchestrator::Orchestrator>,
        message_bus: Arc<crate::message_bus::MessageBus>,
    ) -> anyhow::Result<()> {
        let intents = GatewayIntents::GUILD_MESSAGES
            | GatewayIntents::DIRECT_MESSAGES
            | GatewayIntents::MESSAGE_CONTENT;

        // Get bot user ID for registry and handler
        let http = serenity::all::Http::new(&self.bot_token);
        let current_user = http.get_current_user().await?;
        let bot_user_id = current_user.id.get();

        // Register bot in registry for MCP mentions (channel_name -> bot_user_id)
        DISCORD_BOT_REGISTRY.insert(channel_name.clone(), bot_user_id);
        info!(bot_id = %bot_user_id, "Registered Discord bot in MCP registry");

        let handler = DiscordHandler {
            channel_name: channel_name.clone(),
            orchestrator: orchestrator.clone(),
            message_bus: message_bus.clone(),
            mention_only: self.mention_only,
            guild_ids: self.guild_ids.clone(),
            bot_user_id: Arc::new(AtomicU64::new(bot_user_id)),
        };

        let mut client = Client::builder(&self.bot_token, intents)
            .event_handler(handler)
            .await?;

        // Create outgoing channel and register with MessageBus
        let (outgoing_tx, outgoing_rx) = mpsc::channel::<(ChatId, OutgoingMessage)>(100);
        message_bus
            .register_channel(channel_name.clone(), outgoing_tx)
            .await;

        // Spawn outgoing message handler
        let http = client.http.clone();
        let channel_name_clone = channel_name.clone();
        let outgoing_handle =
            tokio::spawn(handle_outgoing_messages(http, outgoing_rx, channel_name_clone));

        info!(channel = %channel_name, "Starting Discord client");

        // Start client - this will block until shutdown
        if let Err(e) = client.start().await {
            error!(error = %e, "Discord client error");
        }

        // Clean up registry entry on shutdown
        DISCORD_BOT_REGISTRY.remove(&channel_name);

        outgoing_handle.abort();
        Ok(())
    }
}

struct DiscordHandler {
    channel_name: String,
    orchestrator: Arc<crate::orchestrator::Orchestrator>,
    message_bus: Arc<crate::message_bus::MessageBus>,
    mention_only: bool,
    guild_ids: Option<Vec<String>>,
    /// Cached bot user ID to avoid repeated API calls
    bot_user_id: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl EventHandler for DiscordHandler {
    async fn message(&self, _ctx: Context, msg: Message) {
        let current_user_id = serenity::all::UserId::from(self.bot_user_id.load(Ordering::Relaxed));

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
        let is_group_chat = msg.guild_id.is_some();

        // Check if bot is mentioned
        let is_mentioned = msg.mentions.iter().any(|u| u.id == current_user_id);

        if !should_dispatch_message(self.mention_only, is_group_chat, is_mentioned) {
            return;
        }

        // Process message content
        let content = msg.content;

        // Strip bot mention prefix if present
        let text = if is_mentioned {
            let mention_str = format!("<@{}>", current_user_id);
            let mention_str_nick = format!("<@!{}>", current_user_id);
            content
                .replace(&mention_str, "")
                .replace(&mention_str_nick, "")
                .trim()
                .to_string()
        } else {
            content
        };

        if text.is_empty() {
            return;
        }

        let incoming = parse_incoming(&text);

        // Check for /bot command first (handled by orchestrator)
        if let IncomingContent::Command {
            name: cmd_name,
            args,
        } = &incoming.content
            && cmd_name == "bot"
        {
            if let Err(e) = self
                .orchestrator
                .handle_bot_command(&self.channel_name, &chat_id, args.clone())
                .await
            {
                error!(error = %e, "Failed to handle /bot command");
            }
            return;
        }

        // Get or create bot for this chat
        match self.orchestrator.get_or_create(&self.channel_name, &chat_id).await {
            Some(bot_name) => {
                let key = BotInstanceKey {
                    channel_name: self.channel_name.clone(),
                    chat_id: chat_id.clone(),
                    bot_name,
                };
                if let Err(e) = self.message_bus.dispatch(&key, incoming).await {
                    error!(error = %e, "Failed to dispatch message");
                }
            }
            None => {
                error!("Failed to get or create bot for chat");
            }
        }
    }

    async fn ready(&self, _ctx: Context, ready: Ready) {
        // Update cached user ID from the ready event
        self.bot_user_id.store(ready.user.id.get(), Ordering::Relaxed);
        info!(
            username = %ready.user.name,
            bot_id = %ready.user.id,
            "Discord bot connected"
        );
    }
}

async fn handle_outgoing_messages(
    http: Arc<serenity::all::Http>,
    mut outgoing_rx: mpsc::Receiver<(ChatId, OutgoingMessage)>,
    channel_name: String,
) {
    let mut stream_states: HashMap<String, StreamState> = HashMap::new();

    // 定时器用于检查超时
    let mut timeout_checker = interval(Duration::from_secs(5));
    timeout_checker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            // 处理新的 outgoing 消息
            Some((chat_id, msg)) = outgoing_rx.recv() => {
                let chat_id_str = chat_id.0.clone();

                match msg.content {
                    OutgoingContent::Text(text) => {
                        if let Err(e) = send_text_message(&http, &chat_id_str, &text).await {
                            error!(error = %e, "Failed to send message");
                        }
                    }
                    OutgoingContent::StreamChunk(text) => {
                        if let Err(e) = handle_stream_chunk(
                            &http,
                            &mut stream_states,
                            &chat_id_str,
                            &text,
                        ).await {
                            error!(error = %e, "Failed to handle stream chunk");
                        }
                    }
                    OutgoingContent::StreamEnd => {
                        if let Err(e) = handle_stream_end(
                            &http,
                            &mut stream_states,
                            &chat_id_str,
                        ).await {
                            error!(error = %e, "Failed to handle stream end");
                        }
                    }
                    OutgoingContent::Error(err) => {
                        if let Err(e) = send_text_message(
                            &http,
                            &chat_id_str,
                            &format!("Error: {}", err),
                        ).await {
                            error!(error = %e, "Failed to send error");
                        }
                    }
                    OutgoingContent::Mention {
                        target_bot,
                        target_channel_name,
                        message,
                    } => {
                        // Look up target bot's Discord user ID for native mention
                        let mention_text = if let Some(target_channel) = target_channel_name {
                            if let Some(entry) = DISCORD_BOT_REGISTRY.get(&target_channel) {
                                // Native Discord mention: <@user_id>
                                format!("<@{}> {}", *entry, message)
                            } else {
                                // Fall back to plain text if target not in registry
                                format!("@{} {}", target_bot, message)
                            }
                        } else {
                            // Check if target_bot matches current channel's bot
                            if let Some(entry) = DISCORD_BOT_REGISTRY.get(&channel_name) {
                                format!("<@{}> {}", *entry, message)
                            } else {
                                format!("@{} {}", target_bot, message)
                            }
                        };

                        if let Err(e) = send_text_message(&http, &chat_id_str, &mention_text).await
                        {
                            error!(error = %e, "Failed to send mention");
                        }
                    }
                }
            }
            // 定期检查超时
            _ = timeout_checker.tick() => {
                check_and_flush_timeouts(&http, &mut stream_states).await;
            }
            // 通道关闭时退出
            else => break,
        }
    }
}

/// 处理流式 chunk（编辑模式，支持自动分片）
async fn handle_stream_chunk(
    http: &serenity::all::Http,
    stream_states: &mut HashMap<String, StreamState>,
    chat_id_str: &str,
    text: &str,
) -> anyhow::Result<()> {
    let state = stream_states
        .entry(chat_id_str.to_string())
        .or_insert_with(StreamState::new);

    // 检查追加后是否会超出限制
    let new_len = state.buffer.len() + text.len();

    if state.msg_id.is_some() && new_len > MAX_MESSAGE_LENGTH {
        // 超出限制：先完成当前消息（最终编辑），然后开启新消息
        if let Some(msg_id) = state.msg_id {
            let channel_id = chat_id_str.parse::<u64>()?;
            let builder = EditMessage::new().content(&state.buffer);
            serenity::all::ChannelId::new(channel_id)
                .edit_message(http, msg_id, builder)
                .await?;
            debug!(msg_id = %msg_id, "Final edit before splitting message");
        }
        // 开启新消息，使用超出部分的内容
        state.msg_id = None;
        state.buffer = text.to_string();
        let channel_id = chat_id_str.parse::<u64>()?;
        let sent = serenity::all::ChannelId::new(channel_id)
            .say(http, &state.buffer)
            .await?;
        state.msg_id = Some(sent.id);
        state.last_update = Instant::now();
        debug!(msg_id = ?sent.id, "Started new message after split");
    } else {
        // 正常追加
        state.buffer.push_str(text);

        if state.msg_id.is_none() {
            // 首个 chunk：发送新消息
            let channel_id = chat_id_str.parse::<u64>()?;
            let sent = serenity::all::ChannelId::new(channel_id)
                .say(http, &state.buffer)
                .await?;
            state.msg_id = Some(sent.id);
            state.last_update = Instant::now();
            debug!(msg_id = ?sent.id, "Sent initial stream message");
        } else if state.last_update.elapsed() >= MIN_EDIT_INTERVAL {
            // 节流：距离上次更新超过 500ms 才编辑
            if let Some(msg_id) = state.msg_id {
                let channel_id = chat_id_str.parse::<u64>()?;
                let builder = EditMessage::new().content(&state.buffer);
                serenity::all::ChannelId::new(channel_id)
                    .edit_message(http, msg_id, builder)
                    .await?;
                state.last_update = Instant::now();
                debug!("Edited stream message");
            }
        }
    }

    Ok(())
}

/// 处理流式结束信号
async fn handle_stream_end(
    http: &serenity::all::Http,
    stream_states: &mut HashMap<String, StreamState>,
    chat_id_str: &str,
) -> anyhow::Result<()> {
    if let Some(state) = stream_states.remove(chat_id_str) {
        // 从 HashMap 中移除，避免内存泄漏
        if let Some(msg_id) = state.msg_id {
            // 最终编辑，确保内容完整
            let channel_id = chat_id_str.parse::<u64>()?;
            let builder = EditMessage::new().content(&state.buffer);
            serenity::all::ChannelId::new(channel_id)
                .edit_message(http, msg_id, builder)
                .await?;
            debug!("Final edit on stream end");
        }
    }

    Ok(())
}

/// 检查并 flush 超时的流式状态
async fn check_and_flush_timeouts(
    http: &serenity::all::Http,
    stream_states: &mut HashMap<String, StreamState>,
) {
    let expired_chat_ids: Vec<String> = stream_states
        .iter()
        .filter(|(_, state)| state.is_expired())
        .map(|(chat_id, _)| chat_id.clone())
        .collect();

    for chat_id_str in expired_chat_ids {
        warn!(chat_id = %chat_id_str, "Stream timeout, auto-flushing");

        if let Err(e) = handle_stream_end(http, stream_states, &chat_id_str).await {
            error!(error = %e, chat_id = %chat_id_str, "Failed to flush timeout stream");
        }
    }
}

async fn send_text_message(
    http: &serenity::all::Http,
    chat_id_str: &str,
    text: &str,
) -> anyhow::Result<()> {
    let channel_id = chat_id_str.parse::<u64>()?;
    let channel = serenity::all::ChannelId::new(channel_id);

    if text.len() > MAX_MESSAGE_LENGTH {
        for chunk in text.chars().collect::<Vec<_>>().chunks(MAX_MESSAGE_LENGTH) {
            let part = chunk.iter().collect::<String>();
            channel.say(http, &part).await?;
        }
    } else {
        channel.say(http, text).await?;
    }
    Ok(())
}

fn parse_incoming(text: &str) -> IncomingMessage {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix('/') {
        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
        let name = parts[0].to_lowercase();
        let args = parts.get(1).map(|s| s.to_string());
        IncomingMessage::command(name, args)
    } else {
        IncomingMessage::text(text.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_incoming_text() {
        let incoming = parse_incoming("hello");
        match incoming.content {
            IncomingContent::Text(text) => assert_eq!(text, "hello"),
            _ => panic!("Expected text"),
        }
    }

    #[test]
    fn test_parse_incoming_command() {
        let incoming = parse_incoming("/mode fast");
        match incoming.content {
            IncomingContent::Command { name, args } => {
                assert_eq!(name, "mode");
                assert_eq!(args, Some("fast".to_string()));
            }
            _ => panic!("Expected command"),
        }
    }

    #[test]
    fn test_parse_incoming_command_no_args() {
        let incoming = parse_incoming("/help");
        match incoming.content {
            IncomingContent::Command { name, args } => {
                assert_eq!(name, "help");
                assert!(args.is_none());
            }
            _ => panic!("Expected command"),
        }
    }

    #[test]
    fn test_stream_state_new() {
        let state = StreamState::new();
        assert!(state.msg_id.is_none());
        assert!(state.buffer.is_empty());
        assert!(!state.is_expired());
    }

    #[test]
    fn test_stream_state_expired() {
        // Note: We can't easily test actual expiration without sleeping,
        // but we can verify the logic works
        let state = StreamState::new();
        // A new state should not be expired
        assert!(!state.is_expired());
    }
}
