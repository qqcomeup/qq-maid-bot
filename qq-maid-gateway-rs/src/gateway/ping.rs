use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use super::{
    event::C2cMessage,
    logging::{mask_identifier, mask_scope_key, mask_url, mask_url_query},
};
use crate::{
    auth::{AccessTokenManager, AccessTokenSnapshot, AccessTokenSnapshotState},
    config::AppConfig,
};
use qq_maid_common::time_context::{
    diagnostic_time_unix_seconds, format_diagnostic_clock_time_for_display,
    format_diagnostic_elapsed_between_for_display, format_diagnostic_time_ago_for_display_at,
    format_diagnostic_time_for_display, format_diagnostic_time_without_unix_for_display,
    format_duration_for_display, now_diagnostic_time_for_display, now_unix_seconds,
    now_unix_seconds_marker,
};

const LLM_HEALTHZ_TIMEOUT: Duration = Duration::from_millis(800);
const HEARTBEAT_ACK_WARN_SECONDS: i64 = 90;
const HEARTBEAT_ACK_ERROR_SECONDS: i64 = 180;

#[derive(Debug, Clone)]
pub struct GatewayRuntimeStatus {
    pub pid: u32,
    pub instance_id: String,
    pub started_at: String,
    started_instant: Instant,
    state: Arc<Mutex<GatewayRuntimeSnapshot>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GatewayRuntimeSnapshot {
    pub state_error: Option<String>,
    pub last_gateway_connected_at: Option<String>,
    pub last_ready_at: Option<String>,
    pub last_resumed_at: Option<String>,
    pub last_heartbeat_ack_at: Option<String>,
    pub last_reconnect_at: Option<String>,
    pub last_invalid_session: Option<InvalidSessionSnapshot>,
    pub last_c2c_received_at: Option<String>,
    pub last_c2c_message_id: Option<String>,
    pub last_qq_send_success_at: Option<String>,
    pub last_qq_send_failure_at: Option<String>,
    pub last_qq_send_failure_summary: Option<String>,
    pub last_respond_success_at: Option<String>,
    pub last_respond_failure_at: Option<String>,
    pub last_respond_failure_summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSessionSnapshot {
    pub at: String,
    pub can_resume: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LlmHealthSnapshot {
    healthz_url: String,
    status: String,
}

impl GatewayRuntimeStatus {
    pub fn new() -> Self {
        let started_at = now_unix_seconds_marker();
        Self {
            pid: std::process::id(),
            instance_id: format!("gateway-{}-{started_at}", std::process::id()),
            started_at,
            started_instant: Instant::now(),
            state: Arc::new(Mutex::new(GatewayRuntimeSnapshot::default())),
        }
    }

    pub fn uptime_text(&self) -> String {
        format_duration_for_display(self.started_instant.elapsed())
    }

    pub fn snapshot(&self) -> GatewayRuntimeSnapshot {
        match self.state.lock() {
            Ok(state) => state.clone(),
            Err(_) => GatewayRuntimeSnapshot {
                state_error: Some("runtime state lock poisoned".to_owned()),
                ..GatewayRuntimeSnapshot::default()
            },
        }
    }

    pub fn record_gateway_connected(&self) {
        self.update_state(|state| {
            state.last_gateway_connected_at = Some(now_unix_seconds_marker())
        });
    }

    pub fn record_ready(&self) {
        self.update_state(|state| state.last_ready_at = Some(now_unix_seconds_marker()));
    }

    pub fn record_resumed(&self) {
        self.update_state(|state| state.last_resumed_at = Some(now_unix_seconds_marker()));
    }

    pub fn record_heartbeat_ack(&self) {
        self.update_state(|state| state.last_heartbeat_ack_at = Some(now_unix_seconds_marker()));
    }

    pub fn record_reconnect(&self) {
        self.update_state(|state| state.last_reconnect_at = Some(now_unix_seconds_marker()));
    }

    pub fn record_invalid_session(&self, can_resume: bool) {
        self.update_state(|state| {
            state.last_invalid_session = Some(InvalidSessionSnapshot {
                at: now_unix_seconds_marker(),
                can_resume,
            });
        });
    }

    pub fn record_c2c_message_received(&self, message: &C2cMessage) {
        self.update_state(|state| {
            state.last_c2c_received_at = Some(now_unix_seconds_marker());
            state.last_c2c_message_id = Some(mask_identifier(&message.message_id));
        });
    }

    pub fn record_qq_send_success(&self) {
        self.update_state(|state| state.last_qq_send_success_at = Some(now_unix_seconds_marker()));
    }

    pub fn record_qq_send_failure(&self, summary: impl Into<String>) {
        self.update_state(|state| {
            state.last_qq_send_failure_at = Some(now_unix_seconds_marker());
            state.last_qq_send_failure_summary = Some(compact_summary(summary.into()));
        });
    }

    pub fn record_respond_success(&self) {
        self.update_state(|state| state.last_respond_success_at = Some(now_unix_seconds_marker()));
    }

    pub fn record_respond_failure(&self, summary: impl Into<String>) {
        self.update_state(|state| {
            state.last_respond_failure_at = Some(now_unix_seconds_marker());
            state.last_respond_failure_summary = Some(compact_summary(summary.into()));
        });
    }

    fn update_state(&self, update: impl FnOnce(&mut GatewayRuntimeSnapshot)) {
        if let Ok(mut state) = self.state.lock() {
            update(&mut state);
        }
    }

    #[cfg(test)]
    fn new_for_test() -> Self {
        Self {
            pid: 42,
            instance_id: "gateway-test".to_owned(),
            started_at: "unix:1".to_owned(),
            started_instant: Instant::now() - Duration::from_secs(5),
            state: Arc::new(Mutex::new(GatewayRuntimeSnapshot::default())),
        }
    }
}

impl Default for GatewayRuntimeStatus {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PingMode {
    Summary,
    All,
}

pub fn is_ping_command(text: &str) -> bool {
    parse_ping_mode(text).is_some()
}

fn parse_ping_mode(text: &str) -> Option<PingMode> {
    let mut parts = text.split_whitespace();
    let command = parts.next()?;
    if !command.eq_ignore_ascii_case("/ping") {
        return None;
    }
    match (parts.next(), parts.next()) {
        (None, None) => Some(PingMode::Summary),
        (Some(arg), None) if arg.eq_ignore_ascii_case("all") => Some(PingMode::All),
        _ => None,
    }
}

pub async fn build_c2c_ping_reply(
    message: &C2cMessage,
    config: &AppConfig,
    runtime: &GatewayRuntimeStatus,
    auth: &AccessTokenManager,
) -> String {
    let token_snapshot = auth.snapshot().await;
    let llm_health = probe_llm_healthz(&config.respond_url).await;
    render_c2c_ping_reply(message, config, runtime, &token_snapshot, &llm_health)
}

fn render_c2c_ping_reply(
    message: &C2cMessage,
    config: &AppConfig,
    runtime: &GatewayRuntimeStatus,
    token_snapshot: &AccessTokenSnapshot,
    llm_health: &LlmHealthSnapshot,
) -> String {
    let mode = parse_ping_mode(&message.content).unwrap_or(PingMode::Summary);
    render_c2c_ping_reply_at(
        message,
        config,
        runtime,
        token_snapshot,
        llm_health,
        mode,
        now_unix_seconds(),
    )
}

fn render_c2c_ping_reply_at(
    message: &C2cMessage,
    config: &AppConfig,
    runtime: &GatewayRuntimeStatus,
    token_snapshot: &AccessTokenSnapshot,
    llm_health: &LlmHealthSnapshot,
    mode: PingMode,
    now_seconds: i64,
) -> String {
    let snapshot = runtime.snapshot();
    let current_scope = format!("private:{}", message.user_openid);
    // 默认视图只展示可判断健康的摘要；内部 ID、URL、Unix 秒等保留给 `/ping all`。
    let assessment =
        assess_ping_status(&snapshot, runtime, token_snapshot, llm_health, now_seconds);
    let title = match assessment.overall {
        PingSeverity::Normal => "# 🟢 服务运行正常",
        PingSeverity::Warning => "# 🟡 服务可用，但存在警告",
        PingSeverity::Error => "# 🔴 服务异常",
    };

    let mut lines = vec![
        title.to_owned(),
        String::new(),
        format!("> {}", assessment.summary),
        String::new(),
        "## 核心链路".to_owned(),
        "| 模块 | 状态 | 详情 |".to_owned(),
        "|---|---|---|".to_owned(),
    ];
    for row in &assessment.rows {
        lines.push(format!(
            "| {} | {} | {} |",
            markdown_cell(&row.module),
            markdown_cell(&row.status),
            markdown_cell(&row.detail)
        ));
    }

    lines.extend([String::new(), "## 最近事件".to_owned()]);
    for event in &assessment.events {
        lines.push(format!("- {event}"));
    }

    lines.extend([
        String::new(),
        "## 当前消息".to_owned(),
        "| 项目 | 内容 |".to_owned(),
        "|---|---|".to_owned(),
        "| 平台 | QQ 官方机器人 |".to_owned(),
        "| 场景 | 私聊 |".to_owned(),
        "| 事件 | C2C 消息 |".to_owned(),
        format!("| 附件 | {} |", message.attachments.len()),
        format!(
            "| 接收时间 | {} |",
            markdown_cell(&time_or_placeholder(message.timestamp.as_deref()))
        ),
    ]);

    if matches!(mode, PingMode::All) {
        lines.extend([String::new(), "## 调试详情".to_owned()]);
        lines.extend(render_ping_debug_details(
            message,
            config,
            runtime,
            token_snapshot,
            llm_health,
            &snapshot,
            &current_scope,
        ));
    }

    lines.join("\n")
}

fn render_ping_debug_details(
    message: &C2cMessage,
    config: &AppConfig,
    runtime: &GatewayRuntimeStatus,
    token_snapshot: &AccessTokenSnapshot,
    llm_health: &LlmHealthSnapshot,
    snapshot: &GatewayRuntimeSnapshot,
    current_scope: &str,
) -> Vec<String> {
    let invalid_session = snapshot
        .last_invalid_session
        .as_ref()
        .map(|item| {
            format!(
                "{} can_resume={}",
                format_diagnostic_time_for_display(&item.at),
                bool_text(item.can_resume)
            )
        })
        .unwrap_or_else(|| "无".to_owned());
    let state_error = snapshot.state_error.as_deref().unwrap_or("无");

    vec![
        "### 概览".to_owned(),
        format!(
            "- Gateway：{}",
            runtime_status_text(snapshot.state_error.as_deref())
        ),
        format!("- LLM healthz：{}", llm_health.status),
        format!("- 当前时间：{}", now_diagnostic_time_for_display()),
        format!("- pid：{}", runtime.pid),
        format!("- 运行时长：{}", runtime.uptime_text()),
        String::new(),
        "### Gateway".to_owned(),
        format!("- instance：{}", runtime.instance_id),
        format!(
            "- started_at：{}",
            format_diagnostic_time_for_display(&runtime.started_at)
        ),
        format!(
            "- websocket connected：{}",
            diagnostic_time_option_text(snapshot.last_gateway_connected_at.as_deref())
        ),
        format!(
            "- READY：{}",
            diagnostic_time_option_text(snapshot.last_ready_at.as_deref())
        ),
        format!(
            "- RESUMED：{}",
            diagnostic_time_option_text(snapshot.last_resumed_at.as_deref())
        ),
        format!(
            "- heartbeat ack：{}",
            diagnostic_time_option_text(snapshot.last_heartbeat_ack_at.as_deref())
        ),
        format!(
            "- reconnect：{}",
            diagnostic_time_option_text(snapshot.last_reconnect_at.as_deref())
        ),
        format!("- invalid session：{invalid_session}"),
        format!("- 状态读取错误：{state_error}"),
        String::new(),
        "### 消息".to_owned(),
        "- 平台：qq_official_gateway_rs".to_owned(),
        "- 事件类型：c2c_message".to_owned(),
        "- 会话类型：私聊".to_owned(),
        format!("- 当前消息 id：{}", mask_identifier(&message.message_id)),
        format!("- 当前用户：{}", mask_identifier(&message.user_openid)),
        format!("- 当前 scope_key：{}", mask_scope_key(&current_scope)),
        format!(
            "- 当前消息时间：{}",
            diagnostic_time_option_text(message.timestamp.as_deref())
        ),
        format!(
            "- 最近收到：{}",
            diagnostic_time_option_text(snapshot.last_c2c_received_at.as_deref())
        ),
        format!(
            "- 最近消息 id：{}",
            option_text(snapshot.last_c2c_message_id.as_deref())
        ),
        format!("- 附件数量：{}", message.attachments.len()),
        String::new(),
        "### 发送".to_owned(),
        format!(
            "- 最近 QQ 发送成功：{}",
            diagnostic_time_option_text(snapshot.last_qq_send_success_at.as_deref())
        ),
        format!(
            "- 最近 QQ 发送失败：{}",
            diagnostic_time_option_text(snapshot.last_qq_send_failure_at.as_deref())
        ),
        format!(
            "- 失败摘要：{}",
            option_text(snapshot.last_qq_send_failure_summary.as_deref())
        ),
        String::new(),
        "### LLM".to_owned(),
        format!("- respond：{}", mask_url(&config.respond_url)),
        format!("- healthz URL：{}", llm_health.healthz_url),
        format!("- healthz：{}", llm_health.status),
        format!(
            "- 最近 respond 成功：{}",
            diagnostic_time_option_text(snapshot.last_respond_success_at.as_deref())
        ),
        format!(
            "- 最近 respond 失败：{}",
            diagnostic_time_option_text(snapshot.last_respond_failure_at.as_deref())
        ),
        format!(
            "- 失败摘要：{}",
            option_text(snapshot.last_respond_failure_summary.as_deref())
        ),
        String::new(),
        "### 配置".to_owned(),
        format!("- sandbox：{}", bool_text(config.sandbox)),
        format!("- api_base：{}", url_host_path(&config.api_base)),
        format!("- respond_url：{}", url_host_path(&config.respond_url)),
        format!("- respond query：{}", mask_url_query(&config.respond_url)),
        format!("- Markdown：{}", bool_text(config.enable_markdown)),
        format!("- Image：{}", bool_text(config.enable_image)),
        format!("- verbose_log：{}", bool_text(config.verbose_log)),
        format!("- 访问令牌缓存：{}", token_snapshot_text(token_snapshot)),
        format!(
            "- refresh margin：{}s",
            token_snapshot.refresh_margin_seconds
        ),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum PingSeverity {
    Normal,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PingTableRow {
    module: String,
    status: String,
    detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PingAssessment {
    overall: PingSeverity,
    summary: String,
    rows: Vec<PingTableRow>,
    events: Vec<String>,
}

fn assess_ping_status(
    snapshot: &GatewayRuntimeSnapshot,
    runtime: &GatewayRuntimeStatus,
    token_snapshot: &AccessTokenSnapshot,
    llm_health: &LlmHealthSnapshot,
    now_seconds: i64,
) -> PingAssessment {
    let mut overall = PingSeverity::Normal;
    let mut notes = Vec::new();

    // 状态判定只使用已有采集字段，避免 `/ping` 为了展示改动底层连接和重连语义。
    let gateway_row = gateway_row(snapshot, runtime);
    collect_row_severity(&gateway_row, &mut overall);
    if snapshot.state_error.is_some() {
        notes.push("运行时状态读取失败".to_owned());
        overall = overall.max(PingSeverity::Error);
    }

    let qq_row = qq_connection_row(snapshot, now_seconds);
    collect_row_severity(&qq_row, &mut overall);
    collect_reconnect_note(snapshot, &mut notes, &mut overall);

    let heartbeat_row = heartbeat_row(snapshot, runtime, now_seconds);
    collect_row_severity(&heartbeat_row, &mut overall);

    let llm_row = llm_row(llm_health);
    collect_row_severity(&llm_row, &mut overall);

    let receive_row = receive_row(snapshot, now_seconds);
    collect_row_severity(&receive_row, &mut overall);

    let send_row = send_row(snapshot, now_seconds);
    collect_row_severity(&send_row, &mut overall);

    collect_token_note(token_snapshot, &mut notes, &mut overall);
    collect_send_respond_notes(snapshot, &mut notes, &mut overall);

    let rows = vec![
        gateway_row,
        qq_row,
        heartbeat_row,
        llm_row,
        receive_row,
        send_row,
    ];
    let events = recent_events(snapshot, llm_health, now_seconds);
    let summary = summary_text(overall, &notes);

    PingAssessment {
        overall,
        summary,
        rows,
        events,
    }
}

fn gateway_row(snapshot: &GatewayRuntimeSnapshot, runtime: &GatewayRuntimeStatus) -> PingTableRow {
    match snapshot.state_error.as_deref() {
        Some(error) => row("Gateway", PingSeverity::Error, "异常", error),
        None => row(
            "Gateway",
            PingSeverity::Normal,
            "正常",
            &format!("已运行 {}", runtime.uptime_text()),
        ),
    }
}

fn qq_connection_row(snapshot: &GatewayRuntimeSnapshot, now_seconds: i64) -> PingTableRow {
    let Some(connected_at) = snapshot.last_gateway_connected_at.as_deref() else {
        return row(
            "QQ 连接",
            PingSeverity::Warning,
            "待确认",
            "尚未记录 WebSocket 连接",
        );
    };

    let mut detail = format!("WebSocket 已连接于 {}", time_ago(connected_at, now_seconds));
    if let Some(reconnect_at) = snapshot.last_reconnect_at.as_deref() {
        let reconnect_ago = time_ago(reconnect_at, now_seconds);
        if let Some(recovered_at) = reconnect_recovered_at(snapshot, reconnect_at) {
            let recovered =
                format_diagnostic_elapsed_between_for_display(reconnect_at, recovered_at)
                    .map(|elapsed| format!("{elapsed}后恢复"))
                    .unwrap_or_else(|| "已恢复".to_owned());
            detail = format!("{reconnect_ago}发生重连，{recovered}");
        } else {
            return row(
                "QQ 连接",
                PingSeverity::Error,
                "异常",
                &format!("{reconnect_ago}发生重连，当前未发现恢复记录"),
            );
        }
    }

    row("QQ 连接", PingSeverity::Normal, "已连接", &detail)
}

fn heartbeat_row(
    snapshot: &GatewayRuntimeSnapshot,
    runtime: &GatewayRuntimeStatus,
    now_seconds: i64,
) -> PingTableRow {
    let Some(ack_at) = snapshot.last_heartbeat_ack_at.as_deref() else {
        let severity =
            if runtime.started_instant.elapsed().as_secs() > HEARTBEAT_ACK_ERROR_SECONDS as u64 {
                PingSeverity::Error
            } else {
                PingSeverity::Warning
            };
        return row("心跳", severity, "待确认", "尚未收到 ACK");
    };

    let age = age_seconds(ack_at, now_seconds);
    let severity = if age.is_some_and(|age| age > HEARTBEAT_ACK_ERROR_SECONDS) {
        PingSeverity::Error
    } else if age.is_some_and(|age| age > HEARTBEAT_ACK_WARN_SECONDS) {
        PingSeverity::Warning
    } else {
        PingSeverity::Normal
    };
    let label = match severity {
        PingSeverity::Normal => "正常",
        PingSeverity::Warning => "延迟偏高",
        PingSeverity::Error => "超时",
    };
    row(
        "心跳",
        severity,
        label,
        &format!("{}收到 ACK", time_ago(ack_at, now_seconds)),
    )
}

fn llm_row(llm_health: &LlmHealthSnapshot) -> PingTableRow {
    if llm_health_ok(llm_health) {
        row(
            "LLM",
            PingSeverity::Normal,
            "正常",
            &healthz_status_detail(llm_health),
        )
    } else {
        row(
            "LLM",
            PingSeverity::Error,
            "异常",
            &healthz_status_detail(llm_health),
        )
    }
}

fn receive_row(snapshot: &GatewayRuntimeSnapshot, now_seconds: i64) -> PingTableRow {
    match snapshot.last_c2c_received_at.as_deref() {
        Some(received_at) => row(
            "消息接收",
            PingSeverity::Normal,
            "正常",
            &format!("{}收到当前消息", time_ago(received_at, now_seconds)),
        ),
        None => row(
            "消息接收",
            PingSeverity::Warning,
            "待确认",
            "尚未记录收到消息",
        ),
    }
}

fn send_row(snapshot: &GatewayRuntimeSnapshot, now_seconds: i64) -> PingTableRow {
    match latest_attempt(
        snapshot.last_qq_send_success_at.as_deref(),
        snapshot.last_qq_send_failure_at.as_deref(),
    ) {
        Some(AttemptStatus::Success { at }) => row(
            "消息发送",
            PingSeverity::Normal,
            "正常",
            &format!("最近一次发送尝试成功于 {}", time_ago(at, now_seconds)),
        ),
        Some(AttemptStatus::Failure { at }) => {
            let mut detail = format!("最近一次发送尝试失败于 {}", time_ago(at, now_seconds));
            if let Some(summary) = snapshot.last_qq_send_failure_summary.as_deref() {
                detail.push_str(&format!("：{summary}"));
            }
            row("消息发送", PingSeverity::Error, "异常", &detail)
        }
        None => row(
            "消息发送",
            PingSeverity::Normal,
            "未发现失败",
            "暂无发送尝试记录",
        ),
    }
}

fn row(module: &str, severity: PingSeverity, label: &str, detail: &str) -> PingTableRow {
    PingTableRow {
        module: module.to_owned(),
        status: format!("{} {label}", severity_icon(severity)),
        detail: detail.to_owned(),
    }
}

fn severity_icon(severity: PingSeverity) -> &'static str {
    match severity {
        PingSeverity::Normal => "🟢",
        PingSeverity::Warning => "🟡",
        PingSeverity::Error => "🔴",
    }
}

fn collect_row_severity(row: &PingTableRow, overall: &mut PingSeverity) {
    if row.status.starts_with("🔴") {
        *overall = (*overall).max(PingSeverity::Error);
    } else if row.status.starts_with("🟡") {
        *overall = (*overall).max(PingSeverity::Warning);
    }
}

fn collect_reconnect_note(
    snapshot: &GatewayRuntimeSnapshot,
    notes: &mut Vec<String>,
    overall: &mut PingSeverity,
) {
    if let Some(reconnect_at) = snapshot.last_reconnect_at.as_deref() {
        if reconnect_recovered_at(snapshot, reconnect_at).is_some() {
            notes.push("最近发生过重连并已恢复".to_owned());
            *overall = (*overall).max(PingSeverity::Warning);
        } else {
            notes.push("最近重连尚未发现恢复记录".to_owned());
            *overall = (*overall).max(PingSeverity::Error);
        }
    }
    if let Some(invalid) = snapshot.last_invalid_session.as_ref() {
        if session_recovered_at(snapshot, &invalid.at).is_some() {
            notes.push("最近 invalid session 已恢复".to_owned());
            *overall = (*overall).max(PingSeverity::Warning);
        } else {
            notes.push("invalid session 尚未恢复".to_owned());
            *overall = (*overall).max(PingSeverity::Error);
        }
    }
}

fn collect_token_note(
    token_snapshot: &AccessTokenSnapshot,
    notes: &mut Vec<String>,
    overall: &mut PingSeverity,
) {
    if matches!(token_snapshot.state, AccessTokenSnapshotState::RefreshDue) {
        notes.push("访问令牌即将刷新".to_owned());
        *overall = (*overall).max(PingSeverity::Warning);
    }
}

fn collect_send_respond_notes(
    snapshot: &GatewayRuntimeSnapshot,
    notes: &mut Vec<String>,
    overall: &mut PingSeverity,
) {
    match latest_attempt(
        snapshot.last_qq_send_success_at.as_deref(),
        snapshot.last_qq_send_failure_at.as_deref(),
    ) {
        Some(AttemptStatus::Failure { .. }) => {
            notes.push("最近一次 QQ 发送尝试失败".to_owned());
            *overall = (*overall).max(PingSeverity::Error);
        }
        Some(AttemptStatus::Success { .. }) if snapshot.last_qq_send_failure_at.is_some() => {
            notes.push("QQ 发送失败已恢复".to_owned());
            *overall = (*overall).max(PingSeverity::Warning);
        }
        _ => {}
    }

    match latest_attempt(
        snapshot.last_respond_success_at.as_deref(),
        snapshot.last_respond_failure_at.as_deref(),
    ) {
        Some(AttemptStatus::Failure { .. }) => {
            notes.push("最近一次 LLM respond 失败".to_owned());
            *overall = (*overall).max(PingSeverity::Warning);
        }
        Some(AttemptStatus::Success { .. }) if snapshot.last_respond_failure_at.is_some() => {
            notes.push("LLM respond 失败已恢复".to_owned());
            *overall = (*overall).max(PingSeverity::Warning);
        }
        _ => {}
    }
}

fn summary_text(overall: PingSeverity, notes: &[String]) -> String {
    match overall {
        PingSeverity::Normal => {
            "Gateway、QQ WebSocket 和 LLM 均正常，未发现未恢复异常。".to_owned()
        }
        PingSeverity::Warning => {
            let detail = notes
                .first()
                .cloned()
                .unwrap_or_else(|| "存在需要关注的状态".to_owned());
            format!("服务当前可用，但需要关注：{detail}。")
        }
        PingSeverity::Error => {
            let detail = notes
                .first()
                .cloned()
                .unwrap_or_else(|| "存在影响服务的异常".to_owned());
            format!("检测到影响服务的异常：{detail}。")
        }
    }
}

fn recent_events(
    snapshot: &GatewayRuntimeSnapshot,
    llm_health: &LlmHealthSnapshot,
    now_seconds: i64,
) -> Vec<String> {
    let mut events = Vec::new();
    if let Some(state_error) = snapshot.state_error.as_deref() {
        events.push(format!("状态读取失败：{state_error}"));
    }
    if !llm_health_ok(llm_health) {
        events.push(format!("LLM healthz 异常：{}", llm_health.status));
    }
    if let Some(reconnect_at) = snapshot.last_reconnect_at.as_deref() {
        events.push(format!(
            "`{}` QQ WebSocket 断线重连（{}）",
            format_diagnostic_clock_time_for_display(reconnect_at),
            time_ago(reconnect_at, now_seconds)
        ));
        if let Some(recovered_at) = reconnect_recovered_at(snapshot, reconnect_at) {
            let elapsed = format_diagnostic_elapsed_between_for_display(reconnect_at, recovered_at)
                .map(|value| format!("{value}后"))
                .unwrap_or_default();
            events.push(format!(
                "`{}` Session 恢复成功{}",
                format_diagnostic_clock_time_for_display(recovered_at),
                elapsed
            ));
        } else {
            events.push("QQ WebSocket 重连后尚未发现 READY 或 RESUMED".to_owned());
        }
    }
    if let Some(invalid) = snapshot.last_invalid_session.as_ref() {
        let resume_text = if invalid.can_resume {
            "can_resume=true"
        } else {
            "can_resume=false"
        };
        if session_recovered_at(snapshot, &invalid.at).is_some() {
            events.push(format!(
                "`{}` invalid session 已恢复（{}）",
                format_diagnostic_clock_time_for_display(&invalid.at),
                resume_text
            ));
        } else {
            events.push(format!(
                "`{}` invalid session 尚未恢复（{}）",
                format_diagnostic_clock_time_for_display(&invalid.at),
                resume_text
            ));
        }
    }
    append_attempt_event(
        &mut events,
        "QQ 发送",
        snapshot.last_qq_send_success_at.as_deref(),
        snapshot.last_qq_send_failure_at.as_deref(),
        snapshot.last_qq_send_failure_summary.as_deref(),
        now_seconds,
    );
    append_attempt_event(
        &mut events,
        "LLM respond",
        snapshot.last_respond_success_at.as_deref(),
        snapshot.last_respond_failure_at.as_deref(),
        snapshot.last_respond_failure_summary.as_deref(),
        now_seconds,
    );
    if llm_health_ok(llm_health)
        && snapshot.last_qq_send_failure_at.is_none()
        && snapshot.last_respond_failure_at.is_none()
        && snapshot.last_invalid_session.is_none()
    {
        events.push("未发现发送、LLM 或 Session 异常".to_owned());
    }
    if events.is_empty() {
        events.push("未发现需要关注的事件".to_owned());
    }
    events
}

fn append_attempt_event(
    events: &mut Vec<String>,
    label: &str,
    success_at: Option<&str>,
    failure_at: Option<&str>,
    failure_summary: Option<&str>,
    now_seconds: i64,
) {
    match latest_attempt(success_at, failure_at) {
        Some(AttemptStatus::Failure { at }) => {
            let summary = failure_summary
                .filter(|value| !value.trim().is_empty())
                .map(|value| format!("：{value}"))
                .unwrap_or_default();
            events.push(format!(
                "{}失败于 {}{}",
                label,
                time_ago(at, now_seconds),
                summary
            ));
        }
        Some(AttemptStatus::Success { at }) if failure_at.is_some() => {
            events.push(format!(
                "{}曾失败，最近一次尝试成功于 {}",
                label,
                time_ago(at, now_seconds)
            ));
        }
        _ => {}
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptStatus<'a> {
    Success { at: &'a str },
    Failure { at: &'a str },
}

fn latest_attempt<'a>(
    success_at: Option<&'a str>,
    failure_at: Option<&'a str>,
) -> Option<AttemptStatus<'a>> {
    match (success_at, failure_at) {
        (Some(success), Some(failure)) => {
            let success_seconds = diagnostic_time_unix_seconds(success);
            let failure_seconds = diagnostic_time_unix_seconds(failure);
            if failure_seconds >= success_seconds {
                Some(AttemptStatus::Failure { at: failure })
            } else {
                Some(AttemptStatus::Success { at: success })
            }
        }
        (Some(success), None) => Some(AttemptStatus::Success { at: success }),
        (None, Some(failure)) => Some(AttemptStatus::Failure { at: failure }),
        (None, None) => None,
    }
}

fn reconnect_recovered_at<'a>(
    snapshot: &'a GatewayRuntimeSnapshot,
    reconnect_at: &str,
) -> Option<&'a str> {
    latest_after(
        [
            snapshot.last_resumed_at.as_deref(),
            snapshot.last_ready_at.as_deref(),
        ],
        reconnect_at,
    )
}

fn session_recovered_at<'a>(
    snapshot: &'a GatewayRuntimeSnapshot,
    invalid_at: &str,
) -> Option<&'a str> {
    latest_after(
        [
            snapshot.last_resumed_at.as_deref(),
            snapshot.last_ready_at.as_deref(),
        ],
        invalid_at,
    )
}

fn latest_after<'a, const N: usize>(values: [Option<&'a str>; N], base: &str) -> Option<&'a str> {
    let base_seconds = diagnostic_time_unix_seconds(base)?;
    values
        .into_iter()
        .flatten()
        .filter(|value| {
            diagnostic_time_unix_seconds(value).is_some_and(|seconds| seconds >= base_seconds)
        })
        .max_by_key(|value| diagnostic_time_unix_seconds(value).unwrap_or(i64::MIN))
}

fn llm_health_ok(llm_health: &LlmHealthSnapshot) -> bool {
    llm_health.status.starts_with("ok(status=")
}

fn healthz_status_detail(llm_health: &LlmHealthSnapshot) -> String {
    llm_health
        .status
        .strip_prefix("ok(status=")
        .and_then(|rest| rest.strip_suffix(')'))
        .map(|status| format!("healthz {status}"))
        .unwrap_or_else(|| format!("healthz {}", llm_health.status))
}

fn runtime_status_text(state_error: Option<&str>) -> &'static str {
    if state_error.is_some() { "ERROR" } else { "OK" }
}

