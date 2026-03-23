# ACP Bridge

Bridge between messaging platforms and AI agents via the **Agent Client Protocol (ACP)**. Chat with Claude, Gemini, or any ACP-compatible agent directly from Telegram, QQ, Lark/Feishu, or WeChat.

## Features

- **Multi-Platform Support**: Connect to Telegram, QQ, Lark/Feishu, and WeChat simultaneously
- **Multi-Agent Management**: Define multiple bots with different configurations and switch between them on the fly
- **Real-time Streaming**: See agent responses as they are generated, with live updates
- **Session Management**: Persistent conversations with `/new` to start fresh sessions
- **Model & Mode Switching**: Change AI models and modes mid-conversation
- **Per-Chat Isolation**: Each conversation gets its own isolated agent instance
- **MCP Server**: Optional Model Context Protocol server for bot discovery and external integrations
- **Auto-Cleanup**: Idle bots are automatically terminated after 30 minutes to save resources

## Architecture

```
┌─────────────┐   ┌─────────────┐   ┌─────────────┐   ┌─────────────┐
│  Telegram   │   │     QQ      │   │ Lark/Feishu │   │   WeChat    │
└──────┬──────┘   └──────┬──────┘   └──────┬──────┘   └──────┬──────┘
       │                 │                 │                 │
       └─────────────────┴─────────────────┴─────────────────┘
                           │
                    ┌──────▼───────┐
                    │  Channel     │
                    │   Layer      │
                    └──────┬───────┘
                           │
                    ┌──────▼───────┐
                    │ MessageBus   │
                    │  (Router)    │
                    └──────┬───────┘
                           │
                    ┌──────▼───────┐
                    │ Orchestrator │
                    │  (Factory)   │
                    └──────┬───────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
         ┌────▼─────┐  ┌────▼─────┐  ┌────▼─────┐
         │  Bot 1   │  │  Bot 2   │  │  Bot N   │
         │(Chat A)  │  │(Chat B)  │  │(Chat C)  │
         └────┬─────┘  └────┬─────┘  └────┬─────┘
              │            │            │
         ┌────▼─────┐  ┌────▼─────┐  ┌────▼─────┐
         │ ACP CLI  │  │ ACP CLI  │  │ ACP CLI  │
         │(Claude)  │  │(Gemini)  │  │ (Other)  │
         └──────────┘  └──────────┘  └──────────┘
```

## Quick Start

### Prerequisites

- Rust 1.85+ (2024 edition)
- An ACP-compatible agent CLI (e.g., `claude-code`, `gemini`, or custom implementation)
- Bot credentials for at least one supported platform

### Installation

```bash
# Clone the repository
git clone https://github.com/yourusername/acpbridge.git
cd acpbridge

# Build release binary
cargo build --release

# The binary will be at target/release/acpbridge
```

### Configuration

Copy the example configuration and customize it:

```bash
cp config.example.yaml config.yaml
```

Edit `config.yaml` with your settings:

```yaml
# Bot definitions - reusable across channels
bots:
  - name: claude
    description: "Claude Code assistant"
    agent:
      command: claude-code
      args: ["--acp"]
    working_dir: /home/user/projects
    model: claude-sonnet
    instructions: |
      You are a helpful coding assistant.

# Channel definitions
channels:
  - name: telegram-bot
    telegram:
      token: "YOUR_BOT_TOKEN_FROM_BOTFATHER"
    default_bot: claude
    bots: [claude]
```

### Run

```bash
# Run with default config.yaml
./target/release/acpbridge

# Or specify a custom config path
ACP_CONFIG=/path/to/config.yaml ./target/release/acpbridge
```

## Configuration Reference

### Bot Configuration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Unique identifier for the bot |
| `description` | string | No | Human-readable description |
| `agent.command` | string | Yes | Executable to run (must support ACP) |
| `agent.args` | list | No | Arguments passed to the agent |
| `working_dir` | path | No | Working directory for the agent |
| `model` | string | No | Default model to use |
| `config_options` | map | No | ACP session config options (e.g., `reasoning_effort`) |
| `instructions` | string | No | System prompt prepended to first message |
| `show_thinking` | bool | No | Show thinking chunks (default: true) |
| `show_auto_approved` | bool | No | Show auto-approval notices (default: false) |

### Channel Configuration

| Field | Type | Required | Description |
|-------|------|----------|-------------|
| `name` | string | Yes | Unique channel identifier |
| `telegram` / `qq` / `lark` / `wechat` | object | Yes* | Platform-specific config (*one required) |
| `mention_only` | bool | No | Only respond when @mentioned in groups |
| `default_bot` | string | Yes | Default bot for new chats |
| `bots` | list | Yes | Available bots on this channel |

### Platform-Specific Settings

**Telegram:**
```yaml
telegram:
  token: "YOUR_BOT_TOKEN"
```

**QQ:**
```yaml
qq:
  app_id: "YOUR_APP_ID"
  client_secret: "YOUR_CLIENT_SECRET"
```

**Lark/Feishu:**
```yaml
lark:
  app_id: "YOUR_APP_ID"
  app_secret: "YOUR_APP_SECRET"
  # base_url: "https://open.larksuite.com"  # For international Lark
```

**WeChat:**
```yaml
wechat:
  bot_token: "YOUR_BOT_TOKEN"
  # base_url: "https://ilinkai.weixin.qq.com"  # Default: WeChat iLink API
```

### MCP Server (Optional)

Enable the MCP server for external integrations:

```yaml
mcp_server:
  port: 8080
```

Exposes endpoints at: `http://localhost:8080/mcp/{channel}/{bot}/{chat_id}`

## User Commands

Users can interact with bots using these commands:

| Command | Description |
|---------|-------------|
| `/new` | Start a new session (clear history) |
| `/model [name]` | Switch model or list available models |
| `/mode [name]` | Switch mode or list available modes |
| `/config [id value]` | List or set config options |
| `/cd <path>` | Change working directory |
| `/bot [name]` | Switch bot or list available bots |
| `/help` | Show help message |

## Development

```bash
# Build
cargo build

# Run tests
cargo test

# Lint (warnings are treated as errors)
cargo clippy

# Run with debug logging
RUST_LOG=debug cargo run
```

## Project Structure

```
src/
├── main.rs           # Entry point, runtime setup
├── config.rs         # Configuration parsing
├── message_bus.rs    # Message routing between channels and bots
├── orchestrator.rs   # Bot lifecycle management
├── bot.rs            # Per-chat AI agent wrapper
├── acp_client.rs     # ACP protocol implementation
├── mcp_server.rs     # MCP server for external integrations
└── channel/
    ├── mod.rs        # Channel trait definition
    ├── telegram.rs   # Telegram Bot implementation
    ├── qq.rs         # QQ Bot implementation
    ├── lark.rs       # Lark/Feishu implementation
    └── wechat.rs     # WeChat Work implementation
```

## License

MIT License - See LICENSE file for details.
