//! Core 错误兼容入口。
//!
//! `LlmError` 的正式定义已经迁入 `qq-maid-llm`。Core 内部仍保留本模块路径，
//! 避免天气、列车、prompt 等非 LLM 业务在本次拆分中被迫做全仓错误体系重构。

pub use qq_maid_llm::{ErrorInfo, LlmError};
