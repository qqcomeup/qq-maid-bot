# Gateway 到 Core 进程内调用改造分析报告

本文基于当前仓库代码盘点 Gateway -> Core 的请求、响应和发送链路，并给出面向 1.0.0 的渐进式拆除 localhost HTTP 通讯方案。

核心目标是彻底移除 Gateway 与 Core 之间用于业务通信和内部探测的 HTTP 服务，而不是长期并存 HTTP 与进程内两套调用方式。迁移期可以保留 HTTP adapter 做对照和回退，但稳定后必须删除内部业务 HTTP endpoint、HTTP client/server 调用、内部通信 JSON DTO、SSE 传输解析，以及仅服务于内部通信的 readiness 检查和配置项。

本目标同时覆盖两个方向：

- Gateway -> Core 的请求响应链路。
- Core -> Gateway 的 RSS / Todo 等主动推送链路。

本目标不等于删除项目中的所有 HTTP 服务。若 `/healthz` 仍被外部运维系统使用，应改为由顶层应用暴露的进程级健康接口，通过进程内状态汇总 Gateway、Core 等组件状态；组件之间不得通过 `/healthz` 或任何 HTTP endpoint 互相通信。控制台、Markdown render 或其他仍有独立诊断/管理用途的 HTTP endpoint，应逐项确认后保留或调整。

本次分析只关注进程内强类型调用改造，不修改 Rust 代码。当前结论以以下源码为准：

- `src/main.rs`
- `qq-maid-gateway-rs/src/gateway/*`
- `qq-maid-gateway-rs/src/respond.rs`
- `qq-maid-gateway-rs/src/api.rs`
- `qq-maid-core/src/http/routes.rs`
- `qq-maid-core/src/runtime/respond.rs`
- `qq-maid-core/src/runtime/respond/*`
- `qq-maid-core/src/runtime/push.rs`
- `qq-maid-core/src/storage/session.rs`

## 一、当前架构摘要

当前程序已经是单进程启动：`src/main.rs` 先构造并启动 `qq-maid-core` 的 Axum HTTP 服务，等待 `/healthz` 就绪后再启动 `qq-maid-gateway-rs`。但 Gateway 到 Core 的业务调用仍然走本机 HTTP：

```text
QQ Gateway WebSocket
  -> qq-maid-gateway-rs 解析 C2C / Group 事件
  -> gateway respond.rs 构造 JSON RespondRequest
  -> POST http://127.0.0.1:8787/v1/respond
  -> qq-maid-core http/routes.rs 反序列化 HttpRespondRequest
  -> runtime/respond.rs RustRespondService::respond_transport
  -> Core 业务 flow / LLM / storage
  -> HTTP JSON 或 SSE
  -> gateway respond.rs 解析 JSON / SSE
  -> gateway render.rs 选择 text / markdown
  -> QQ OpenAPI /v2/users 或 /v2/groups 发送
```

反向主动推送链路也仍走本机 HTTP，也属于本次最终要拆除的内部业务通信：

```text
Core RSS / Todo reminder
  -> runtime/push.rs GatewayPushClient
  -> POST gateway /internal/push
  -> gateway/push.rs
  -> QQ OpenAPI 发送
```

因此，真正要拆的是同一进程内 Core 和 Gateway 之间的业务 HTTP、JSON、SSE、readiness probe、push HTTP 服务，而不是重写 QQ 接入或 Core 业务 flow。若保留 `/healthz`，它也应属于顶层应用的外部运维接口，而不是 Core/Gateway 组件间通信接口。

## 二、请求调用链

### 1. 统一进程启动

文件：`src/main.rs`

- `main()` 调用 `qq_maid_core::app::load_dotenv_files()` 和 `init_tracing()`。
- 构造 `CoreRuntime::from_config(core_config)`。
- `core_runtime.serve_with_shutdown(...)` 启动 Core HTTP。
- `wait_for_core_ready(&core_healthz_url, ...)` 使用 `reqwest` 轮询 `/healthz`。
- 就绪后调用 `qq_maid_gateway_rs::app::run_with_config(gateway_config)`。

当前行为明确依赖 Core HTTP 监听地址和 `/healthz` 就绪探测。进程内改造后，启动顺序仍需要保留 Core 组件装配先于 Gateway，但不应再通过 HTTP 判断 ready；组件初始化成功与否应由构造函数、handle 状态或顶层应用状态机表达。

### 2. QQ 原始事件入口

文件：`qq-maid-gateway-rs/src/gateway/protocol.rs`

主要函数和类型：

- `run_gateway_once(...)`
- `handle_envelope(...)`
- `GatewayEnvelope`
- `parse_c2c_message(...)`
- `parse_group_message(...)`

输入：

- QQ Gateway WebSocket `Message::Text` 或 `Message::Binary`。

输出：

- C2C 事件转为 `C2cMessage`。
- 群 at / 普通群消息转为 `GroupMessage`。

职责归属：

- 平台适配层。负责 QQ WebSocket、opcode、READY/RESUMED、heartbeat、IDENTIFY/RESUME、event type 分发。
- 不应进入 Core。

### 3. QQ 事件解析 DTO

文件：`qq-maid-gateway-rs/src/gateway/event.rs`

主要类型：

- `GatewayEnvelope { op, d, s, t, id }`
- `C2cMessage`
- `GroupMessage`
- `MessageReply`
- `Attachment`
- `GroupEventType`
- `RawC2cMessage`
- `RawGroupMessage`
- `RawAuthor`
- `RawMessageReply`

`C2cMessage` 字段：

```text
message_id
user_openid
content
reply
timestamp
attachments
```

`GroupMessage` 字段：

```text
message_id
group_openid
member_openid
content
reply
timestamp
attachments
event_type
author_is_bot
author_is_self
```

字段转换和降级：

- C2C `message_id` 来自 `id` / `message_id`，再退到 `event_id`。
- C2C `user_openid` 优先从 `author.user_openid`、`author.openid`、`author.member_openid`、顶层 `user_openid`、顶层 `openid` 获取。
- 群 `group_openid` 兼容旧字段 `group_id`。
- 群 `member_openid` 优先从 `author.member_openid`、`author.user_openid`、`author.openid`、顶层 `member_openid`、顶层 `user_openid` 获取。
- `author.id` 仅作为不可信旧事件兜底，并打脱敏 warn。
- `content` 缺失时默认空字符串并 trim。
- `reply` 只提取一层 `message_id`，来源为 `reply`、`quote` 或 `[CQ:reply,id=...]`。
- `Attachment::note()` 将附件降级为文本：`[附件 {content_type}: {filename} {url}]`，缺失值分别用 `unknown`、`unnamed`、`no-url`。

职责归属：

- `GatewayEnvelope`、`Raw*`、`C2cMessage`、`GroupMessage` 属于 QQ 平台适配层。
- `Attachment` 原始结构也属于 Gateway；Core 不应直接依赖 QQ 附件字段。
- `[CQ:reply]` 是历史兼容解析，不代表当前走 OneBot。

### 4. Gateway 过滤、去重、冷却和本地信号

文件：`qq-maid-gateway-rs/src/gateway/mod.rs`

