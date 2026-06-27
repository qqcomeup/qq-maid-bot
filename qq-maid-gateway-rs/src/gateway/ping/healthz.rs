use qq_maid_core::service::{CoreHealthSnapshot, UpstreamState, UpstreamStatusSnapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LlmHealthSnapshot {
    pub(super) healthz_url: String,
    pub(super) status: String,
    pub(super) upstream: LlmUpstreamSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum LlmUpstreamSnapshot {
    Unavailable,
    Unverified,
    Available {
        last_success_at: Option<String>,
        provider: Option<String>,
        model: Option<String>,
        fallback_used: bool,
    },
    Error {
        last_checked_at: Option<String>,
        error_summary: String,
    },
}

pub(super) fn core_health_snapshot(snapshot: &CoreHealthSnapshot) -> LlmHealthSnapshot {
    LlmHealthSnapshot {
        healthz_url: "in-process".to_owned(),
        status: if snapshot.ok {
            "ok(in-process)".to_owned()
        } else {
            "unavailable".to_owned()
        },
        upstream: if snapshot.ok {
            parse_upstream(snapshot.upstream.clone())
        } else {
            LlmUpstreamSnapshot::Unavailable
        },
    }
}

fn parse_upstream(upstream: UpstreamStatusSnapshot) -> LlmUpstreamSnapshot {
    match upstream.state {
        UpstreamState::Available => LlmUpstreamSnapshot::Available {
            last_success_at: upstream.last_success_at,
            provider: upstream.provider,
            model: upstream.model,
            fallback_used: upstream.fallback_used,
        },
        UpstreamState::Error => LlmUpstreamSnapshot::Error {
            last_checked_at: upstream.last_checked_at,
            error_summary: safe_upstream_error_summary(upstream.error_summary.as_deref()),
        },
        UpstreamState::Unverified => LlmUpstreamSnapshot::Unverified,
    }
}

/// Gateway 对 Core health 文本再做一次防御性过滤，避免异常错误摘要泄露凭据。
pub(super) fn safe_upstream_error_summary(value: Option<&str>) -> String {
    let value = value.unwrap_or("上游调用失败").replace(['\r', '\n'], " ");
    let lower = value.to_ascii_lowercase();
    if [
        "authorization",
        "bearer",
        "api_key",
        "api key",
        "token",
        "secret",
        "sk-",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
    {
        return "上游调用失败（错误详情已隐藏）".to_owned();
    }
    let mut summary = value.chars().take(80).collect::<String>();
    if value.chars().count() > 80 {
        summary.push_str("...");
    }
    summary
}

pub(super) fn llm_health_ok(llm_health: &LlmHealthSnapshot) -> bool {
    llm_health.status.starts_with("ok(")
}

pub(super) fn healthz_status_detail(llm_health: &LlmHealthSnapshot) -> String {
    if llm_health.status == "ok(in-process)" {
        "in-process ok".to_owned()
    } else {
        format!("core {}", llm_health.status)
    }
}
