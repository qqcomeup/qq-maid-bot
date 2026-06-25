//! OpenAI 兼容 Chat Completions adapter。
//!
//! OpenAI fallback 和 DeepSeek 都复用同一套 `/chat/completions` HTTP/SSE 实现，
//! 只在 base URL、API key 和模型规则上区分。

use reqwest::{StatusCode, header};
use serde_json::{Value, json};

use crate::{
    error::LlmError,
    metrics::MetricsRecorder,
    provider::{
        ChatOutcome,
        types::{ChatMessage, ChatRole, TokenUsage},
    },
    sse::{parse_sse_frame, take_sse_frame},
};

use super::fallback::{
    should_retry_non_stream_after_empty_stream, should_retry_non_stream_after_stream_error,
};

const OPENAI_DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";

/// OpenAI 兼容 Chat Completions 客户端包装。
#[derive(Clone)]
pub(crate) struct ChatCompletionsClient {
    client: reqwest::Client,
    api_key: String,
    base_url: Option<String>,
}

impl ChatCompletionsClient {
    pub(crate) fn new(
        api_key: impl Into<String>,
        base_url: Option<&str>,
        http_client: reqwest::Client,
    ) -> Self {
        Self {
            client: http_client,
            api_key: api_key.into(),
            base_url: base_url
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned),
        }
    }
}

/// 执行可选流式 Chat Completions，并在流式失败或空流时补一次非流式请求。
pub(crate) async fn chat_completions_with_stream_fallback(
    stream: bool,
    client: &ChatCompletionsClient,
    provider: &str,
    model: &str,
    max_output_tokens: u64,
    messages: &[ChatMessage],
) -> Result<ChatOutcome, LlmError> {
    if stream {
        match stream_completion(client, provider, model, max_output_tokens, messages).await {
            Ok(outcome) => {
                if !should_retry_non_stream_after_empty_stream(&outcome) {
                    return Ok(outcome);
                }
                tracing::warn!(
                    provider,
                    model = %model,
                    "streaming chat completions returned empty reply; retrying once with non-stream request"
                );
            }
            Err(err) => {
                // 兼容网关经常只在 SSE 链路上不稳定；先补同 provider 非流式请求，
                // 避免过早切换到跨模型候选并产生额外行为差异。
                if !should_retry_non_stream_after_stream_error(&err) {
                    return Err(err);
                }
                tracing::warn!(
                    provider,
                    model = %model,
                    error_code = err.code.as_str(),
                    error_stage = err.stage.as_str(),
                    "streaming chat completions failed; retrying once with non-stream request"
                );
            }
        }
    }

    non_stream_completion(client, provider, model, max_output_tokens, messages).await
}

fn chat_completions_url(base_url: Option<&str>) -> String {
    let base_url = base_url
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(OPENAI_DEFAULT_BASE_URL);
    format!("{}/chat/completions", base_url.trim_end_matches('/'))
}

fn chat_completions_payload(
    messages: &[ChatMessage],
    model: &str,
    max_output_tokens: u64,
    stream: bool,
) -> Result<Value, LlmError> {
    let messages = chat_completions_messages(messages)?;
    let mut payload = json!({
        "model": model,
        "messages": messages,
        "max_tokens": max_output_tokens,
    });
    if stream {
        payload["stream"] = json!(true);
        // 部分兼容网关忽略该选项；官方接口会在最终 chunk 返回 usage。
        payload["stream_options"] = json!({"include_usage": true});
    }
    Ok(payload)
}

fn chat_completions_messages(messages: &[ChatMessage]) -> Result<Vec<Value>, LlmError> {
    if messages.is_empty() {
        return Err(LlmError::new(
            "bad_request",
            "messages must not be empty",
            "request",
        ));
    }
    let converted = messages
        .iter()
        .filter(|message| !message.content.trim().is_empty())
        .map(chat_completions_message)
        .collect::<Vec<_>>();
    if converted.is_empty() {
        return Err(LlmError::new(
            "bad_request",
            "messages must contain non-empty content",
            "request",
        ));
    }
    Ok(converted)
}

fn chat_completions_message(message: &ChatMessage) -> Value {
    let role = match message.role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
    };
    json!({
        "role": role,
        "content": [{"type": "text", "text": message.content.as_str()}],
    })
}

