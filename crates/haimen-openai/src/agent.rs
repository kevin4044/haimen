use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use haimen_core::provider::{
    AgentEventStream, AgentLogEvent, AgentOutput, AgentProvider, TextStream,
};

/// 会话历史默认保留的消息条数（含 user 与 assistant）
pub const DEFAULT_HISTORY_CAP: usize = 20;

/// 单行 SSE 的解析结果
#[derive(Debug, PartialEq, Eq)]
enum SseLine {
    /// 一段非空文本增量
    Delta(String),
    /// 流结束标记 `data: [DONE]`
    Done,
    /// 空行、注释或空 delta
    Skip,
}

/// OpenAI Chat Completions 的一条消息
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct ChatMessage {
    role: String,
    content: String,
    /// 本轮 id，仅用于进程内配对 user/assistant，不序列化进请求体
    #[serde(skip)]
    turn_id: u64,
}

/// OpenAI 兼容 Agent
///
/// 通过 HTTP `POST {base_url}/chat/completions` 调用模型。
/// `process` 走非流式 JSON；`process_stream` 按 SSE 边读边吐文本增量。
/// 多轮上下文由本进程内的会话历史维护（trait 不提供历史数组）。
pub struct OpenAIAgent {
    /// API 根地址，已去掉尾部 `/`
    base_url: String,
    /// 为空则不发送 Authorization 头
    api_key: Option<String>,
    /// 模型名
    model: String,
    /// HTTP 请求总超时（秒）；0 表示不额外设置超时
    timeout_secs: u64,
    /// 单个会话保留的消息条数上限
    history_cap: usize,
    /// 会话历史：session_id -> 消息队列
    history: Arc<Mutex<HashMap<String, VecDeque<ChatMessage>>>>,
    /// 复用连接的 HTTP 客户端
    http: reqwest::Client,
}

impl OpenAIAgent {
    /// 构造 OpenAI 兼容 Agent
    ///
    /// - `base_url`：API 根路径，尾部 `/` 会被去掉后再拼具体路径
    /// - `api_key`：为空或纯空白时不发送 `Authorization` 头
    /// - `model`：请求体中的模型名
    /// - `timeout_secs`：单次 HTTP 调用的总超时（秒）
    pub fn new(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
        timeout_secs: u64,
    ) -> Self {
        let api_key = api_key
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Self {
            base_url: normalize_base_url(&base_url.into()),
            api_key,
            model: model.into(),
            timeout_secs,
            history_cap: DEFAULT_HISTORY_CAP,
            history: Arc::new(Mutex::new(HashMap::new())),
            http: build_http_client(timeout_secs),
        }
    }

