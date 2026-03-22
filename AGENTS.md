# Agent Guidelines

This document defines rules for AI agents working on this codebase.

## Build, Test, and Lint

```bash
cargo build          # Compile
cargo test           # Run all tests
cargo test <name>    # Run a single test by name substring
cargo clippy         # Lint (treat warnings as errors)
```

Always run `cargo clippy` before committing. This project treats warnings as errors.

## Architecture

ACP Connector bridges chat platforms (Telegram, QQ, Lark/Feishu) to AI agents via the **Agent Client Protocol (ACP)**. Users interact with AI agents through their messaging app.

### Data Flow

```
Chat Platforms ──→ Channel ──→ MessageBus ──→ Bot ──→ AcpClient ──→ Agent subprocess
(Telegram/QQ/Lark)              (routing)    (per-chat)            (claude-code, gemini, etc.)
                                    ↑
                              Orchestrator (lifecycle management)
                                    ↓
                              MCP Server (optional: bot discovery/mention)
```

### Core Components

- **`main.rs`** — Entry point. Loads config, creates shared `MessageBus` and `Orchestrator`, spawns channels in a `LocalSet`, runs idle cleanup every 30s.
- **`config.rs`** — Parses `config.yaml`. Top-level entities: `bots` (reusable agent definitions) and `channels` (platform connections with bot assignments). Platform type is inferred via untagged enum deserialization.
- **`message_bus.rs`** — Pure routing layer. Decouples channels from bots. Maintains per-channel outgoing senders and per-instance `BotInstanceKey(channel_name, chat_id, bot_name)` event senders. Channels call `dispatch()` inbound; bots call `send()` outbound.
- **`orchestrator.rs`** — Bot factory and lifecycle manager. `get_or_create()` finds or spawns a bot for a (channel, chat_id) pair. `cleanup_idle()` removes bots inactive >30min. Intercepts `/bot` command to switch bots. Locks `active_bots` before `running_bots` to prevent deadlock.
- **`bot.rs`** — Per-chat AI agent wrapper. Owns one `AcpClient`, listens on its event channel, forwards output through `MessageBus`. Handles user commands (`/new`, `/model`, `/mode`, `/config`, `/cd`, `/help`). Prepends `instructions` from config to the first user message only.
- **`acp_client.rs`** — ACP protocol implementation (~1300 lines). Spawns the agent as a subprocess with stdio pipes. Manages session state (session_id, models, modes, config_options). Handles streaming with 60s inactivity timeout. Restarts on disconnect with exponential backoff (1s → 60s). Captures first 4KB of agent stdout/stderr for startup diagnostics.
- **`channel/`** — `Channel` trait with three implementations:
  - `telegram.rs` — Uses `teloxide`. Edits messages in-place for streaming (500ms throttle). Max message: 4096 bytes.
  - `qq.rs` — WebSocket to QQ Open Platform. Token manager handles auth. Supports C2C and group messages.
  - `lark.rs` — WebSocket via `openlark-client`. Configurable `base_url` for Feishu vs Lark International.
- **`mcp_server.rs`** — Optional HTTP server (rmcp + axum). Exposes `search_bots` and `mention_bot` tools at `http://localhost:{port}/mcp/{channel}/{bot}/{chat_id}`.

### Threading Model

Each Bot runs in a **separate OS thread** with its own tokio runtime and `LocalSet`, because the ACP SDK requires `spawn_local()`. Channels run in the main runtime's `LocalSet`. The MCP server runs as a regular tokio task.

### Streaming

All channels buffer `StreamChunk` messages and flush on `StreamEnd`. Telegram edits messages in-place; QQ and Lark reconstruct the full message on each update. AcpClient signals `StreamEnd` after 60s of chunk inactivity.

## Code Quality Rules

### No Warning Suppressions

**DO NOT use `#[allow(...)]` attribute macros to suppress compiler or linter warnings.**

This includes but is not limited to:
- `#[allow(dead_code)]`
- `#[allow(unused_variables)]`
- `#[allow(unused_imports)]`
- `#[allow(unused_mut)]`
- `#[allow(clippy::...)]`
- Any other warning suppression attributes

Fix the underlying issue instead:
- Remove unused code, imports, or `mut`
- Prefix unused variables with `_`
- Address clippy hints directly

### Conventions

- **Error handling**: `anyhow::Result<T>` with early returns. Errors become user-facing messages or are logged — no panics.
- **Logging**: `tracing` crate. Use `error` for failures, `warn` for recoverable issues, `info` for important events, `debug` for details.
- **Concurrency**: `Arc<Mutex<T>>` for shared state, `mpsc` channels for message passing. Lock `active_bots` before `running_bots` to maintain consistent lock ordering.
- **Config**: YAML-based (`config.yaml`). See `config.example.yaml` for the full schema. Bot references in channels are validated at load time.
- **Adding a new chat platform**: Implement the `Channel` trait (`start()` method), register with `MessageBus`, and add the platform config variant to the untagged enum in `config.rs`.