async fn send_chat_completions_request(
    client: &ChatCompletionsClient,
    payload: &Value,
    stream: bool,
) -> Result<reqwest::Response, LlmError> {
    let mut request = client
        .client
        .post(chat_completions_url(client.base_url.as_deref()))
        .bearer_auth(&client.api_key)
        .json(payload);
    if stream {
        request = request.header(header::ACCEPT, "text/event-stream");
    }
    let response = request.send().await.map_err(|err| {
        if err.is_timeout() {
            LlmError::timeout("http")
        } else {
            let context = if stream {
                "Chat Completions stream request failed"
            } else {
                "Chat Completions request failed"
            };
            LlmError::http(format!("{context}: {err}"))
        }
    })?;
    let status = response.status();
    if !status.is_success() {
        return Err(chat_status_error(status, response).await);
    }
    Ok(response)
}

async fn chat_status_error(status: StatusCode, response: reqwest::Response) -> LlmError {
    let detail = response.text().await.unwrap_or_default();
    let detail = truncate_error_detail(detail.trim(), 500);
    let message = if detail.is_empty() {
        format!("Chat Completions returned HTTP {}", status.as_u16())
    } else {
        format!(
            "Chat Completions returned HTTP {}: {detail}",
            status.as_u16()
        )
    };
    match status.as_u16() {
        401 | 403 => LlmError::config(message),
        400 | 404 | 422 => LlmError::new("bad_request", message, "http"),
        429 => LlmError::new("rate_limited", message, "http"),
        500..=599 => LlmError::new("upstream_unavailable", message, "http"),
        _ => LlmError::http(message),
    }
}

fn truncate_error_detail(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut truncated = value.chars().take(limit).collect::<String>();
    truncated.push_str("...");
    truncated
}

pub(crate) async fn non_stream_completion(
    client: &ChatCompletionsClient,
    provider: &str,
    model: &str,
    max_output_tokens: u64,
    messages: &[ChatMessage],
) -> Result<ChatOutcome, LlmError> {
    let recorder = MetricsRecorder::start();
    let payload = chat_completions_payload(messages, model, max_output_tokens, false)?;
    let response = send_chat_completions_request(client, &payload, false).await?;
    let body: Value = response.json().await.map_err(|err| {
        LlmError::provider(format!("invalid Chat Completions JSON: {err}"), "json")
    })?;
    let reply = extract_chat_completion_text(&body).ok_or_else(|| {
        LlmError::provider("Chat Completions returned empty text output", "provider")
    })?;
    let usage = extract_chat_completion_usage(&body);
    let metrics = recorder.finish(provider, model, false);

    Ok(ChatOutcome {
        reply,
        metrics,
        usage,
        fallback_used: false,
    })
}

pub(crate) async fn stream_completion(
    client: &ChatCompletionsClient,
    provider: &str,
    model: &str,
    max_output_tokens: u64,
    messages: &[ChatMessage],
) -> Result<ChatOutcome, LlmError> {
    let mut recorder = MetricsRecorder::start();
    let payload = chat_completions_payload(messages, model, max_output_tokens, true)?;
    let mut response = send_chat_completions_request(client, &payload, true).await?;
    let mut frame_buffer = Vec::new();
    let mut answer = String::new();
    let mut final_message = String::new();
    let mut usage = None;

    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|err| LlmError::http(format!("Chat Completions stream failed: {err}")))?
    {
        frame_buffer.extend_from_slice(&chunk);
        while let Some(frame) = take_sse_frame(&mut frame_buffer) {
            let Some(event) = parse_sse_frame(&frame)? else {
                continue;
            };
            recorder.mark_event();
            handle_chat_stream_event(
                &event.data,
                &mut recorder,
                &mut answer,
                &mut final_message,
                &mut usage,
            )?;
        }
    }
    if !frame_buffer.is_empty()
        && let Some(event) = parse_sse_frame(&frame_buffer)?
    {
        recorder.mark_event();
        handle_chat_stream_event(
            &event.data,
            &mut recorder,
            &mut answer,
            &mut final_message,
            &mut usage,
        )?;
    }

    if answer.trim().is_empty() {
        answer = final_message;
    }
    let reply = answer.trim().to_owned();
    if reply.is_empty() {
        return Err(LlmError::provider(
            "Chat Completions returned empty text output",
            "provider",
        ));
    }
    let metrics = recorder.finish(provider, model, true);

    Ok(ChatOutcome {
        reply,
        metrics,
        usage,
        fallback_used: false,
    })
}