主要函数和类型：

- `handle_c2c_message(...)`
- `handle_group_message(...)`
- `resolve_signals(...)`
- `MessageCache = HashMap<String, String>`
- `BotOutboundCache`

文件：`qq-maid-gateway-rs/src/gateway/group_filter.rs`

主要函数和类型：

- `should_ignore_group_message(...)`
- `should_process_group_message(...)`
- `GroupCooldowns`

文件：`qq-maid-gateway-rs/src/gateway/dedupe.rs`

主要函数和类型：

- `MessageDedupe`

职责和行为：

- C2C 使用 `MessageDedupe` 做 10 分钟 TTL 去重。
- 群消息过滤自身消息、机器人消息、空内容。
- 普通群消息按 `GroupMessageMode` 控制：`off`、`command`、`mention`、`active`。
- `GroupAtMessage` 始终处理。
- 普通群消息使用群级 3 秒、用户级 10 秒冷却。
- `reply_cache` 仅 C2C 使用，用 `message_id -> content` 短时回填 `reply.content`，当前没有 TTL 或容量上限。
- `BotOutboundCache` 记录机器人发出的群消息 ID，用于判断用户是否回复机器人消息，当前没有 TTL 或容量上限。

职责归属：

- 全部属于 Gateway 平台适配和交互策略，不应进入 Core。
- 进程内调用后仍应保留在 Gateway。

### 5. Gateway 到 Core 的内容拼接

文件：`qq-maid-gateway-rs/src/respond.rs`

主要函数：

- `build_respond_content(...)`
- `build_group_respond_content(...)`
- `build_respond_content_parts(...)`
- `append_attachment_notes(...)`

行为：

```text
无引用：
  原始消息 content
  附件备注逐行追加

有引用：
  [reply message_id=xxx]
  引用正文（仅当本地 cache 命中）
  [/reply]
  原始消息 content
  附件备注逐行追加
```

字段丢失和降级：

- 引用只保留 `message_id` 和可选本地回填正文。
- 附件结构化字段不进 HTTP DTO，只折叠为文本备注。
- 群聊没有像 C2C 一样的引用正文本地回填。

职责判断：

- 当前这是历史中转层和平台适配层的混合点。
- 1.0.0 拆 HTTP 时，短期可保留该字符串协议以保持行为稳定。
- 后续如果 Core 需要结构化引用/附件，应新建通用 `CoreMessageContext`，不能把 QQ raw attachment 直接塞进 Core。

### 6. Gateway HTTP 请求 DTO

文件：`qq-maid-gateway-rs/src/respond.rs`

类型：

- `RespondRequest`
- `UpstreamCheckRequest`
- `RespondClient`

`RespondRequest` 字段：

```text
scope_key
content
platform
event_type
user_id
group_id
guild_id
channel_id
message_id
timestamp
```

映射函数：

- `RespondRequest::from_c2c_message(...)`
- `RespondRequest::from_group_message(...)`

C2C 映射：

```text
scope_key = private:{user_openid}
content = build_respond_content(...)
platform = qq_official
event_type = c2c_message
user_id = user_openid
group_id = None
guild_id = None
channel_id = None
message_id = message.message_id
timestamp = message.timestamp
```

群映射：

```text
scope_key = group:{group_openid}
content = build_group_respond_content(...)
platform = qq_official
event_type = group_at_message / group_message
user_id = member_openid
group_id = group_openid
guild_id = None
channel_id = None
message_id = message.message_id
timestamp = message.timestamp
```

重要约束：

- 群聊 `scope_key` 必须保持群级目标语义，不能拆成成员分片，否则 session、RSS、Todo 等群级能力会改变归属。
- `user_id` 在群聊中只是成员身份，可能为 `None`。

职责判断：

- 这是内部 HTTP 传输 DTO，应在进程内改造完成后删除。
- 会话归属映射需要迁移到 `CoreRequest.conversation`；`scope_key` 不再由 Gateway 直接输入，而由 Core 从 `CoreConversation` 派生。

### 7. Core HTTP 接收 DTO

文件：`qq-maid-core/src/http/routes.rs`

类型：

- `HttpRespondRequest`
- `HttpDiagnosticAction`

行为：

- `HttpRespondRequest` 使用 `#[serde(deny_unknown_fields)]`。
- 未知字段直接 HTTP 400。
- `diagnostic = upstream_check` 时走 `run_upstream_check(...)`，不进入业务 flow，不创建 session。
- 普通请求通过 `impl From<HttpRespondRequest> for RespondRequest` 转成 Core 内部 `RespondRequest`。

字段保留：

- HTTP DTO 字段与 Gateway `RespondRequest` 基本一致，额外包含 `diagnostic`。

字段丢失：

- Gateway 响应端没有消费 Core 返回的 `metrics` 和 `usage`；serde 默认忽略未知响应字段。
- HTTP DTO 不接受 Core 内部 `RespondRequest` 的 `session_id`、`user_text`、`metadata`、`system_prompts` 等字段。

职责判断：

- `HttpRespondRequest` 是传输层 DTO，不是稳定领域模型。
- 1.0.0 若拆除内部 HTTP，可删除或仅作为可选外部调试 API 的边界；不应继续作为 Gateway -> Core 主通道。

### 8. Core 业务分发入口

文件：`qq-maid-core/src/runtime/respond.rs`

主要类型：

- `RustRespondService`
- `RespondStores`
- `RespondExecutors`
- `RespondServiceOptions`

主要函数：

- `RustRespondService::respond(...)`
- `RustRespondService::respond_transport(...)`

`respond_transport(...)` 分发顺序：

```text
1. req.effective_user_text()
2. 构造 SessionMeta(scope_key, user_id, group_id, guild_id, channel_id, platform)
3. get_active 当前活跃 session
4. pending operation 优先处理，除非是 pending-bypass session command
5. session command: /new /resume /list /clear /state /compact /help
6. get_or_create_active
7. translation command
8. weather command
9. train command
10. web search command；允许时可返回 RespondTransport::Stream
11. RSS flow
12. Todo flow
13. Memory flow
14. fallback 普通聊天 handle_chat
```

职责归属：

- Core 业务层。应直接复用，不应搬进 Gateway。
- 进程内改造的第一目标是让 Gateway 直接调用此处或一个更薄的 Core facade。

### 9. Core 内部 RespondRequest 不是目标 CoreRequest

文件：`qq-maid-core/src/runtime/respond/types.rs`

类型：

- `RespondRequest`
- `RespondPurpose`
- `RespondResponse`
- `RespondTransport`
- `RespondStream`
- `RespondStreamEvent`
- `ChatResponse`

`RespondRequest` 字段包含：

```text
session_id
model
purpose
user_text
content
scope_key
user_id / group_id / guild_id / channel_id
message_id / timestamp
platform / event_type
system_prompts
memory_context
knowledge_context
session_context
history_messages
session
metadata
```

判断：

- 该类型同时承载外部用户消息和 Core 内部 LLM 子任务参数。
- `system_prompts`、`memory_context`、`knowledge_context`、`history_messages`、`session`、`metadata` 是 Core 内部编排状态。
- 目标 `CoreRequest` 不应直接复用该类型，否则会把内部 prompt/session/LLM 细节暴露给 Gateway。

