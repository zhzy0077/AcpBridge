//! Telegram channel implementation

use crate::channel::{
    Channel, ChatId, IncomingContent, IncomingMessage, OutgoingContent, OutgoingMessage,
    should_dispatch_message,
};
use crate::message_bus::BotInstanceKey;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use teloxide::prelude::*;
use teloxide::types::{MessageEntityKind, MessageEntityRef, MessageId, UserId};
use tokio::sync::mpsc;
use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, error, warn};

/// Telegram 单条消息最大长度
const MAX_MESSAGE_LENGTH: usize = 4096;
/// 编辑消息的最小间隔（节流）
const MIN_EDIT_INTERVAL: Duration = Duration::from_millis(500);
/// 流式消息超时时间（30秒未收到新 chunk 则自动 flush）
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

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
pub struct TelegramChannel {
    token: String,
    mention_only: bool,
}

impl TelegramChannel {
    pub fn new(token: String, mention_only: bool) -> Self {
        Self {
            token,
            mention_only,
        }
    }
}

#[async_trait::async_trait]
impl Channel for TelegramChannel {
    async fn start(
        &self,
        channel_name: String,
        orchestrator: Arc<crate::orchestrator::Orchestrator>,
        message_bus: Arc<crate::message_bus::MessageBus>,
    ) -> anyhow::Result<()> {
        let bot = Bot::new(&self.token);
        let me = bot.get_me().await?;
        let bot_username = me.username().to_string();
        let bot_user_id = me.id;

        // Create outgoing channel and register with MessageBus
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<(ChatId, OutgoingMessage)>(100);
        message_bus
            .register_channel(channel_name.clone(), outgoing_tx)
            .await;

        let bot_for_outgoing = bot.clone();
        let outgoing_handle = tokio::spawn(async move {
            // 本地状态管理，无需 Arc<RwLock>
            let mut stream_states: HashMap<String, StreamState> = HashMap::new();

            // 定时器用于检查超时
            let mut timeout_checker = interval(Duration::from_secs(5));
            timeout_checker.set_missed_tick_behavior(MissedTickBehavior::Skip);

            loop {
                tokio::select! {
                    // 处理新的 outgoing 消息
                    Some((chat_id, msg)) = outgoing_rx.recv() => {
                        let chat_id_str = chat_id.0.clone();
                        let chat_id_i64: i64 = chat_id_str.parse().unwrap_or(0);
                        let tg_chat_id = teloxide::types::ChatId(chat_id_i64);

                        match msg.content {
                            OutgoingContent::Text(text) => {
                                // 非流式消息直接发送
                                if let Err(e) = send_text_message(&bot_for_outgoing, tg_chat_id, &text).await {
                                    error!(error = %e, "Failed to send message");
                                }
                            }
                            OutgoingContent::StreamChunk(text) => {
                                if let Err(e) = handle_stream_chunk(
                                    &bot_for_outgoing,
                                    &mut stream_states,
                                    &chat_id_str,
                                    tg_chat_id,
                                    &text,
                                ).await {
                                    error!(error = %e, "Failed to handle stream chunk");
                                }
                            }
                            OutgoingContent::StreamEnd => {
                                if let Err(e) = handle_stream_end(
                                    &bot_for_outgoing,
                                    &mut stream_states,
                                    &chat_id_str,
                                    tg_chat_id,
                                ).await {
                                    error!(error = %e, "Failed to handle stream end");
                                }
                            }
                            OutgoingContent::Error(err) => {
                                if let Err(e) = send_text_message(
                                    &bot_for_outgoing,
                                    tg_chat_id,
                                    &format!("Error: {}", err),
                                ).await {
                                    error!(error = %e, "Failed to send error");
                                }
                            }
                            OutgoingContent::Mention { target_bot, message, .. } => {
                                // Telegram has no native bot mention; fall back to plain text
                                if let Err(e) = send_text_message(
                                    &bot_for_outgoing,
                                    tg_chat_id,
                                    &format!("@{} {}", target_bot, message),
                                ).await {
                                    error!(error = %e, "Failed to send mention");
                                }
                            }
                        }
                    }
                    // 定期检查超时
                    _ = timeout_checker.tick() => {
                        check_and_flush_timeouts(&bot_for_outgoing, &mut stream_states).await;
                    }
                    // 通道关闭时退出
                    else => break,
                }
            }
        });

        // Handle incoming messages
        let orch = orchestrator.clone();
        let mb = message_bus.clone();
        let name = channel_name.clone();
        let mention_only = self.mention_only;
        let handler_bot_username = bot_username.clone();
        let handler_bot_user_id = bot_user_id;

        let handler = move |msg: Message, _bot: Bot| {
            let orch = orch.clone();
            let mb = mb.clone();
            let name = name.clone();
            let bot_username = handler_bot_username.clone();
            let bot_user_id = handler_bot_user_id;

            async move {
                let chat_id = ChatId(msg.chat.id.0.to_string());
                if let Some(text) = msg.text() {
                    let is_group_chat = msg.chat.is_group() || msg.chat.is_supergroup();
                    let is_mentioned = mention_only
                        && is_group_chat
                        && msg.parse_entities().is_some_and(|entities| {
                            entities_mention_bot(&entities, &bot_username, bot_user_id)
                        });
                    if !should_dispatch_message(mention_only, is_group_chat, is_mentioned) {
                        return ResponseResult::Ok(());
                    }

                    let incoming = parse_incoming(text);

                    // Check for /bot command first (handled by orchestrator)
                    if let IncomingContent::Command {
                        name: cmd_name,
                        args,
                    } = &incoming.content
                        && cmd_name == "bot"
                    {
                        if let Err(e) = orch.handle_bot_command(&name, &chat_id, args.clone()).await
                        {
                            error!(error = %e, "Failed to handle /bot command");
                        }
                        return ResponseResult::Ok(());
                    }

                    // Get or create bot for this chat
                    match orch.get_or_create(&name, &chat_id).await {
                        Some(bot_name) => {
                            let key = BotInstanceKey {
                                channel_name: name.clone(),
                                chat_id: chat_id.clone(),
                                bot_name,
                            };
                            if let Err(e) = mb.dispatch(&key, incoming).await {
                                error!(error = %e, "Failed to dispatch message");
                            }
                        }
                        None => {
                            error!("Failed to get or create bot for chat");
                        }
                    }
                }
                ResponseResult::Ok(())
            }
        };

        teloxide::repl(bot, handler).await;
        outgoing_handle.abort();
        Ok(())
    }
}