fn handle_chat_stream_event(
    data: &str,
    recorder: &mut MetricsRecorder,
    answer: &mut String,
    final_message: &mut String,
    usage: &mut Option<TokenUsage>,
) -> Result<(), LlmError> {
    let value = serde_json::from_str::<Value>(data).map_err(|err| {
        LlmError::provider(
            format!("invalid Chat Completions stream JSON: {err}"),
            "sse",
        )
    })?;
    if let Some(event_usage) = extract_chat_completion_usage(&value) {
        *usage = Some(event_usage);
    }
    let Some(choices) = value.get("choices").and_then(Value::as_array) else {
        return Ok(());
    };
    for choice in choices {
        if let Some(delta) = choice
            .get("delta")
            .and_then(|delta| extract_content_value(delta.get("content")))
            && !delta.is_empty()
        {
            recorder.mark_token();
            answer.push_str(&delta);
        }
        if let Some(message) = choice
            .get("message")
            .and_then(|message| extract_content_value(message.get("content")))
            && !message.is_empty()
        {
            final_message.push_str(&message);
        }
    }
    Ok(())
}

fn extract_chat_completion_text(body: &Value) -> Option<String> {
    let choices = body.get("choices").and_then(Value::as_array)?;
    let mut parts = Vec::new();
    for choice in choices {
        let Some(text) = choice
            .get("message")
            .and_then(|message| extract_content_value(message.get("content")))
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
        else {
            continue;
        };
        parts.push(text);
    }
    let text = parts.join("");
    if text.trim().is_empty() {
        None
    } else {
        Some(text)
    }
}

