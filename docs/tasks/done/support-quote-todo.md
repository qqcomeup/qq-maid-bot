# 任务：支持引用消息创建 Todo

## 排查结论

### 数据流现状

```
QQ Event → Gateway 解析 → [reply 标签] → Core respond → 命令解析 → Todo 流程
```

### 三个断点

#### 断点 1：引用正文不解析（Gateway）

**文件：`qq-maid-gateway-rs/src/gateway/event.rs`**

- `RawMessageReply` 只提取 `message_id`，不提取内容字段
- `extract_message_reply()` 固定设置 `content: None`
- QQ 官方 API 的 `reply`/`quote` 字段确实只提供 message_id，不提供被引用消息正文

**文件：`qq-maid-gateway-rs/src/gateway/mod.rs`**

- `resolve_signals()` 已有缓存机制：将看到的消息 message_id → content 暂存，供后续回复引用时回填 reply.content
- **但只对 C2C 消息调用**（`handle_c2c_message` 第 360 行）
- **群消息不触发 `resolve_signals`**（`handle_group_message` 签名无 `reply_cache` 参数）
- 即使 C2C，`reply_cache` 只缓存 **收到的** 消息内容；**机器人发出的** 回复内容未被缓存

#### 断点 2：`[reply]` 前缀阻断命令解析（Gateway → Core 边界）

**文件：`qq-maid-gateway-rs/src/respond.rs`** `build_respond_content_parts()`

- 将引用信息格式化为 `[reply message_id=xxx]\n(content?)\n[/reply]\n用户消息`
- **`[reply` 前缀导致所有 slash 命令无法被识别**（无论是否有引用正文）

**文件：`qq-maid-core/src/runtime/command/mod.rs`**

- `parse_slash_command()` 要求文本以 `/` 开头，遇到 `[reply` 直接返回 None

**影响范围**：引用消息 + `/todo` `/查` `/天气` `/记忆` 等所有 slash 命令均受影响。

#### 断点 3：Todo 流程没有引用内容输入通道（Core）

**文件：`qq-maid-core/src/runtime/respond/types.rs`**

- `RespondRequest` 无 `reply_text` 或类似字段
- `RespondRequest::effective_user_text()` 原样返回 content，含 `[reply]` 标签

**文件：`qq-maid-core/src/runtime/respond/todo_flow/mod.rs`** `parse_todo_draft()`

- 只接收 `user_text` 一个参数
- 没有拼接引用正文的逻辑

### 方案

#### A. Gateway：解析引用正文

1. **`gateway/mod.rs`** `handle_group_message()`：
   - 增加 `reply_cache: &mut MessageCache` 参数
   - 调用 `resolve_signals(&mut message, reply_cache)`
2. **`gateway/protocol.rs`**：分发群消息时传入 `reply_cache`
3. **`gateway/mod.rs`** 发送响应后：将机器人发出的回复内容写入 `reply_cache`（目前只有 message_id 写入 `BotOutboundCache`）

#### B. Core：剥离 `[reply]` 标签 + 透传引用内容

4. **`runtime/respond/common.rs`**：新增函数
   - `extract_reply_block(text: &str) -> (clean_text: String, reply_text: Option<String>)`
   - 识别 `[reply message_id=...]...[/reply]` 块并提取
5. **`runtime/respond.rs`** `respond_transport()`：
   - 分裂 user_text 为「干净消息」和「引用正文」
   - 干净消息用于命令解析
   - 引用正文写入 `RespondRequest.metadata["reply_text"]`
6. **`runtime/respond/todo_flow/mod.rs`** `handle_todo_flow()`：
   - 读取 `metadata["reply_text"]`
   - `todo_add` 路径：将引用正文作为任务素材，当前消息作为操作指令，传给 LLM 解析
   - 当前消息中的显式时间和要求优先级高于引用正文

#### C. 异常处理（全部在 todo_flow 内）

7. 引用正文为空 → 不生成空待办，返回提示
8. 无引用 → 现有流程不受影响
9. 群聊 Pending 校验沿用现有 `initiator_user_id` 隔离

### 不需要改的

- `gateway/respond.rs` `build_respond_content_parts()`：保持现有的 `[reply]` 块格式，供 Chat 流程的 LLM 上下文使用
- `runtime/respond/chat_flow.rs` `build_chat_messages()`：LLM 聊天继续使用带 `[reply]` 标签的原始 user_text
- `storage/todo.rs`：Todo 持久化层不修改
- `runtime/pending/`：Pending 确认流程不修改
- `runtime/respond/todo_flow/pending.rs`：Pending 操作确认不修改

### 验收

1. 引用文本并发送 `/todo add 建个待办` → 能正确生成带引用内容的 Todo 草稿
2. 引用不可获取（空内容、纯图片） → 不生成空待办，给出提示
3. 无引用的 `/todo add` 流程不变
4. 群聊其他人不能确认发起人的 Pending
5. `cargo fmt --all -- --check`、`cargo clippy --workspace --all-targets --all-features -- -D warnings`、`cargo test --workspace --all-features` 全部通过

### 未解决的问题

- QQ 官方 API 的 reply/quote 字段不提供被引用消息的正文内容。引用正文只能通过本地缓存（`reply_cache`）解析已见过的消息。对于机器人重启后或缓存过期后的引用，无法获取被引用内容。
- 如需更可靠的引用解析，需要调用 QQ OpenAPI 的 Get Message 接口按 message_id 查询历史消息。这一方案需额外评估。
