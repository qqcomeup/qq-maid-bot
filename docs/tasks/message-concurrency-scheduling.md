# 消息并发调度架构改造

## 背景

当前 Gateway 的消息处理链路是全局串行的。

`qq-maid-gateway-rs/src/gateway/protocol.rs:run_gateway_once` 的 WebSocket 主循环收到一条消息后，在当前 `tokio::select!` 分支内同步等待 `handle_envelope` → `handle_c2c_message` / `handle_group_message` 完整执行结束，才会回到循环顶部继续读取下一条 WebSocket 消息。

完整阻塞链路：

```text
run_gateway_once (protocol.rs:88)
  └─ tokio::select! { read.next() }
       └─ handle_envelope (protocol.rs:199)
            └─ handle_c2c_message (mod.rs:333) / handle_group_message (mod.rs:163)
                 └─ RespondClient::respond_c2c / respond_group (respond.rs)
                      └─ CoreHandle::respond (service.rs:168)
                           └─ timeout { service.respond(req) }  ← 等待 LLM 完整回复
                                └─ 流式路径: start_core_response_stream (service.rs:319)
                                     └─ tokio::spawn { run_streaming_respond }
                                          └─ Gateway consume_respond_stream (mod.rs:301)
                                               └─ send_group_outbound_with_fallback  ← QQ 发送
```

关键阻塞点：
- **`handle_c2c_message`（mod.rs:333）**：私聊消息 await `respond.respond_c2c` + `consume_respond_stream` + `send_outbound_with_fallback`
- **`handle_group_message`（mod.rs:163）**：群聊消息 await `respond.respond_group` + `consume_respond_stream` + `send_group_outbound_with_fallback`
- 两条路径都在 `handle_envelope` 的同步调用链内完成，`read.next()` 要等这一切结束。

因此群聊中一次 LLM 长回复会阻塞所有其他私聊和群聊消息。

## 目标

将消息处理模型从「所有消息全局串行」改为「同会话严格串行、不同会话并发、重型 LLM 调用受全局上限控制」。

完成后应满足：
1. 群聊长响应不阻塞其他私聊/群聊。
2. 同一会话（私聊用户或群）内消息严格按接收顺序处理。
3. 不出现会话历史乱序、pending 串线、重复发送或状态覆盖。
4. 不允许无限创建任务或无限积压消息。
5. 调度器具有明确的启动、关闭、回收和异常处理机制。

## 总体设计

### 架构分层

```
┌─────────────────────────────────────────────────────────┐
│ Gateway 层（qq-maid-gateway-rs）                         │
│                                                         │
│  run_gateway_once WS 读循环                              │
│        │                                                │
│        ▼                                                │
│  handle_envelope 解析与路由                              │
│        │                                                │
│        ▼                                                │
│  MessageDispatcher（新增 dispatcher.rs）                 │
│    · 按 scope_key 创建 / 查找会话队列                     │
│    · 有界 mpsc 队列入队                                  │
│    · 单 scope 单 worker 串行                             │
│    · 全局活跃 worker 上限                                │
│    · worker 空闲回收                                     │
│    · 队列满载系统通知                                    │
│    · 关闭信号传播                                        │
│                                                         │
├─────────────────────────────────────────────────────────┤
│ Core 层（qq-maid-core）                                  │
│                                                         │
│  LimitingLlmProvider（新增，包装 DynLlmProvider）         │
│    · 全局 Semaphore 控制 LLM 调用并发                    │
│    · 配置来自 Core AppConfig                             │
│    · 对调用方完全透明（实现 LlmProvider trait）            │
│                                                         │
└─────────────────────────────────────────────────────────┘
```

**职责边界：**

| 组件 | 归属 | 职责 |
|------|------|------|
| `MessageDispatcher` | Gateway | 按 scope 排队、同会话串行、不同会话并发、worker 生命周期、消息背压、系统通知 |
| `LimitingLlmProvider` | Core | 在 `LlmProvider::chat()` 入口统一控制所有 LLM 调用并发数 |

Dispatcher **不持有** LLM Semaphore，不感知 Core 内部的 LLM 并发限制。

### 数据流

```text
WS 读循环 (不阻塞)
        │
        ▼
MessageDispatcher.try_enqueue(msg)
        │
        ├─ 成功 → 消息进入 scope 队列 → worker 消费
        │                                      │
        │                                      ▼
        │                              handle_c2c/group_message
        │                                      │
        │                                      ▼
        │                              RespondClient
        │                                      │
        │                                      ▼
        │                              CoreHandle::respond
        │                                      │
        │                                      ▼
        │                              LlmChatService::respond
        │                                      │
        │                                      ▼
        │                              LimitingLlmProvider::chat  ← Semaphore permit
        │                                      │
        │                                      ▼
        │                              真正的 Provider::chat
        │
        └─ 队列满 → Dispatcher 系统通知通道 → send_rejection_message → QQ
```

