//! Gateway 进程内主动推送实现。
//!
//! Core 只通过 `PushSink` 交付推送意图；本模块负责 QQ 平台发送、Markdown
//! 失败后的文本 fallback、发送状态记录，以及群推送成功后的 BotOutboundCache 回填。

use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use qq_maid_core::runtime::push::{PushError, PushIntent, PushResult, PushSink, PushTargetType};
use tokio::{sync::Notify, time::timeout};
use tracing::{info, warn};

use crate::{
    api::{QqApiClient, build_c2c_text_payload, build_group_text_payload},
    gateway::{
        BotOutboundCache, logging::mask_identifier, outbound::record_qq_send_result,
        ping::GatewayRuntimeStatus,
    },
    markdown::MarkdownPayload,
};

#[derive(Clone)]
pub struct GatewayPushSink {
    inner: Arc<Mutex<Option<GatewayPushRuntime>>>,
    ready: Arc<Notify>,
}

#[derive(Clone)]
struct GatewayPushRuntime {
    api: QqApiClient,
    runtime: GatewayRuntimeStatus,
    group_outbound_cache: Arc<Mutex<BotOutboundCache>>,
}

impl GatewayPushSink {
    pub fn unbound() -> Self {
        Self {
            inner: Arc::new(Mutex::new(None)),
            ready: Arc::new(Notify::new()),
        }
    }

    pub(crate) fn bind(
        &self,
        api: QqApiClient,
        runtime: GatewayRuntimeStatus,
        group_outbound_cache: Arc<Mutex<BotOutboundCache>>,
    ) {
        // Core scheduler 可能在 Gateway 首次连接 QQ 前启动，因此 sink 需要先存在；
        // 真正发送前必须已绑定运行期上下文，否则返回可观测错误而不是静默丢消息。
        *self.inner.lock().unwrap() = Some(GatewayPushRuntime {
            api,
            runtime,
            group_outbound_cache,
        });
        self.ready.notify_waiters();
    }

    async fn runtime(&self) -> Result<GatewayPushRuntime, PushError> {
        if let Some(runtime) = self.inner.lock().unwrap().clone() {
            return Ok(runtime);
        }

        // 统一进程启动时 Core 的 RSS / Todo 定时器和 QQ Gateway 连接并行启动。
        // 首次推送如果撞上 Gateway 尚未 bind，等待一小段时间可避免把正常启动竞态记成推送失败。
        let notified = self.ready.notified();
        if timeout(Duration::from_secs(30), notified).await.is_err() {
            return Err(PushError::Failed {
                summary: "gateway push sink is not ready".to_owned(),
            });
        }

        self.inner
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| PushError::Failed {
                summary: "gateway push sink is not ready".to_owned(),
            })
    }
}

#[async_trait]
impl PushSink for GatewayPushSink {
    async fn push(&self, intent: PushIntent) -> Result<PushResult, PushError> {
        let runtime = self.runtime().await?;
        runtime.push(intent).await
    }
}

impl GatewayPushRuntime {
    async fn push(&self, intent: PushIntent) -> Result<PushResult, PushError> {
        let target_id = intent.target.target_id.trim();
        let text = intent.text.trim();
        if target_id.is_empty() || text.is_empty() {
            return Err(PushError::Failed {
                summary: "target_id and text are required".to_owned(),
            });
        }

        let fallback_text = intent
            .fallback_text
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(text);
        let message_type = intent.message_type.trim();
        let result = match intent.target.target_type {
            PushTargetType::Private => {
                send_private_push(&self.api, target_id, message_type, text, fallback_text).await
            }
            PushTargetType::Group => {
                send_group_push(&self.api, target_id, message_type, text, fallback_text).await
            }
        };
        record_qq_send_result(&self.runtime, &result);
        if intent.target.target_type == PushTargetType::Group
            && let Ok(Some(message_id)) = result.as_ref()
        {
            self.group_outbound_cache
                .lock()
                .unwrap()
                .insert(Some(message_id.clone()));
        }
        match result {
            Ok(message_id) => {
                info!(
                    target_type = %intent.target.target_type.as_str(),
                    target = %mask_identifier(target_id),
                    "gateway push sent"
                );
                Ok(PushResult { message_id })
            }
            Err(err) => {
                warn!(
                    target_type = %intent.target.target_type.as_str(),
                    target = %mask_identifier(target_id),
                    error = %err.log_summary(),
                    "gateway push failed"
                );
                Err(PushError::Failed {
                    summary: err.log_summary(),
                })
            }
        }
    }
}

async fn send_private_push(
    api: &QqApiClient,
    target_id: &str,
    message_type: &str,
    text: &str,
    fallback_text: &str,
) -> crate::api::SendResult {
    match message_type {
        "markdown" => {
            let markdown = MarkdownPayload::new(text.to_owned());
            match api.send_c2c_markdown(target_id, None, &markdown).await {
                Ok(message_id) => Ok(message_id),
                Err(err) => {
                    warn!(
                        target = %mask_identifier(target_id),
                        error = %err.log_summary(),
                        "markdown push failed; falling back to text"
                    );
                    api.send_c2c_text(target_id, None, fallback_text).await
                }
            }
        }
        "text" | "" => {
            // 主动推送没有原始 QQ msg_id，因此只发送 content/msg_type/msg_seq。
            let _shape = build_c2c_text_payload(text, None, 1);
            api.send_c2c_text(target_id, None, text).await
        }
        _ => Err(crate::api::ApiError::Unsupported("message_type")),
    }
}

async fn send_group_push(
    api: &QqApiClient,
    target_id: &str,
    message_type: &str,
    text: &str,
    fallback_text: &str,
) -> crate::api::SendResult {
    match message_type {
        "markdown" => {
            let markdown = MarkdownPayload::new(text.to_owned());
            match api.send_group_markdown(target_id, None, &markdown).await {
                Ok(message_id) => Ok(message_id),
                Err(err) => {
                    warn!(
                        target = %mask_identifier(target_id),
                        error = %err.log_summary(),
                        "group markdown push failed; falling back to text"
                    );
                    api.send_group_text(target_id, None, fallback_text).await
                }
            }
        }
        "text" | "" => {
            // QQ 群 openid 主动消息使用 /v2/groups/{group_openid}/messages。
            let _shape = build_group_text_payload(text, None, 1);
            api.send_group_text(target_id, None, text).await
        }
        _ => Err(crate::api::ApiError::Unsupported("message_type")),
    }
}
