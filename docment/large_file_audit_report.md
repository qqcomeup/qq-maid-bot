
# 任务：分阶段拆分超长 Rust 源文件并降低模块维护成本

## 背景

项目中目前存在多个超过 1000 行的 Rust 源文件。静态审计发现，这些文件普遍存在以下问题：

* 多种职责集中在同一文件；
* 重复的数据库查询、状态更新或测试构造逻辑；
* Gateway 私聊、群聊、流式响应和发送统计存在重复；
* 部分测试支撑文件过大，fixture 和 mock 工厂层级较深；
* 部分模块虽然超过 1000 行，但结构仍较清晰，不应仅因行数进行机械拆分。

本次任务的目的不是单纯消灭超过 1000 行的文件，而是：

1. 按职责和变化原因建立清晰模块边界；
2. 降低后续新增功能和修复 Bug 时的回归风险；
3. 消除明显重复代码；
4. 保持所有现有对外行为、接口、持久化格式和业务语义不变；
5. 采用分阶段、小步提交方式执行，避免一次性重排整个项目。

涉及的主要文件包括：

* `qq-maid-gateway-rs/src/gateway/mod.rs`
* `qq-maid-gateway-rs/src/gateway/ping.rs`
* `qq-maid-llm/src/storage/todo.rs`
* `qq-maid-llm/src/runtime/weather/mod.rs`
* `qq-maid-common/src/time_context.rs`
* `qq-maid-llm/src/runtime/respond/tests/support.rs`
* `qq-maid-llm/src/runtime/respond/tests/todo.rs`
* `qq-maid-llm/src/storage/rss.rs`
* `qq-maid-llm/src/storage/session.rs`

## 总体目标

完成一次纯结构性、低风险的模块整理，使以下效果成立：

* Gateway 主入口不再承载大量事件解析、消息处理、流式响应和发送统计细节；
* Ping 的状态采集、健康判断、Markdown 展示和 healthz 探测相互独立；
* Todo、Weather、Time Context 按自然职责拆分；
* 测试 mock、fixture、seed 和回复生成逻辑不再集中在单个超长文件；
* RSS 和 Session 仅做必要的轻量去重，不为了减少行数强行拆分；
* 所有用户可见行为、命令输出、HTTP 接口、数据库 schema 和持久化语义保持兼容；
* 所有测试、构建、格式化和静态检查通过。

## 执行原则

1. 先阅读仓库根目录及相关目录中的：

   * `AGENTS.md`
   * `README.md`
   * crate 级 README
   * Makefile、Justfile、CI 配置和测试约定
2. 使用搜索确认实际调用链、模块边界和可见性要求。
3. 以下文件路径和模块名仅为建议，最终以仓库实际结构为准。
4. 每次只处理一个独立模块或一个明确阶段。
5. 每个阶段完成后立即运行对应测试。
6. 不要在一次提交中同时重构多个高风险生产模块。
7. 若发现审计报告与仓库实际代码不一致，以仓库事实为准，并在完成报告中说明。
8. 不以“文件必须低于 1000 行”为硬目标。
9. 优先拆分职责，不为消除少量重复引入复杂泛型、宏或难读抽象。
10. 本任务原则上不修改业务逻辑；若发现现有 Bug，只记录，不要顺手修复，除非该 Bug 会阻止结构拆分或测试通过。

---

# 第一阶段：整理 Ping 模块

## 目标

将当前 `gateway/ping.rs` 中混合的以下职责拆开：

* Ping 命令识别和总入口；
* Gateway 运行时状态；
* LLM healthz 探测；
* Ping 健康评估；
* Markdown 报告渲染；
* 时间展示辅助；
* 测试。

建议结构：

```text
qq-maid-gateway-rs/src/gateway/ping/
  mod.rs
  status.rs
  assess.rs
  render.rs
  healthz.rs
  tests.rs
```

实际文件命名可根据仓库约定调整。

## 实现要求

1. 保留原有对外入口和调用方式，例如：

   * Ping 命令识别；
   * Ping 回复构建；
   * Gateway 状态记录接口。
2. 对外 API 尽量通过 `pub use` 或原入口转发保持兼容。
3. 将事实采集、健康判断和展示文本分离：

   * `status` 只描述状态事实；
   * `assess` 根据事实生成健康结论和诊断项；
   * `render` 只负责 Markdown 展示；
   * `healthz` 只负责 LLM 健康探测。