    /// 给请求加上可选的 Bearer 鉴权
    fn apply_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self.api_key.as_deref() {
            Some(key) if !key.is_empty() => builder.bearer_auth(key),
            _ => builder,
        }
    }

    /// 取出已有历史（含尚未完成的 in-flight user），追加本轮 user 并立刻写入。
    /// 同 `session_id` 的并发请求因此能带上进行中的轮次，而不是等 SSE 结束后才可见。
    /// `session_id == None` 时生成新 id。
    fn prepare_turn(
        &self,
        message: &str,
        session_id: Option<&str>,
    ) -> Result<(String, Vec<ChatMessage>, u64), String> {
        let sid = match session_id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(id) => id.to_string(),
            None => new_session_id(),
        };
        let turn_id = next_turn_id();
        let user = ChatMessage {
            role: "user".to_string(),
            content: message.to_string(),
            turn_id,
        };
        let mut guard = self
            .history
            .lock()
            .map_err(|e| format!("读取会话历史失败: {e}"))?;
        let queue = guard.entry(sid.clone()).or_default();
        let mut messages: Vec<ChatMessage> = queue.iter().cloned().collect();
        messages.push(user.clone());
        push_capped(queue, self.history_cap, user);
        Ok((sid, messages, turn_id))
    }

    /// `POST /chat/completions`。成功时返回响应体流；失败时读完错误正文再返回。
    async fn post_chat(
        &self,
        messages: &[ChatMessage],
        stream: bool,
    ) -> Result<reqwest::Response, String> {
        if self.base_url.is_empty() {
            return Err("openai provider 需要配置 base_url".to_string());
        }
        if self.model.trim().is_empty() {
            return Err("openai provider 需要配置 model".to_string());
        }
        let url = chat_completions_url(&self.base_url);
        let payload = build_chat_payload(&self.model, messages, stream);
        tracing::debug!(
          url = %url,
          model = %self.model,
          stream,
          timeout_secs = self.timeout_secs,
          "请求 OpenAI chat completions"
        );
        let started = Instant::now();
        let response = self
            .apply_auth(self.http.post(&url).json(&payload))
            .send()
            .await
            .map_err(|e| {
                let err = redact(&e.to_string(), self.api_key.as_deref());
                tracing::warn!(
                  error = %err,
                  elapsed_ms = started.elapsed().as_millis() as u64,
                  "OpenAI 请求失败"
                );
                format!("OpenAI 请求失败: {err}")
            })?;
        tracing::debug!(
          status = %response.status(),
          elapsed_ms = started.elapsed().as_millis() as u64,
          "OpenAI 响应头已到达"
        );
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return Err("认证失败，请检查 api_key".to_string());
        }
        if !(200..300).contains(&status) {
            let body = response.text().await.unwrap_or_default();
            let summary = summarize_body(&redact(&body, self.api_key.as_deref()));
            if summary.is_empty() {
                return Err(format!("OpenAI 请求失败（HTTP {status}）"));
            }
            return Err(format!("OpenAI 请求失败（HTTP {status}）: {summary}"));
        }
        Ok(response)
    }
}

#[async_trait]
impl AgentProvider for OpenAIAgent {
    fn name(&self) -> &str {
        "openai"
    }

    /// 非流式调用：`stream: false`，取 `choices[0].message.content`
    async fn process(
        &self,
        message: &str,
        session_id: Option<&str>,
        _work_dir: &str,
    ) -> Result<(AgentOutput, String), String> {
        if message.trim().is_empty() {
            return Err("消息为空，无法调用 OpenAI".to_string());
        }
        let (sid, messages, turn_id) = self.prepare_turn(message, session_id)?;
        let response = self.post_chat(&messages, false).await?;
        let body = response.text().await.map_err(|e| {
            format!(
                "读取 OpenAI 响应失败: {}",
                redact(&e.to_string(), self.api_key.as_deref())
            )
        })?;
        let text = extract_message_content(&body)?;
        commit_assistant(&self.history, &sid, self.history_cap, turn_id, &text)?;
        Ok((
            AgentOutput {
                text,
                events: Vec::new(),
            },
            sid,
        ))
    }

    /// SSE 流式调用：解析到 `delta.content` 即写入文本流，不先缓冲完整响应
    async fn process_stream(
        &self,
        message: &str,
        session_id: Option<&str>,
        _work_dir: &str,
    ) -> Result<(TextStream, String, AgentEventStream), String> {
        if message.trim().is_empty() {
            return Err("消息为空，无法调用 OpenAI".to_string());
        }
        let (sid, messages, turn_id) = self.prepare_turn(message, session_id)?;
        let response = self.post_chat(&messages, true).await?;
        let (text_tx, text_rx) = mpsc::channel::<String>(32);
        spawn_sse_reader(
            response,
            text_tx,
            Arc::clone(&self.history),
            sid.clone(),
            turn_id,
            self.history_cap,
        );
        let stream: TextStream = Box::pin(ReceiverStream::new(text_rx));
        // 本 provider 不产生 thinking / tool 事件，sender 立即丢弃则接收端立刻结束
        let (_events_tx, events_rx) = mpsc::channel::<AgentLogEvent>(1);
        Ok((stream, sid, events_rx))
    }

