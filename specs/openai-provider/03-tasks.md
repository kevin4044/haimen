# 03 - 任务单

> 规模：单个任务。预期改动约 400–500 行（含测试）。
> 完成判定：`04-acceptance.sh` 退出码为 0。

---

### T1 — 新增 `haimen-openai` crate，实现 OpenAI 兼容 provider 并注册

### 目标

新增一个走 HTTP 的 Agent provider，连接任意 OpenAI 兼容的
`/chat/completions` 接口（本机场景是 Hermes gateway），**必须支持 SSE 流式**。

### 先读这三处，照抄它们的写法

1. `crates/haimen-core/src/provider.rs` —— `AgentProvider` trait 的精确签名、
   `TextStream` / `AgentEventStream` / `AgentOutput` 的类型
2. `crates/haimen-hermes/Cargo.toml` —— 新 crate 的 Cargo.toml 形状
3. `crates/haimen-hermes/src/agent.rs` —— provider 实现的结构、错误处理风格、
   单元测试的组织方式

### 要做的事

**A. 新建 crate**

```
crates/haimen-openai/
├── Cargo.toml
└── src/
    ├── lib.rs
    └── agent.rs
```

`Cargo.toml` 按 `02-design.md` 第 5 节给的模板写。
注意 `reqwest` 用 `{ workspace = true }`（workspace 里已有 reqwest 0.13，
带 `json` 和 `stream` feature）。

**B. 实现 `OpenAIAgent`**

按 `02-design.md` 第 7 节：

- `new(base_url, api_key, model, timeout_secs) -> Self`
- `impl AgentProvider for OpenAIAgent`
  - `name()` → `"openai"`
  - `process()` → 非流式 POST
  - `process_stream()` → **SSE 流式，边读边吐**
  - `check_available()` → `GET {base_url}/models`
- 自己维护对话历史（`Mutex<HashMap<String, VecDeque<ChatMessage>>>`），
  上限 20 条
- `session_id == None` 时生成新 id

**C. 注册到 workspace 和 provider 注册表**

1. 根 `Cargo.toml` 的 `[workspace] members` 里加 `"crates/haimen-openai"`
   （放在 `"crates/haimen-hermes"` 之后）
2. 根 `Cargo.toml` 的 `[dependencies]` 里加
   `haimen-openai = { path = "crates/haimen-openai", version = "0.1" }`
   （和现有那几个 crate 依赖的写法保持一致）
3. `src/agents/registry.rs`：
   - 顶部加 `use haimen_openai::OpenAIAgent;`
   - `builtin()` 里在 hermes 注册之后，加上 `02-design.md` 第 8 节给出的
     `.register("openai", "OpenAI 兼容", ...)` 代码块

**D. 单元测试**

按 `02-design.md` 第 9 节，写 6 个**不依赖网络**的单元测试。

### 硬性约束

- 库代码里**不允许** `unwrap()` / `expect()` / `panic!()`（测试代码除外）
- **不要**改 `web-ui/`（前端要 Node 构建，本阶段跳过）
- **不要**改 `src/config/settings.rs`
- **不要**改现有 4 个 provider 的行为
- **不要**引入新的第三方 crate（`reqwest` 已在 workspace）
- **不要**把 api_key 打进任何日志
- 所有新增公开项要有中文文档注释（与仓库现有风格一致）

### 改动白名单

只允许改动这些路径：

```
Cargo.toml
Cargo.lock
crates/haimen-openai/Cargo.toml
crates/haimen-openai/src/lib.rs
crates/haimen-openai/src/agent.rs
src/agents/registry.rs
specs/openai-provider/
```

超出任何一条，验收脚本会判 FAIL。

### 自检命令

改完后依次跑，全绿再交付：

```bash
cd <repo>
cargo fmt --all
cargo fmt --all -- --check
cargo clippy -- -D warnings
cargo test -- --test-threads=1
```
