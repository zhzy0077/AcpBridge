# Channel Architecture Refactoring

## Problem

Every channel (Telegram, Discord, QQ, Lark, WeChat) re-implements the same
orchestration logic inside its monolithic `start()` method:

- **Incoming dispatch pipeline** (~40 lines × 6 — QQ duplicates for C2C/GROUP):
  `parse_incoming → should_dispatch → /bot command → get_or_create → BotInstanceKey → dispatch`
- **Outgoing message loop** — identical `tokio::select!` + `OutgoingContent` match in all 5
- **Stream buffering** — two strategies, each copy-pasted across channels
- **`parse_incoming()`** — literally the same function in 4 channels
- **`check_and_flush_timeouts()`** — same pattern in all 5
- **Reconnect with exponential backoff** — same code in QQ, Lark, WeChat

~4500 lines of channel code; ~30% is duplicated orchestration glue.

## Design: `ChannelOrchestrator` + `ChannelAdapter` trait

Split the current monolithic `Channel::start()` into:

```
ChannelOrchestrator (shared, replaces Channel::start)
├── Owns: MessageBus registration, incoming dispatch, outgoing loop,
│         stream buffering, timeout management
│
└── Delegates platform I/O to ChannelAdapter (per-platform)
    ├── run_incoming()     — platform connection + message receiving
    ├── send_text()        — send a complete message
    ├── send_and_track()   — send + return handle for editing (optional)
    ├── edit_message()     — edit a tracked message (optional)
    ├── format_mention()   — platform-specific mention syntax
    └── max_message_len()  — platform constant
```

### `ChannelAdapter` trait

What each platform must implement — pure platform I/O, no orchestration logic.

```rust
/// Opaque handle to a sent message, used for edit-in-place streaming.
/// Each platform stores its own ID type inside.
pub struct MessageHandle(pub String);

#[async_trait]
pub trait ChannelAdapter: Send + Sync + 'static {
    /// Platform name for logging
    fn platform(&self) -> &'static str;

    /// Max chars per message on this platform
    fn max_message_length(&self) -> usize;

    /// Whether this platform supports editing a sent message for streaming.
    /// If true, the orchestrator uses edit-in-place streaming.
    /// If false, it buffers and sends once on StreamEnd.
    fn supports_edit(&self) -> bool { false }

    /// Start receiving messages. Send each incoming user message into `incoming_tx`.
    /// This is the platform's main loop (polling, WebSocket, long-poll, etc.).
    /// The adapter owns its own connection lifecycle and reconnection.
    async fn run_incoming(
        &self,
        incoming_tx: mpsc::Sender<RawIncoming>,
    ) -> Result<()>;

    /// Send a single text message
    async fn send_text(&self, chat_id: &ChatId, text: &str) -> Result<()>;

    /// Send a message and return a handle for future edits.
    /// Only called when supports_edit() returns true.
    async fn send_and_track(
        &self,
        chat_id: &ChatId,
        text: &str,
    ) -> Result<MessageHandle> {
        self.send_text(chat_id, text).await?;
        Err(anyhow!("tracking not supported"))
    }

    /// Edit a previously sent message.
    /// Only called when supports_edit() returns true.
    async fn edit_message(
        &self,
        chat_id: &ChatId,
        handle: &MessageHandle,
        text: &str,
    ) -> Result<()> {
        let _ = (chat_id, handle, text);
        Err(anyhow!("edit not supported"))
    }

    /// Format a mention in this platform's native syntax.
    /// Default: plain text fallback.
    fn format_mention(
        &self,
        target_bot: &str,
        _target_channel: Option<&str>,
        message: &str,
    ) -> String {
        format!("@{} {}", target_bot, message)
    }
}
```

### `RawIncoming` — what adapters send via mpsc

```rust
/// Raw incoming message from the platform, before orchestration.
/// The adapter extracts platform-specific fields; the orchestrator
/// handles parse_incoming, should_dispatch, /bot, get_or_create, dispatch.
pub struct RawIncoming {
    pub chat_id: ChatId,
    pub text: String,
    pub is_group: bool,
    pub is_mentioned: bool,
}
```

Using mpsc (not callbacks) because:
- Decouples adapter lifetime from orchestrator — adapters can be `'static`
- Natural backpressure via bounded channel
- Easy to test: send synthetic `RawIncoming` into the channel

### `ChannelOrchestrator`