fn extract_content_value(value: Option<&Value>) -> Option<String> {
    match value? {
        Value::String(text) => Some(text.to_owned()),
        Value::Array(items) => {
            let text = items
                .iter()
                .filter_map(|item| {
                    let item_type = item.get("type").and_then(Value::as_str);
                    if matches!(item_type, Some("text") | None) {
                        item.get("text").and_then(Value::as_str)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("");
            (!text.is_empty()).then_some(text)
        }
        _ => None,
    }
}

fn extract_chat_completion_usage(body: &Value) -> Option<TokenUsage> {
    let usage = body.get("usage")?;
    let input_tokens = usage
        .get("prompt_tokens")
        .or_else(|| usage.get("input_tokens"))
        .and_then(Value::as_u64);
    let cached_input_tokens = usage
        .get("prompt_tokens_details")
        .or_else(|| usage.get("input_tokens_details"))
        .and_then(|details| details.get("cached_tokens"))
        .and_then(Value::as_u64);
    let output_tokens = usage
        .get("completion_tokens")
        .or_else(|| usage.get("output_tokens"))
        .and_then(Value::as_u64);
    let total_tokens = usage.get("total_tokens").and_then(Value::as_u64);
    if matches!(
        (
            input_tokens,
            output_tokens,
            total_tokens,
            cached_input_tokens
        ),
        (None | Some(0), None | Some(0), None | Some(0), None)
    ) {
        return None;
    }
    Some(TokenUsage {
        input_tokens,
        cached_input_tokens,
        output_tokens,
        total_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        extract::State,
        http::{StatusCode, header},
        response::IntoResponse,
        routing::post,
    };
    use std::sync::Arc;
    use tokio::{net::TcpListener, sync::Mutex};

    #[derive(Debug)]
    struct MockChatState {
        bodies: Vec<String>,
        status: StatusCode,
        requests: Vec<Value>,
    }

    async fn mock_chat_handler(
        State(state): State<Arc<Mutex<MockChatState>>>,
        body: Body,
    ) -> impl IntoResponse {
        let bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap();
        let request: Value = serde_json::from_slice(&bytes).unwrap();
        let mut state = state.lock().await;
        state.requests.push(request);
        let body = state.bodies.remove(0);
        (
            state.status,
            [(header::CONTENT_TYPE, "text/event-stream")],
            body,
        )
    }

    async fn spawn_mock_chat(
        bodies: Vec<String>,
        status: StatusCode,
    ) -> (String, Arc<Mutex<MockChatState>>) {
        let state = Arc::new(Mutex::new(MockChatState {
            bodies,
            status,
            requests: Vec::new(),
        }));
        let app = Router::new()
            .route("/v1/chat/completions", post(mock_chat_handler))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}/v1"), state)
    }

    #[tokio::test]
    async fn non_stream_chat_completion_extracts_text_and_usage() {
        let (base_url, state) = spawn_mock_chat(
            vec![
                json!({
                    "choices": [{"message": {"content": "ok"}}],
                    "usage": {
                        "prompt_tokens": 2,
                        "completion_tokens": 3,
                        "total_tokens": 5,
                        "prompt_tokens_details": {"cached_tokens": 0}
                    }
                })
                .to_string(),
            ],
            StatusCode::OK,
        )
        .await;
        let client =
            ChatCompletionsClient::new("test-key", Some(&base_url), reqwest::Client::new());

        let outcome = non_stream_completion(
            &client,
            "openai",
            "gpt-test",
            1200,
            &[ChatMessage::user("hi")],
        )
        .await
        .unwrap();

        assert_eq!(outcome.reply, "ok");
        assert_eq!(outcome.usage.unwrap().cached_input_tokens, Some(0));
        assert_eq!(
            state.lock().await.requests[0]["messages"][0]["content"][0]["type"],
            "text"
        );
    }

    #[tokio::test]
    async fn stream_chat_completion_extracts_delta() {
        let body = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"你\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"好\"}}],\"usage\":{\"prompt_tokens\":1,\"completion_tokens\":2,\"total_tokens\":3}}\n\n",
            "data: [DONE]\n\n",
        )
        .to_owned();
        let (base_url, _state) = spawn_mock_chat(vec![body], StatusCode::OK).await;
        let client =
            ChatCompletionsClient::new("test-key", Some(&base_url), reqwest::Client::new());

        let outcome = stream_completion(
            &client,
            "openai",
            "gpt-test",
            1200,
            &[ChatMessage::user("hi")],
        )
        .await
        .unwrap();

        assert_eq!(outcome.reply, "你好");
        assert_eq!(outcome.usage.unwrap().total_tokens, Some(3));
    }

    #[tokio::test]
    async fn empty_stream_retries_non_stream() {
        let (base_url, state) = spawn_mock_chat(
            vec![
                "data: [DONE]\n\n".to_owned(),
                json!({"choices": [{"message": {"content": "retry ok"}}]}).to_string(),
            ],
            StatusCode::OK,
        )
        .await;
        let client =
            ChatCompletionsClient::new("test-key", Some(&base_url), reqwest::Client::new());

        let outcome = chat_completions_with_stream_fallback(
            true,
            &client,
            "openai",
            "gpt-test",
            1200,
            &[ChatMessage::user("hi")],
        )
        .await
        .unwrap();

        assert_eq!(outcome.reply, "retry ok");
        assert_eq!(state.lock().await.requests.len(), 2);
    }

    #[tokio::test]
    async fn non_stream_empty_reply_is_error() {
        let (base_url, _state) = spawn_mock_chat(
            vec![json!({"choices": [{"message": {"content": ""}}]}).to_string()],
            StatusCode::OK,
        )
        .await;
        let client =
            ChatCompletionsClient::new("test-key", Some(&base_url), reqwest::Client::new());

        let err = non_stream_completion(
            &client,
            "openai",
            "gpt-test",
            1200,
            &[ChatMessage::user("hi")],
        )
        .await
        .unwrap_err();

        assert_eq!(err.code, "provider_error");
    }

    #[tokio::test]
    async fn status_codes_are_classified() {
        let (base_url, _state) = spawn_mock_chat(
            vec!["rate limited".to_owned()],
            StatusCode::TOO_MANY_REQUESTS,
        )
        .await;
        let client =
            ChatCompletionsClient::new("test-key", Some(&base_url), reqwest::Client::new());

        let err = non_stream_completion(
            &client,
            "openai",
            "gpt-test",
            1200,
            &[ChatMessage::user("hi")],
        )
        .await
        .unwrap_err();

        assert_eq!(err.code, "rate_limited");
        assert!(err.message.contains("HTTP 429"));
    }

    #[test]
    fn custom_endpoint_is_used() {
        assert_eq!(
            chat_completions_url(Some("https://proxy.example/v1/")),
            "https://proxy.example/v1/chat/completions"
        );
    }
}
