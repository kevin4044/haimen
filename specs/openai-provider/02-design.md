# 02 - 设计：新增 OpenAI 兼容 Agent Provider

## 1. 背景

haimen 现有的 4 个 Agent provider 全部是 **CLI 子进程**（claude-code / codex /
openclaw / hermes）。每次对话都要 fork 一个新进程并重新加载配置，实测在
Hermes 上单轮耗时 **9.4 秒**，无法用于语音场景。

但 Hermes 本身有**常驻 gateway**，暴露 OpenAI 兼容接口：

```
http://192.168.50.33:8642/p/<profile>/v1/chat/completions
```

实测同一句话：**CLI 9.4 秒 → HTTP 0.69 秒**（首 chunk 0.73 秒，支持 SSE 流式）。

本设计新增一个走 HTTP 的 provider，绕开进程启动开销。

## 2. 目标

- 新增 `openai` provider，兼容任意 OpenAI `/chat/completions` 接口
- **必须支持 SSE 流式**（否则流式 TTS 退化成批处理）
- 支持多轮上下文
- 不破坏任何现有 provider

## 3. 接口契约

已有 trait，定义在 `crates/haimen-core/src/provider.rs`：

```rust
pub type TextStream = Pin<Box<dyn Stream<Item = String> + Send>>;
pub type AgentEventStream = tokio::sync::mpsc::Receiver<AgentLogEvent>;

#[async_trait]
pub trait AgentProvider: Send + Sync {
    fn name(&self) -> &str;

    async fn process(
        &self,
        message: &str,
        session_id: Option<&str>,
        work_dir: &str,
    ) -> Result<(AgentOutput, String), String>;

    async fn process_stream(
        &self,
        message: &str,
        session_id: Option<&str>,
        work_dir: &str,
    ) -> Result<(TextStream, String, AgentEventStream), String>;

    async fn check_available(&self) -> Result<(), String>;
}
```

三处语义：
- `session_id == None` 表示新会话；返回值第二项是**新的 session_id**
- `process_stream` 返回的 `TextStream` 每 `Item` 是一段**文本增量**（不是整句）
- `check_available` 在启动/切换时调用，返回 `Err` 会终止 chat_loop

## 4. 非目标

- 不改前端 UI 的提供商下拉框（需要 pnpm 构建，本阶段不做；用 settings.toml 配置）
- 不改 `src/config/settings.rs`（`providers` 已是 `HashMap<String, HashMap<String,String>>`，字段自由）
- 不实现 tools/function calling（由服务端 agent 自行处理）
- 不实现 thinking / tool_use 事件上报（`AgentEventStream` 可为空流）

## 5. 新增 crate 结构

```
crates/haimen-openai/
├── Cargo.toml
└── src/
    ├── lib.rs        // pub use agent::{OpenAIAgent, ...}; 常量
    └── agent.rs      // 实现 + 单元测试
```

`Cargo.toml` 照抄 `crates/haimen-hermes/Cargo.toml` 的形状：

```toml
[package]
name = "haimen-openai"
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
description = "AI 网关 OpenAI 兼容 Agent（HTTP /chat/completions，SSE 流式）"
license = "MIT"
repository = "https://github.com/kevin4044/haimen"

[dependencies]
haimen-core = { path = "../haimen-core", version = "0.1" }
async-trait = { workspace = true }
futures-util = { workspace = true }
serde = { workspace = true }
serde_json = { workspace = true }
tokio = { workspace = true }
tokio-stream = { workspace = true }
tracing = { workspace = true }
reqwest = { workspace = true }
```

## 6. 配置

配置从 `GatewayConfig.providers` 读，**不需要改 settings.rs**：

```toml
[gateway]
active_provider = "openai"

[gateway.providers.openai]
base_url = "http://192.168.50.33:8642/p/u5bb6-u5ead-u7ba1-u5bb6/v1"
api_key = "kgU..."
model = "u5bb6-u5ead-u7ba1-u5bb6"
```

字段规则：
- `base_url`：**去掉尾部 `/`**，拼 `/chat/completions`
- `api_key`：可选，为空则不发送 `Authorization` 头
- `model`：必填，缺失时 `check_available` 返回明确错误
- 三者缺失时返回明确错误信息，不要 panic

## 7. 实现要点

### 7.1 `OpenAIAgent` 结构

```rust
pub struct OpenAIAgent {
    base_url: String,
    api_key: Option<String>,
    model: String,
    timeout_secs: u64,
    history_cap: usize,
    // 会话历史：session_id -> 轮次
    history: Mutex<HashMap<String, VecDeque<ChatMessage>>>,
    http: reqwest::Client,
}
```

### 7.2 多轮上下文

**trait 只给了 `message` 和 `session_id`，没有历史数组**，所以 provider 自己维护：