4. 拆分 `render_ping_debug_details`，避免单函数继续承载完整报告。
5. 消除以下明显重复：

   * reconnect/session 恢复时间处理；
   * 相似的 note 收集逻辑；
   * 重复的时间格式化控制流。
6. heartbeat 的阈值、状态判断等规则必须保持原语义。
7. 不在本阶段重新设计 `/ping` 文案或状态规则。
8. 保留并迁移现有测试，必要时补充模块级单元测试。

## 验收标准

* `/ping` 的用户可见输出与拆分前保持一致；
* Gateway 状态判断逻辑保持一致；
* LLM healthz 请求行为保持一致；
* 原有 Ping 测试全部通过；
* 新模块之间职责明确，没有循环依赖；
* `ping/mod.rs` 主要保留入口、导出和流程编排。

## 执行状态

执行状态（2026-06-18）：已完成。

实际结果：

* 原 `qq-maid-gateway-rs/src/gateway/ping.rs` 已拆分为 `qq-maid-gateway-rs/src/gateway/ping/` 目录。
* `mod.rs` 保留 `/ping` 命令识别、`build_c2c_ping_reply` 总入口、子模块声明和 `GatewayRuntimeStatus` / `GatewayRuntimeSnapshot` / `InvalidSessionSnapshot` re-export。
* `status.rs` 放置 Gateway 运行时状态事实和 `record_*` 记录接口；`healthz.rs` 放置 LLM `/healthz` 短超时探测；`assess.rs` 放置健康评估、核心链路行和最近事件；`render.rs` 放置 Markdown 摘要和 `/ping all` 调试详情渲染；`time.rs` 放置 Ping 展示用时间辅助；`tests.rs` 迁移原 Ping 测试。
* `/ping` 用户可见输出、`/ping all` 调试详情结构、heartbeat 90 秒 warning / 180 秒 error 阈值、Gateway 状态记录接口、LLM healthz 状态格式和 URL 脱敏行为保持不变。
* 公开调用路径继续通过 `gateway::ping::{GatewayRuntimeStatus, build_c2c_ping_reply, is_ping_command}` 使用；测试辅助 `new_for_test`、状态写入 helper 和内部渲染/探测类型仅使用 `pub(super)`，未扩大到 crate 级公开。
* 已拆分 `render_ping_debug_details` 为概览、Gateway、消息、发送、LLM、配置等小渲染函数；恢复耗时展示复用 `recovery_elapsed_text`；发送和 respond note 收集复用 `collect_attempt_note`。
* 新增中文模块注释说明 `/ping` facade 的职责边界，新增中文注释说明 runtime 快照只保存脱敏消息 ID；保留并迁移原有关于默认视图隐藏内部 ID、URL 和 Unix 秒的有效注释。
* 未删除有效业务注释；原注释随对应逻辑移动或保留。
* 拆分前执行 `make test-gateway`：通过；`qq-maid-common` 9 个测试通过，`qq-maid-gateway-rs` 52 个测试通过。
* 拆分后执行 `cargo fmt -p qq-maid-gateway-rs`：通过。
* 拆分后执行 `make test-gateway`：通过；`qq-maid-common` 9 个测试通过，`qq-maid-gateway-rs` 52 个测试通过，`cargo check -p qq-maid-common` 和 `cargo check -p qq-maid-gateway-rs` 通过。
* 未执行 `make test`，因为本阶段只修改 Gateway Ping 模块，按当前任务最低要求执行 `make test-gateway`。
* 本阶段代码拆分提交：`1d3773e`；本审计文档更新仍未提交。
* 确认本阶段未写入 token、secret、API Key、openid 原文、真实群聊或私聊数据。

---

# 第二阶段：渐进拆分 Gateway 主循环

## 目标

降低 `gateway/mod.rs` 的职责和长度，重点解决：

* `handle_c2c_message` 过长；
* `handle_group_message` 与私聊流程重复；
* 流式和非流式响应处理重复；
* WebSocket 协议处理与业务处理混合；
* 发送统计存在多套机制。

建议优先提取低风险边界，再处理共享逻辑。

建议结构：

```text
qq-maid-gateway-rs/src/gateway/
  mod.rs
  dispatch.rs
  ws_protocol.rs
  message_handler.rs
  response_handler.rs
  send_adapter.rs
```

不要机械按该结构执行，应根据仓库实际代码确认自然边界。

## 实现顺序

### 2.1 低风险提取

优先提取：

1. WebSocket 消息解析与协议辅助；
2. envelope/event 分发；
3. 与主循环无关的纯 helper；
4. 独立类型和内部枚举。

此步骤不改 C2C 和 Group 的核心执行语义。