```rust
pub struct ChannelOrchestrator {
    channel_name: String,
    mention_only: bool,
    orchestrator: Arc<Orchestrator>,
    message_bus: Arc<MessageBus>,
}

impl ChannelOrchestrator {
    /// Main entry point — replaces the old Channel::start().
    pub async fn run(self, adapter: Arc<dyn ChannelAdapter>) -> Result<()> {
        // 1. Create outgoing channel, register with MessageBus
        let (outgoing_tx, outgoing_rx) = mpsc::channel(100);
        self.message_bus
            .register_channel(self.channel_name.clone(), outgoing_tx)
            .await;

        // 2. Create incoming channel
        let (incoming_tx, incoming_rx) = mpsc::channel(100);

        // 3. Spawn adapter's incoming loop
        let incoming_handle = tokio::spawn({
            let adapter = adapter.clone();
            let tx = incoming_tx;
            async move { adapter.run_incoming(tx).await }
        });

        // 4. Spawn shared incoming dispatcher
        let dispatch_handle = tokio::spawn(
            self.run_incoming_dispatcher(incoming_rx)
        );

        // 5. Spawn shared outgoing loop
        let outgoing_handle = tokio::spawn(
            self.run_outgoing_loop(adapter, outgoing_rx)
        );

        // 6. Wait for any task to finish
        tokio::select! {
            _ = incoming_handle => {},
            _ = dispatch_handle => {},
            _ = outgoing_handle => {},
        }
        Ok(())
    }
}
```

### Incoming dispatcher (shared, ~30 lines total)

```rust
/// All the logic that used to be duplicated in every channel's incoming handler.
async fn run_incoming_dispatcher(
    &self,
    mut incoming_rx: mpsc::Receiver<RawIncoming>,
) -> Result<()> {
    while let Some(raw) = incoming_rx.recv().await {
        if !should_dispatch_message(self.mention_only, raw.is_group, raw.is_mentioned) {
            continue;
        }

        let incoming = parse_incoming(&raw.text);

        // /bot command → orchestrator
        if let IncomingContent::Command { name, args } = &incoming.content
            && name == "bot"
        {
            let _ = self.orchestrator
                .handle_bot_command(&self.channel_name, &raw.chat_id, args.clone())
                .await;
            continue;
        }

        // Normal message → get_or_create → dispatch
        if let Some(bot_name) = self.orchestrator
            .get_or_create(&self.channel_name, &raw.chat_id)
            .await
        {
            let key = BotInstanceKey {
                channel_name: self.channel_name.clone(),
                chat_id: raw.chat_id,
                bot_name,
            };
            if let Err(e) = self.message_bus.dispatch(&key, incoming).await {
                error!(error = %e, "Failed to dispatch message");
            }
        }
    }
    Ok(())
}
```

### Outgoing loop with stream strategy (shared)

```rust
async fn run_outgoing_loop(
    &self,
    adapter: Arc<dyn ChannelAdapter>,
    mut outgoing_rx: mpsc::Receiver<(ChatId, OutgoingMessage)>,
) -> Result<()> {
    let mut streamer: Box<dyn StreamHandler> = if adapter.supports_edit() {
        Box::new(EditInPlaceStream::new(adapter.clone()))
    } else {
        Box::new(BufferAndSendStream::new(adapter.clone()))
    };

    let mut timeout_checker = interval(Duration::from_secs(5));
    timeout_checker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            Some((chat_id, msg)) = outgoing_rx.recv() => {
                match msg.content {
                    OutgoingContent::Text(text) => {
                        send_with_splitting(&*adapter, &chat_id, &text).await;
                    }
                    OutgoingContent::StreamChunk(text) => {
                        streamer.on_chunk(&chat_id, &text).await;
                    }
                    OutgoingContent::StreamEnd => {
                        streamer.on_end(&chat_id).await;
                    }
                    OutgoingContent::Error(err) => {
                        let _ = adapter.send_text(&chat_id, &format!("Error: {err}")).await;
                    }
                    OutgoingContent::Mention { target_bot, target_channel_name, message } => {
                        let text = adapter.format_mention(
                            &target_bot, target_channel_name.as_deref(), &message,
                        );
                        let _ = adapter.send_text(&chat_id, &text).await;
                    }
                }
            }
            _ = timeout_checker.tick() => {
                streamer.flush_expired().await;
            }
            else => break,
        }
    }
    Ok(())
}
```

### Stream strategies

```rust
#[async_trait]
trait StreamHandler: Send {
    async fn on_chunk(&mut self, chat_id: &ChatId, text: &str);
    async fn on_end(&mut self, chat_id: &ChatId);
    async fn flush_expired(&mut self);
}

/// Telegram/Discord: edit messages in place with throttling
struct EditInPlaceStream { /* msg handles, buffers, throttle timer */ }

/// QQ/Lark/WeChat: accumulate chunks, send once on StreamEnd
struct BufferAndSendStream { /* buffers with timestamps */ }
```

