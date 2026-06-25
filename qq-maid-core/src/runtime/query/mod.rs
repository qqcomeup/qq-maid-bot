//! 联网搜索查询兼容入口。
//!
//! OpenAI Responses + `web_search` 的协议实现已迁入 `qq-maid-llm`。
//! Core 继续保留 `Query*` 命名，供 `/查` 业务 flow、测试和 HTTP facade 使用。

use std::sync::Arc;

use crate::{config::AppConfig, error::LlmError};

pub use qq_maid_llm::web_search::{
    WebSearchExecutor as QueryExecutor, WebSearchOutcome as QueryOutcome,
    WebSearchRequest as QueryRequest, WebSearchResponse as QueryResponse,
    WebSearchSource as QuerySource,
};

/// 动态派发的搜索查询执行器。
pub type DynQueryExecutor = Arc<dyn QueryExecutor>;

/// 根据配置构建搜索查询执行器。
pub fn build_query_executor(config: &AppConfig) -> Result<DynQueryExecutor, LlmError> {
    qq_maid_llm::web_search::build_web_search_executor(&config.llm_config())
}