### 2.2 统一响应处理

确认私聊和群聊的公共处理阶段，例如：

```text
消息上下文构造
→ 本地命令处理
→ LLM 调用
→ 流式或非流式响应处理
→ QQ 发送
→ 发送状态记录
```

优先抽取以下共享能力：

* LLM 请求执行；
* 流式响应事件消费；
* 非流式响应处理；
* 错误回退回复；
* 发送成功/失败状态记录。

不要仅把 C2C 和 Group 分别搬到两个大文件中而保留全部重复。

### 2.3 统一发送统计

当前如果同时存在：

* sender trait 包装；
* `send_*_with_status`；
* 显式 `record_qq_send_result`；

请确认各自调用范围和语义，选择一种清晰的一致方式。

要求：

* 每次真实 QQ 发送只记录一次结果；
* 发送失败不能被吞掉；
* 不允许伪造成功状态；
* 不改变现有日志、诊断和 Ping 所依赖的状态语义。

## 实现要求

1. 保留：

   * WebSocket 建连、重连、心跳和恢复语义；
   * 私聊和群聊事件处理；
   * 消息去重；
   * `/ping` 本地处理；
   * LLM 流式与非流式调用；
   * QQ 消息回发；
   * 错误回退；
   * 发送状态记录。
2. 不修改公开接口和配置项。
3. 不修改 QQ 消息格式、引用逻辑和用户可见文案。
4. 不改变 C2C 与 Group 的权限或触发条件。
5. 不进行无关命名重构或格式调整。
6. 若公共流程差异较大，不强行做万能 handler；允许通过小型 context、trait 或明确分支保留差异。
7. 避免引入过度泛型、宏或复杂状态机。

## 验收标准

* Gateway 所有原有测试通过；
* 私聊和群聊均能正常调用 LLM 并发送回复；
* 流式和非流式响应行为与原实现一致；
* `/ping` 仍可本地处理；
* 重连、心跳、恢复和去重行为不变；
* 发送统计不会重复记录或漏记；
* `gateway/mod.rs` 主要保留：

  * 模块声明；
  * 常量；
  * `run` 或主循环入口；
  * 少量流程编排。

---

# 第三阶段：拆分 Todo 存储模块

## 目标

整理 `qq-maid-llm/src/storage/todo.rs`，降低以下职责混合：

* 类型定义；
* schema/migration；
* CRUD；
* 批量状态变更；
* SQL 查询与 row mapping；
* 搜索评分；
* 排序；
* 文本规范化；
* 日期推断与展示；
* 测试。

建议结构：

```text
qq-maid-llm/src/storage/todo/
  mod.rs
  types.rs
  store.rs
  query.rs
  search.rs
  time.rs
  tests.rs
```

如果某些模块代码很少，可合并，避免碎片化。

## 实现要求

1. 保持现有 `TodoStore`、数据结构和调用方式兼容。
2. 保持 SQLite schema、migration、索引、软删除和状态语义不变。
3. 消除三个批量状态操作中的明显模板重复：

   * 完成；
   * 恢复；
   * 取消完成或对应现有操作。
4. 消除单条 complete/cancel 的明显重复，但不要为了 DRY 引入难读抽象。
5. 统一重复的：

   * SELECT 列；
   * row mapping；
   * 查询构造；
   * 状态过滤。
6. 搜索评分和排序逻辑应独立于数据库 CRUD。
7. 中文日期推断和展示逻辑保持原行为。
8. 现有测试全部迁移并保持通过。
9. 不修改 Todo 命令行为、快照编号、确认流程、搜索结果和排序结果。

## 验收标准

* Todo CRUD、批量操作、搜索、排序、日期推断全部行为不变；
* 数据库兼容现有数据；
* 原有测试全部通过；
* 没有新增重复 SQL 和 row mapping；
* 不出现动态 SQL 注入风险；
* 不通过硬编码绕过状态校验。

---

# 第四阶段：拆分 Weather 模块

## 目标

整理 `qq-maid-llm/src/runtime/weather/mod.rs`，分离：

* 公开天气模型和 trait；
* 和风天气 API 实现；
* API 内部反序列化结构；
* 地理位置选择和匹配；
* HTTP 请求辅助；
* 测试。

建议结构：

```text
qq-maid-llm/src/runtime/weather/
  mod.rs
  types.rs
  qweather.rs
  geo.rs
  http.rs
  tests.rs
```

`http.rs` 仅在确有自然公共逻辑时创建。

## 实现要求

