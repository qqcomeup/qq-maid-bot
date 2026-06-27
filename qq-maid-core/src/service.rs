//! Core 进程内服务契约。
//!
//! Gateway 只依赖本模块暴露的强类型边界，不直接访问 Core 内部 store、HTTP
//! route 或 provider 细节。scope_key 统一由 Core 根据会话目标派生，避免跨层出现
//! 两套会话归属事实。

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use tokio::time::timeout;
use tracing::{error, warn};

use crate::{
    config::AppConfig,
    error::{ErrorInfo, LlmError},
    http::routes::AppState,
    provider::types::{ChatMessage, ChatRequest, ChatRole},
    runtime::respond::{
        RespondExecutors, RespondRequest, RespondResponse, RespondServiceOptions, RespondStores,
        RustRespondService,
    },
    util::metrics::MetricsRecorder,
};

pub use qq_maid_llm::provider::status::{UpstreamState, UpstreamStatusSnapshot};

#[async_trait]
pub trait CoreService: Send + Sync {
    async fn respond(&self, request: CoreRequest) -> Result<CoreResponse, CoreError>;

    async fn upstream_check(&self) -> Result<(), CoreError>;

    fn health_snapshot(&self) -> CoreHealthSnapshot;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreRequest {
    pub text: String,
    pub platform: Platform,
    pub actor: CoreActor,
    pub conversation: CoreConversation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    QqOfficial,
    OneBot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreActor {
    pub user_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CoreConversation {
    Private { peer_id: String },
    Group { group_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreResponse {
    pub ok: bool,
    pub text: Option<String>,
    pub markdown: Option<String>,
    pub handled: Option<bool>,
    pub session_id: Option<String>,
    pub command: Option<String>,
    pub diagnostics: Option<serde_json::Value>,
    pub error: Option<ErrorInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoreHealthSnapshot {
    pub ok: bool,
    pub provider: String,
    pub model: String,
    pub stream: bool,
    pub upstream: UpstreamStatusSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{code}@{stage}: {message}")]
pub struct CoreError {
    pub code: String,
    pub stage: String,
    pub message: String,
}

#[derive(Clone)]
pub struct CoreHandle {
    state: Arc<AppState>,
}

impl CoreHandle {
    pub fn new(state: AppState) -> Self {
        Self {
            state: Arc::new(state),
        }
    }

    fn respond_service(&self) -> RustRespondService {
        let state = self.state.as_ref();
        RustRespondService::new(
            state.provider.clone(),
            RespondExecutors {
                query_executor: state.query_executor.clone(),
                weather_executor: state.weather_executor.clone(),
                train_executor: state.train_executor.clone(),
            },
            RespondStores {
                memory_store: state.memory_store.clone(),
                session_store: state.session_store.clone(),
                todo_store: state.todo_store.clone(),
                rss_store: state.rss_store.clone(),
            },
            state.rss_fetcher.clone(),
            state.knowledge_index.clone(),
            state.prompt_config.clone(),
            respond_options(&state.config),
        )
    }
}

#[async_trait]
impl CoreService for CoreHandle {
    async fn respond(&self, request: CoreRequest) -> Result<CoreResponse, CoreError> {
        let req: RespondRequest = request.into();
        let service = self.respond_service();
        let recorder = MetricsRecorder::start();
        let scope_key = req.scope_key.clone();
        let state = self.state.as_ref();
        let result = timeout(
            Duration::from_secs(state.config.request_timeout_seconds),
            service.respond(req),
        )
        .await;

        match result {
            Ok(Ok(response)) => Ok(response.into()),
            Ok(Err(err)) => {
                warn_core_error(&scope_key, &err);
                Err(err.into())
            }
            Err(_) => {
                let err = LlmError::timeout("request");
                error_core_error(&scope_key, &err);
                let _metrics = recorder.fail(
                    state.provider.name(),
                    state.provider.model(),
                    state.provider.stream_enabled(),
                );
                Err(err.into())
            }
        }
    }

    async fn upstream_check(&self) -> Result<(), CoreError> {
        let state = self.state.as_ref();
        let request = ChatRequest {
            session_id: "diagnostic:upstream_check".to_owned(),
            model: None,
            messages: vec![ChatMessage {
                role: ChatRole::User,
                content: "这是连通性检查。请只回复 OK。".to_owned(),
            }],
            metadata: HashMap::from([("purpose".to_owned(), "upstream_check".to_owned())]),
        };

        match timeout(
            Duration::from_secs(state.config.request_timeout_seconds),
            state.provider.chat(request),
        )
        .await
        {
            Ok(Ok(outcome)) if !outcome.reply.trim().is_empty() => Ok(()),
            Ok(Ok(_)) => {
                let error = LlmError::provider("upstream returned empty response", "diagnostic");
                // 空正文不能证明响应解析可用，必须显式覆盖为失败状态。
                state.upstream_status.record_failure(&error);
                Err(CoreError::new(
                    "provider_error",
                    "diagnostic",
                    "上游返回空响应",
                ))
            }
            Ok(Err(error)) => Err(error.into()),
            Err(_) => {
                let error = LlmError::timeout("upstream_check");
                // timeout 会取消被观测 provider 的 future，因此在入口补记失败状态。
                state.upstream_status.record_failure(&error);
                Err(error.into())
            }
        }
    }

    fn health_snapshot(&self) -> CoreHealthSnapshot {
        let state = self.state.as_ref();
        CoreHealthSnapshot {
            ok: true,
            provider: state.provider.name().to_owned(),
            model: state.provider.model().to_owned(),
            stream: state.provider.stream_enabled(),
            upstream: state.upstream_status.snapshot(),
        }
    }
}

impl CoreRequest {
    pub fn scope_key(&self) -> String {
        match &self.conversation {
            CoreConversation::Private { peer_id } => format!("private:{peer_id}"),
            CoreConversation::Group { group_id } => format!("group:{group_id}"),
        }
    }
}

impl From<CoreRequest> for RespondRequest {
    fn from(value: CoreRequest) -> Self {
        let scope_key = value.scope_key();
        let (group_id, channel_id, event_type) = match &value.conversation {
            CoreConversation::Private { .. } => (None, None, "c2c_message"),
            CoreConversation::Group { group_id } => (Some(group_id.clone()), None, "group_message"),
        };
        Self {
            content: value.text,
            scope_key,
            user_id: value.actor.user_id,
            group_id,
            guild_id: None,
            channel_id,
            platform: value.platform.as_str().to_owned(),
            event_type: event_type.to_owned(),
            ..Default::default()
        }
    }
}

impl Platform {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QqOfficial => "qq_official",
            Self::OneBot => "onebot",
        }
    }
}

impl From<RespondResponse> for CoreResponse {
    fn from(value: RespondResponse) -> Self {
        Self {
            ok: value.ok,
            text: value.text,
            markdown: value.markdown,
            handled: value.handled,
            session_id: value.session_id,
            command: value.command,
            diagnostics: value.diagnostics,
            error: value.error,
        }
    }
}

impl CoreError {
    pub fn new(
        code: impl Into<String>,
        stage: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            stage: stage.into(),
            message: message.into(),
        }
    }

    pub fn as_info(&self) -> ErrorInfo {
        ErrorInfo {
            code: self.code.clone(),
            message: self.message.clone(),
            stage: self.stage.clone(),
        }
    }
}

impl From<LlmError> for CoreError {
    fn from(value: LlmError) -> Self {
        Self {
            code: value.code,
            stage: value.stage,
            message: value.message,
        }
    }
}

fn respond_options(config: &AppConfig) -> RespondServiceOptions {
    RespondServiceOptions {
        title_model: config.title_model.clone(),
        todo_model: config.todo_model.clone(),
        memory_model: config.memory_model.clone(),
        compact_model: config.compact_model.clone(),
        translation_model: config.translation_model.clone(),
        rss_summary_max_chars: config.rss_summary_max_chars as usize,
        rss_seen_retention: config.rss_seen_retention as usize,
    }
}

fn warn_core_error(scope_key: &str, err: &LlmError) {
    warn!(
        scope_key,
        error_code = err.code,
        error_stage = err.stage,
        "core respond request failed"
    );
}

fn error_core_error(scope_key: &str, err: &LlmError) {
    error!(
        scope_key,
        error_code = err.code,
        error_stage = err.stage,
        "core respond request timed out"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_conversation_derives_private_scope() {
        let req = CoreRequest {
            text: "hello".to_owned(),
            platform: Platform::QqOfficial,
            actor: CoreActor {
                user_id: Some("u1".to_owned()),
            },
            conversation: CoreConversation::Private {
                peer_id: "u1".to_owned(),
            },
        };

        let respond: RespondRequest = req.into();

        assert_eq!(respond.scope_key, "private:u1");
        assert_eq!(respond.platform, "qq_official");
        assert_eq!(respond.user_id.as_deref(), Some("u1"));
        assert_eq!(respond.group_id, None);
    }

    #[test]
    fn group_conversation_derives_group_scope_without_member_split() {
        let req = CoreRequest {
            text: "/todo".to_owned(),
            platform: Platform::QqOfficial,
            actor: CoreActor { user_id: None },
            conversation: CoreConversation::Group {
                group_id: "g1".to_owned(),
            },
        };

        let respond: RespondRequest = req.into();

        assert_eq!(respond.scope_key, "group:g1");
        assert_eq!(respond.platform, "qq_official");
        assert_eq!(respond.user_id, None);
        assert_eq!(respond.group_id.as_deref(), Some("g1"));
    }
}