## 实现要求

### 1. 解耦 WebSocket 读取与业务处理

**目标文件**：`qq-maid-gateway-rs/src/gateway/protocol.rs`

`run_gateway_once` 的 `tokio::select!` 中 `read.next()` 分支当前直接 await `handle_envelope`。改造后：

1. 解析 WS frame → 提取 `GatewayEnvelope`。
2. 解析业务事件 → 提取 `C2cMessage` 或 `GroupMessage`。
3. 将消息投递给 `MessageDispatcher`（见第 2 节）。
4. 立即回到 `tokio::select!` 继续读取下一条 WS 消息。

不得在 WS 读取循环中等待 LLM、Core 响应流、QQ 发送、数据库操作。

心跳（`OP_HEARTBEAT`）、重连（`OP_RECONNECT`）、关闭帧（`Message::Close`）仍留在主循环中处理，不受业务并发改造影响。

### 2. 引入统一消息调度器（MessageDispatcher）

**目标文件**：新建 `qq-maid-gateway-rs/src/gateway/dispatcher.rs`

`MessageDispatcher` 统一负责以下职责，不包含 LLM 并发控制：

- 根据 `ConversationKey`（即 `CoreRequest::scope_key()`）查找或创建会话队列
- 将消息放入对应会话的 `tokio::sync::mpsc` 有界队列
- 创建和管理会话 worker（每个 scope 一个 `tokio::spawn`）
- 控制单会话队列容量（`CONVERSATION_QUEUE_CAPACITY`）
- 控制全局活跃 worker 数量上限（`MAX_ACTIVE_CONVERSATION_WORKERS`）
- 管理 worker 空闲回收（`CONVERSATION_WORKER_IDLE_TIMEOUT_SECS`）
- 维护 worker 注册表及其竞态保护（见第 11 节）
- 管理系统拒绝通知通道（见第 8 节）
- 处理 Gateway 关闭（接收 `CancellationToken`）
- 记录调度日志

不要将零散的 `tokio::spawn` 放在 `handle_c2c_message` / `handle_group_message` 中。

C2C 和 Group 两种消息类型应转换为统一的内部消息后进入同一个调度层。当前 C2C 和 Group 处理函数在 `mod.rs` 中结构相似，可在 Dispatcher 内部用 enum 统一消息类型。

### 3. LLM 并发限制器（LimitingLlmProvider）

**目标文件**：新建 `qq-maid-core/src/provider/limiter.rs`

LLM 并发限制是 Core 层的基础设施，不是 MessageDispatcher 的职责。

#### 3.1 注入位置

在 `LlmProvider` trait 的 `chat()` 入口统一控制。新建 `LimitingLlmProvider`，实现 `LlmProvider` trait，内部持有：

- `inner: DynLlmProvider` — 真正的 provider
- `semaphore: Option<Arc<tokio::sync::Semaphore>>` — 并发限制器

```rust
#[async_trait]
impl LlmProvider for LimitingLlmProvider {
    async fn chat(&self, req: ChatRequest) -> Result<ChatOutcome, LlmError> {
        let _permit = match &self.semaphore {
            Some(sem) => Some(sem.acquire().await),
            None => None,
        };
        self.inner.chat(req).await
    }
    // stream_chat 自动继承 chat 的 permit（由内部调用链保证）
    fn name(&self) -> &'static str { self.inner.name() }
    fn model(&self) -> &str { self.inner.model() }
    fn stream_enabled(&self) -> bool { self.inner.stream_enabled() }
}
```

#### 3.2 为什么放在这里

所有重型 LLM 调用都经过 `LlmProvider::chat()`：

| 调用场景 | 调用点 | 经过路径 |
|---------|--------|---------|
| 普通聊天 | `llm_service.rs:159` | `LlmChatService::respond` → `provider.chat()` |
| `/查` 联网搜索 | `search_flow.rs` | 通过 `query_executor` → 内部 `provider.chat()` |
| 自动标题 | `title.rs:51` | `generate_session_title` → `provider.chat()` |
| 翻译命令 / RSS 翻译 | `translation.rs:145` | `TranslationService::translate` → `provider.chat()` |
| 记忆草稿提取 | `memory_flow.rs` | `LlmChatService::respond` → `provider.chat()` |
| 会话压缩 | `compact` flow | `LlmChatService::respond` → `provider.chat()` |
| Todo LLM 解析 | `todo_flow` | `LlmChatService::respond` → `provider.chat()` |
| RSS 翻译 | `scheduler.rs:335` | `TranslationService::translate` → `provider.chat()` |