### 10. 命令、RAG、LLM 分发

文件：`qq-maid-core/src/runtime/respond/chat_flow.rs`

- `handle_chat(...)` 是普通聊天兜底路径。
- 空消息会写入 session 并返回 `empty_chat`。
- 群聊不做成员编号追问；私聊会使用成员映射。
- 普通聊天会调用 `KnowledgeIndex::search_context(&user_text)` 注入 RAG。
- 普通聊天会调用 `build_memory_context(&meta)` 注入长期记忆。
- 最终通过 `LlmChatService::respond(...)` 调 LLM。

文件：`qq-maid-core/src/runtime/respond/llm_service.rs`

- `LlmChatService::respond(...)`
- `build_respond_messages(...)`
- `RespondPurpose::{Chat, MemoryDraft, TodoParse, Compact}`

职责归属：

- RAG、prompt、memory、session、LLM 请求组装属于 Core。
- Provider 协议、fallback、SSE frame、Web Search 传输属于 `qq-maid-llm`，Core 只复用公开入口。

## 三、响应调用链

### 1. Core 非流式响应

Core 统一响应类型：`qq-maid-core/src/runtime/respond/types.rs::RespondResponse`

字段：

```text
ok
text
markdown
handled
session_id
command
diagnostics
metrics
usage
error
```

构造来源：

- 命令回复：`runtime/respond/common.rs::command_response(...)`
- 结构化命令双通道：`structured_command_body(...)`
- 普通聊天：`llm_service.rs::response_from_output(...)`
- Web Search：`search_flow.rs::build_web_search_success_response(...)`
- 错误：`http/routes.rs` 捕获 `LlmError` 或 timeout 后构造 `ok:false`

HTTP 输出：

- `http/routes.rs::respond(...)`
- 成功 JSON：`Json(*response).into_response()`
- 业务错误：HTTP 200 + `RespondResponse { ok:false, error: Some(err.as_info()), ... }`
- 请求超时：HTTP 200 + `ok:false`，错误为 `LlmError::timeout("request")`
- JSON payload 不合法：HTTP 400 + `invalid_request`

职责判断：

- `RespondResponse` 接近目标 `CoreResponse` 的业务结果部分。
- HTTP status、JSON 反序列化错误、Axum response 是传输职责，进程内调用后应删除或迁移为 `CoreError`。

### 2. Gateway 非流式解析和发送

文件：`qq-maid-gateway-rs/src/respond.rs`

类型：

- Gateway 侧 `RespondResponse`
- `RespondTransport::Json`
- `RespondError`

行为：

- `respond_c2c(...)` / `respond_group(...)` 发 HTTP POST。
- `Accept: text/event-stream, application/json`
- 非 2xx 转 `RespondError::Status { status, body }`。
- JSON decode 失败转 `RespondError::Http`。
- HTTP 成功后解析 Gateway 自己定义的 `RespondResponse`。

文件：`qq-maid-gateway-rs/src/gateway/mod.rs`

发送流程：

- C2C：`handle_c2c_message(...)`
- 群聊：`handle_group_message(...)`
- `ok:false` 通过 `respond_not_ok_to_qq_text(...)` 转安全错误文案。
- `render_respond_response(...)` 将 `RespondResponse` 渲染成 `OutboundMessage`。
- `send_outbound_with_fallback(...)` 或 `send_group_outbound_with_fallback(...)` 发 QQ。

文件：`qq-maid-gateway-rs/src/render.rs`

- `render_respond_response(response, enable_markdown, enable_image)`
- 有 `text` 才发送。
- `enable_markdown=true` 且 `markdown` 非空时优先 QQ Markdown，fallback 为 `text`。
- 否则发纯文本。
- `_enable_image` 当前未实际用于 RespondResponse 渲染。

文件：`qq-maid-gateway-rs/src/api.rs`

- C2C 文本：`send_c2c_text(...)` -> `/v2/users/{openid}/messages`
- 群文本：`send_group_text(...)` -> `/v2/groups/{group_openid}/messages`
- Markdown：`send_c2c_markdown(...)` / `send_group_markdown(...)`
- 图片：`send_c2c_image(...)`
- Markdown / image 发送失败会 fallback text。
- `extract_sent_message_id(...)` 尝试从 QQ 返回体提取已发送消息 ID。

文件：`qq-maid-gateway-rs/src/gateway/outbound.rs`

- `RuntimeRecordingSender`
- `RuntimeRecordingGroupSender`
- `record_qq_send_result(...)`

职责判断：

- Markdown/text/image 渲染和 QQ 发送是 Gateway 职责。
- `RespondResponse` 的 Gateway 侧复制类型是 HTTP 传输遗留，应由统一 `CoreResponse` 替换。

### 3. Core 流式响应

文件：`qq-maid-core/src/runtime/respond/search_flow.rs`

真实流式路径：

- 仅 `/查` / `/查询` / `/search` Web Search 命令。
- 需要 `allow_streaming=true` 且 `send_mode=streaming`。
- 空参数、超长参数仍返回 JSON。

调用链：

```text
respond_transport(...)
  -> parse_web_search_command(...)
  -> handle_web_search_command_stream(...)
  -> tokio::spawn stream_web_search_command(...)
  -> query_executor.query_stream(QueryRequest, delta_tx)
  -> RespondStreamEvent::Delta
  -> RespondStreamEvent::Final { RespondResponse }
```

文件：`qq-maid-core/src/http/routes.rs`

- `accepts_streaming(...)` 检查 `Accept` 是否包含 `text/event-stream`。
- `stream_response(...)` 将内部 `RespondStreamEvent` 编码为 SSE：
  - `event: delta`
  - `event: final`

职责判断：

- Web Search 流式生产是 Core/LLM 职责。
- SSE 编码是 HTTP 传输职责。
- 进程内调用后，可直接返回 `RespondStream` 或回调式事件，不需要 SSE frame。

### 4. Gateway 流式解析和发送

文件：`qq-maid-gateway-rs/src/respond.rs`

主要函数：

- `is_stream_response(...)`
- `spawn_respond_stream(...)`
- `take_sse_frame(...)`
- `parse_sse_frame(...)`
- `send_stream_final_error(...)`

行为：

- 通过 `Content-Type` 是否包含 `text/event-stream` 判断流式。
- 本地手写 SSE frame parser，只识别 `delta` 和 `final`。
- 其它 event 忽略。
- `[DONE]` 忽略。
- 读流失败时发送一个 `ok:false final` 给上层。
- 流结束但没有 final 时发送 `ok:false final`。

文件：`qq-maid-gateway-rs/src/gateway/streaming.rs`

主要函数：

- `build_streaming_buffered_response(...)`
- `collect_streaming_final_response(...)`
- `handle_streaming_respond_response(...)`

行为：