fn time_ago(value: &str, now_seconds: i64) -> String {
    format_diagnostic_time_ago_for_display_at(value, now_seconds)
        .unwrap_or_else(|| format_diagnostic_time_without_unix_for_display(value))
}

fn time_or_placeholder(value: Option<&str>) -> String {
    value
        .filter(|text| !text.trim().is_empty())
        .map(format_diagnostic_time_without_unix_for_display)
        .unwrap_or_else(|| "未提供".to_owned())
}

fn age_seconds(value: &str, now_seconds: i64) -> Option<i64> {
    diagnostic_time_unix_seconds(value).map(|seconds| now_seconds.saturating_sub(seconds))
}

fn markdown_cell(value: &str) -> String {
    value
        .replace('|', "\\|")
        .replace('\r', " ")
        .replace('\n', " ")
}

async fn probe_llm_healthz(respond_url: &str) -> LlmHealthSnapshot {
    let Ok(healthz_url) = healthz_url_from_respond_url(respond_url) else {
        return LlmHealthSnapshot {
            healthz_url: "invalid url".to_owned(),
            status: "invalid url".to_owned(),
        };
    };
    let healthz_url_text = mask_url(healthz_url.as_str());

    let client = match reqwest::Client::builder()
        .timeout(LLM_HEALTHZ_TIMEOUT)
        .build()
    {
        Ok(client) => client,
        Err(_) => {
            return LlmHealthSnapshot {
                healthz_url: healthz_url_text,
                status: "client build failed".to_owned(),
            };
        }
    };

    match client.get(healthz_url.clone()).send().await {
        Ok(response) => {
            let status = response.status();
            let summary = if status.is_success() {
                format!("ok(status={})", status.as_u16())
            } else {
                format!("http status {}", status.as_u16())
            };
            LlmHealthSnapshot {
                healthz_url: healthz_url_text,
                status: summary,
            }
        }
        Err(error) => LlmHealthSnapshot {
            healthz_url: healthz_url_text,
            status: healthz_error_summary(&error),
        },
    }
}