### Message splitting (shared utility)

```rust
/// Split and send a long message respecting platform limits.
async fn send_with_splitting(
    adapter: &dyn ChannelAdapter,
    chat_id: &ChatId,
    text: &str,
) -> Result<()> {
    let max = adapter.max_message_length();
    if text.chars().count() <= max {
        return adapter.send_text(chat_id, text).await;
    }
    for chunk in split_by_chars(text, max) {
        adapter.send_text(chat_id, &chunk).await?;
    }
    Ok(())
}
```

## Reconnection stays in adapters

Reconnection stays per-adapter because:

- Connection models are fundamentally different (library-managed polling,
  WebSocket with session resume, HTTP long-poll)
- Some need platform-specific state across reconnects (QQ session_id + seq
  for RESUME vs IDENTIFY)
- Telegram and Discord use libraries that manage their own reconnection
- The actual reconnect loop is trivial (~10 lines); the real complexity
  is in platform-specific connection setup
- Forcing a generic "connection" abstraction adds complexity for little gain

Each adapter's `run_incoming()` owns its full connection lifecycle.

## What each adapter looks like after migration

### Telegram (~180 lines, down from 420)

```rust
impl ChannelAdapter for TelegramAdapter {
    fn platform(&self) -> &'static str { "telegram" }
    fn max_message_length(&self) -> usize { 4096 }
    fn supports_edit(&self) -> bool { true }

    async fn run_incoming(&self, tx: mpsc::Sender<RawIncoming>) -> Result<()> {
        // teloxide::repl — extract chat_id, text, is_group, is_mentioned
        // send RawIncoming { chat_id, text, is_group, is_mentioned } into tx
    }

    async fn send_text(&self, chat_id: &ChatId, text: &str) -> Result<()> { /* bot.send_message */ }
    async fn send_and_track(&self, chat_id: &ChatId, text: &str) -> Result<MessageHandle> { /* send + return msg_id */ }
    async fn edit_message(&self, chat_id: &ChatId, handle: &MessageHandle, text: &str) -> Result<()> { /* bot.edit_message_text */ }
}
```

### QQ (~600 lines, down from 1016)

```rust
impl ChannelAdapter for QqAdapter {
    fn platform(&self) -> &'static str { "qq" }
    fn max_message_length(&self) -> usize { 4096 }

    async fn run_incoming(&self, tx: mpsc::Sender<RawIncoming>) -> Result<()> {
        // WebSocket loop with reconnect, heartbeat, session resume
        // Parse C2C_MESSAGE_CREATE / GROUP_AT_MESSAGE_CREATE
        // Send RawIncoming into tx
    }

    async fn send_text(&self, chat_id: &ChatId, text: &str) -> Result<()> {
        // HTTP POST with token, passive reply, etc.
    }

    fn format_mention(&self, target_bot: &str, target_channel: Option<&str>, message: &str) -> String {
        // QQ native <@!bot_id> syntax via registry lookup
    }
}
```

## Summary

| Component | Before | After |
|-----------|--------|-------|
| `Channel` trait | 1 method: `start()` does everything | Replaced by `ChannelAdapter` (platform I/O only) |
| Incoming dispatch | Duplicated 6× across channels | Once in `ChannelOrchestrator::run_incoming_dispatcher` |
| Outgoing loop | Duplicated 5× across channels | Once in `ChannelOrchestrator::run_outgoing_loop` |
| Stream buffering | 2 strategies, each copy-pasted | `StreamHandler` trait with 2 impls |
| `parse_incoming` | Duplicated 4× | Once in `channel/mod.rs` |
| Total channel code | ~4500 lines | ~3000 lines (est.) |

## Todos

- [ ] Implement `ChannelAdapter` trait, `RawIncoming`, `MessageHandle` in `channel/mod.rs`
- [ ] Implement `ChannelOrchestrator` with incoming dispatcher + outgoing loop
- [ ] Implement `StreamHandler` trait + `EditInPlaceStream` + `BufferAndSendStream`
- [ ] Extract shared `parse_incoming()` and `send_with_splitting()`
- [ ] Rewrite TelegramAdapter
- [ ] Rewrite DiscordAdapter
- [ ] Rewrite QqAdapter
- [ ] Rewrite LarkAdapter
- [ ] Rewrite WeChatAdapter
- [ ] Update `main.rs` to use `ChannelOrchestrator::run(adapter)`
- [ ] Update AGENTS.md architecture docs