- 私聊不会逐条发送 delta，而是缓冲到最终文本后发一条 QQ 消息。
- 群聊也只收集最终响应后发一条。
- `ok:false final` 且有 buffered delta 时，私聊会尽量发部分内容；群聊会通过统一发送函数处理。
- 如果流提前结束且没有 final，当前实现只 warn 并不发送 buffered delta。注释写“流式异常结束时若有缓冲文本，仍尝试回发部分内容”，实际实现不完全一致。

流式与非流式行为差异：

- 非流式 HTTP 业务错误会稳定转用户可见错误文案。
- 流式提前结束且无 final 时可能静默不回。
- 流式目前只影响 `/查`，普通聊天仍走 JSON。
- 群聊流式没有逐 delta 发送，和私聊一致缓冲最终文本。

迁移建议：

- 拆 HTTP 时优先删除 Gateway 手写 SSE parser。
- CoreResponse 可以先只支持最终响应；Web Search 流式作为后迁移项。
- 若仍要保留流式能力，应使用进程内 `mpsc::Receiver<CoreResponseEvent>` 或 callback，而不是 SSE 文本。

### 5. 主动推送响应链路

文件：`qq-maid-core/src/runtime/push.rs`

类型：

- `GatewayPushClient`
- `GatewayPushTarget`
- `GatewayPushTargetType`
- `PushPayload`

行为：

- Core RSS / Todo reminder 不直接调用 QQ OpenAPI。
- 通过 reqwest POST Gateway `/internal/push`。
- 可带 `X-QQ-Maid-Push-Token`。

文件：`qq-maid-gateway-rs/src/gateway/push.rs`

类型：

- `PushRequest`
- `PushState`

行为：

- 默认监听 `127.0.0.1`。
- 校验 token。
- `target_type=private/group`。
- `message_type=text/markdown`。
- Markdown 失败 fallback text。
- 群推送成功后写入 `BotOutboundCache`，使用户回复机器人推送消息可被 mention 模式识别。

职责判断：

- 这是 Core -> Gateway 反向本机 HTTP 通讯。
- 它不属于 Gateway -> Core respond 主线，但属于 Core/Gateway 间内部业务 HTTP 通讯；1.0.0 目标下应迁移为进程内 `PushSink`，不能作为长期保留链路。

## 四、现有 DTO 清单

| 类型名 | 文件路径 | 所属层 | 用途 | 上游 | 下游 | 是否建议保留 | 原因 |
|---|---|---|---|---|---|---|---|
| `GatewayEnvelope` | `qq-maid-gateway-rs/src/gateway/event.rs` | Gateway 平台适配 | QQ WebSocket envelope | QQ Gateway | `parse_c2c_message` / `parse_group_message` | 保留 | QQ 协议边界类型 |
| `RawC2cMessage` | `qq-maid-gateway-rs/src/gateway/event.rs` | Gateway 平台适配 | 反序列化 QQ C2C payload | `GatewayEnvelope.d` | `C2cMessage` | 保留但私有 | 平台 raw DTO，不进 Core |
| `RawGroupMessage` | `qq-maid-gateway-rs/src/gateway/event.rs` | Gateway 平台适配 | 反序列化 QQ 群 payload | `GatewayEnvelope.d` | `GroupMessage` | 保留但私有 | 平台 raw DTO，不进 Core |
| `C2cMessage` | `qq-maid-gateway-rs/src/gateway/event.rs` | Gateway 归一化事件 | C2C 内部事件 | `parse_c2c_message` | `handle_c2c_message` | 保留 | Gateway 内部清晰边界 |
| `GroupMessage` | `qq-maid-gateway-rs/src/gateway/event.rs` | Gateway 归一化事件 | 群消息内部事件 | `parse_group_message` | `handle_group_message` | 保留 | Gateway 内部清晰边界 |
| `MessageReply` | `qq-maid-gateway-rs/src/gateway/event.rs` | Gateway 归一化事件 | 引用消息信息 | raw reply / quote / CQ | content 拼接 / group filter | 短期保留 | 未来可映射到通用引用上下文 |
| `Attachment` | `qq-maid-gateway-rs/src/gateway/event.rs` | Gateway 平台适配 | QQ 附件字段 | raw payload | 文本备注 | 保留在 Gateway | 不应直接进入 Core 通用 DTO |
| Gateway `RespondRequest` | `qq-maid-gateway-rs/src/respond.rs` | HTTP 传输 | POST `/v1/respond` JSON body | `C2cMessage` / `GroupMessage` | Core HTTP | 删除 | 进程内 `CoreRequest` 替代 |
| `UpstreamCheckRequest` | `qq-maid-gateway-rs/src/respond.rs` | HTTP 传输 | `/ping check` 诊断请求 | Gateway ping | Core HTTP | 删除 | 改为 `CoreService::upstream_check` |
| Gateway `RespondResponse` | `qq-maid-gateway-rs/src/respond.rs` | HTTP 传输 | 解析 Core JSON / SSE final | Core HTTP | render/send | 删除 | 用共享 `CoreResponse` 替代 |
| Gateway `RespondTransport` | `qq-maid-gateway-rs/src/respond.rs` | HTTP 传输 | JSON / SSE 分支 | reqwest response | gateway send | 删除 | 进程内响应不需要 HTTP transport |
| Gateway `RespondStreamEvent` | `qq-maid-gateway-rs/src/respond.rs` | HTTP/SSE 传输 | 解析 SSE delta/final | SSE parser | streaming.rs | 删除 | 改为 Core 进程内 event 或后迁移 |
| `HttpRespondRequest` | `qq-maid-core/src/http/routes.rs` | Core HTTP facade | `/v1/respond` 接收 DTO | Gateway HTTP | Core `RespondRequest` | 删除或降级外部调试 | 1.0.0 内部主链路不再走 HTTP |
| `HttpDiagnosticAction` | `qq-maid-core/src/http/routes.rs` | Core HTTP facade | `/ping check` 诊断动作 | Gateway HTTP | `run_upstream_check` | 删除 | 改为 Core handle 方法 |
| Core `RespondRequest` | `qq-maid-core/src/runtime/respond/types.rs` | Core 内部编排 | 业务 flow 和 LLM 子任务参数 | HTTP facade / 内部 flow | `RustRespondService` / `LlmChatService` | 保留但建议改名 | 更像 `RespondContext`，不适合作为外部 `CoreRequest` |
| `RespondPurpose` | `qq-maid-core/src/runtime/respond/types.rs` | Core 内部编排 | 区分聊天、记忆、Todo、压缩 | Core flow | LLM message builder | 保留 | Core 内部模型路由需要 |
| Core `RespondResponse` | `qq-maid-core/src/runtime/respond/types.rs` | Core 业务结果 | 统一业务响应 | 各 flow | HTTP / Gateway | 保留并演进为 `CoreResponse` | 已接近目标响应 |
| Core `RespondTransport` | `qq-maid-core/src/runtime/respond/types.rs` | Core/HTTP 传输 | JSON 或 Stream | `respond_transport` | HTTP route | 短期保留 | 流式迁移前可复用，最终不应表示 HTTP |
| Core `RespondStreamEvent` | `qq-maid-core/src/runtime/respond/types.rs` | Core 流式事件 | Web Search delta/final | search flow | HTTP SSE | 后迁移 | 可改名为 `CoreResponseEvent` |
| `SessionMeta` | `qq-maid-core/src/storage/session.rs` | Core 领域上下文 | session 归属和 scope 推断 | `RespondRequest` | storage/session flow | 保留 | Core 稳定领域模型 |
| `PushPayload` | `qq-maid-core/src/runtime/push.rs` | Core->Gateway HTTP | 主动推送请求 body | RSS/Todo | gateway `/internal/push` | 删除 | 用 `PushIntent` / `PushSink` 替代 |
| `PushRequest` | `qq-maid-gateway-rs/src/gateway/push.rs` | Gateway HTTP | `/internal/push` 接收 DTO | Core push HTTP | QQ send | 删除 | 进程内 push 不需要 HTTP DTO |
| `OutboundMessage` | `qq-maid-gateway-rs/src/render.rs` | Gateway 出站 | text/markdown/image 发送意图 | Core response render | QQ API | 保留 | Gateway 发送层合适边界 |
| `C2cReplyTarget` / `GroupReplyTarget` | `qq-maid-gateway-rs/src/api.rs` | Gateway 出站 | QQ 发送目标 | Gateway handler | QQ API | 保留 | 平台发送目标 |