在 `chat()` 这一层控制，无需在每个调用点单独加 permit，也不会遗漏新增调用场景。

#### 3.3 不放 CoreHandle::respond 入口

`CoreHandle::respond` 是所有消息的统一入口，包含本地命令（`/ping`、`/todo` 列表等）和重型 LLM 调用。在此处统一获取 permit 会导致本地命令被 Semaphore 阻塞。Semaphore 必须放在「真正调用模型」的边界，即 `LlmProvider::chat()`。

#### 3.4 装配位置

`LimitingLlmProvider` 在 `LlmRuntime::from_config_with_push_sink`（`qq-maid-core/src/app/`）中组装，替换原始的 `DynLlmProvider` 后再注入 `AppState` 和 `RustRespondService`。

#### 3.5 配置归属

配置项 `MAX_CONCURRENT_RESPONSES` 属于 Core 配置，添加到 `qq-maid-core/src/config.rs` 的 `AppConfig`。

permit 申请与释放语义：
- **申请**：`chat()` 被调用时，若 semaphore 存在则 `acquire().await`
- **释放**：`chat()` 返回时（正常 / 错误），`_permit` drop 自动释放
- **超时**：依赖已有的 `LLM_REQUEST_TIMEOUT_SECONDS`，在 CoreHandle 层通过 `tokio::time::timeout` 取消 future，取消时会 drop permit
- **取消**：通过 `CoreResponseStream::cancel()` 取消，依赖 `CancellationToken` 或 timeout 传播到 `provider.chat()` future 的 drop，从而释放 permit

### 4. ConversationKey：直接复用 scope_key

**已存在**：`qq-maid-core/src/service.rs:429`

```rust
impl CoreRequest {
    pub fn scope_key(&self) -> String {
        match &self.conversation {
            CoreConversation::Private { peer_id } => format!("private:{peer_id}"),
            CoreConversation::Group { group_id } => format!("group:{group_id}"),
        }
    }
}
```

- `"private:{peer_id}"` — 同一私聊用户的所有消息进入同一队列
- `"group:{group_id}"` — 同一群的所有消息进入同一队列
- 私聊和群聊天然隔离（不同前缀）

`scope_key` 是 `SessionStore` 使用的会话边界（`sessions.scope_key` 列），直接复用可保证调度语义与存储语义一致。

Gateway 在 `respond.rs` 中调用 `core_request_from_c2c_message` / `core_request_from_group_message` 后即可通过 `CoreRequest::scope_key()` 获得 key，不需要在 Gateway 层重新定义。

### 5. 每个会话使用有界消息队列

每会话维护一个 `tokio::sync::mpsc::channel(CONVERSATION_QUEUE_CAPACITY)` 队列。

每会话同时只允许一个 worker 消费消息（单 `tokio::spawn`，循环 recv → 处理 → recv）。

要求：
- 同会话严格按入队顺序处理（FIFO mpsc）
- 当前消息完成（业务处理 + LLM + QQ 发送 + 错误处理）后才处理下一条
- 不同会话的 worker 可以并发运行
- 不允许单会话无限积压
- 队列满时行为见第 8 节

### 6. 不同会话并发处理

可并发场景：
- 群聊 A 与私聊 B 并发
- 群聊 A 与群聊 B 并发
- 私聊 A 与私聊 B 并发
- 普通聊天与另一会话的本地命令并发

不因等待 LLM、消费流式响应、网络发送、摘要、自动标题而阻塞其他会话。

### 7. 轻型命令不得被 LLM 并发池阻塞

LLM Semaphore 位于 `LlmProvider::chat()` 入口，不经过 `chat()` 的本地命令天然不受影响：

- `/ping` — 本地诊断，在 `handle_c2c_message` 中提前返回（mod.rs:358），不调用 `provider.chat()`
- `/todo` 本地查询、列表、完成、删除 — 纯 SQLite 操作，不调用 LLM
- `/记忆` 不带参数查看列表 — 纯 SQLite 操作
- `/恢复` / `/resume` 无参数列表 — 纯 SQLite 操作
- `/help` — 本地帮助文本
- `/rss` 订阅列表/删除 — 本地操作
- `/state` — 本地状态查询

即使消息进入 Dispatcher 队列并被 worker 处理，这些命令的调用链不经过 `LlmProvider::chat()`，不会请求 Semaphore permit。

### 8. 消息背压与满载策略

#### 8.1 三层资源边界

| 限制 | 配置 | 默认值 | 所属模块 |
|------|------|--------|---------|
| 单会话队列容量 | `CONVERSATION_QUEUE_CAPACITY` | 16 | Gateway |
| 全局活跃 worker 上限 | `MAX_ACTIVE_CONVERSATION_WORKERS` | 64 | Gateway |
| 全局 LLM 调用并发 | `MAX_CONCURRENT_RESPONSES` | 4 | Core |

