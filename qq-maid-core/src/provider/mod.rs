//! Core 侧 Provider 兼容入口。
//!
//! Provider、模型路由、fallback 和健康观测实现已迁入 `qq-maid-llm`；
//! 这里仅保留历史模块路径，供业务 flow 和测试继续引用统一类型。

pub use qq_maid_llm::provider::*;