## 五、内部 HTTP 职责清单

### 可删除职责

- Gateway -> Core JSON 序列化和反序列化。
- Gateway 侧 `QQ_MAID_RESPOND_URL` 配置和 `RespondClient` reqwest POST。
- Core `HttpRespondRequest` 和 `/v1/respond` 内部业务入口。
- Core HTTP SSE 编码。
- Gateway HTTP SSE parser。
- `Accept: text/event-stream, application/json` 协商。
- HTTP status 到 Gateway `RespondError::Status` 的转换。
- 统一进程启动时的 `/healthz` HTTP ready probe。
- Core -> Gateway `/internal/push` HTTP 服务、push token、push host/port。
- Core 组件自身承载的内部 `/healthz` 服务；如需保留健康检查，应上移为顶层应用的进程级 HTTP endpoint。

### 需迁移职责

- Core 请求超时：现在在 `http/routes.rs` 用 `tokio::time::timeout` 包住 `service.respond_transport(...)`。进程内调用后需要迁移到 `CoreService` facade 或 Gateway 调用侧，避免一次 Core 调用无限挂起。
- 错误转换：`LlmError` 到用户可见安全文案的映射目前分散在 Core HTTP 和 Gateway respond.rs。需要明确 `Err(CoreError)` 表示技术异常，`Ok(CoreResponse)` 表示业务结果；迁移期 `ok/error` 只能作为旧响应兼容字段。
- Upstream check：现在通过 HTTP `diagnostic=upstream_check`。应迁移为 `CoreService::upstream_check()`。
- Health snapshot：现在 Core `/healthz` 返回 provider、model、stream、upstream。`/ping` 和顶层进程级 `/healthz` 都应改为读取进程内 `CoreService::health_snapshot()` / Gateway runtime snapshot。
- 流式事件：如果保留 Web Search 流式，需要迁移为进程内 `CoreResponseEvent`，不再通过 SSE 文本。
- 指标和日志：HTTP 层的 success/error/timeout 日志要迁移到 Core facade，避免拆 HTTP 后失去可观测性。
- `reply_cache` 和 `BotOutboundCache` 清理：这是现有风险，但不属于拆 HTTP 主任务，建议单独治理，避免混入主迁移。

### 暂时保留或逐项确认职责

- Core 的 Axum Web 控制台 `/console/` 和 `/api/v1/markdown/render` 如果 1.0.0 仍需要本地控制台，可以作为独立可选 HTTP 服务保留。
- 外部诊断 HTTP 如果仍需要给 `botctl`、`validate-runtime.sh` 或外部运维系统使用，可以保留只读诊断端口，但必须由顶层应用暴露进程级状态，不能由 Core/Gateway 组件互相调用。
- `/healthz` 如保留，应改为顶层进程健康接口，汇总 Core、Gateway、RSS/Todo scheduler、provider upstream 等进程内状态；组件之间不允许通过该接口通信。
- `RespondTransport::Stream` 可在迁移流式前短期保留为 Core 内部 event transport，但应与 HTTP/SSE 解耦。

## 六、目标架构建议

### 1. 目标调用接口

正式接口只保留一层，避免 `CoreService` 和 Gateway `CoreResponder` 双重抽象长期并存。建议二选一：

- Core 暴露 `CoreHandle`，Gateway 持有 `Arc<dyn CoreService>`。
- Gateway 消费侧定义 `CoreResponder` trait，Core `CoreHandle` 实现该 trait。

报告推荐第一种：由 `qq-maid-core` 提供 `CoreHandle` / `CoreService`，Gateway 只依赖这个能力，不再额外包装 `HttpCoreResponder` / `InProcessCoreResponder` 作为正式架构。

```rust
#[async_trait::async_trait]
pub trait CoreService: Send + Sync {
    async fn respond(&self, request: CoreRequest) -> Result<CoreResponse, CoreError>;
    async fn upstream_check(&self) -> Result<(), CoreError>;
    fn health_snapshot(&self) -> CoreHealthSnapshot;
}
```

如果流式后续保留，可后置增加：

```rust
async fn respond_stream(
    &self,
    request: CoreRequest,
) -> Result<CoreResponseStream, CoreError>;
```

第一阶段不建议把流式塞进 `respond(...)`，否则会把最难迁移的 SSE 行为拖进初版边界。

### 2. CoreRequest 最小字段

不要设计大量 `Option` 的万能 DTO。当前最小可用边界应避免 `scope_key` 和 target 双重事实来源：Gateway 传入会话归属事实，Core 负责从 `conversation` 派生 `scope_key` 和 `SessionMeta`。

```rust
pub struct CoreRequest {
    pub text: String,
    pub platform: Platform,
    pub actor: CoreActor,
    pub conversation: CoreConversation,
}

pub enum Platform {
    QqOfficial,
    OneBot,
}

pub struct CoreActor {
    pub user_id: Option<String>,
}

pub enum CoreConversation {
    Private { peer_id: String },
    Group { group_id: String },
}
```

派生规则：

```text
CoreConversation::Private { peer_id } -> scope_key = private:{peer_id}
CoreConversation::Group { group_id }   -> scope_key = group:{group_id}
```

`Platform` 第一版至少要包含 `QqOfficial`，因为当前 Core `SessionMeta.platform`、诊断和 metadata 已经依赖平台语义；不能继续靠空字符串 fallback 到 `qq`。`OneBot` 是否第一版实现可按实际需要决定，但 enum 留出明确表达比裸字符串更稳定。

可选增强但建议分阶段加入：

```rust
pub struct CoreMessageContext {
    pub reply: Option<CoreReplyContext>,
    pub attachments: Vec<CoreAttachmentNote>,
}

pub struct CoreReplyContext {
    pub message_id: String,
    pub text: Option<String>,
}

pub struct CoreAttachmentNote {
    pub kind: String,
    pub filename: Option<String>,
    pub url: Option<String>,
}
```

