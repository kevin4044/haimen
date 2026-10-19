//! OpenAI 兼容 HTTP Agent。
//!
//! 连接任意实现了 `/chat/completions` 的服务（含 Hermes gateway），
//! 并以 SSE 流式返回文本增量。

mod agent;

pub use agent::{DEFAULT_HISTORY_CAP, OpenAIAgent};
