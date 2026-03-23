//! Telegram channel adapter implementation

use crate::channel::{ChannelAdapter, ChatId, MessageHandle, RawIncoming};
use anyhow::Result;
use async_trait::async_trait;
use teloxide::prelude::*;
use teloxide::types::{MessageEntityKind, MessageEntityRef, MessageId, UserId};
use tokio::sync::mpsc;

/// Telegram single message max length
const MAX_MESSAGE_LENGTH: usize = 4096;

/// Telegram adapter
#[derive(Debug, Clone)]
pub struct TelegramAdapter {
    token: String,
}

impl TelegramAdapter {
    /// Create a new Telegram adapter
    pub fn new(token: String) -> Self {
        Self { token }
    }
}

#[async_trait]
impl ChannelAdapter for TelegramAdapter {
    fn platform(&self) -> &'static str {
        "telegram"
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
        let bot = Bot::new(&self.token);
        let me = bot.get_me().await?;
        let bot_username = me.username().to_string();
        let bot_user_id = me.id;

        let handler = move |msg: Message, _bot: Bot| {
            let tx = incoming_tx.clone();
            let bot_username = bot_username.clone();
            let bot_user_id = bot_user_id;

            async move {
                if let Some(text) = msg.text() {
                    let chat_id = ChatId(msg.chat.id.0.to_string());
                    let is_group = msg.chat.is_group() || msg.chat.is_supergroup();
                    let is_mentioned = msg.parse_entities().is_some_and(|entities| {
                        entities_mention_bot(&entities, &bot_username, bot_user_id)
                    });

                    let raw = RawIncoming {
                        chat_id,
                        text: text.to_string(),
                        is_group,
                        is_mentioned,
                    };

                    let _ = tx.send(raw).await;
                }
                ResponseResult::Ok(())
            }
        };

        teloxide::repl(bot, handler).await;
        Ok(())
    }

    async fn send_text(&self, chat_id: &ChatId, text: &str) -> Result<()> {
        let bot = Bot::new(&self.token);
        let tg_chat_id = parse_chat_id(chat_id)?;
        bot.send_message(tg_chat_id, text).await?;
        Ok(())
    }

    async fn send_and_track(&self, chat_id: &ChatId, text: &str) -> Result<MessageHandle> {
        let bot = Bot::new(&self.token);
        let tg_chat_id = parse_chat_id(chat_id)?;
        let sent = bot.send_message(tg_chat_id, text).await?;
        Ok(MessageHandle(sent.id.0.to_string()))
    }

    async fn edit_message(
        &self,
        chat_id: &ChatId,
        handle: &MessageHandle,
        text: &str,
    ) -> Result<()> {
        let bot = Bot::new(&self.token);
        let tg_chat_id = parse_chat_id(chat_id)?;
        let msg_id: i32 = handle.0.parse()?;
        bot.edit_message_text(tg_chat_id, MessageId(msg_id), text)
            .await?;
        Ok(())
    }
}

fn parse_chat_id(chat_id: &ChatId) -> Result<teloxide::types::ChatId> {
    let id: i64 = chat_id.0.parse()?;
    Ok(teloxide::types::ChatId(id))
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

#[cfg(test)]
mod tests {
    use super::*;
    use teloxide::types::{MessageEntity, User};

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