#### 8.2 队列满载处理

**问题**：队列满时需要向 QQ 发送「当前消息较多」提示，但 WS 读取循环不能等待 QQ 网络发送。

**方案**：`MessageDispatcher` 内部维护一个独立的**系统通知通道**：

```rust
struct MessageDispatcher {
    // 每个 scope 的消息队列注册表
    queues: DashMap<String, QueueEntry>,
    // 系统通知发送通道（有界）
    reject_tx: mpsc::Sender<RejectNotification>,
    // ...
}

struct RejectNotification {
    target: QqReplyTarget,  // C2cReplyTarget 或 GroupReplyTarget
    message: String,
}
```

一条专用的通知 worker（`tokio::spawn`）消费此通道并执行 QQ 发送。

通道容量建议与 `MAX_ACTIVE_CONVERSATION_WORKERS` 一致，满载时行为：

1. **拒绝通知通道未满**：正常入队通知 worker → 发送「当前消息较多，请稍后再试」。
2. **拒绝通知通道已满**：不创建新任务，记录结构化警告日志（scope_key、队列长度、容量、原因），递增 `reject_dropped` 计数器。
3. **禁止**：在此路径上无限 `tokio::spawn`、静默丢弃、伪造成功。

通知 worker 与业务 worker 使用同一个 `QqApiClient` 和 `GatewayRuntimeStatus`。

#### 8.3 活跃 worker 满载处理

当活跃 worker 数达到 `MAX_ACTIVE_CONVERSATION_WORKERS` 时：

1. 已有 scope 的消息**仍可入队**（其 worker 已存在）。
2. 新 scope 的消息**拒绝创建 worker**，通过系统通知通道返回错误。
3. 日志记录当前活跃 worker 数和上限。

### 9. Dispatcher 内部系统通知机制

通知 worker 生命周期：
- Dispatcher 创建时启动，绑定 Dispatcher 的 `CancellationToken`
- 循环从 `reject_tx` 接收 `RejectNotification`
- 对每条通知调用 `send_c2c_text_with_status` / `send_group_text_with_status`
- 通知发送失败记录 warning，不重试
- Dispatcher 关闭时收到 cancel，停止接收新通知，drain 已有通知或超时取消

### 10. worker 生命周期管理

#### 10.1 创建与复用

- 首条消息到达新 scope 时，若全局 worker 数未达上限，创建 worker（`tokio::spawn`）
- 后续同 scope 消息通过已有 `mpsc::Sender` 入队，复用 worker
- worker 创建成功后递增全局计数器，退出时递减

#### 10.2 空闲回收

- Worker 在 `mpsc::Receiver::recv()` 上使用 `tokio::time::timeout(CONVERSATION_WORKER_IDLE_TIMEOUT_SECS)`
- 超时后 worker 退出并从注册表删除自己的条目

#### 10.3 worker 注册表竞态保护（关键设计）

每个注册表条目携带单调递增的 `worker_id: u64`：

```rust
struct QueueEntry {
    tx: mpsc::Sender<GatewayMessage>,
    worker_id: u64,
}

struct MessageDispatcher {
    queues: DashMap<String, QueueEntry>,
    next_worker_id: AtomicU64,
    // ...
}
```

创建规则：
1. 新 worker 创建时，分配唯一 `worker_id`，写入 `QueueEntry`。
2. Worker 退出时，比较注册表中当前 `worker_id` 与自身 `worker_id`：
   - **相等** → 删除注册表条目（自己是当前 worker）
   - **不等** → 说明已有新 worker 接管，不操作注册表
3. `worker_id` 通过 `AtomicU64::fetch_add(1)` 递增，永不回绕（实际使用中不可能溢出）。

这保证以下竞态安全：
- 旧 worker 空闲退出与新消息创建 worker 同时发生 → 新 worker 有更大的 `worker_id`，旧 worker 退出时发现 id 不匹配，不删除新条目
- Worker panic 后新消息重新创建 → 新 worker 覆盖 `QueueEntry`（新 `tx` + 新 `worker_id`），旧 worker 的残留 sender 已无消费者
- 新旧 worker 不会同时消费同一队列 → `QueueEntry.tx` 只有一个有效 sender（最新创建的）

#### 10.4 Worker panic 恢复

- `tokio::spawn` 返回的 `JoinHandle` 不强制 `catch_unwind`
- Panic 后 `JoinHandle` 变为 cancelled 状态，worker 对应的 `mpsc::Sender` 的 `send()` 会返回错误
- 下一条消息到来时，由于 `send()` 失败，Dispatcher 检测到 worker 已死，分配新 `worker_id` 重建 worker 和队列
- 旧队列中的残留消息丢失，记录 warning 日志（scope_key、原 worker_id、残留消息数）

