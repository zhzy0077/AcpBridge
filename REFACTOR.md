# Channel 层重构设计

## 架构总览

```
Channel Adapter ←→ Channel Manager ←→ MessageHandler ←→ Bot Manager ←→ Bot
                   (supervisor)        (纯转换)          (lifecycle)

ChannelMessage →                                                    → BotMessage
ChannelResponse ←                                                   ← BotResponse
```

### 模块职责

| 模块 | 职责 | 不碰 |
|------|------|------|
| **Channel Adapter** | 平台 I/O、过滤(should_dispatch)、mention 检测、连接/重连、流式策略 | 不知道 Bot 存在 |
| **Channel Manager** | 监控 Adapter 生命周期(crash→restart)、ConversationId 路由 | 不碰消息内容 |
| **MessageHandler** | `ChannelMessage ↔ BotMessage`、`BotResponse ↔ ChannelResponse` 纯转换 | 不管生命周期，无状态 |
| **Bot Manager** | Bot 创建/销毁/切换，不做路由 | 不碰消息内容，不存 ConversationId |
| **Bot** | ACP agent 交互 | 不知道 Channel 存在 |

### 消息路由机制

- **Channel Manager** 持有 `HashMap<ConversationId, ChannelPair>`，负责路由
- **Bot Manager** 只管生命周期，创建 Bot 时返回 `(Sender<BotMessage>, Receiver<BotResponse>)`，不参与路由
- **MessageHandler** 是纯转换函数，无状态
- mpsc channel 本身就是路由——建好之后消息直接流动，不需要查表

### 新会话流程

```
1. 新会话到达 Channel Manager
2. Channel Manager 请求 Bot Manager: "创建一个 Bot"
3. Bot Manager 创建 Bot，返回 (Sender<BotMessage>, Receiver<BotResponse>)
4. Channel Manager 存下这对 channel，用 ConversationId 索引
5. 后续消息: Channel Manager 通过 MessageHandler 转换后，直接通过存好的 Sender 发送
6. 回复通过 Receiver 流回，经 MessageHandler 转换为 ChannelResponse，路由回 Adapter
```

## 消息类型设计

### 四种消息命名

以 Orchestrator（Channel Manager + Bot Manager）为中心，Message = 输入，Response = 输出：

| 方向 | 类型名 |
|------|--------|
| Adapter → Channel Manager | `ChannelMessage` |
| Channel Manager → Adapter | `ChannelResponse` |
| Bot Manager → Bot | `BotMessage` |
| Bot → Bot Manager | `BotResponse` |

### 共享类型

```rust
pub struct Mention {
    pub target_bot: String,
    pub target_channel: Option<String>,
}

pub enum Body {
    Text(String),
    // 未来: Image, File, Voice, ...
}

/// Content 是 struct 不是 enum。
/// Body 是消息内容（text/image/...），mentions 是消息级元数据。
/// Mention 不是一种 content type，而是可以附加在任何 Body 上的注解。
pub struct Content {
    pub body: Body,
    pub mentions: Vec<Mention>,
}
```

### Channel 侧消息

```rust
/// ConversationId 由 Channel Manager 分配，对外唯一。
/// 平台原始 chat_id 不泄漏到 Channel Manager 之外。
pub struct ConversationId(pub Uuid);

/// Adapter → Channel Manager
/// Adapter 已完成 should_dispatch 过滤，只产出应处理的消息。
pub struct ChannelMessage {
    pub conversation_id: ConversationId,
    pub content: Content,
}

/// Channel Manager → Adapter
pub struct ChannelResponse {
    pub conversation_id: ConversationId,
    pub content: Content,
}
```

`ChannelMessage` 和 `ChannelResponse` 完全对称，共用同一个 `Content`。

### Bot 侧消息

待设计（BotMessage、BotResponse 的 variant）。

## Channel Adapter 接口

```rust
#[async_trait]
pub trait ChannelAdapter: Send + Sync + 'static {
    fn platform(&self) -> &'static str;
    fn max_message_length(&self) -> usize;

    /// 同时处理收发。Adapter 自行管理：
    /// - 平台连接与重连（可用共享 run_with_reconnect）
    /// - should_dispatch 过滤（mention_only、is_group、is_mentioned）
    /// - mention 格式化（内部实现，不暴露到 trait）
    /// - 流式策略（如果保留的话，用共享 StreamHandler 组合）
    async fn run(
        &self,
        incoming_tx: mpsc::Sender<ChannelMessage>,
        outgoing_rx: mpsc::Receiver<ChannelResponse>,
    ) -> Result<()>;
}
```

从原来 6+ 个 trait 方法缩减到 3 个。以下方法全部变为 adapter 内部实现：
- `format_mention`
- `supports_edit` / `as_editable`
- `send_and_track` / `edit_message`
- `send_text`（由 adapter 的 outgoing 循环内部调用）

## 关键设计决策

### 删除 Streaming

Streaming（edit-in-place）在聊天平台上用户收益有限（通过 edit 实现，有 rate limit 和闪烁问题）。
删除后的简化：

| 删除项 | 影响 |
|--------|------|
| `StreamHandler` trait + `EditInPlaceStream` + `BufferAndSendStream` | ~170 LOC |
| `EditableAdapter` / `as_editable` / `supports_edit` | trait 设计问题根源 |
| `MessageHandle` 类型 | 跨模块传递的状态 |
| `send_and_track` / `edit_message` | 两个额外 trait 方法 |
| 每个 chat 的流式状态 + 超时检测 + rate limit | HashMap + interval tick |
| `BotResponse::StreamChunk` / `StreamEnd` | 两个 variant |

替代方案：AcpClient 端累积 chunk，达到阈值（字数或时间）后发一条完整的 `BotResponse::Text`。通过更频繁地发完整消息来补偿用户体验。

### Content 是 struct 不是 enum

- Mention 不是一种 content type，而是消息级元数据
- 一条图片消息也可以带 mention
- Body（Text/Image/...）和 mentions 是正交的两个维度

### ChatId 不泄漏

- 平台原始 chat_id（如 `"c2c:openid"`、`"12345"`）只在 Adapter 和 Channel Manager 内部使用
- 对外统一使用 `ConversationId`（系统分配）
- Channel Manager 内部维护 `HashMap<ConversationId, ConversationState>` 做翻译

## 共享工具（待实现）

### TokenManager（QQ + Lark 共用）

```rust
pub struct TokenManager {
    token: RwLock<Option<TokenInfo>>,
    refresh_lock: tokio::sync::Mutex<()>,
}

pub struct TokenResponse {
    pub access_token: String,
    pub expires_in_secs: u64,
}

impl TokenManager {
    pub fn new() -> Self;

    /// 获取有效 token。如果缓存过期，调用 fetch 闭包刷新。
    pub async fn get_token<F, Fut>(&self, fetch: F) -> Result<String>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<TokenResponse>>;
}
```

### run_with_reconnect（QQ + Lark + WeChat 共用）

```rust
pub async fn run_with_reconnect<F, Fut>(platform: &str, mut connect: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    // 指数退避: 1s → 2s → 4s → ... → 60s max
}
```

## 待设计

- [ ] Channel Manager 接口（supervisor + ConversationId 路由）
- [ ] MessageHandler 接口（纯转换函数）
- [ ] Bot Manager 接口（Bot 生命周期）
- [ ] BotMessage / BotResponse 的 variant 定义
- [ ] Bot 接口