    /// `GET {base_url}/models`：200 可用，401/403 视为鉴权失败
    async fn check_available(&self) -> Result<(), String> {
        if self.base_url.is_empty() {
            return Err("openai provider 需要配置 base_url".to_string());
        }
        if self.model.trim().is_empty() {
            return Err("openai provider 需要配置 model".to_string());
        }
        let url = models_url(&self.base_url);
        tracing::debug!(url = %url, "检查 OpenAI 兼容服务是否可用");
        let started = Instant::now();
        match self.apply_auth(self.http.get(&url)).send().await {
            Ok(resp) => {
                let status = resp.status().as_u16();
                tracing::debug!(
                    status,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "OpenAI 模型列表已返回"
                );
                match status {
                    200 => Ok(()),
                    401 | 403 => Err("认证失败，请检查 api_key".to_string()),
                    code => Err(format!("OpenAI 模型列表请求失败（HTTP {code}）")),
                }
            }
            Err(e) => {
                let err = redact(&e.to_string(), self.api_key.as_deref());
                tracing::warn!(error = %err, "OpenAI 模型列表请求失败");
                Err(format!("OpenAI 模型列表请求失败: {err}"))
            }
        }
    }
}

/// 后台读 SSE：每解析出一个非空 delta 就发送，结束后把 assistant 插回对应 user 之后
fn spawn_sse_reader(
    response: reqwest::Response,
    tx: mpsc::Sender<String>,
    history: Arc<Mutex<HashMap<String, VecDeque<ChatMessage>>>>,
    sid: String,
    turn_id: u64,
    cap: usize,
) {
    let started = Instant::now();
    tokio::spawn(async move {
        let assistant = collect_sse(response, &tx).await;
        if let Err(err) = commit_assistant(&history, &sid, cap, turn_id, &assistant) {
            tracing::warn!(error = %err, "写入 OpenAI 会话历史失败");
        }
        drop(tx);
        tracing::debug!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            chars = assistant.chars().count(),
            "OpenAI 流式响应结束"
        );
    });
}

/// 边读字节边按行解析 SSE，返回已拼出的 assistant 文本
async fn collect_sse(response: reqwest::Response, tx: &mpsc::Sender<String>) -> String {
    let mut buf: Vec<u8> = Vec::new();
    let mut assistant = String::new();
    let mut byte_stream = response.bytes_stream();
    while let Some(item) = byte_stream.next().await {
        let chunk = match item {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(error = %e, "OpenAI SSE 读取中断");
                break;
            }
        };
        buf.extend_from_slice(&chunk);
        if !drain_complete_lines(&mut buf, &mut assistant, tx).await {
            return assistant;
        }
    }
    if !buf.is_empty() {
        let leftover = decode_line(std::mem::take(&mut buf));
        let _ = apply_sse_line(&leftover, &mut assistant, tx).await;
    }
    assistant
}

/// 处理缓冲区里所有完整行。接收端断开或遇到 `[DONE]` 时返回 false。
async fn drain_complete_lines(
    buf: &mut Vec<u8>,
    assistant: &mut String,
    tx: &mpsc::Sender<String>,
) -> bool {
    loop {
        let Some(pos) = buf.iter().position(|b| *b == b'\n') else {
            return true;
        };
        let raw: Vec<u8> = buf.drain(..=pos).collect();
        let line = decode_line(raw);
        if !apply_sse_line(&line, assistant, tx).await {
            return false;
        }
    }
}

/// 应用一行 SSE。返回 false 表示流应停止（结束标记或对端已关闭）。
async fn apply_sse_line(line: &str, assistant: &mut String, tx: &mpsc::Sender<String>) -> bool {
    match parse_sse_line(line) {
        SseLine::Delta(text) => {
            assistant.push_str(&text);
            tx.send(text).await.is_ok()
        }
        SseLine::Done => false,
        SseLine::Skip => true,
    }
}

/// 去掉行尾换行后按 UTF-8 解码；非法字节用替换字符，不中断流
fn decode_line(mut raw: Vec<u8>) -> String {
    if raw.last() == Some(&b'\n') {
        raw.pop();
    }
    if raw.last() == Some(&b'\r') {
        raw.pop();
    }
    String::from_utf8(raw)
        .unwrap_or_else(|err| String::from_utf8_lossy(err.as_bytes()).into_owned())
}