1. 保留现有天气 executor 构建和调用 API。
2. 保留：

   * 实时天气；
   * 三日预报；
   * 天气预警；
   * 空气质量；
   * 生活指数；
   * 地理位置匹配；
   * 错误回退。
3. 可以提取以下公共逻辑：

   * HTTP 状态检查；
   * 错误正文截断；
   * JSON 解码错误包装；
   * API code 校验。
4. 不强行将不同认证方式、路径结构和返回格式统一成一个复杂万能 fetch 函数。
5. 和风天气 v7 与其他接口的差异应清晰保留。
6. 所有序列化字段、URL 参数、认证方式和错误语义保持不变。
7. 测试按职责迁移。

## 验收标准

* 所有天气功能行为不变；
* 地理位置匹配结果不变；
* 请求参数、认证方式和 URL 结构不变；
* 原有天气测试通过；
* `weather/mod.rs` 主要保留对外导出和 executor 构建。

---

# 第五阶段：拆分 Time Context

## 目标

整理 `qq-maid-common/src/time_context.rs`，分离：

* 时间上下文数据结构；
* 中文自然语言时间推断；
* 通用展示格式；
* 诊断时间格式；
* 测试。

建议结构：

```text
qq-maid-common/src/time_context/
  mod.rs
  infer.rs
  display.rs
  diagnostic.rs
  tests.rs
```

## 实现要求

1. 保持所有公开函数和类型兼容。
2. 将重复的诊断时间格式化控制流收敛为简单 helper。
3. 可以将相似的相对时间词解析改成表驱动，但必须保持匹配优先级。
4. 中文词语必须避免短词抢先匹配长词，例如：

   * 大后天；
   * 后天；
   * 明天；
   * 今天。
5. 保持北京时间、时区、unix 时间和展示格式语义不变。
6. 不修改依赖此模块的 Todo、查询、Gateway 或诊断输出行为。

## 验收标准

* 原有时间解析测试全部通过；
* 相对时间词解析结果与原实现一致；
* 所有展示格式保持一致；
* 无时区回归；
* 对外 API 不变。

---

# 第六阶段：整理测试支撑代码

## 6.1 `respond/tests/support.rs`

### 目标

将以下职责拆开：

* MockProvider；
* MockWeatherExecutor；
* MockQueryExecutor；
* mock 回复生成；
* service fixture；
* seed 数据；
* 请求构造。

建议结构：

```text
respond/tests/support/
  mod.rs
  mock_provider.rs
  mock_weather.rs
  mock_query.rs
  replies.rs
  fixtures.rs
  seed.rs
```

### 实现要求

1. 保持所有现有测试调用方式尽量兼容。
2. 简化过深的 `test_service_*` 工厂调用链。
3. 优先通过：

   * builder；
   * 配置结构体；
   * 少量明确 fixture；
     来替代 6 层嵌套函数。
4. 拆分 `mock_todo_parse_reply` 的长分支。
5. 可以按业务意图补充分区注释，但不要为每个显而易见的 helper 添加无意义注释。
6. 不修改测试预期和生产代码行为。

## 6.2 `respond/tests/todo.rs`

### 目标

减少重复的：

* owner 构造；
* TodoItemDraft 构造；
* add + confirm 流程；
* seed 流程；
* 长测试中的多规则混合验证。

### 实现要求

1. 提取本地 helper，例如：

   * `owner()`；
   * `draft()`；
   * `add_todo()`；
   * `add_and_confirm_todo()`。
2. 将超过 100 行且验证多个独立规则的测试拆开。
3. 可以按业务域分组：

   * add；
   * edit/delete；
   * done/undo；
   * search；
   * pending；
   * snapshot。
4. 测试名本身足够清晰时，不强制添加重复注释。
5. 不减少核心测试覆盖。

## 验收标准

* 所有 respond/Todo 测试通过；
* 测试 helper 依赖清晰；
* fixture 工厂层级明显降低；
* 不通过删除测试来减少代码；
* 不降低关键业务路径覆盖率。

---

# 第七阶段：RSS 和 Session 轻量整理

## 7.1 RSS

处理 `qq-maid-llm/src/storage/rss.rs`，只做低风险去重。

### 实现要求

1. 提取重复 SELECT 列定义。
2. 提取统一 row mapping helper。
3. 将 `upsert_item_state_unlocked` 中的以下职责拆开：

   * 查询旧状态；
   * revision 分类判断；
   * 兼容降噪判断；
   * 数据库写入。