fn entities_mention_bot(
    entities: &[MessageEntityRef<'_>],
    bot_username: &str,
    bot_user_id: UserId,
) -> bool {
    let bot_mention = format!("@{}", bot_username);

    entities.iter().any(|entity| match entity.kind() {
        MessageEntityKind::Mention => entity.text().eq_ignore_ascii_case(&bot_mention),
        MessageEntityKind::TextMention { user } => user.id == bot_user_id,
        _ => false,
    })
}

/// 处理流式 chunk（编辑模式，支持自动分片）
async fn handle_stream_chunk(
    bot: &Bot,
    stream_states: &mut HashMap<String, StreamState>,
    chat_id_str: &str,
    tg_chat_id: teloxide::types::ChatId,
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
            bot.edit_message_text(tg_chat_id, msg_id, &state.buffer)
                .await?;
            debug!("Final edit before splitting message");
        }
        // 开启新消息，使用超出部分的内容
        state.msg_id = None;
        state.buffer = text.to_string();
        let sent = bot.send_message(tg_chat_id, &state.buffer).await?;
        state.msg_id = Some(sent.id);
        state.last_update = Instant::now();
        debug!(msg_id = ?sent.id, "Started new message after split");
    } else {
        // 正常追加
        state.buffer.push_str(text);

        if state.msg_id.is_none() {
            // 首个 chunk：发送新消息
            let sent = bot.send_message(tg_chat_id, &state.buffer).await?;
            state.msg_id = Some(sent.id);
            state.last_update = Instant::now();
            debug!(msg_id = ?sent.id, "Sent initial stream message");
        } else if state.last_update.elapsed() >= MIN_EDIT_INTERVAL {
            // 节流：距离上次更新超过 500ms 才编辑
            if let Some(msg_id) = state.msg_id {
                bot.edit_message_text(tg_chat_id, msg_id, &state.buffer)
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
    bot: &Bot,
    stream_states: &mut HashMap<String, StreamState>,
    chat_id_str: &str,
    tg_chat_id: teloxide::types::ChatId,
) -> anyhow::Result<()> {
    if let Some(state) = stream_states.remove(chat_id_str) {
        // 从 HashMap 中移除，避免内存泄漏
        if let Some(msg_id) = state.msg_id {
            // 最终编辑，确保内容完整
            bot.edit_message_text(tg_chat_id, msg_id, &state.buffer)
                .await?;
            debug!("Final edit on stream end");
        }
    }

    Ok(())
}

/// 检查并 flush 超时的流式状态
async fn check_and_flush_timeouts(bot: &Bot, stream_states: &mut HashMap<String, StreamState>) {
    let expired_chat_ids: Vec<String> = stream_states
        .iter()
        .filter(|(_, state)| state.is_expired())
        .map(|(chat_id, _)| chat_id.clone())
        .collect();

    for chat_id_str in expired_chat_ids {
        warn!(chat_id = %chat_id_str, "Stream timeout, auto-flushing");

        let chat_id_i64: i64 = chat_id_str.parse().unwrap_or(0);
        let tg_chat_id = teloxide::types::ChatId(chat_id_i64);

        if let Err(e) = handle_stream_end(bot, stream_states, &chat_id_str, tg_chat_id).await {
            error!(error = %e, chat_id = %chat_id_str, "Failed to flush timeout stream");
        }
    }
}

async fn send_text_message(
    bot: &Bot,
    chat_id: teloxide::types::ChatId,
    text: &str,
) -> anyhow::Result<()> {
    if text.len() > MAX_MESSAGE_LENGTH {
        for chunk in text.chars().collect::<Vec<_>>().chunks(MAX_MESSAGE_LENGTH) {
            let part = chunk.iter().collect::<String>();
            bot.send_message(chat_id, &part).await?;
        }
    } else {
        bot.send_message(chat_id, text).await?;
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
    use teloxide::types::{MessageEntity, User};

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
    fn test_entities_mention_bot_by_username() {
        let text = "@TestBot hello";
        let entities = vec![MessageEntity::new(MessageEntityKind::Mention, 0, 8)];
        let parsed = MessageEntityRef::parse(text, &entities);
        assert!(entities_mention_bot(&parsed, "TestBot", UserId(42)));
    }

    #[test]
    fn test_entities_mention_bot_by_user_id() {
        let text = "Bot hello";
        let entities = vec![MessageEntity::new(
            MessageEntityKind::TextMention {
                user: User {
                    id: UserId(42),
                    is_bot: true,
                    first_name: "Bot".to_string(),
                    last_name: None,
                    username: Some("TestBot".to_string()),
                    language_code: None,
                    is_premium: false,
                    added_to_attachment_menu: false,
                },
            },
            0,
            3,
        )];
        let parsed = MessageEntityRef::parse(text, &entities);
        assert!(entities_mention_bot(&parsed, "OtherBot", UserId(42)));
    }
}