第一阶段可继续传 `text = build_respond_content(...)`，把引用和附件维持为现有文本协议。这样可先拆 HTTP 通讯，不同时改变业务语义。

### 3. CoreResponse 最小字段和错误通道

正式接口应避免两层成功状态。建议收敛为：

- `Err(CoreError)`：超时、存储失败、provider 调用失败、内部状态异常等技术异常。
- `Ok(CoreResponse)`：Core 正常处理出的业务结果。

`ok` / `error` 可在迁移期保留，以兼容旧 `RespondResponse` 和旧 Gateway 渲染逻辑，但必须标注为临时字段，最终不应要求调用者同时判断 `Result` 和 `ok`。

```rust
pub struct CoreResponse {
    // 迁移期兼容字段，最终应由 Result 成功/失败表达。
    pub ok: bool,
    pub text: Option<String>,
    pub markdown: Option<String>,
    pub handled: bool,
    pub session_id: Option<String>,
    pub command: Option<String>,
    pub diagnostics: Option<serde_json::Value>,
    // 迁移期兼容字段，技术错误应优先进入 Err(CoreError)。
    pub error: Option<CoreErrorInfo>,
}
```

`metrics` 和 `usage` 建议继续保留在 Core 内部日志和诊断中；是否暴露给 Gateway 取决于 `/ping` 是否需要展示。Gateway 当前没有消费它们，因此 `CoreResponse` 初版可不带。

### 4. 字段归属

应继续留在 Gateway 的字段和行为：

- QQ raw envelope、opcode、event type 原文。
- `message_id` 用于去重和 QQ 被动回复。
- `timestamp` 原文。
- `author_is_bot`、`author_is_self`。
- 群消息触发策略、at/回复机器人判断、冷却。
- QQ access token、appid、secret、sandbox、intent。
- Markdown/image/text 发送 fallback。
- `reply_cache`、`BotOutboundCache`。
- QQ OpenAPI URL 和 payload。

应进入 Core 稳定领域模型：

- 当前用户输入文本。
- `Platform`。
- actor user id。
- `CoreConversation` private/group 归属，并由 Core 派生 `scope_key`。
- session、pending、memory、todo、RSS、knowledge、prompt、LLM 分发。

不应进入通用 DTO 的平台专属字段：

- `C2C_MESSAGE_CREATE`、`GROUP_AT_MESSAGE_CREATE`、`GROUP_MESSAGE_CREATE` 原始事件名。
- `guild_id`、`channel_id`，除非 1.0.0 明确恢复频道模型。
- QQ 附件 raw 字段。
- HTTP header、Content-Type、Accept、status code。
- `/internal/push` token。

### 5. HTTP adapter 的定位

因为 1.0.0 目标是拆掉 Core 和 Gateway 之间的 HTTP 通讯，HTTP adapter 只建议短期存在：

- 初期可以临时保留旧 HTTP 代码做对照，但不新增正式配置或编译期开关。
- 回退优先依赖独立 commit 和 `git revert`，避免把 HTTP fallback 固化为产品能力。
- 1.0.0 默认路径应只使用进程内 `CoreHandle`。
- 等进程内链路覆盖 C2C、群、ping check、RSS/Todo push 后，删除 HTTP adapter、`QQ_MAID_RESPOND_URL` 和 `/internal/push`。
- 不应把 HTTP adapter 写成长期双轨架构；每个迁移阶段都要有明确的删除条件。

## 七、分阶段迁移计划

### 阶段 1：建立 CoreRequest / CoreResponse 和 CoreHandle

目标：

- 新增 `CoreRequest`、`CoreResponse`、`CoreError`、`CoreHandle` / `CoreService`。
- `CoreRequest` 使用 `platform + actor + conversation`，Core 内部派生 `scope_key`。
- 不改变现有 HTTP 行为。
- `CoreHandle` 内部复用 `RustRespondService::respond_transport(req, false)`。
- 明确 `Err(CoreError)` 与迁移期 `CoreResponse.ok/error` 的边界。

涉及文件：

- `qq-maid-core/src/runtime/respond/types.rs` 或新增 `qq-maid-core/src/core_service.rs`
- `qq-maid-core/src/runtime/respond.rs`
- `qq-maid-core/src/app/mod.rs`
- `qq-maid-core/src/lib.rs`

风险：

- `RustRespondService::new(...)` 当前在 HTTP handler 中每次构造。需要抽 helper，避免进程内 facade 和 HTTP route 复制装配逻辑。
- Core 内部 `RespondRequest` 命名容易和新 `CoreRequest` 混淆。
- `scope_key` 必须只能由 Core 从 `CoreConversation` 派生，不能再由 Gateway 输入。

测试：

- 新增 Core facade 单测：C2C 最小文本 -> session command / empty chat / 普通 chat mock provider。
- 验证 `Private { peer_id }` 和 `Group { group_id }` 派生的 `scope_key`。
- 验证 `Platform::QqOfficial` 进入 `SessionMeta.platform`。
- 验证技术异常进入 `Err(CoreError)`，迁移期 `ok/error` 只用于兼容响应。
- 复跑 `cargo test -p qq-maid-core`。

回退方式：

- 删除新增 facade，不影响现有 HTTP 链路。

### 阶段 2：Gateway 接入 CoreHandle，迁移最简单非流式链路

目标：

- Gateway 持有 Core 提供的 `Arc<dyn CoreService>` 或 `CoreHandle`。
- 先迁移 C2C 非 `/ping`、非流式 JSON 最小链路。
- 暂不新增正式 HTTP fallback 开关；旧 HTTP 代码只作为迁移期未删除代码存在。

涉及文件：

- `qq-maid-gateway-rs/src/respond.rs`
- `qq-maid-gateway-rs/src/gateway/mod.rs`
- `qq-maid-gateway-rs/src/app/mod.rs`
- `src/main.rs`

风险：

- Gateway crate 当前依赖方向允许 gateway -> core，但要避免把 Core store/executor 泄漏到 gateway 深处。
- 需要调整 `run_with_config` 入口参数，把 Core handle 注入 Gateway。

测试：

- `qq-maid-gateway-rs/src/respond.rs` 映射测试。
- `gateway/mod.rs` handler mock responder 测试。
- `cargo test -p qq-maid-gateway-rs`。

回退方式：

- 通过 revert 阶段 2 提交回到 HTTP 调用，不新增长期配置或编译期开关。

### 阶段 3：迁移普通聊天和命令链路

目标：

- C2C 和群聊普通消息、session 命令、todo/memory/rss/weather/train/translation 命令全部通过进程内 Core responder。
- 保持 Gateway 原有过滤、去重、冷却、render、QQ send 不变。

涉及文件：

- `qq-maid-gateway-rs/src/gateway/mod.rs`
- `qq-maid-gateway-rs/src/respond.rs`
- `qq-maid-core/src/runtime/respond/*`

风险：

- 群聊 `scope_key` 必须由 Core 从 `CoreConversation::Group { group_id }` 派生为 `group:{group_openid}`。
- 群成员 `user_id=None` 的 pending/todo/memory 行为要与当前 HTTP 一致。
- Gateway 必须传 `Platform::QqOfficial`，不能依赖 Core 空平台 fallback。

