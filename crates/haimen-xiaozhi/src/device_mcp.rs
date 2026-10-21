//! 设备侧 MCP 客户端。
//!
//! 这条链路只做一件事：把 haimen 的确定性管理操作（例如设置 assets 下载
//! 地址、重启设备）包装成 MCP `tools/call`，通过设备现有的 MQTT/WebSocket
//! 连接下发。它**不经过** LLM、Agent、ASR/TTS 或对话策略层。
//!
//! 设备是 MCP server，haimen 是 MCP client；消息外层由具体传输层实现，
//! 本模块只定义门面和错误类型，方便 HTTP/API 层与测试替换。

use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

/// 设备 MCP 调用失败。
#[derive(Debug, thiserror::Error)]
pub enum DeviceMcpError {
    /// 设备不在线（MQTT 连接不存在或写队列拒绝）。
    #[error("设备不在线")]
    Offline,
    /// 固件不支持该工具/协议（例如未升级到带 assets 工具的版本）。
    #[error("设备不支持该 MCP 工具: {0}")]
    Unsupported(String),
    /// 设备没有在超时内回复 JSON-RPC 响应。
    #[error("设备 MCP 调用超时")]
    Timeout,
    /// 设备返回 JSON-RPC error。
    #[error("设备 MCP 返回错误: {0}")]
    ToolError(String),
    /// 传输层/序列化失败。
    #[error("设备 MCP 传输失败: {0}")]
    Transport(String),
}

/// 管理面需要的设备 MCP 能力。
///
/// 只暴露 `tools/call`；不提供 `initialize`/`tools/list`，因为当前固件
/// 直接接受 `tools/call`，主题升级也不需要先发现工具列表。
#[async_trait]
pub trait DeviceMcpHub: Send + Sync {
    /// 调用设备工具并等待 JSON-RPC 响应。
    async fn call_tool(
        &self,
        client_id: &str,
        tool: &str,
        arguments: Value,
        timeout: Duration,
    ) -> Result<Value, DeviceMcpError>;

    /// 调用设备工具但不等待响应（用于 `self.reboot` 这类会重启的调用）。
    async fn notify_tool(
        &self,
        client_id: &str,
        tool: &str,
        arguments: Value,
    ) -> Result<(), DeviceMcpError>;
}

static DEVICE_MCP_HUB: RwLock<Option<Arc<dyn DeviceMcpHub>>> = RwLock::new(None);

/// 由 gateway / serve 在 MQTT 传输层启动后显式安装。
pub fn install_device_mcp_hub(hub: Arc<dyn DeviceMcpHub>) {
    let mut slot = DEVICE_MCP_HUB
        .write()
        .unwrap_or_else(|err| err.into_inner());
    *slot = Some(hub);
}

/// 取当前设备 MCP 门面；`None` = MQTT 传输层没跑（当前 WS 模式不提供）。
pub fn device_mcp_hub() -> Option<Arc<dyn DeviceMcpHub>> {
    let slot = DEVICE_MCP_HUB.read().unwrap_or_else(|err| err.into_inner());
    slot.clone()
}