/// 创建带总超时的 HTTP 客户端；构建失败时退回默认客户端
fn build_http_client(timeout_secs: u64) -> reqwest::Client {
    let builder = reqwest::Client::builder();
    let builder = if timeout_secs == 0 {
        builder
    } else {
        builder.timeout(std::time::Duration::from_secs(timeout_secs))
    };
    builder.build().unwrap_or_else(|err| {
        tracing::warn!(error = %err, "创建 OpenAI HTTP 客户端失败，改用默认客户端");
        reqwest::Client::new()
    })
}

/// 去掉首尾空白和尾部 `/`
fn normalize_base_url(base_url: &str) -> String {
    base_url.trim().trim_end_matches('/').to_string()
}

/// `{base_url}/chat/completions`，`base_url` 尾部有无 `/` 结果相同
fn chat_completions_url(base_url: &str) -> String {
    format!("{}/chat/completions", normalize_base_url(base_url))
}

/// `{base_url}/models`
fn models_url(base_url: &str) -> String {
    format!("{}/models", normalize_base_url(base_url))
}

/// 非流式 / 流式请求体：`model`、`messages`、`stream`
fn build_chat_payload(model: &str, messages: &[ChatMessage], stream: bool) -> serde_json::Value {
    serde_json::json!({
      "model": model,
      "messages": messages,
      "stream": stream,
    })
}

/// 解析一行 SSE。非 `data:` 行、空行、空 content 都跳过。
fn parse_sse_line(line: &str) -> SseLine {
    let line = line.trim();
    if line.is_empty() || line.starts_with(':') {
        return SseLine::Skip;
    }
    let Some(data) = line.strip_prefix("data:") else {
        return SseLine::Skip;
    };
    let data = data.trim_start();
    if data == "[DONE]" {
        return SseLine::Done;
    }
    match extract_delta(data) {
        Some(content) => SseLine::Delta(content),
        None => SseLine::Skip,
    }
}

/// 从 chunk JSON 取 `choices[0].delta.content`；缺失或空串返回 None
fn extract_delta(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let content = value.pointer("/choices/0/delta/content")?.as_str()?;
    if content.is_empty() {
        None
    } else {
        Some(content.to_string())
    }
}

/// 从非流式响应取 `choices[0].message.content`
fn extract_message_content(body: &str) -> Result<String, String> {
    let value: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("解析 OpenAI 响应失败: {e}"))?;
    if let Some(err) = value.get("error") {
        let msg = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("未知错误");
        return Err(format!("OpenAI 返回错误: {msg}"));
    }
    let content = value
        .pointer("/choices/0/message/content")
        .and_then(|c| c.as_str())
        .map(str::trim)
        .unwrap_or("");
    if content.is_empty() {
        return Err("OpenAI 返回为空".to_string());
    }
    Ok(content.to_string())
}

/// 把 assistant 插到对应 in-flight user 之后；空回复或 cap=0 时不写入
fn commit_assistant(
    history: &Mutex<HashMap<String, VecDeque<ChatMessage>>>,
    sid: &str,
    cap: usize,
    turn_id: u64,
    assistant: &str,
) -> Result<(), String> {
    if cap == 0 || assistant.is_empty() {
        return Ok(());
    }
    let mut guard = history
        .lock()
        .map_err(|e| format!("写入会话历史失败: {e}"))?;
    let queue = guard.entry(sid.to_string()).or_default();
    let Some(user_pos) = queue
        .iter()
        .position(|m| m.turn_id == turn_id && m.role == "user")
    else {
        return Ok(());
    };
    let insert_at = user_pos + 1;
    if queue
        .get(insert_at)
        .is_some_and(|m| m.turn_id == turn_id && m.role == "assistant")
    {
        return Ok(());
    }
    queue.insert(
        insert_at,
        ChatMessage {
            role: "assistant".to_string(),
            content: assistant.to_string(),
            turn_id,
        },
    );
    while queue.len() > cap {
        queue.pop_front();
    }
    Ok(())
}