测试：

- Core 现有 `runtime/respond/tests/*`。
- Gateway group mode / dedupe / render 测试。
- 增加跨模块集成测试：GroupMessage -> CoreRequest -> CoreResponse -> OutboundMessage。

回退方式：

- 恢复 Gateway responder 为 HTTP 实现。

### 阶段 4：迁移引用、身份、附件上下文字段

目标：

- 第一小步仍复用现有 `build_respond_content(...)` 文本协议。
- 第二小步再引入 `CoreMessageContext`，将引用和附件以通用结构传入 Core。
- Core 是否消费结构化上下文由业务需求决定，不能影响已确认 session/pending 数据格式。

涉及文件：

- `qq-maid-gateway-rs/src/gateway/event.rs`
- `qq-maid-gateway-rs/src/respond.rs`
- `qq-maid-core/src/core_service.rs` 或 equivalent
- `qq-maid-core/src/runtime/respond.rs`

风险：

- 引用正文当前 C2C 依赖本地短缓存，群聊没有同等能力。
- 附件目前只是文本备注，改结构化后 prompt 和命令解析可能变化。
- `timestamp` 不应驱动 Core 时间语义，避免改变 Todo/查询解析。

测试：

- 引用消息 content 拼接回归。
- 附件备注回归。
- 群聊成员身份缺失回归。

回退方式：

- 退回 `text` 字符串协议。

### 阶段 5：迁移 `/ping check`、health snapshot 和顶层健康接口

目标：

- `/ping check` 直接调用 `CoreService::upstream_check()`。
- `/ping` 诊断直接读取 `CoreService::health_snapshot()`。
- 删除统一入口对 Core `/healthz` ready probe 的依赖。
- 如外部运维仍需要 `/healthz`，改为由顶层应用暴露进程级健康接口，汇总 Core handle、Gateway runtime、RSS/Todo scheduler 和 provider upstream 状态。
- 明确禁止 Core/Gateway 组件之间通过该顶层 `/healthz` 通信。

涉及文件：

- `src/main.rs`
- `qq-maid-gateway-rs/src/gateway/ping/*`
- `qq-maid-gateway-rs/src/respond.rs`
- `qq-maid-core/src/app/mod.rs`

风险：

- `/ping` 当前展示依赖 healthz JSON 结构。
- Upstream check 必须保持“不创建 session、不触发业务 flow”的约束。
- 顶层 `/healthz` 的 schema 需要兼顾外部运维脚本和组件内部状态边界。

测试：

- gateway ping tests。
- Core upstream check mock provider 测试。
- 顶层进程级 health snapshot 聚合测试。
- 启动顺序测试：Core handle 构造成功后即可启动 Gateway，不再 HTTP 轮询 ready。

回退方式：

- 回退到上一阶段的进程内 Core handle；不恢复组件间 `/healthz` 调用。外部顶层 `/healthz` 可独立保留。

### 阶段 6：迁移流式响应

目标：

- 删除 Gateway SSE parser。
- Core 流式 Web Search 通过进程内 `CoreResponseEvent` 传输。
- 或在 1.0.0 先统一降级为最终响应，后续再恢复进程内流式。

涉及文件：

- `qq-maid-core/src/runtime/respond/search_flow.rs`
- `qq-maid-core/src/http/routes.rs`
- `qq-maid-gateway-rs/src/respond.rs`
- `qq-maid-gateway-rs/src/gateway/streaming.rs`

风险：

- 当前流式提前断流行为与注释不一致。
- Gateway 实际缓冲最终文本发送，用户可见收益有限。
- Web Search 流式涉及 `qq-maid-llm` 的 `web_search_stream`。

测试：

- 流式 delta + final。
- 流式 final ok:false。
- 流提前结束且有 buffered delta。
- 私聊和群聊一致性。

回退方式：

- 对 `/查` 强制非流式，保留业务可用性。

### 阶段 7：迁移发送后错误回退

目标：

- CoreResponse 错误到 QQ 文案转换统一化。
- Markdown/image fallback 保持 Gateway 发送层负责。

涉及文件：

- `qq-maid-gateway-rs/src/gateway/mod.rs`
- `qq-maid-gateway-rs/src/gateway/group_filter.rs`
- `qq-maid-gateway-rs/src/respond.rs`
- `qq-maid-gateway-rs/src/api.rs`

风险：

- 旧 HTTP `RespondError::Status` / `RespondError::Http` 的用户可见文案需要迁移到 `CoreError`。
- 技术异常和业务响应不能长期双通道并存。

测试：

- Markdown fallback。
- Core error fallback 文案。
- 超时、provider、storage、config 等 `CoreError` 到 QQ 文案。

回退方式：

- revert 阶段 7 提交，恢复上一阶段错误处理。

### 阶段 8：迁移 Core -> Gateway 主动推送 HTTP

目标：

- 新增 `PushSink` trait。
- Core RSS/Todo reminder 只产生 `PushIntent`。
- 统一进程装配时把 Gateway push sender 注入 Core。
- 删除 `/internal/push` HTTP 和 `GatewayPushClient` reqwest 实现。

涉及文件：

- `qq-maid-core/src/runtime/push.rs`
- `qq-maid-core/src/runtime/rss/scheduler.rs`
- `qq-maid-core/src/runtime/todo_reminder.rs`
- `qq-maid-gateway-rs/src/gateway/push.rs`
- `qq-maid-gateway-rs/src/gateway/mod.rs`
- `src/main.rs`

风险：

- RSS/Todo scheduler 生命周期当前在 Core runtime 内部 spawn。
- Gateway API client 和 auth 在 Gateway runtime 内构造，注入方向需要设计清楚。
- 群推送成功后必须继续写入 `BotOutboundCache`。

测试：

- RSS scheduler push mock。
- Todo reminder push mock。
- Gateway push sender markdown fallback。
- 群 push 写 outbound cache。

回退方式：

- 保留 `/internal/push` 到最后，确认进程内 push 稳定后删除。

### 阶段 9：删除旧 HTTP DTO、SSE 和内部服务

目标：

- 删除 Gateway `RespondClient` HTTP 实现、`QQ_MAID_RESPOND_URL`、Gateway SSE parser。
- 删除 Core `/v1/respond` 内部业务入口，或仅保留外部调试 API 且不被 Gateway 使用。
- 删除 `/internal/push`。
- 删除 `LLM_SERVER_HOST` / `LLM_SERVER_PORT` 对内部主链路的依赖。
- 删除 Core 组件级内部 `/healthz`；如需健康检查，保留或新增顶层进程级 `/healthz`。
- 更新 README、runtime `.env.example`、脚本和诊断文档。

涉及文件：

- `qq-maid-gateway-rs/src/respond.rs`
- `qq-maid-core/src/http/routes.rs`
- `qq-maid-core/src/app/mod.rs`
- `src/main.rs`
- `runtime/.env.example`
- `README.md`
- `qq-maid-core/README.md`
- `qq-maid-gateway-rs/README.md`
- `runtime/README.md`
- `scripts/*`

风险：

