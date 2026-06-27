//! Core 主动推送边界。
//!
//! Core 只表达“要推给谁、推什么内容”，不携带 HTTP URL、token 或 QQ 原始
//! payload。实际 QQ 发送、Markdown fallback 和群消息缓存仍由 Gateway 实现。

use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushTargetType {
    Private,
    Group,
}

impl PushTargetType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Group => "group",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushTarget {
    pub target_type: PushTargetType,
    pub target_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushIntent {
    pub target: PushTarget,
    pub message_type: String,
    pub text: String,
    pub fallback_text: Option<String>,
}

#[derive(Debug, Error)]
pub enum PushError {
    #[error("push failed: {summary}")]
    Failed { summary: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushResult {
    pub message_id: Option<String>,
}

#[async_trait]
pub trait PushSink: Send + Sync {
    async fn push(&self, intent: PushIntent) -> Result<PushResult, PushError>;
}