/// 追加一条消息，长度超过 `cap` 时丢掉最旧的
fn push_capped(queue: &mut VecDeque<ChatMessage>, cap: usize, msg: ChatMessage) {
    if cap == 0 {
        return;
    }
    queue.push_back(msg);
    while queue.len() > cap {
        queue.pop_front();
    }
}

/// 生成进程内唯一的会话 id（时间戳 + 递增计数，不引入额外 crate）
fn new_session_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("openai_{millis}_{n}")
}

/// 生成进程内唯一的轮次 id，用于把流式 assistant 插回对应 user 之后
fn next_turn_id() -> u64 {
    static SEQ: AtomicU64 = AtomicU64::new(1);
    SEQ.fetch_add(1, Ordering::Relaxed)
}

/// 错误文案里去掉 api_key，避免密钥进入日志或返回值
fn redact(text: &str, api_key: Option<&str>) -> String {
    match api_key.filter(|k| !k.is_empty()) {
        Some(key) => text.replace(key, "***"),
        None => text.to_string(),
    }
}

/// 截断错误正文，避免把整段响应塞进错误信息
fn summarize_body(body: &str) -> String {
    let trimmed = body.trim();
    let mut chars = trimmed.chars();
    let short: String = chars.by_ref().take(200).collect();
    if chars.next().is_some() {
        format!("{short}…")
    } else {
        short
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_build_url() {
        assert_eq!(
            chat_completions_url("http://127.0.0.1:8642/v1"),
            "http://127.0.0.1:8642/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("http://127.0.0.1:8642/v1/"),
            "http://127.0.0.1:8642/v1/chat/completions"
        );
        assert_eq!(
            chat_completions_url("http://127.0.0.1:8642/v1///"),
            "http://127.0.0.1:8642/v1/chat/completions"
        );
    }

    #[test]
    fn test_parse_sse_line() {
        let delta = r#"data: {"choices":[{"delta":{"content":"你"}}]}"#;
        assert_eq!(parse_sse_line(delta), SseLine::Delta("你".to_string()));
        assert_eq!(parse_sse_line(""), SseLine::Skip);
        assert_eq!(parse_sse_line(": keep-alive"), SseLine::Skip);
        assert_eq!(parse_sse_line("data: [DONE]"), SseLine::Done);
        assert_eq!(
            parse_sse_line(r#"data: {"choices":[{"delta":{"content":""}}]}"#),
            SseLine::Skip
        );
    }

    #[test]
    fn test_extract_delta() {
        let json = r#"{"choices":[{"delta":{"content":"好"}}]}"#;
        assert_eq!(extract_delta(json).as_deref(), Some("好"));
        assert_eq!(
            extract_delta(r#"{"choices":[{"delta":{"content":""}}]}"#),
            None
        );
        assert_eq!(extract_delta(r#"{"choices":[{"delta":{}}]}"#), None);
        assert_eq!(extract_delta("not-json"), None);
    }

    fn chat(role: &str, content: &str, turn_id: u64) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
            turn_id,
        }
    }

    fn roles_and_contents(queue: &VecDeque<ChatMessage>) -> Vec<(&str, &str)> {
        queue
            .iter()
            .map(|m| (m.role.as_str(), m.content.as_str()))
            .collect()
    }

    #[test]
    fn test_history_cap() {
        let mut queue = VecDeque::new();
        for i in 0..25 {
            push_capped(&mut queue, 20, chat("user", &i.to_string(), i as u64));
        }
        assert_eq!(queue.len(), 20);
        assert_eq!(queue.front().expect("队列非空").content, "5");
        assert_eq!(queue.back().expect("队列非空").content, "24");
    }

    #[test]
    fn test_in_flight_user_visible_to_next_prepare() {
        let agent = OpenAIAgent::new("http://127.0.0.1:1", None, "m", 0);
        let (sid, first, _) = agent
            .prepare_turn("hello", Some("sess"))
            .expect("首轮 prepare 应成功");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].content, "hello");
        let (_, second, _) = agent
            .prepare_turn("interrupt", Some(&sid))
            .expect("打断轮 prepare 应成功");
        assert_eq!(second.len(), 2);
        assert_eq!(second[0].content, "hello");
        assert_eq!(second[1].content, "interrupt");
        let guard = agent.history.lock().expect("历史锁");
        let queue = guard.get(&sid).expect("会话应已存在");
        assert_eq!(
            roles_and_contents(queue),
            vec![("user", "hello"), ("user", "interrupt")]
        );
    }

    #[test]
    fn test_assistant_inserts_after_matching_in_flight_user() {
        let agent = OpenAIAgent::new("http://127.0.0.1:1", None, "m", 0);
        let (sid, _, turn_a) = agent
            .prepare_turn("hello", Some("sess"))
            .expect("首轮 prepare 应成功");
        let (_, _, turn_b) = agent
            .prepare_turn("interrupt", Some(&sid))
            .expect("打断轮 prepare 应成功");
        commit_assistant(&agent.history, &sid, DEFAULT_HISTORY_CAP, turn_a, "reply-a")
            .expect("写入 A 的 assistant 应成功");
        {
            let guard = agent.history.lock().expect("历史锁");
            let queue = guard.get(&sid).expect("会话应已存在");
            assert_eq!(
                roles_and_contents(queue),
                vec![
                    ("user", "hello"),
                    ("assistant", "reply-a"),
                    ("user", "interrupt")
                ]
            );
        }
        commit_assistant(&agent.history, &sid, DEFAULT_HISTORY_CAP, turn_b, "reply-b")
            .expect("写入 B 的 assistant 应成功");
        let guard = agent.history.lock().expect("历史锁");
        let queue = guard.get(&sid).expect("会话应已存在");
        assert_eq!(
            roles_and_contents(queue),
            vec![
                ("user", "hello"),
                ("assistant", "reply-a"),
                ("user", "interrupt"),
                ("assistant", "reply-b")
            ]
        );
    }

    #[test]
    fn test_later_turn_finishing_first_does_not_reorder_pairs() {
        let agent = OpenAIAgent::new("http://127.0.0.1:1", None, "m", 0);
        let (sid, _, turn_a) = agent
            .prepare_turn("hello", Some("sess"))
            .expect("首轮 prepare 应成功");
        let (_, _, turn_b) = agent
            .prepare_turn("interrupt", Some(&sid))
            .expect("打断轮 prepare 应成功");
        commit_assistant(&agent.history, &sid, DEFAULT_HISTORY_CAP, turn_b, "reply-b")
            .expect("后开始的轮次先完成也应成功");
        commit_assistant(&agent.history, &sid, DEFAULT_HISTORY_CAP, turn_a, "reply-a")
            .expect("先开始的轮次后完成也应成功");
        let guard = agent.history.lock().expect("历史锁");
        let queue = guard.get(&sid).expect("会话应已存在");
        assert_eq!(
            roles_and_contents(queue),
            vec![
                ("user", "hello"),
                ("assistant", "reply-a"),
                ("user", "interrupt"),
                ("assistant", "reply-b")
            ]
        );
    }

    #[test]
    fn test_new_session_id() {
        let a = new_session_id();
        let b = new_session_id();
        assert_ne!(a, b);
        assert!(a.starts_with("openai_"));
        assert!(b.starts_with("openai_"));
    }

    #[test]
    fn test_payload_shape() {
        let messages = vec![chat("user", "hi", 0)];
        let payload = build_chat_payload("demo-model", &messages, false);
        assert_eq!(payload["model"], "demo-model");
        assert_eq!(payload["stream"], false);
        assert_eq!(payload["messages"][0]["role"], "user");
        assert_eq!(payload["messages"][0]["content"], "hi");
        assert!(payload["messages"][0].get("turn_id").is_none());
        assert!(payload.get("messages").and_then(|v| v.as_array()).is_some());
    }
}
