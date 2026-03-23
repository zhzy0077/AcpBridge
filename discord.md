# Discord Channel Implementation Plan

## Goal

Add Discord as a new channel type so users can interact with ACP bots through Discord servers and DMs. Discord's rich message editing support makes it ideal for streaming responses, and its large user base makes it a natural fit for the platform.

## Config

```yaml
channels:
  - name: my-discord
    discord:
      bot_token: "YOUR_DISCORD_BOT_TOKEN"
      # Optional: restrict to specific guild (server) IDs.
      # If omitted, the bot responds in all guilds it's been added to.
      # guild_ids: ["123456789012345678"]
    default_bot: plan-agent
    bots: [plan-agent, execute-agent]
    mention_only: false
```

## Prerequisites

1. Create a Discord application at https://discord.com/developers/applications.
2. Under **Bot**, copy the bot token.
3. Under **Bot → Privileged Gateway Intents**, enable **Message Content Intent** (required to read message text).
4. Generate an invite URL under **OAuth2 → URL Generator** with scopes `bot` and permissions: Send Messages, Read Message History, Embed Links.
5. Invite the bot to your server using the generated URL.

## Changes

### 1. Cargo.toml

Add `serenity = { version = "0.12", default-features = false, features = ["client", "gateway", "model", "rustls_backend"] }`.

Serenity is the most mature Rust Discord library. It handles the Gateway WebSocket connection and REST API, and exposes an event-handler model that maps cleanly to the existing Channel pattern.

### 2. config.rs

- Add `DiscordConfig` struct:
  ```rust
  pub struct DiscordConfig {
      pub bot_token: String,
      pub guild_ids: Option<Vec<String>>,
  }
  ```
- Add `PlatformConfig::Discord { discord: DiscordConfig }` variant to the untagged enum.

### 3. src/channel/discord.rs

Implement `Channel` trait following the existing patterns (Telegram for streaming edits, Lark/QQ for WebSocket lifecycle).

#### Connection & Events

- Build a serenity `Client` with the bot token and required `GatewayIntents`:
  ```rust
  GatewayIntents::GUILD_MESSAGES
      | GatewayIntents::DIRECT_MESSAGES
      | GatewayIntents::MESSAGE_CONTENT
  ```
- Implement serenity's `EventHandler` trait to receive `message` events.
- Run `client.start()` which manages the Gateway WebSocket internally (including reconnection).

#### Incoming Messages

- In the `message` handler:
  - Extract `channel_id` as `ChatId` (Discord channel = one chat context).
  - Ignore messages from the bot's own user ID (`ctx.cache.current_user().id`).
  - `mention_only` check: inspect `message.mentions` for the bot's user ID.
  - Determine `is_group_chat`: `true` for guild channels, `false` for DMs.
  - Call `should_dispatch_message(mention_only, is_group_chat, is_mentioned)`.
  - Parse `/commands` (messages starting with `/`) vs plain text → `IncomingMessage`.
  - Strip `<@bot_id>` mention prefix from text before dispatching.
  - Route through `orchestrator.get_or_create()` → `message_bus.dispatch()`.
- Optional guild filtering: if `guild_ids` is set, ignore messages from unlisted guilds.

#### Outgoing Messages

- Spawn a task listening on `outgoing_rx` from MessageBus.
- `Text` / `Error` → `channel_id.say(&ctx.http, &text)`.
- `StreamChunk` → buffer chunks; send initial message, then edit via `message.edit(&ctx.http, ...)` (same 500ms throttle as Telegram).
- `StreamEnd` → final edit to flush buffer, remove from stream state.
- `Mention` → send message with `<@{user_id}>` syntax for native Discord mentions.

#### Stream Editing

Discord supports editing messages via the REST API:
```
PATCH /channels/{channel_id}/messages/{message_id}
{ "content": "updated text" }
```

Serenity exposes this as `message.edit()`. Use the same `StreamState` pattern as Telegram:
```rust
struct StreamState {
    msg_id: Option<MessageId>,
    buffer: String,
    last_update: Instant,
}
```

- **Throttle**: 500ms between edits (consistent with Telegram).
- **Max message**: 2000 characters. Auto-split when buffer exceeds this — send current message as final, start a new one for remaining chunks.
- **Timeout**: 30-second stream inactivity timeout with 5-second periodic flush check.

#### Bot Registry (for MCP mentions)

```rust
static DISCORD_BOT_REGISTRY: LazyLock<DashMap<String, String>> = ...;
```
Maps the bot's Discord user ID → channel_name for cross-channel mention resolution.

### 4. channel/mod.rs + main.rs

- `mod discord; pub use discord::DiscordChannel;`
- Add `PlatformConfig::Discord { discord }` arm in `main.rs` match to construct `DiscordChannel::new(discord.bot_token, discord.guild_ids, mention_only)`.

### 5. config.example.yaml

Add commented-out Discord example:
```yaml
  # Example: Discord Bot channel
  # - name: my-discord
  #   discord:
  #     bot_token: "YOUR_DISCORD_BOT_TOKEN"
  #     # Optional: restrict to specific guild (server) IDs
  #     # guild_ids: ["123456789012345678"]
  #   mention_only: false
  #   default_bot: plan-agent
  #   bots: [plan-agent, execute-agent]
```

## Design Notes

- **Channel = ChatId**: each Discord text channel (or DM channel) is one chat context. This aligns with how Discord users expect conversations to be scoped.
- **Serenity over twilight**: Serenity provides a higher-level API with built-in reconnection, caching, and the `EventHandler` trait — reducing boilerplate. Twilight is more modular but would require wiring up gateway + HTTP + cache manually, which isn't necessary here.
- **Message Content Intent**: This is a privileged intent that must be explicitly enabled in the Discord Developer Portal. Without it, the bot cannot read message text in guild channels. The bot will log a clear error if this intent is missing.
- **Rate limits**: Discord enforces rate limits on message edits (~5/5s per channel). The 500ms edit throttle keeps us well within limits. Serenity's HTTP client handles `429 Too Many Requests` responses with automatic retry.
- **Slash commands**: Not implemented in v1. Text-based `/commands` (like `/bot`, `/model`) are parsed from message content, consistent with other channels. Discord slash commands could be added later as an enhancement.
- **Thread support**: Discord threads are separate channels with their own `channel_id`, so they work automatically as distinct `ChatId` values — no special handling needed.
- **Embeds**: v1 uses plain text only. Rich embeds (for error formatting, bot status, etc.) could be added later.
- **Multi-bot per server**: Each bot is a separate Discord application/user. Users switch bots with `/bot` like other channels. For MCP `mention_bot`, the bot formats `<@{user_id}>` mentions natively.