### 11. 关闭与重连语义

#### 11.1 临时 WebSocket 断线与自动重连

`gateway::run`（mod.rs:94）中的重连 loop 结构：

```rust
loop {
    match run_gateway_once(...).await {
        Ok(()) | Err(_) => {
            sleep(reconnect_delay()).await;
            // 重连，复用同一个 MessageDispatcher
        }
    }
}
```

- `MessageDispatcher` 在 `run` 中创建一次，不随 `run_gateway_once` 重复创建
- 断线期间已入队任务继续执行（worker 仍存活）
- QQ 发送会失败（WebSocket 断开），worker 正常处理错误并继续
- 重连后新消息继续入队

#### 11.2 正常进程退出（Ctrl+C）

`main.rs` 中 `gateway_handle.abort()` 改为通过 `CancellationToken` 触发 graceful shutdown：

1. 收到 Ctrl+C → 触发 `shutdown_token.cancel()`
2. `MessageDispatcher` 停止接收新消息（`try_enqueue` 返回 `Err(Shutdown)`）
3. WS 主循环中的 `handle_envelope` 收到 shutdown 后不再投递
4. 启动 **drain 阶段**：
   - 已入队任务继续处理
   - 最大等待 `SHUTDOWN_DRAIN_TIMEOUT_SECS`（建议常量 10 秒）
   - Drain 期间：记录每完成一条消息递减计数
5. Drain 超时后：
   - 向每个 worker 发送取消（通过各 scope 的 `CancellationToken` 或 drop sender）
   - 等待 worker `JoinHandle` 完成（带短超时，如 1 秒）
   - 记录未完成任务数量和剩余活跃 worker 数量
6. 不允许关闭过程无限等待 LLM、流消费或 QQ 网络请求

#### 11.3 shutdown 常量

```rust
/// 正常退出时，等待已入队任务完成的最大时间。
const SHUTDOWN_DRAIN_TIMEOUT_SECS: u64 = 10;
/// drain 超时后，等待单个 worker 退出的最大时间。
const WORKER_CANCEL_TIMEOUT_SECS: u64 = 1;
```

### 12. 保持流式响应能力

**现状**：已打通 `CoreHandle::respond` 流式路径（`service.rs:168-206`），Gateway 通过 `consume_respond_stream`（`mod.rs:301`）消费 `CoreResponseStream`。

**要求**：
- 不同会话可同时消费各自的响应流（不同 worker 并发）
- 同一会话仍只消费一条响应流（单 worker 串行保证）
- 一流失败不影响其他会话
- 流结束后才允许同会话下一条消息开始
- 跨会话流事件不混合
- 不退回非流式实现

### 13. reply cache 隔离

#### 13.1 当前实现

`MessageCache`（`mod.rs:58`）是 `HashMap<String, String>`，key 为 `message_id`（QQ 消息 ID）。

`resolve_signals`（`mod.rs:81`）：
- 将每条 C2C 消息的 `message_id → content` 写入 cache
- 当 reply 引用了已知 `message_id` 时，回填 `reply.content`

#### 13.2 隔离依据

- **Key 隔离**：`message_id` 是 QQ 平台全局唯一的消息 ID，不同私聊、不同群的消息 ID 不会冲突。cache 隔离性由 key 的唯一性自然保证，**不依赖**会话队列隔离。
- **并发保护**：当前无锁（单线程访问）。并发改造后需将其包裹为 `Arc<Mutex<HashMap<String, String>>>`。`resolve_signals` 在 worker 线程中调用，每次短暂获取锁后释放。
- **淘汰策略**：当前 `MessageCache` 无淘汰（无限增长）。并发改造后可保持现状（内存占用极小），或增加简单 LRU 上限。

#### 13.3 回归测试要求

- 跨私聊：两个私聊用户同时发送互引 reply 消息，确认 cache key 不冲突
- 跨群聊：两个群同时发送 reply，确认不互串
- 私聊 + 群聊并发：同时发送，确认 cache key 空间不重叠

### 14. 排查共享状态安全性

**关键共享状态及当前锁模型**：