- `session_id == None`：生成新 id（用现有依赖，不要新增 uuid/rand crate 时可
  用时间戳 + 进程内计数器拼串），历史为空
- `session_id == Some(id)`：取出历史，追加 user 消息后一起发送
- 每次成功后把 user 消息和 assistant 回复写回历史
- 历史超过 `history_cap`（默认 20 条）时从头部丢弃

### 7.3 非流式 `process`

`POST {base_url}/chat/completions`，body：

```json
{"model": "...", "messages": [...], "stream": false}
```

取 `choices[0].message.content` 作为 `AgentOutput.text`，`events` 置空。

### 7.4 流式 `process_stream`（关键）

`POST {base_url}/chat/completions`，`"stream": true`。

响应是 SSE，逐行格式：

```
data: {"choices":[{"delta":{"content":"你"}}]}
data: {"choices":[{"delta":{"content":"好"}}]}
data: [DONE]
```

要求：
- **边读边吐**：解析到一个 `delta.content` 就通过 `TextStream` 推出去，
  **不允许先收完整响应再 yield**（否则整个改动失去意义）
- 忽略空 `content`（有些服务会先发一个空 delta）
- 遇到 `data: [DONE]` 结束
- 非 `data:` 开头的行（空行、注释）跳过
- 服务端中途断开时，已吐出的文本正常结束，不要 panic

实现方式建议：`reqwest::Response::bytes_stream()` + `tokio_stream` 包装，
用 `futures_util::stream::unfold` 或内部 spawn 一个 Task + `mpsc` 通道。

### 7.5 `AgentEventStream`

本 provider 不产生 thinking / tool_use 事件，返回一个立即关闭的空 channel：

```rust
let (_tx, rx) = tokio::sync::mpsc::channel(1);  // _tx 立即 drop 则 rx 立即结束
Ok((stream, sid, rx))
```

注意：不要 `tokio::spawn` 一个什么都不做的任务，也不要让 `tx` 存活到流结束。

### 7.6 `check_available`

`GET {base_url}/models`：
- 200 → `Ok(())`
- 401/403 → `Err("认证失败，请检查 api_key")`
- 其他 / 网络错误 → `Err` 带上状态码或错误摘要

### 7.7 错误处理

- 所有失败用 `Result<_, String>`，**不要在库代码里 `unwrap()` / `expect()` / `panic!()`**
  （测试代码可以）
- `tracing::debug!` / `warn!` 记录请求耗时和错误，但**不要打印 api_key**

## 8. 注册

在 `src/agents/registry.rs`：

1. 顶部加 `use haimen_openai::OpenAIAgent;`
2. `builtin()` 里增加一段（位置放在 hermes 之后）：

```rust
registry
    .register("openai", "OpenAI 兼容", |config| {
        let p = config.providers.get("openai");
        let base_url = p
            .and_then(|p| p.get("base_url"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "openai provider 需要在 [gateway.providers.openai] 配置 base_url".to_string()
            })?;
        let model = p
            .and_then(|p| p.get("model"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                "openai provider 需要在 [gateway.providers.openai] 配置 model".to_string()
            })?;
        let api_key = p
            .and_then(|p| p.get("api_key"))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        Ok(Box::new(OpenAIAgent::new(
            base_url,
            api_key,
            model,
            config.agent_timeout_secs,
        )))
    })
    .expect("内置 Agent openai 注册失败");
```

注意：`registry.register` 的闭包返回 `Result<Box<dyn AgentProvider>, String>`，
直接 `?` 传播即可；`.expect` 只用于「注册表本身损坏」这种程序错误。

## 9. 测试要求

单元测试放在 `crates/haimen-openai/src/agent.rs` 的 `#[cfg(test)] mod tests`，
**不依赖网络**（纯逻辑测试）：

1. `test_build_url` —— `base_url` 尾部带 `/` 和不带 `/` 都能拼出正确 URL
2. `test_parse_sse_line` —— `data: {...}` / 空行 / `data: [DONE]` 的解析
3. `test_extract_delta` —— 从 chunk JSON 里取 `delta.content`，空 delta 返回 None
4. `test_history_cap` —— 超过上限后旧消息被丢弃
5. `test_new_session_id` —— 连续两次调用生成不同 id
6. `test_payload_shape` —— 非流式请求体包含 model / messages / stream

**不要写需要网络的集成测试。**

## 10. 明确不做

- 不要改 `web-ui/`（前端构建需要 Node，本阶段跳过）
- 不要改 `src/config/settings.rs`
- 不要改现有 4 个 provider 的行为
- 不要动 `specs/` 目录之外的其他文件
- 不要引入新的第三方 crate（`reqwest` 已在 workspace）