- 部署脚本和诊断脚本可能仍 curl `/healthz` 或 POST `/v1/respond`。
- 若 `/healthz` 被外部运维使用，需要同步切到顶层进程级 schema，避免误删外部健康能力。
- 本地 Web 控制台若依赖 `/v1/respond` 调试面板，需要同步移除或改为独立调试接口。

测试：

- `cargo fmt --all -- --check`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo test --workspace --all-features`
- `cargo build --workspace --release --all-features`
- 脚本 `bash -n scripts/*.sh`
- 本地启动验证。

回退方式：

- 这是删除阶段，应在前面阶段稳定后单独提交；回退为 revert 该提交。

## 八、风险排序

### 高风险

- 流式响应：当前只有 `/查` 使用 SSE，但 Gateway 手写 parser、Core SSE 编码、提前断流行为不一致。建议后迁移，或 1.0.0 先统一非流式。
- Core -> Gateway 主动推送：RSS/Todo scheduler 在 Core 内 spawn，Gateway 发送能力在 Gateway runtime 内。拆 `/internal/push` 需要重新设计注入方向。
- 请求超时：Core HTTP 当前包了一层 request timeout；进程内调用如果不迁移 timeout，Gateway 任务可能长期等待。
- 顶层健康接口：外部 `/healthz` 若继续存在，必须汇总进程内组件状态，不能退化成 Core/Gateway 之间新的 HTTP 通信通道。

### 中高风险

- 引用消息：当前 C2C 引用正文依赖本地短缓存，群聊无正文回填。结构化迁移容易改变 prompt 输入。
- 身份字段：Core 派生后的群聊 `scope_key` 是群级，`user_id` 是成员级且可缺失；pending、memory、todo 都依赖这些边界。
- 消息缓存：`reply_cache` 和 `BotOutboundCache` 无 TTL/容量，这是独立运行风险；不应混入 HTTP 拆除主提交，但后续治理时要避免影响回复机器人消息触发。
- 错误回退：HTTP 非 2xx、HTTP 200 `ok:false`、SSE final `ok:false`、SSE 无 final、QQ 发送失败分别有不同处理路径，需要统一。

### 中风险

- Markdown：Core 普通聊天可能返回 `markdown`，Gateway `enable_markdown=true` 时优先 QQ Markdown，失败再 fallback。进程内 DTO 不应把 Markdown 决策搬到 Core。
- `/ping`：当前本地 `/ping` 同时依赖 gateway runtime、Core healthz、upstream check。拆 HTTP 后要保持诊断信息完整，并改为读取进程内状态。
- Web 控制台：`runtime/static/index.html` 当前有 `/v1/respond` 调试入口。删除内部 HTTP 后要决定是否保留外部调试能力。

### 低风险

- 普通非流式 C2C 文本：最适合首条迁移。
- 命令类 JSON 响应：Core 已统一返回 `RespondResponse`。
- QQ 发送层：可基本原样保留，只替换上游 response 类型。

## 九、建议优先实施的第一阶段

建议第一阶段只做文档和类型/接口铺垫，不改用户可见行为：

1. 在 Core 新增 `CoreRequest`、`CoreResponse`、`CoreError`、`CoreHandle` / `CoreService`。
2. 新增 `CoreHandle` / `CoreService` facade，把 `CoreRequest` 转为现有 Core 内部 `RespondRequest`。
3. 复用 `RustRespondService::respond_transport(req, false)`，不启用流式。
4. 抽出 Core respond service 装配 helper，避免 HTTP route 和进程内 facade 双重构造。
5. Gateway 暂不接入新接口。
6. 添加单测覆盖：
   - `CoreConversation::Private { peer_id }` -> `private:{peer_id}` 派生。
   - `CoreConversation::Group { group_id }` -> `group:{group_id}` 派生。
   - `Platform::QqOfficial` 进入 Core `SessionMeta.platform`。
   - `upstream_check` 不创建 session。
   - `CoreResponse` 从 `RespondResponse` 转换时保留 text/markdown/command。
   - 技术错误进入 `Err(CoreError)`，迁移期 `ok/error` 只作为旧响应兼容。

这个阶段可独立提交、可回退，不触碰 QQ Gateway 主循环和发送链路。它为下一阶段把 Gateway responder 从 HTTP 换成进程内调用提供稳定接口。

如果希望更激进地贴近 1.0.0，也可以在同阶段把 Core 内部 `RespondRequest` 改名为 `RespondContext`，但这会扩大改动面，建议单独提交。

后续真正实施时，第一批代码提交还需要特别守住两个边界：

- Gateway 不应直接构造 `RustRespondService`，也不应持有 Core store/executor；统一由 Core 暴露小型 handle。
- HTTP route 当前承担的 `request_timeout_seconds` 超时不能丢失，进程内 facade 应保留同等超时语义。

## 十、待确认事项

- 1.0.0 是否仍保留本地 Web 控制台。如果保留，`/console/` 和 Markdown render HTTP 可以独立存在；如果不保留，可与内部 HTTP 一起删除。
- 1.0.0 是否需要外部 HTTP 调试 `/v1/respond`。如果不需要，`HttpRespondRequest`、SSE、respond debug UI 都可删除。
- 1.0.0 是否保留 Web Search 流式用户体验。当前 QQ 侧也是缓冲最终文本发送，删除 SSE 后先降级非流式成本较低。
- `CoreRequest` 是否第一版就结构化引用/附件。建议先保留现有文本协议，等 HTTP 拆完再结构化。
- `/healthz` 是否仍被外部运维系统使用。若使用，应改为顶层应用暴露的进程级健康接口，汇总 Gateway、Core、scheduler、provider 状态；不得由组件之间调用。
- RSS/Todo scheduler 和 Gateway sender 的注入方向需要在实现前确定：是 Core 持有 `PushSink`，还是统一 app 层持有 scheduler 并注入 Gateway sender。

## 子 Agent 结论汇总

本次按仓库要求调用了多个子 agent 并交叉验证：

- `Lagrange`：分析 Gateway 请求入口、QQ 事件解析、content/附件/引用/身份字段和 HTTP DTO。
- `Gauss`：分析 Core `/v1/respond` 接收、内部 DTO、分发顺序、session/memory/todo/search/RAG/LLM 复用点。
- `Zeno`：分析 Core 响应到 Gateway 发送链路、JSON/SSE、Markdown、缓存、主动/被动发送和风险。
- `Helmholtz`：补充 1.0.0 不强求早期兼容时的拆 HTTP 目标、最小 DTO 和最终可删除链路。
- `Goodall`：确认分析报告适合放入 `docs/tasks/done/`，建议执行 `git diff --check`。
- `Hooke`：独立 review 迁移计划，补充测试范围、timeout 语义、启动/关停、healthz 和第一阶段边界风险。

子 agent 结论已与本地源码阅读交叉核对，未把未经验证的结论直接作为最终方案。

## 验证说明

本次只新增分析文档，未修改 Rust 代码，未执行格式化、构建或单元测试。

建议提交前至少执行：

```bash
git diff --check
```

本报告未读取、写入或输出真实 `.env`、token、AppSecret、openid、群 ID、私聊内容、SQLite 数据或日志。