fn healthz_url_from_respond_url(respond_url: &str) -> Result<reqwest::Url, ()> {
    let mut url = reqwest::Url::parse(respond_url.trim()).map_err(|_| ())?;
    url.set_path("/healthz");
    url.set_query(None);
    url.set_fragment(None);
    Ok(url)
}

fn healthz_error_summary(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "timeout".to_owned()
    } else if error.is_connect() {
        "connect failed".to_owned()
    } else if error.is_request() {
        "request failed".to_owned()
    } else {
        "healthz failed".to_owned()
    }
}

fn token_snapshot_text(snapshot: &AccessTokenSnapshot) -> String {
    let state = match snapshot.state {
        AccessTokenSnapshotState::Empty => "empty",
        AccessTokenSnapshotState::Cached => "cached",
        AccessTokenSnapshotState::RefreshDue => "refresh_due",
    };
    match snapshot.expires_in_seconds {
        Some(seconds) => format!("{state}, expires_in={seconds}s"),
        None => state.to_owned(),
    }
}

fn url_host_path(url: &str) -> String {
    match reqwest::Url::parse(url.trim()) {
        Ok(parsed) => {
            let host = parsed.host_str().unwrap_or("unknown-host");
            let port = parsed
                .port()
                .map(|port| format!(":{port}"))
                .unwrap_or_default();
            format!("{host}{port}{}", parsed.path())
        }
        Err(_) => "invalid url".to_owned(),
    }
}

