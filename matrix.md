# Matrix Channel Implementation Plan

## Goal

Add Matrix as a new channel type so users can run a local homeserver (e.g. Conduit) and use any Matrix client (Element, etc.) to interact with bots. This enables the "multi-agent local collaboration" scenario — multiple bots in one Matrix room, talking to each other via MCP mention.

## Config

```yaml
channels:
  - name: local-matrix
    matrix:
      homeserver_url: "http://localhost:6167"
      username: "bot"
      password: "secret"
      # OR: access_token: "syt_..."
    default_bot: plan-agent
    bots: [plan-agent, execute-agent]
    mention_only: false
```

## Changes

### 1. Cargo.toml

Add `matrix-sdk = "0.16"`. No E2EE features needed for local use.

### 2. config.rs

- Add `MatrixConfig` struct:
  ```rust
  pub struct MatrixConfig {
      pub homeserver_url: String,
      pub username: String,
      pub password: Option<String>,
      pub access_token: Option<String>,
  }
  ```
- Add `PlatformConfig::Matrix { matrix: MatrixConfig }` variant.

### 3. src/channel/matrix.rs

Implement `Channel` trait following the Telegram pattern.

#### Login & Sync

- Build client from `homeserver_url`.
- Login with username/password or reuse access_token.
- Auto-join rooms on invite (event handler on `StrippedRoomMemberEvent`).
- Run `client.sync()` to receive events.

#### Incoming Messages

- Register handler for `RoomMessageEvent`.
- Extract `room_id` as `ChatId`.
- Ignore messages from the bot's own user ID.
- Parse `/commands` vs plain text → `IncomingMessage`.
- `mention_only` check: look for bot's user ID in `m.mentions.user_ids` (structured, no text parsing needed).
- Route through `orchestrator.get_or_create()` → `message_bus.dispatch()`.

#### Outgoing Messages

- Loop on `outgoing_rx` from MessageBus.
- `Text` / `Error` → `room.send(RoomMessageEventContent::text_plain(...))`.
- `StreamChunk` → buffer chunks, send initial message, then edit via `m.replace` relation (same 500ms throttle as Telegram).
- `StreamEnd` → final edit to flush buffer.
- `Mention` → send message with both:
  - `m.mentions: { user_ids: ["@target:server"] }` (notification)
  - `formatted_body` with `<a href="https://matrix.to/#/@target:server">@target</a>` pill (display)

#### Stream Editing

Matrix supports message edits via replacement events:
```json
{
  "m.new_content": { "body": "updated text", "msgtype": "m.text" },
  "m.relates_to": { "rel_type": "m.replace", "event_id": "$original_event_id" }
}
```
Use this for in-place streaming, same pattern as Telegram's `edit_message`.

### 4. channel/mod.rs + main.rs

- `mod matrix; pub use matrix::MatrixChannel;`
- Add `PlatformConfig::Matrix { matrix }` arm in main.rs match.

### 5. config.example.yaml

Add commented-out Matrix example.

## Design Notes

- **Room = ChatId**: each Matrix room is one chat context.
- **No E2EE**: unnecessary for local Conduit. Can add later.
- **In-memory sync store**: no persistent state across restarts for v1. Fine for local use.
- **Multi-bot room**: user creates a room, invites all bot users. Each bot is a separate Matrix user on the homeserver. MCP `search_bots` / `mention_bot` handles inter-bot routing.