4. 保持以下语义不变：

   * 首次订阅历史条目标记；
   * seen/pending 状态；
   * revision hash；
   * 更新条目重新推送；
   * 兼容降噪；
   * SQLite schema。
5. 不在本阶段修改 RSS 翻译、抓取或推送业务逻辑。
6. 不为了减少行数强制拆成多个文件。

## 7.2 Session

处理 `qq-maid-llm/src/storage/session.rs`，只做明确简单的去重。

### 实现要求

1. 检查 `Connection` 和 `Transaction` 的重复 active-session 写入逻辑。
2. 若可以通过接受 `&Connection` 或现有 rusqlite 解引用能力安全统一，则消除重复。
3. 不为十几行重复引入复杂 trait 或泛型。
4. `redact_sensitive_text` 只有在仓库中存在多个真实调用方时才考虑移入 common。
5. 若脱敏仅属于 Session 业务，则保留在 Session 模块。
6. JSON helper 只有在多个 storage 模块已有同类重复时才提取。
7. 保持 session、message、pending 和 active mapping 数据兼容。

## 验收标准

* RSS 和 Session 原有测试通过；
* SQLite 数据格式和 migration 不变；
* RSS 增量推送判断不变；
* Session 活跃会话和 pending 行为不变；
* 不新增不必要的公共 API。

---

# 禁止事项

* 不要一次性拆分所有文件。
* 不要仅为了低于 1000 行机械切文件。
* 不要进行与任务无关的大规模重构。
* 不要更换技术栈或数据库方案。
* 不要修改公开接口语义。
* 不要修改命令、配置项或用户可见输出。
* 不要修改数据库 schema 或持久化格式，除非拆分无法进行；如确有必要，必须先停止并说明。
* 不要通过大量 `pub` 暴露内部实现。
* 不要使用宏隐藏简单 SQL 或业务流程。
* 不要为了消除少量重复引入复杂泛型。
* 不要吞掉错误。
* 不要伪造成功状态。
* 不要伪造测试、构建或静态检查结果。
* 不要顺手修复无关 Bug。
* 不要删除现有测试以使重构通过。
* 不要调整无关代码格式。

# 测试要求

每个阶段完成后运行与影响范围匹配的测试。

优先使用仓库已有命令，例如：

```bash
make test-gateway
make test-llm
make test
```

具体命令以仓库实际 Makefile、README、AGENTS.md 或 CI 配置为准。

同时运行适用的：

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
cargo build --workspace
```

若项目不支持其中某条命令，应使用仓库实际命令替代，并在完成报告中说明。

对于 Gateway 重构，至少验证：

1. 私聊消息；
2. 群聊消息；
3. `/ping`；
4. 流式响应；
5. 非流式响应；
6. QQ 发送失败；
7. LLM 请求失败；
8. WebSocket 重连和状态记录。

对于 SQLite 模块，至少验证：

1. 现有数据库可正常读取；
2. CRUD；
3. 批量状态操作；
4. migration；
5. 软删除；
6. row mapping；
7. 异常路径回滚。

# 提交和执行方式

请按阶段执行，每个阶段使用独立提交。

建议提交顺序：

1. `refactor(gateway): split ping responsibilities`
2. `refactor(gateway): extract websocket dispatch`
3. `refactor(gateway): unify response handling`
4. `refactor(todo): split storage responsibilities`
5. `refactor(weather): split qweather module`
6. `refactor(common): split time context`
7. `refactor(tests): reorganize respond test support`
8. `refactor(storage): simplify rss and session helpers`

如果某阶段改动过大，应继续拆成更小提交。

不要将纯移动代码和行为修改放在同一个提交中。

# 完成后输出

每完成一个阶段，请输出：

1. 本阶段发现的原始问题。
2. 采用的模块边界和拆分思路。
3. 修改了哪些文件。
4. 哪些代码只是移动。
5. 哪些代码做了去重或重构。
6. 是否改变了任何行为。
7. 执行了哪些测试、构建、格式化和静态检查。
8. 每项命令的实际结果。
9. 是否存在无法运行的检查及原因。
10. 是否发现审计报告与仓库实际情况不一致。
11. 是否存在后续风险或建议。
12. 当前阶段对应的 Git commit hash。

全部阶段完成后，再提供一份总报告，包括：

* 最终目录结构；
* 各超长文件的处理结果；
* 哪些文件保留为大文件以及原因；
* 对外 API 和持久化兼容性说明；
* 所有测试结果；
* 未完成项和风险。

请先从第一阶段 Ping 模块开始，不要一次执行全部阶段。