| 状态 | 位置 | 锁模型 | 并发改造要求 |
|------|------|--------|-------------|
| `SessionStore` | `storage/session.rs` | `Mutex<Connection>` (std) | ✅ 已有锁，不跨 .await 持有 |
| `MemoryStore` | `storage/memory.rs` | `Mutex<Connection>` (std) | ✅ 同上 |
| `TodoStore` | `storage/todo.rs` | `Mutex<Connection>` (std) | ✅ 同上 |
| `RssStore` | `storage/rss.rs` | `Mutex<Connection>` (std) | ✅ 同上 |
| `KnowledgeStore` | `storage/knowledge.rs` | `Mutex<Connection>` (std) | ✅ 同上 |
| `reply_cache` | `mod.rs` `HashMap` | 无锁 | ⚠️ 需改为 `Arc<Mutex<HashMap>>` |
| `group_outbound_cache` | `mod.rs` | `Arc<Mutex<BotOutboundCache>>` | ✅ 已有锁，但需检查 C2C 路径是否意外共享 |
| `GatewayRuntimeStatus` | `ping/mod.rs` | 内部 `Mutex` | ✅ 已有锁 |
| `MessageDedupe` | `dedupe.rs` | 内部 `Mutex<HashMap>` | ✅ 已有锁 |
| `GroupCooldowns` | `group_filter.rs` | 内部 `Mutex` | ✅ 已有锁 |
| `ResumeState` | `protocol.rs` 局部 | 无锁 | ⚠️ 需确认只在 WS 主循环中修改（主循环仍串行，安全） |
| `UpstreamStatus` | `qq-maid-llm` | 内部 `RwLock` | ✅ 已有锁 |

**特别排查**：

- **自动标题后台任务**（`chat_flow.rs:323`）：已经通过 `tokio::spawn` 独立运行，调用 `session_store.update_title_if_current` 使用条件更新。并发改造后该后台任务仍可与同一 scope 的 worker 并发运行（worker 处理下一条消息时，之前的 auto title 可能还在跑）。`update_title_if_current` 的条件更新（只覆盖 `DEFAULT_SESSION_TITLE`）已提供保护。

- **session history 读写**：同 scope 内部由 worker 串行保证无快照覆盖。后台自动标题写入通过条件更新保护。

- **SQLite 锁范围**：所有 `Mutex<Connection>` 都是 `std::sync::Mutex`，只在同步 SQL 操作期间持有。当前代码无跨 `.await` 持有情况，改造时保持不变。

### 15. 排查 SQLite 锁范围

当前 SQLite 使用 `std::sync::Mutex<Connection>`（`storage/database.rs:33`），改造不要求变更连接池。

检查要点（`qq-maid-core/src/storage/` 下所有 `connection()` 调用点）：
- session、memory、todo、rss、knowledge 的 `connection()` 返回 `MutexGuard` 后，都在同步代码块内使用
- 不存在跨 `.await` 持有 `MutexGuard` 的情况
- 如果改造后并发 worker 增多导致数据库锁竞争成为新瓶颈，应记录而非擅自重构

### 16. 增加可观测性

在 `MessageDispatcher` 中增加结构化日志（使用 `tracing`），至少记录：

- 活跃会话 worker 数量（创建 +1，退出 -1）
- 当前全局 worker 数与上限
- 单会话队列长度（入队 / 出队时可选记录）
- 队列满拒绝次数（`reject_total` 计数器）
- 系统通知丢弃次数（`reject_dropped` 计数器）
- worker 创建、退出、panic 恢复
- 关闭时剩余任务和 worker 数量

在 `LimitingLlmProvider` 中增加日志：

- 当前等待 permit 的任务数（`semaphore.available_permits()` 反推）

日志中不输出完整消息正文或用户 openid（复用 `mask_openid` 脱敏）。

## 配置要求

### Core 配置

新增到 `qq-maid-core/src/config.rs` 的 `AppConfig`：

```env
# 全局 LLM 调用最大并发数；0 表示不限制
MAX_CONCURRENT_RESPONSES=4
```

| 属性 | 值 |
|------|-----|
| 默认值 | 4 |
| 最小值 | 0（不限制） |
| 最大值 | 256（硬上限，防止误配置） |
| 0 的语义 | 不启用并发限制。内部使用 `Option<Semaphore>`，0 时 `semaphore = None`，`chat()` 直接透传。 |
| 非法值 | 启动时返回配置错误（如负数、超过 256） |

### Gateway 配置

新增到 `qq-maid-gateway-rs/src/config/mod.rs` 的 `AppConfig`：

```env
# 单会话消息队列容量
CONVERSATION_QUEUE_CAPACITY=16
# 全局活跃会话 worker 最大数量
MAX_ACTIVE_CONVERSATION_WORKERS=64
# 会话 worker 空闲回收超时（秒）
CONVERSATION_WORKER_IDLE_TIMEOUT_SECS=300
```

| 配置 | 默认值 | 最小值 | 最大值 | 0 的语义 |
|------|--------|--------|--------|---------|
| `CONVERSATION_QUEUE_CAPACITY` | 16 | 1 | 256 | 非法，启动报错 |
| `MAX_ACTIVE_CONVERSATION_WORKERS` | 64 | 1 | 1024 | 非法，启动报错 |
| `CONVERSATION_WORKER_IDLE_TIMEOUT_SECS` | 300 | 10 | 3600 | 非法，启动报错 |

