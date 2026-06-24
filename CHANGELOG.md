# Changelog

本文档基于 [keep a changelog](https://keepachangelog.com/zh-CN/1.0.0/) 格式，记录每个已发布版本的变更。

## [v0.3.4] - 2026-06-24

### Added
- `/todo add` 支持火车行程识别与 12306 时刻校验：
  - LLM 解析车次/站点/日期后调用 12306 接口校验站点存在性与站序
  - 支持跨日行程时间计算（`dayDifference` 字段）
  - 校验失败或接口异常时不创建 Todo，返回针对性提示
  - 支持纯数字车次（如 1461）与字母前缀车次（G/D/C/Z/T/K）
  - `NotTrain` 识别结果自动回退普通 Todo 解析
- Todo 每日提醒后台调度（`runtime/todo_reminder.rs`）：
  - 按 Asia/Shanghai 每日定时扫描 pending 个人待办
  - 只推送可验证 private target 的 owner，群待办不主动推送
  - 同 owner 多 private scope 合并，冲突 target 脱敏跳过
  - 每日每 owner 仅发送一次，当天失败会自动补跑重试
- 通用 GatewayPushClient（`runtime/push.rs`），供 RSS 与 Todo 提醒共用
- 12306 接口 `stationNo`/`dayDifference` 兼容兜底，缺失时不阻塞 `/火车` 查询
- `day_difference_reliable` 字段：Provider 层可为展示兜底，校验层拒绝不可信跨日数据
- 火车 Todo 回看候选 `startDay` 逻辑：首个候选报站点错误时继续查询，避免误杀跨日行程
- 配置项 `TODO_DAILY_REMINDER_ENABLED` / `TODO_DAILY_REMINDER_TIME`

### Changed
- README 补充 Todo 每日提醒能力说明
- `.env.example` 补充默认推送入口说明

## [v0.3.3] - 2026-06-21

### Added
- Web 控制台路由安全头中间件（X-Content-Type-Options、X-Frame-Options、CSP）
- `scripts/validate-release-runtime.sh` — 待发布 runtime 目录完整性校验脚本
- `scripts/botctl.sh` — 统一启停控制脚本（start/stop/restart/status/logs/health/console）
- 群消息 `group_message_mode` 配置项，支持 `off` / `command` / `mention` / `active`
- OpenAI 兼容 GLM provider 支持
- `qq-maid-gateway-rs` 推送到 `qq-maid-llm` 内部 `/internal/push` 端点，支持群 @ 消息透传

### Changed
- `deploy.sh` 增加构建产物校验和缺失检测
- `Makefile install` target 正确拷贝 release 二进制和控制脚本到 `runtime/`
- `scripts/llmctl.sh` 增加 `LINES` 日志行数配置支持
- `scripts/llmctl.sh` 增加 `console` 子命令
- `runtime/.env.example` 更新配置项
- `qq-maid-llm` Web 控制台功能：配置开关、CORS 管理、Markdown 渲染接口

### Fixed
- 移除 unused import 警告
- `cargo fmt` 格式化修复
- 修复 console 测试断言与 HTML 标题不一致

## [v0.3.2] - 2026-06-20

### Added
- 拆分运行时目录校验与发布包校验脚本
- 新增 OpenAI `chat_only` 模式、可选群聊处理与运维脚本
- 新增 `git clone` 后本地部署快速开始指南
- 添加本地一键部署脚本 `scripts/deploy-local.sh`

### Changed
- 隐藏 todo 内部 ID，统一使用列表序号和关键词匹配
- 为命令回复引入独立的 Markdown 与纯文本双通道
- 拆分天气模块为 `types/qweather` 子模块，添加格式单测
- 更新天气回复格式为 Markdown 并添加诊断字段
- 为 LLM 上游调用添加健康状态观测并支持 `/ping check` 诊断
- 添加 install 目标将构建产物安装到 `runtime/`
- 抽取分层帮助模块并替换内联 `/help` 回复
- 简化 todo target 解析分支，消除无意义条件判断
- `README-dev.md` 重命名为 `DEVELOPMENT.md`

### Fixed
- 群 push 返回的 message_id 写入共享 BotOutboundCache
- 修复群聊作用域与部署控制台回归
- 安全增强、部署加固及脚本一致性优化
- 合并嵌套 if 满足 clippy 警告

## [v0.3.1] - 2026-06-19

### Fixed
- 为 Windows 构建添加 zip 安装步骤

## [v0.3.0] - 2026-06-19

### Added
- 扩展多平台发布矩阵，支持 Linux/Windows/macOS/Android 六平台构建
- 添加超长文件拆分审计报告
- 精简 `markdown_cell` 中的换行替换逻辑

### Changed
- 将 `/ping` 模块拆分为子模块

## [v0.2.0] - 2026-06-18

### Added
- 为 `/ping` 添加摘要/详情双视图和 Markdown 支持
- 升级 GitHub Actions 依赖版本

## [v0.1.0] - 2026-06-18

### Added
- 首个公开可用版本
- Rust Gateway + LLM 双服务架构
- QQ 官方机器人接入（C2C 私聊、群聊 at 消息）
- 普通聊天、会话管理、长期记忆、Todo、RSS/Atom 订阅
- 联网查询、天气、翻译等命令
- SQLite 持久化（Session、Todo、Memory、RSS）

[v0.3.4]: https://github.com/kuliantnt/qq-maid-bot/compare/v0.3.3...v0.3.4
[v0.3.3]: https://github.com/kuliantnt/qq-maid-bot/compare/v0.3.2...v0.3.3
[v0.3.2]: https://github.com/kuliantnt/qq-maid-bot/compare/v0.3.0...v0.3.2
[v0.3.1]: https://github.com/kuliantnt/qq-maid-bot/compare/v0.3.0...v0.3.1
[v0.3.0]: https://github.com/kuliantnt/qq-maid-bot/compare/v0.2.0...v0.3.0
[v0.2.0]: https://github.com/kuliantnt/qq-maid-bot/compare/v0.1.0...v0.2.0
[v0.1.0]: https://github.com/kuliantnt/qq-maid-bot/commits/v0.1.0