fn option_text(value: Option<&str>) -> &str {
    value.filter(|text| !text.trim().is_empty()).unwrap_or("无")
}

fn diagnostic_time_option_text(value: Option<&str>) -> String {
    value
        .filter(|text| !text.trim().is_empty())
        .map(format_diagnostic_time_for_display)
        .unwrap_or_else(|| "无".to_owned())
}

fn bool_text(value: bool) -> &'static str {
    if value { "enabled" } else { "disabled" }
}

fn compact_summary(summary: String) -> String {
    let text = summary.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut compact = text.chars().take(120).collect::<String>();
    if text.chars().count() > 120 {
        compact.push_str("...");
    }
    compact
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
        time::Duration,
    };

    use super::*;

    fn config() -> AppConfig {
        AppConfig {
            app_id: "appid".to_owned(),
            app_secret: "app-secret-value".to_owned(),
            sandbox: false,
            api_base: "https://api.sgroup.qq.com".to_owned(),
            token_refresh_margin: Duration::from_secs(60),
            respond_url: "http://127.0.0.1:8787/v1/respond?debug=1&token=real-token&timeout=800"
                .to_owned(),
            enable_markdown: false,
            enable_image: true,
            verbose_log: false,
            push_enabled: true,
            push_host: "127.0.0.1".to_owned(),
            push_port: 8788,
            push_token: None,
        }
    }

    fn message() -> C2cMessage {
        C2cMessage {
            message_id: "msg-sensitive-123456".to_owned(),
            user_openid: "user-openid-123456".to_owned(),
            content: "/ping".to_owned(),
            reply: None,
            timestamp: Some("2026-06-10T12:00:00+08:00".to_owned()),
            attachments: Vec::new(),
        }
    }

    fn message_with_content(content: &str) -> C2cMessage {
        C2cMessage {
            content: content.to_owned(),
            ..message()
        }
    }

    fn token_snapshot() -> AccessTokenSnapshot {
        AccessTokenSnapshot {
            state: AccessTokenSnapshotState::Cached,
            expires_in_seconds: Some(120),
            refresh_margin_seconds: 60,
        }
    }

    fn health(status: &str) -> LlmHealthSnapshot {
        LlmHealthSnapshot {
            healthz_url: "http://127.0.0.1:8787/healthz".to_owned(),
            status: status.to_owned(),
        }
    }

    fn spawn_one_response_server(response: &'static [u8]) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut buffer = [0; 1024];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(response);
        });
        format!("http://{addr}/v1/respond?token=server-token&debug=1")
    }

    #[test]
    fn detects_ping_command_case_insensitively() {
        assert!(is_ping_command(" /PING "));
        assert!(is_ping_command(" /ping all "));
        assert!(is_ping_command(" /PING ALL "));
        assert!(!is_ping_command("/ping now"));
        assert!(!is_ping_command("/ping all extra"));
    }

    #[test]
    fn runtime_records_recent_events_without_full_message_id() {
        let runtime = GatewayRuntimeStatus::new_for_test();
        let message = message();

        runtime.record_gateway_connected();
        runtime.record_ready();
        runtime.record_resumed();
        runtime.record_heartbeat_ack();
        runtime.record_reconnect();
        runtime.record_invalid_session(false);
        runtime.record_c2c_message_received(&message);
        runtime.record_respond_success();
        runtime.record_respond_failure("http status 500\nwith details");
        runtime.record_qq_send_success();
        runtime.record_qq_send_failure("request failed timeout with a long but safe summary");

        let snapshot = runtime.snapshot();

        assert!(snapshot.last_gateway_connected_at.is_some());
        assert!(snapshot.last_ready_at.is_some());
        assert!(snapshot.last_resumed_at.is_some());
        assert!(snapshot.last_heartbeat_ack_at.is_some());
        assert!(snapshot.last_reconnect_at.is_some());
        assert_eq!(
            snapshot
                .last_invalid_session
                .as_ref()
                .map(|item| item.can_resume),
            Some(false)
        );
        assert_eq!(
            snapshot.last_c2c_message_id.as_deref(),
            Some("******123456")
        );
        assert!(snapshot.last_respond_success_at.is_some());
        assert_eq!(
            snapshot.last_respond_failure_summary.as_deref(),
            Some("http status 500 with details")
        );
        assert!(snapshot.last_qq_send_success_at.is_some());
        assert!(snapshot.last_qq_send_failure_at.is_some());
    }

    #[test]
    fn renders_summary_ping_reply_without_debug_noise_or_secrets() {
        let runtime = GatewayRuntimeStatus::new_for_test();
        runtime.update_state(|state| {
            state.last_gateway_connected_at = Some("unix:1000".to_owned());
            state.last_ready_at = Some("unix:1001".to_owned());
            state.last_heartbeat_ack_at = Some("unix:1190".to_owned());
            state.last_c2c_received_at = Some("unix:1200".to_owned());
            state.last_qq_send_success_at = Some("unix:1180".to_owned());
            state.last_respond_success_at = Some("unix:1170".to_owned());
        });

        let reply = render_c2c_ping_reply_at(
            &message(),
            &config(),
            &runtime,
            &token_snapshot(),
            &health("ok(status=200)"),
            PingMode::Summary,
            1200,
        );

        assert!(reply.contains("# 🟢 服务运行正常"));
        assert!(reply.contains("## 核心链路"));
        assert!(reply.contains("| Gateway | 🟢 正常 | 已运行 "));
        assert!(reply.contains("| QQ 连接 | 🟢 已连接 | WebSocket 已连接于 3分钟20秒前 |"));
        assert!(reply.contains("| 心跳 | 🟢 正常 | 10秒前收到 ACK |"));
        assert!(reply.contains("| LLM | 🟢 正常 | healthz 200 |"));
        assert!(reply.contains("| 消息接收 | 🟢 正常 | 刚刚收到当前消息 |"));
        assert!(reply.contains("| 消息发送 | 🟢 正常 | 最近一次发送尝试成功于 20秒前 |"));
        assert!(reply.contains("- 未发现发送、LLM 或 Session 异常"));
        assert!(reply.contains("| 接收时间 | 2026-06-10 12:00:00 +08:00 |"));
        assert!(!reply.contains("## 调试详情"));
        assert!(!reply.contains("pid："));
        assert!(!reply.contains("instance："));
        assert!(!reply.contains("respond_url："));
        assert!(!reply.contains("当前用户："));
        assert!(!reply.contains("scope_key"));
        assert!(!reply.contains("unix:"));
        assert!(!reply.contains("user-openid-123456"));
        assert!(!reply.contains("msg-sensitive-123456"));
        assert!(!reply.contains("real-token"));
        assert!(!reply.contains("app-secret-value"));
        assert!(!reply.contains("Authorization"));
    }

    #[test]
    fn renders_ping_all_with_debug_details_without_secrets() {
        let runtime = GatewayRuntimeStatus::new_for_test();
        runtime.update_state(|state| {
            state.last_gateway_connected_at = Some("unix:1000".to_owned());
            state.last_ready_at = Some("unix:1001".to_owned());
            state.last_resumed_at = Some("unix:1105".to_owned());
            state.last_heartbeat_ack_at = Some("unix:1190".to_owned());
            state.last_reconnect_at = Some("unix:1100".to_owned());
            state.last_c2c_received_at = Some("unix:1200".to_owned());
            state.last_c2c_message_id = Some("******123456".to_owned());
            state.last_respond_failure_at = Some("unix:1140".to_owned());
            state.last_respond_failure_summary = Some("connect failed".to_owned());
            state.last_respond_success_at = Some("unix:1170".to_owned());
            state.last_qq_send_failure_at = Some("unix:1130".to_owned());
            state.last_qq_send_failure_summary = Some("http status 429".to_owned());
            state.last_qq_send_success_at = Some("unix:1180".to_owned());
        });

        let reply = render_c2c_ping_reply_at(
            &message_with_content("/ping all"),
            &config(),
            &runtime,
            &token_snapshot(),
            &health("ok(status=200)"),
            PingMode::All,
            1200,
        );

        assert!(reply.contains("# 🟡 服务可用，但存在警告"));
        assert!(reply.contains("5秒后恢复"));
        assert!(reply.contains("QQ 发送曾失败，最近一次尝试成功于 20秒前"));
        assert!(reply.contains("LLM respond曾失败，最近一次尝试成功于 30秒前"));
        assert!(reply.contains("## 调试详情"));
        assert!(reply.contains("### 概览"));
        assert!(reply.contains("### Gateway"));
        assert!(reply.contains("### 消息"));
        assert!(reply.contains("### 发送"));
        assert!(reply.contains("### LLM"));
        assert!(reply.contains("### 配置"));
        assert!(reply.contains("LLM healthz：ok(status=200)"));
        assert!(reply.contains("当前时间："));
        assert!(reply.contains("+08:00 (unix:"));
        assert!(reply.contains("started_at：1970-01-01 08:00:01 +08:00 (unix:1)"));
        assert!(reply.contains("当前消息时间：2026-06-10 12:00:00 +08:00"));
        assert!(!reply.contains("当前消息时间：2026-06-10T12:00:00+08:00"));
        assert!(!reply.contains("最近收到：unix:"));
        assert!(!reply.contains("最近 respond 失败：unix:"));
        assert!(!reply.contains("最近 QQ 发送失败：unix:"));
        assert!(reply.contains("respond query：debug=1&token=***&timeout=800"));
        assert!(reply.contains("当前用户：******123456"));
        assert!(reply.contains("当前 scope_key：private:******123456"));
        assert!(reply.contains("访问令牌缓存：cached, expires_in=120s"));
        assert!(!reply.contains("记忆模块"));
        assert!(!reply.contains("存储"));
        assert!(!reply.contains("user-openid-123456"));
        assert!(!reply.contains("msg-sensitive-123456"));
        assert!(!reply.contains("real-token"));
        assert!(!reply.contains("app-secret-value"));
        assert!(!reply.contains("Authorization"));
    }

    #[tokio::test]
    async fn build_ping_degrades_invalid_llm_url_to_field_summary() {
        let mut config = config();
        config.respond_url = "http://".to_owned();
        let runtime = GatewayRuntimeStatus::new_for_test();
        let auth = AccessTokenManager::new(
            reqwest::Client::new(),
            "appid",
            "app-secret-value",
            Duration::from_secs(60),
        );

        let reply = build_c2c_ping_reply(&message(), &config, &runtime, &auth).await;

        assert!(reply.contains("# 🔴 服务异常"));
        assert!(reply.contains("| LLM | 🔴 异常 | healthz invalid url |"));
        assert!(reply.contains("LLM healthz 异常：invalid url"));
        assert!(!reply.contains("respond_url：invalid url"));
        assert!(!reply.contains("访问令牌缓存：empty"));
        assert!(!reply.contains("app-secret-value"));
    }

    #[tokio::test]
    async fn llm_healthz_probe_reports_http_status_and_masks_url() {
        let respond_url = spawn_one_response_server(
            b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n",
        );

        let result = probe_llm_healthz(&respond_url).await;

        assert_eq!(result.status, "http status 503");
        assert!(result.healthz_url.ends_with("/healthz"));
        assert!(!result.healthz_url.contains("server-token"));
    }
}