更新 `runtime/config/.env.example` 添加以上配置项及注释。

配置缺失时使用默认值，不阻止程序启动。

## 必须保持的现有行为

- 同会话消息顺序不变
- 会话历史顺序不变（`SessionStore` 的 `history` 追加写入）
- pending 只能由正确 scope 和正确用户继续（`pending.rs` 的 scope 校验）
- C2C 和群聊身份识别逻辑不退化（`respond.rs` 的 `core_request_from_*`）
- 流式响应继续工作（`CoreResponseStream` / `consume_respond_stream`）
- 自动标题不覆盖手工标题（`update_title_if_current` 条件更新）
- 现有命令语义不变
- 不修改与消息调度无关的业务功能

## 禁止事项

- 不要在 WS 入口无界 `tokio::spawn` 每条消息
- 不要自己实现线程池
- 不要使用 `std::thread::spawn` 处理完整消息
- 不要使用 `spawn_blocking` 承载完整异步响应链路
- 不要允许无限消息积压
- 不要让同一会话多任务并行修改历史
- 不要用全局 Mutex 包住完整消息处理
- 不要持有同步锁跨 `.await`
- 不要在 WS 主循环中直接 await QQ 发送（包括拒绝通知）
- 不要让拒绝通知通道自身无限积压
- 不要让零 permit 的 Semaphore 导致所有 LLM 请求永久等待
- 不要进行无关大规模重构
- 不要伪造测试结果

## 关键源码索引

| 组件 | 文件 | 关键函数/结构 |
|------|------|--------------|
| 主入口 | `src/main.rs` | `main`, `gateway_handle` |
| Core 启动 | `qq-maid-core/src/app/` | `LlmRuntime::from_config_with_push_sink` |
| Gateway 启动 | `qq-maid-gateway-rs/src/app/mod.rs` | `run_with_config` |
| 主循环 | `qq-maid-gateway-rs/src/gateway/mod.rs` | `run` (line 94) |
| WS 循环 | `qq-maid-gateway-rs/src/gateway/protocol.rs` | `run_gateway_once` (line 88) |
| 事件分发 | `qq-maid-gateway-rs/src/gateway/protocol.rs` | `handle_envelope` (line 199) |
| 私聊处理 | `qq-maid-gateway-rs/src/gateway/mod.rs` | `handle_c2c_message` (line 333) |
| 群聊处理 | `qq-maid-gateway-rs/src/gateway/mod.rs` | `handle_group_message` (line 163) |
| 流消费 | `qq-maid-gateway-rs/src/gateway/mod.rs` | `consume_respond_stream` (line 301) |
| Reply cache | `qq-maid-gateway-rs/src/gateway/mod.rs` | `resolve_signals` (line 81), `MessageCache` (line 58) |
| Gateway→Core | `qq-maid-gateway-rs/src/respond.rs` | `RespondClient`, `core_request_from_*` |
| Core 入口 | `qq-maid-core/src/service.rs` | `CoreHandle::respond` (line 168) |
| 流式起点 | `qq-maid-core/src/service.rs` | `start_core_response_stream` (line 319) |
| scope_key | `qq-maid-core/src/service.rs` | `CoreRequest::scope_key` (line 429) |
| LLM Provider trait | `qq-maid-llm/src/provider/mod.rs` | `LlmProvider` (line 60), `chat()` |
| Provider 调用 | `qq-maid-core/src/runtime/respond/llm_service.rs` | `LlmChatService::respond` → `provider.chat()` (line 159) |
| 自动标题 LLM | `qq-maid-core/src/runtime/respond/title.rs` | `provider.chat()` (line 51) |
| 翻译 LLM | `qq-maid-core/src/runtime/translation.rs` | `self.provider.chat()` (line 145) |
| RSS 翻译 LLM | `qq-maid-core/src/runtime/rss/scheduler.rs` | `translation_service.translate()` (line 335) |
| Gateway 配置 | `qq-maid-gateway-rs/src/config/mod.rs` | `AppConfig` |
| Core 配置 | `qq-maid-core/src/config.rs` | `AppConfig` (line 120) |
| 全局状态 | `qq-maid-core/src/http/routes.rs` | `AppState` (line 38) |
| SQLite 封装 | `qq-maid-core/src/storage/database.rs` | `SqliteDatabase`, `Mutex<Connection>` |
| 自动标题 | `qq-maid-core/src/runtime/respond/chat_flow.rs` | `tokio::spawn` (line 323) |
| 去重 | `qq-maid-gateway-rs/src/gateway/dedupe.rs` | `MessageDedupe` |
| 冷却 | `qq-maid-gateway-rs/src/gateway/group_filter.rs` | `GroupCooldowns` |
| 运行时状态 | `qq-maid-gateway-rs/src/gateway/ping/mod.rs` | `GatewayRuntimeStatus` |
| 环境变量模板 | `runtime/config/.env.example` | — |

## 验收标准

### 核心用户故事

1. 群聊 A 正在生成长回复时，用户私聊发送 `/ping`，私聊能立即处理。
2. 群聊 A 正在生成长回复时，用户私聊发送 `/todo`，待办命令可独立处理。
3. 群聊 A 发生 LLM 超时或错误，不影响其他私聊和群聊。
4. 两个不同群可同时处理普通聊天。
5. 两个不同私聊可同时处理消息。

### 会话顺序

1. 同一 scope 连续两条消息，第二条等待第一条完整处理。
2. 同一 scope 回复顺序与接收顺序一致。
3. 不出先后发消息先写入历史。
4. 不出同一 scope 多响应流交错发送。

### 会话隔离

1. 不同 scope 历史不串线。
2. 私聊与群聊历史不串线。
3. pending 不被其他 scope 消费。
4. reply cache 不返回其他 scope 消息（通过 message_id 全局唯一性 + Mutex 保护，不依赖会话队列隔离）。

### 并发限制

1. 同时运行的重型 LLM 调用不超过 `MAX_CONCURRENT_RESPONSES`（0 时不限制）。
2. 同时活跃的会话 worker 不超过 `MAX_ACTIVE_CONVERSATION_WORKERS`。
3. 等待 LLM permit 不阻塞 WS 继续读取消息。
4. 同一 scope 积压消息不提前占用多个 LLM permit（permit 只在 `chat()` 入口获取）。
5. 轻型本地命令不被 LLM Semaphore 阻塞（不经过 `chat()`）。

### 背压与资源

1. 单会话队列不超过 `CONVERSATION_QUEUE_CAPACITY`。
2. 队列满时通过系统通知通道发送提示；通知通道自身满载时记录日志和计数，不死循环。
3. 空闲 worker 能在 `CONVERSATION_WORKER_IDLE_TIMEOUT_SECS` 后回收。
4. 大量不同 scope 瞬间涌入时，新 scope 受 `MAX_ACTIVE_CONVERSATION_WORKERS` 限制，已有 scope 不受影响。
5. Gateway 重连不重复创建 Dispatcher 或 worker。

### 数据一致性

1. 不出旧 session 快照覆盖新历史。
2. 自动标题不覆盖手工标题。
3. 失败请求不被记为成功。
4. 消息不重复回复。
5. 数据库锁不跨 `.await` 持有。

### 关闭与重连

1. 临时 WebSocket 断线时 Dispatcher 保持存活，已入队任务继续执行或明确失败。
2. 正常退出时：停止接收新消息 → drain 已入队任务（等待 `SHUTDOWN_DRAIN_TIMEOUT_SECS`）→ 超时取消剩余 worker → 记录未完成任务数。
3. 关闭过程不无限等待 LLM、流消费或 QQ 网络请求。

### worker 注册表竞态

1. 旧 worker 空闲退出时，若已有新 worker 接管，不删除注册表条目。
2. Worker panic 后下一条消息能重建 worker，不永久阻塞该 scope。
3. 新旧 worker 不会同时消费同一 scope 消息（通过 `worker_id` 比较保证）。

## 测试要求

**必测场景**（优先使用 mock provider / channel / barrier，不依赖真实 LLM）：

- 同 scope 消息严格串行
- 不同 scope 消息并发
- 群聊长响应不阻塞私聊 `/ping`
- 全局 LLM 并发上限生效（`MAX_CONCURRENT_RESPONSES`）
- `MAX_CONCURRENT_RESPONSES=0` 时不限制
- 全局活跃 worker 上限生效（`MAX_ACTIVE_CONVERSATION_WORKERS`）
- 已达 worker 上限时已有 scope 仍可入队
- 同 scope 积压不占用多个 LLM permit
- 单会话队列满载：拒绝通知发送成功
- 单会话队列满载：拒绝通知通道满时日志计数，不死循环
- worker 空闲回收
- worker 空闲退出与新消息创建 worker 的竞态
- worker panic 后恢复
- Gateway 临时断线重连：Dispatcher 不重建
- Gateway 正常关闭：drain + 超时取消 + 计数
- session history 不乱序
- pending 不串线
- reply cache 跨私聊、跨群聊、私聊群聊并发隔离
- 流式响应跨 scope 不混合

完成后运行：
```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## 本次不处理

- SQLite 连接池改造（当前 `Mutex<Connection>` 保持不变）
- 分布式消息队列
- 多进程共享会话调度
- QQ 原生流式展示
- Provider 技术栈重写
- 与并发无关的业务重构
